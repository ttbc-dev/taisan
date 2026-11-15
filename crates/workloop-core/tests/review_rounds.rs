//! Review-round and repair-decision contract: rounds are append-only
//! per head, dispositions cover the exact finding set at decision
//! time, repair unlocks only on the two prerequisites, and reassessment
//! is bounded by the Work-wide fix budget.

use workloop_core::command::FindingChoice;
use workloop_core::domain::WorkPolicy;
use workloop_core::identity::{CommitRevision, HostInstanceId, RepositoryId, RepositoryRef};
use workloop_core::review::{
    Finding, FindingDecision, FindingDisposition, RepairAttempt, RepairGate, RepairPrerequisite,
    ReviewError, ReviewRepair,
};

fn repo() -> RepositoryRef {
    RepositoryRef::new(
        HostInstanceId::trusted("review-host"),
        RepositoryId::trusted("review-repo"),
    )
}

fn head(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn finding(id: &str) -> Finding {
    Finding {
        finding_id: id.into(),
        content_digest: Some(format!("digest-{id}")),
    }
}

fn conflicting_finding(id: &str) -> Finding {
    Finding {
        finding_id: id.into(),
        content_digest: Some(format!("other-{id}")),
    }
}

fn disposition(id: &str, choice: FindingChoice) -> FindingDisposition {
    FindingDisposition {
        finding_id: id.into(),
        disposition: choice,
    }
}

fn decide_all(ledger: &mut ReviewRepair, round_id: &str, head: &CommitRevision, ids: &[&str]) {
    ledger
        .record_decisions(
            round_id,
            head,
            ids.iter()
                .map(|id| disposition(id, FindingChoice::Fix))
                .collect(),
        )
        .unwrap();
}

/// Drive one repair+reassessment cycle on `head`: open its round,
/// record one finding and its disposition, then attempt the repair.
fn cycle(ledger: &mut ReviewRepair, head: &CommitRevision, round_id: &str) -> RepairAttempt {
    ledger.record_current_head(head.clone());
    ledger.open_round(round_id, head.clone()).unwrap();
    let finding_id = format!("{round_id}-f");
    ledger
        .append_finding(round_id, finding(&finding_id))
        .unwrap();
    decide_all(ledger, round_id, head, &[finding_id.as_str()]);
    ledger.begin_repair(round_id).unwrap()
}

#[test]
fn a_recorded_review_round_is_append_only() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    ledger.append_finding("round-1", finding("f-1")).unwrap();
    ledger.append_finding("round-1", finding("f-2")).unwrap();
    let recorded = ledger.round("round-1").unwrap().clone();

    // Re-observing a recorded finding with different content is a
    // conflict, never an edit.
    assert!(matches!(
        ledger.append_finding("round-1", conflicting_finding("f-1")),
        Err(ReviewError::ConflictingFinding { .. })
    ));
    // An identical re-observation is idempotent.
    ledger.append_finding("round-1", finding("f-1")).unwrap();
    // The round id cannot be rebound to another head.
    assert!(matches!(
        ledger.open_round("round-1", head("head-other")),
        Err(ReviewError::ConflictingRound { .. })
    ));
    // A second round id cannot claim an already-recorded head.
    assert!(matches!(
        ledger.open_round("round-2", h1.clone()),
        Err(ReviewError::HeadAlreadyRecorded { .. })
    ));
    // Findings cannot enter a round that was never recorded.
    assert!(matches!(
        ledger.append_finding("round-x", finding("f-9")),
        Err(ReviewError::UnknownRound { .. })
    ));
    assert!(matches!(
        ledger.record_decisions("round-x", &h1, vec![]),
        Err(ReviewError::UnknownRound { .. })
    ));

    assert_eq!(
        ledger.round("round-1").unwrap(),
        &recorded,
        "refused operations leave the recorded round byte-identical"
    );
    assert_eq!(recorded.head_sha, h1);
    assert_eq!(
        recorded
            .findings()
            .iter()
            .map(|f| f.finding_id.as_str())
            .collect::<Vec<_>>(),
        ["f-1", "f-2"]
    );
}

#[test]
fn same_head_finding_growth_grows_the_required_decision_set() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    ledger.append_finding("round-1", finding("f-1")).unwrap();
    decide_all(&mut ledger, "round-1", &h1, &["f-1"]);
    assert_eq!(ledger.repair_gate("round-1").unwrap(), RepairGate::Unlocked);

    // A finding observed later at the same head appends to the round
    // and grows its required decision set.
    ledger.append_finding("round-1", finding("f-2")).unwrap();
    assert_eq!(
        ledger.repair_gate("round-1").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        },
        "the earlier decision covers fewer findings than the grown round"
    );
    // A partial decision submission is invalid and records nothing.
    let decisions_before = ledger.decisions().len();
    assert!(matches!(
        ledger.record_decisions("round-1", &h1, vec![disposition("f-1", FindingChoice::Fix)]),
        Err(ReviewError::Coverage { .. })
    ));
    assert_eq!(ledger.decisions().len(), decisions_before);

    // Repair stays blocked and no fix invocation is charged.
    let RepairAttempt::Held { missing } = ledger.begin_repair("round-1").unwrap() else {
        panic!("repair must be held on the undecided finding")
    };
    assert!(missing.contains(&RepairPrerequisite::FindingsDecided));
    assert_eq!(ledger.fix_invocations_used(), 0);
    assert!(ledger.passes().is_empty());

    // Covering the grown set exactly unlocks repair again.
    decide_all(&mut ledger, "round-1", &h1, &["f-1", "f-2"]);
    assert_eq!(ledger.repair_gate("round-1").unwrap(), RepairGate::Unlocked);
}

#[test]
fn a_head_change_invalidates_decisions_bound_to_the_previous_head() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    let h2 = head("head-2");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    ledger.append_finding("round-1", finding("f-1")).unwrap();
    ledger.append_finding("round-1", finding("f-2")).unwrap();
    decide_all(&mut ledger, "round-1", &h1, &["f-1", "f-2"]);
    assert_eq!(ledger.repair_gate("round-1").unwrap(), RepairGate::Unlocked);

    ledger.record_current_head(h2);

    // Coverage is still complete, but the decisions' decided_head no
    // longer matches the current head: only the head prerequisite is
    // missing, and repair stays blocked.
    assert_eq!(
        ledger.repair_gate("round-1").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::CurrentHead]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-1").unwrap(),
        RepairAttempt::Held { .. }
    ));
    // The prior decisions remain recorded as evidence — they authorize
    // nothing at the new head, but they are never rewritten.
    assert_eq!(ledger.decisions().len(), 2);
    assert!(
        ledger
            .decisions()
            .iter()
            .all(|d| d.decided_head == h1 && d.round_id == "round-1")
    );
    // A stale round accepts no new decisions either.
    assert!(matches!(
        ledger.record_decisions(
            "round-1",
            &h1,
            vec![
                disposition("f-1", FindingChoice::Fix),
                disposition("f-2", FindingChoice::Fix),
            ],
        ),
        Err(ReviewError::StaleRound { .. })
    ));
    assert_eq!(ledger.decisions().len(), 2);
}

#[test]
fn repair_unlocks_only_when_head_matches_decided_head_and_every_finding_is_decided() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    let h2 = head("head-2");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    ledger.append_finding("round-1", finding("f-1")).unwrap();
    decide_all(&mut ledger, "round-1", &h1, &["f-1"]);

    // Missing prerequisite one, independently: the head still matches
    // decided_head but f-2 has no recorded disposition.
    ledger.append_finding("round-1", finding("f-2")).unwrap();
    assert_eq!(
        ledger.repair_gate("round-1").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-1").unwrap(),
        RepairAttempt::Held { .. }
    ));
    assert_eq!(ledger.fix_invocations_used(), 0);

    // Restore full coverage, then move the head: missing prerequisite
    // two, independently — every finding is decided but the current
    // head no longer matches decided_head.
    decide_all(&mut ledger, "round-1", &h1, &["f-1", "f-2"]);
    ledger.record_current_head(h2);
    assert_eq!(
        ledger.repair_gate("round-1").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::CurrentHead]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-1").unwrap(),
        RepairAttempt::Held { .. }
    ));
    assert_eq!(ledger.fix_invocations_used(), 0);

    // Both prerequisites together unlock the pass. A legal backward
    // head move restores the decided head.
    ledger.record_current_head(h1.clone());
    assert_eq!(ledger.repair_gate("round-1").unwrap(), RepairGate::Unlocked);
    match ledger.begin_repair("round-1").unwrap() {
        RepairAttempt::Started { pass } => {
            assert_eq!(pass.ordinal, 1);
            assert_eq!(pass.round_id, "round-1");
            assert_eq!(pass.decided_head, h1);
        }
        other => panic!("an unlocked gate under budget must start a pass: {other:?}"),
    }
    assert_eq!(ledger.fix_invocations_used(), 1);
    assert_eq!(ledger.passes().len(), 1);
}

#[test]
fn reassessment_is_bounded_by_the_work_wide_fix_budget() {
    let bound = WorkPolicy::default().max_fix_invocations;
    assert_eq!(bound, 3, "the documented default correction budget");
    let mut ledger = ReviewRepair::new(bound);

    // Drive repair+reassessment to the bound: each cycle publishes a
    // new head, re-reviews it, decides its findings, and starts the
    // next bounded pass.
    for i in 1..=bound {
        let h = head(&format!("head-{i}"));
        let attempt = cycle(&mut ledger, &h, &format!("round-{i}"));
        assert!(
            matches!(attempt, RepairAttempt::Started { .. }),
            "pass {i} of {bound} must start: {attempt:?}"
        );
    }
    assert_eq!(ledger.fix_invocations_used(), bound);
    assert_eq!(ledger.passes().len(), bound as usize);
    assert_eq!(
        ledger
            .passes()
            .iter()
            .map(|pass| pass.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );

    // The next eligible repair reaches the bound: exhaustion is
    // recorded and the attempt escalates instead of starting a pass.
    let h4 = head("head-4");
    ledger.record_current_head(h4.clone());
    ledger.open_round("round-4", h4.clone()).unwrap();
    ledger
        .append_finding("round-4", finding("round-4-f"))
        .unwrap();
    decide_all(&mut ledger, "round-4", &h4, &["round-4-f"]);
    assert_eq!(ledger.repair_gate("round-4").unwrap(), RepairGate::Unlocked);

    let attempt = ledger.begin_repair("round-4").unwrap();
    let RepairAttempt::Escalated { exhaustion } = attempt else {
        panic!("the bound must escalate, not start another pass: {attempt:?}")
    };
    assert_eq!(exhaustion.bound, "recovery.max_fix_invocations");
    assert_eq!(exhaustion.fix_invocations_used, bound);
    assert_eq!(exhaustion.max_fix_invocations, bound);
    assert_eq!(exhaustion.round_id, "round-4");
    assert_eq!(exhaustion.head_sha, h4);
    assert_eq!(
        ledger.passes().len(),
        bound as usize,
        "bound exhaustion never starts another pass"
    );
    assert_eq!(ledger.fix_invocations_used(), bound);
    assert_eq!(ledger.exhaustion(), Some(&exhaustion));

    // Repeated attempts escalate on the same recorded exhaustion.
    let again = ledger.begin_repair("round-4").unwrap();
    assert!(matches!(again, RepairAttempt::Escalated { .. }));
    assert_eq!(ledger.passes().len(), bound as usize);
    assert_eq!(
        ledger.exhaustion().map(|e| e.round_id.as_str()),
        Some("round-4"),
        "the exhaustion record is written once and never rewritten"
    );
}

#[test]
fn per_finding_disposition_evidence_survives_repair() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    for id in ["f-1", "f-2", "f-3"] {
        ledger.append_finding("round-1", finding(id)).unwrap();
    }
    ledger
        .record_decisions(
            "round-1",
            &h1,
            vec![
                disposition("f-1", FindingChoice::Fix),
                disposition("f-2", FindingChoice::Dismiss),
                disposition("f-3", FindingChoice::Accept),
            ],
        )
        .unwrap();
    let recorded: Vec<FindingDecision> = ledger
        .decisions()
        .iter()
        .filter(|d| d.round_id == "round-1")
        .cloned()
        .collect();
    assert_eq!(recorded.len(), 3);

    // Run repair passes and re-review on successor heads; every
    // original disposition record must persist verbatim.
    for i in 2..=3 {
        let h = head(&format!("head-{i}"));
        cycle(&mut ledger, &h, &format!("round-{i}"));
    }

    for original in &recorded {
        assert!(
            ledger.decisions().contains(original),
            "disposition evidence for {} was lost across repair",
            original.finding_id
        );
    }
    let dispositions = ledger.dispositions_for("round-1");
    assert_eq!(dispositions.len(), 3);
    assert_eq!(dispositions["f-1"], FindingChoice::Fix);
    assert_eq!(dispositions["f-2"], FindingChoice::Dismiss);
    assert_eq!(dispositions["f-3"], FindingChoice::Accept);
    // The old round and its decided head are retained as history.
    let round = ledger.round("round-1").unwrap();
    assert_eq!(round.head_sha, h1);
    assert_eq!(round.findings().len(), 3);
}

#[test]
fn a_round_without_a_fix_selection_holds_instead_of_consuming_budget() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);

    // A zero-finding round has vacuous coverage but selects nothing
    // to repair — it gets an empty assessment, not a fix pass.
    let h_empty = head("head-empty");
    ledger.record_current_head(h_empty.clone());
    ledger.open_round("round-empty", h_empty).unwrap();
    assert_eq!(
        ledger.repair_gate("round-empty").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-empty").unwrap(),
        RepairAttempt::Held { .. }
    ));

    // An all-Dismiss/Accept round is fully dispositioned but has
    // nothing to fix.
    let h_dismiss = head("head-dismiss");
    ledger.record_current_head(h_dismiss.clone());
    ledger
        .open_round("round-dismiss", h_dismiss.clone())
        .unwrap();
    ledger
        .append_finding("round-dismiss", finding("f-1"))
        .unwrap();
    ledger
        .append_finding("round-dismiss", finding("f-2"))
        .unwrap();
    ledger
        .record_decisions(
            "round-dismiss",
            &h_dismiss,
            vec![
                disposition("f-1", FindingChoice::Dismiss),
                disposition("f-2", FindingChoice::Accept),
            ],
        )
        .unwrap();
    assert_eq!(
        ledger.repair_gate("round-dismiss").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-dismiss").unwrap(),
        RepairAttempt::Held { .. }
    ));

    // An all-Wait round is fully covered but unresolved — it holds
    // for Attention rather than starting a fix pass.
    let h_wait = head("head-wait");
    ledger.record_current_head(h_wait.clone());
    ledger.open_round("round-wait", h_wait.clone()).unwrap();
    ledger.append_finding("round-wait", finding("f-1")).unwrap();
    ledger
        .record_decisions(
            "round-wait",
            &h_wait,
            vec![disposition("f-1", FindingChoice::Wait)],
        )
        .unwrap();
    assert_eq!(
        ledger.repair_gate("round-wait").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        }
    );
    assert!(matches!(
        ledger.begin_repair("round-wait").unwrap(),
        RepairAttempt::Held { .. }
    ));

    assert_eq!(ledger.fix_invocations_used(), 0);
    assert!(ledger.passes().is_empty());
    assert!(ledger.exhaustion().is_none());
}

#[test]
fn a_fix_selection_in_the_exact_set_unlocks_repair() {
    let mut ledger = ReviewRepair::new(WorkPolicy::default().max_fix_invocations);
    let h1 = head("head-1");
    ledger.record_current_head(h1.clone());
    ledger.open_round("round-1", h1.clone()).unwrap();
    for id in ["f-fix", "f-dismiss", "f-accept", "f-wait"] {
        ledger.append_finding("round-1", finding(id)).unwrap();
    }
    // The exact-set judgment selects one occurrence for repair; the
    // other dispositions settle or wait independently of the pass.
    ledger
        .record_decisions(
            "round-1",
            &h1,
            vec![
                disposition("f-fix", FindingChoice::Fix),
                disposition("f-dismiss", FindingChoice::Dismiss),
                disposition("f-accept", FindingChoice::Accept),
                disposition("f-wait", FindingChoice::Wait),
            ],
        )
        .unwrap();
    assert_eq!(ledger.repair_gate("round-1").unwrap(), RepairGate::Unlocked);
    match ledger.begin_repair("round-1").unwrap() {
        RepairAttempt::Started { pass } => {
            assert_eq!(pass.ordinal, 1);
            assert_eq!(pass.round_id, "round-1");
            assert_eq!(pass.decided_head, h1);
        }
        other => panic!("a selected fix must start a pass: {other:?}"),
    }
    assert_eq!(ledger.fix_invocations_used(), 1);

    // Re-deciding the only Fix selection to Dismiss closes the gate
    // again: the latest exact-set judgment selects nothing to repair.
    let h2 = head("head-2");
    ledger.record_current_head(h2.clone());
    ledger.open_round("round-2", h2.clone()).unwrap();
    ledger.append_finding("round-2", finding("g-1")).unwrap();
    ledger
        .record_decisions("round-2", &h2, vec![disposition("g-1", FindingChoice::Fix)])
        .unwrap();
    assert_eq!(ledger.repair_gate("round-2").unwrap(), RepairGate::Unlocked);
    ledger
        .record_decisions(
            "round-2",
            &h2,
            vec![disposition("g-1", FindingChoice::Dismiss)],
        )
        .unwrap();
    assert_eq!(
        ledger.repair_gate("round-2").unwrap(),
        RepairGate::Blocked {
            missing: vec![RepairPrerequisite::FindingsDecided]
        },
        "the latest disposition governs: re-deciding to Dismiss unselects repair"
    );
    assert!(matches!(
        ledger.begin_repair("round-2").unwrap(),
        RepairAttempt::Held { .. }
    ));
    assert_eq!(ledger.fix_invocations_used(), 1);
}
