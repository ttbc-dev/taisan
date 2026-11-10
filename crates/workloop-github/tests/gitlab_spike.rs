//! U9.4 — named fixture-driven runs of the narrow GitLab spike harness.
//!
//! Denial, lost-response and restart each produce a retained evidence
//! record (a `SpikeJournal`) with per-effect receipts; every applied
//! write's receipt is reassessed by a fresh head read; live rows report
//! `not_authorized` without the recorded authorization. Fixture runs
//! support contract claims only — no live host is contacted.

use workloop_core::identity::{
    HostActorId, HostActorRef, HostInstanceId, RepositoryId, RepositoryRef,
};
use workloop_github::gitlab_spike::*;

fn scope() -> SpikeScope {
    SpikeScope {
        host: HostInstanceId::trusted("gitlab-spike-host"),
        repository: RepositoryId::trusted("isolated-project"),
        branch_prefix: "spike/u94/".into(),
        actor: HostActorRef {
            host: HostInstanceId::trusted("gitlab-spike-host"),
            actor: HostActorId::trusted("spike-actor"),
        },
    }
}

fn repo() -> RepositoryRef {
    scope().repository()
}

fn row(id: &str, op: SpikeOp) -> SpikeRow {
    SpikeRow {
        id: id.into(),
        lane: RowLane::Fixture,
        op,
    }
}

fn live_plan() -> Vec<SpikeRow> {
    plan()
        .into_iter()
        .map(|r| SpikeRow {
            lane: RowLane::Live,
            ..r
        })
        .collect()
}

/// The narrow plan: push, open, comment — each head-bound — then a
/// guarded advance, a stale-head comment abort, and a fresh head read.
fn plan() -> Vec<SpikeRow> {
    vec![
        row(
            "r1-push",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: None,
                produced: "head-1".into(),
            },
        ),
        row(
            "r2-create",
            SpikeOp::MrCreate {
                source_branch: "spike/u94/w".into(),
                target_branch: "main".into(),
            },
        ),
        row(
            "r3-comment",
            SpikeOp::CommentPost {
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "spike note".into(),
            },
        ),
        row(
            "r4-push-advance",
            SpikeOp::GuardedPush {
                branch: "spike/u94/w".into(),
                expected_prior: Some("head-1".into()),
                produced: "head-2".into(),
            },
        ),
        // Recorded against the superseded head: the write must abort.
        row(
            "r5-stale-comment",
            SpikeOp::CommentPost {
                source_branch: "spike/u94/w".into(),
                expected_head: "head-1".into(),
                body: "stale note".into(),
            },
        ),
        row(
            "r6-head-read",
            SpikeOp::HeadRead {
                target: HeadTarget::ChangeFromBranch {
                    source_branch: "spike/u94/w".into(),
                },
            },
        ),
    ]
}

fn record<'a>(journal: &'a SpikeJournal, row_id: &str) -> &'a RowRecord {
    journal
        .records
        .iter()
        .find(|r| r.row_id == row_id)
        .unwrap_or_else(|| panic!("row {row_id} recorded"))
}

fn applied_revision(record: &RowRecord) -> &workloop_core::identity::CommitRevision {
    match &record.outcome {
        RowOutcome::Receipt(WriteReceipt::Applied { revision, .. }) => revision,
        other => panic!("{}: expected applied receipt: {other:?}", record.row_id),
    }
}

/// Persist a scenario's journal under the test tmpdir — the retained
/// evidence record is inspectable, and serializes for the restart path.
fn persist(scenario: &str, journal: &SpikeJournal) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("u94-spike");
    std::fs::create_dir_all(&dir).unwrap();
    let json = serde_json::to_string_pretty(journal).unwrap();
    std::fs::write(dir.join(format!("{scenario}.json")), &json).unwrap();
}

/// The named fixture-driven run for U9.4's fault cases: denial,
/// lost-response and restart each produce a retained evidence record
/// with per-effect receipts.
#[test]
fn denial_lost_response_and_restart_retain_evidence() {
    // --- Denial: the actor's permission for comment writes is denied.
    // Denial is distinct from unsupported — the operation exists, the
    // actor may not perform it; reads and other writes are unaffected.
    let mut remote = GitlabSpikeRemote::new();
    remote.deny(SpikeOpKind::CommentPost);
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan());
    let journal = harness.into_journal();
    for id in ["r1-push", "r2-create", "r4-push-advance"] {
        assert!(
            matches!(
                record(&journal, id).outcome,
                RowOutcome::Receipt(WriteReceipt::Applied { .. })
            ),
            "{id} applies"
        );
    }
    for id in ["r3-comment", "r5-stale-comment"] {
        match &record(&journal, id).outcome {
            RowOutcome::Receipt(WriteReceipt::Denied { reason }) => {
                assert!(reason.contains("denied"), "explicit denial: {reason}");
            }
            other => panic!("{id}: denied write expected: {other:?}"),
        }
    }
    // Denied permission never blocks reads: the head read still
    // observes the remote — reconciliation survives lost mutation
    // permission.
    match &record(&journal, "r6-head-read").outcome {
        RowOutcome::Observed(obs) => {
            assert_eq!(obs.head.as_ref().map(|h| h.object.as_str()), Some("head-2"));
        }
        other => panic!("head read unaffected by write denial: {other:?}"),
    }
    assert_eq!(
        remote.applied().len(),
        3,
        "only the two pushes and the create landed — denied writes touched nothing"
    );
    persist("denial", &journal);

    // --- Lost response: the push lands remotely but its response is
    // lost. The receipt stays `unknown`; a fresh head read reconciles
    // the facts — never a blind retry, never an upgraded receipt.
    let mut remote = GitlabSpikeRemote::new();
    remote.lose_next_response();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan()[..1]);
    let r1 = record(harness.journal(), "r1-push");
    assert!(
        matches!(
            r1.outcome,
            RowOutcome::Receipt(WriteReceipt::Unknown { .. })
        ),
        "the lost response reports unknown: {:?}",
        r1.outcome
    );
    assert!(
        r1.reassessment.is_none(),
        "an unresolved write is not reassessed"
    );
    // The effect landed — the remote has the branch — but the receipt
    // stays unknown until reconcile establishes the facts.
    assert_eq!(
        harness.remote_mut().branch_head(&repo(), "spike/u94/w"),
        Some("head-1".to_string())
    );
    harness.reconcile();
    let r1 = record(harness.journal(), "r1-push");
    let reconciliation = r1.reconciliation.as_ref().expect("reconciled");
    assert_eq!(reconciliation.effect, ReconciledEffect::Present);
    assert_eq!(
        reconciliation.observed.as_ref().map(|h| h.object.as_str()),
        Some("head-1")
    );
    assert!(
        matches!(
            r1.outcome,
            RowOutcome::Receipt(WriteReceipt::Unknown { .. })
        ),
        "reconcile establishes facts; the receipt is not upgraded"
    );
    persist("lost_response", harness.journal());

    // --- Restart: the journal survives the harness; a resumed run
    // reconciles the unresolved create under the recorded binding even
    // while the installation default moved — then finishes the plan.
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan()[..1]);
    harness.remote_mut().lose_next_response();
    harness.run(&plan()[1..2]);
    let r2 = record(harness.journal(), "r2-create");
    assert!(matches!(
        r2.outcome,
        RowOutcome::Receipt(WriteReceipt::Unknown { .. })
    ));
    // Persist and drop: the restart boundary.
    let journal_json = serde_json::to_string(harness.journal()).unwrap();
    let journal: SpikeJournal = serde_json::from_str(&journal_json).unwrap();
    drop(harness);
    // The installation default moved under the run — reconciliation
    // still binds the recorded scope, not the moved default.
    let mut moved_default = scope();
    moved_default.repository = RepositoryId::trusted("installation-default-repo");
    let mut resumed = SpikeHarness::resume(&mut remote, None, journal, &moved_default);
    resumed.reconcile();
    let change = {
        let r2 = record(resumed.journal(), "r2-create");
        let reconciliation = r2.reconciliation.as_ref().expect("create reconciled");
        assert_eq!(reconciliation.effect, ReconciledEffect::Present);
        let change = reconciliation
            .change
            .clone()
            .expect("the created MR is established");
        assert_eq!(
            change.repository,
            repo(),
            "reconciled under the recorded binding"
        );
        change
    };
    // The run continues under the recorded scope: remaining rows drive
    // against the reconciled state; already-recorded rows are not
    // re-driven.
    resumed.run(&plan()[2..]);
    let journal = resumed.into_journal();
    assert_eq!(journal.records.len(), plan().len());
    match &record(&journal, "r3-comment").outcome {
        RowOutcome::Receipt(WriteReceipt::Applied { fact, .. }) => match fact.as_ref() {
            EffectFact::CommentPosted { change: c, .. } => assert_eq!(c, &change),
            other => panic!("comment fact expected: {other:?}"),
        },
        other => panic!("the resumed comment applies: {other:?}"),
    }
    assert!(
        journal.events.iter().any(|e| matches!(
            e,
            JournalEvent::Resumed {
                binding_diverged: true,
                ..
            }
        )),
        "the restart event records the diverged default: {:?}",
        journal.events
    );
    persist("restart", &journal);
}

/// The named test for revision-bound reassessment: each applied write's
/// receipt is followed by a fresh head read whose observed revision
/// equals the receipt's.
#[test]
fn each_applied_write_receipt_binds_a_fresh_head_read() {
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&plan());
    let journal = harness.into_journal();

    let mut applied = 0;
    for record in &journal.records {
        let RowOutcome::Receipt(WriteReceipt::Applied { revision, .. }) = &record.outcome else {
            continue;
        };
        applied += 1;
        let reassessment = record
            .reassessment
            .as_ref()
            .unwrap_or_else(|| panic!("{}: applied write reassessed", record.row_id));
        assert!(
            reassessment.bound,
            "{}: fresh head read {:?} must equal the receipt {:?}",
            record.row_id, reassessment.observed, reassessment.receipt
        );
        assert_eq!(reassessment.observed.as_ref(), Some(revision));
    }
    assert_eq!(applied, 4, "two pushes, the create, the comment");

    // The binding is per-target: the second push's reassessment binds
    // the new head, not the first's.
    let r4 = record(&journal, "r4-push-advance");
    assert_eq!(
        applied_revision(r4).object,
        "head-2",
        "the advanced push reassessed against its own revision"
    );

    // The stale-head comment aborts — a failed receipt, no write, no
    // reassessment.
    let r5 = record(&journal, "r5-stale-comment");
    assert!(matches!(
        r5.outcome,
        RowOutcome::Receipt(WriteReceipt::Failed { .. })
    ));
    assert!(r5.reassessment.is_none());
    assert_eq!(
        remote.applied().len(),
        4,
        "the aborted comment wrote nothing"
    );

    // The fresh head read observes the moved head.
    match &record(&journal, "r6-head-read").outcome {
        RowOutcome::Observed(obs) => {
            assert_eq!(obs.head.as_ref().map(|h| h.object.as_str()), Some("head-2"));
            assert_eq!(
                obs.change.as_ref().map(|c| c.change_request.as_str()),
                Some("!1"),
                "the read resolves the opened MR's scoped identity"
            );
        }
        other => panic!("head read row: {other:?}"),
    }
}

/// Live rows are gated on the recorded authorization: without it the
/// harness reports `not_authorized` and the remote sees no write; an
/// authorization recorded for a different scope does not open the gate;
/// a matching authorization executes the row — here against the fixture
/// transport, so this proves gate mechanics only, never a live claim.
#[test]
fn live_rows_require_the_recorded_authorization() {
    // No recorded authorization: every live row reports not_authorized.
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&live_plan());
    for record in &harness.journal().records {
        assert_eq!(
            record.outcome,
            RowOutcome::NotAuthorized,
            "{}: unrecorded live authorization cannot execute",
            record.row_id
        );
    }
    assert!(
        remote.applied().is_empty() && remote.branch_head(&repo(), "spike/u94/w").is_none(),
        "an unauthorized live row touches nothing"
    );

    // An authorization recorded for a different scope does not cover.
    let mut other_scope = scope();
    other_scope.branch_prefix = "spike/other/".into();
    let foreign = LiveAuthorization {
        record: "owner approval — other scope".into(),
        binding: other_scope.binding(),
    };
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, Some(foreign));
    harness.run(&live_plan());
    assert!(
        harness
            .journal()
            .records
            .iter()
            .all(|r| r.outcome == RowOutcome::NotAuthorized),
        "a foreign-scope authorization opens nothing"
    );
    assert!(remote.applied().is_empty());

    // A matching recorded authorization opens the gate: the rows then
    // execute on the harness's transport — the fixture remote stands in
    // here, proving gate mechanics only.
    let authorization = LiveAuthorization {
        record: "owner approval ref — recorded 2026-11-09".into(),
        binding: scope().binding(),
    };
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, Some(authorization));
    harness.run(&live_plan());
    let journal = harness.journal();
    assert!(
        journal
            .records
            .iter()
            .all(|r| r.outcome != RowOutcome::NotAuthorized),
        "the recorded authorization admits the rows"
    );
    assert_eq!(
        remote.applied().len(),
        4,
        "the live-lane plan drove the writes"
    );
}

/// The narrow surface is closed: the harness reaches only the four
/// operations — every remote call site in the module is one of the four
/// `SpikeRemote` methods, the operation kind set holds exactly four,
/// out-of-scope operation payloads cannot deserialize, and the
/// contract's other write operations stay unsupported and explicit in
/// every journal.
#[test]
fn no_operation_beyond_the_four_is_reachable() {
    const SOURCE: &str = include_str!("../src/gitlab_spike.rs");
    let remote_calls = SOURCE.matches("self.remote.").count();
    let allowed: usize = [
        "self.remote.guarded_push(",
        "self.remote.mr_create(",
        "self.remote.comment_post(",
        "self.remote.head_read(",
    ]
    .iter()
    .map(|call| SOURCE.matches(call).count())
    .sum();
    assert_eq!(
        remote_calls, allowed,
        "every remote call the harness makes is one of the four operations"
    );
    assert_eq!(
        SpikeOpKind::ALL.len(),
        4,
        "guarded push, MR create, comment post, head read — nothing else"
    );

    // No out-of-scope operation payload deserializes into a row: an
    // applied receipt for one is not constructible.
    for payload in [
        serde_json::json!({ "op": "approve_change" }),
        serde_json::json!({ "op": "mark_ready" }),
        serde_json::json!({ "op": "resolve_thread" }),
        serde_json::json!({ "op": "update_change_request" }),
        serde_json::json!({ "op": "post_inline_comment" }),
        serde_json::json!({ "op": "publish_review_feedback" }),
        serde_json::json!({ "op": "request_changes" }),
    ] {
        assert!(
            serde_json::from_value::<SpikeOp>(payload.clone()).is_err(),
            "{payload} is not a spike operation — stays unreachable"
        );
    }

    // Unsupported stays explicit: every journal names the contract
    // write operations outside the narrow set.
    let mut remote = GitlabSpikeRemote::new();
    let harness = SpikeHarness::new(scope(), &mut remote, None);
    for name in UNSUPPORTED_OPERATIONS {
        assert!(
            harness
                .journal()
                .unsupported_operations
                .iter()
                .any(|u| u == name),
            "journal records {name} as unsupported"
        );
    }
}

/// Writes stay inside the isolated scope: a row targeting a branch
/// outside the scope's prefix fails before the remote is touched.
#[test]
fn writes_outside_the_isolated_scope_never_reach_the_remote() {
    let mut remote = GitlabSpikeRemote::new();
    let mut harness = SpikeHarness::new(scope(), &mut remote, None);
    harness.run(&[row(
        "out-of-scope",
        SpikeOp::GuardedPush {
            branch: "main".into(),
            expected_prior: None,
            produced: "head-1".into(),
        },
    )]);
    match &record(harness.journal(), "out-of-scope").outcome {
        RowOutcome::Receipt(WriteReceipt::Failed { reason }) => {
            assert!(reason.starts_with("scope:"), "scope violation: {reason}");
        }
        other => panic!("out-of-scope write refused: {other:?}"),
    }
    assert!(
        remote.applied().is_empty(),
        "the remote saw no out-of-scope write"
    );
}
