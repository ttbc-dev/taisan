//! Anomaly-census contract tests (issue #150): the bounded read-only
//! inventory of stuck lane states. All fixtures are synthetic — no real
//! installation state, no live host.

use std::collections::BTreeSet;

use workloop_core::census::{
    AnomalyCensus, CensusBounds, FindingOwnership, JudgmentWaitItem, OrphanedFinding,
    SourceGateItem, anomaly_census,
};
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command, handle_observation};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn other_host() -> HostInstanceId {
    HostInstanceId::trusted("other-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn target(object: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(object),
        base: None,
        target_revision: None,
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::IssueRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    submit_with_sources(store, key, vec![])
}

fn submit_with_sources(
    store: &SqliteStore,
    key: &str,
    sources: Vec<SourceSnapshot>,
) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources,
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        work_id,
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = d
    else {
        panic!("submit {key}: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn census(store: &SqliteStore) -> AnomalyCensus {
    census_with(store, &CensusBounds::default())
}

fn census_with(store: &SqliteStore, bounds: &CensusBounds) -> AnomalyCensus {
    store
        .transact_deferred(|tx| anomaly_census(tx, bounds, &host()))
        .unwrap()
}

/// The change the Work opened plus its observed head — the two facts
/// `current_target` derives the diff target from.
fn open_intent(store: &SqliteStore, work_id: &WorkId, action_id: &str) {
    store
        .transact(|tx| {
            if tx
                .action_intents()?
                .iter()
                .any(|i| i.action_id == action_id)
            {
                return Ok(());
            }
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.into(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "seed".into(),
                        body: String::new(),
                    },
                    payload_digest: "test".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })
        })
        .unwrap();
}

fn open_change(store: &SqliteStore, work_id: &WorkId, object: &str, at: i64) {
    open_intent(store, work_id, "act-open");
    for fact in [
        ObservationFact::HostActionResult {
            action_id: "act-open".into(),
            authenticated_actor: None,
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev(object),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
        ObservationFact::ChangeHead {
            change: change(),
            head: rev(object),
        },
    ] {
        handle_observation(
            store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact,
                observed_unix_seconds: at,
            },
            at,
        )
        .unwrap();
    }
}

/// A complete review over `source_keys` on `object`'s target, through
/// the same ingest path a real delivery uses — seals an immutable
/// round.
fn seal_review(
    store: &SqliteStore,
    work_id: &WorkId,
    object: &str,
    source_keys: &[&str],
    execution_id: Option<&ExecutionId>,
    at: i64,
) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.cloned(),
            fact: ObservationFact::Review {
                target: target(object),
                source: "reviewer".into(),
                source_revision: None,
                completeness: Completeness::Complete,
                occurrences: source_keys
                    .iter()
                    .map(|key| OccurrenceInput {
                        source_key: (*key).into(),
                        content_digest: None,
                        severity: None,
                        position: None,
                        thread_id: None,
                    })
                    .collect(),
            },
            observed_unix_seconds: at,
        },
        at,
    )
    .unwrap();
}

/// Seal a round directly — for completeness states the observation
/// path never seals (`partial:`/`unavailable:`).
fn seal_round_raw(
    store: &SqliteStore,
    work_id: &WorkId,
    object: &str,
    completeness: &str,
    source_keys: &[&str],
) {
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            let round_id = format!("round-{}-{}", work_id.0, seq);
            tx.apply(&Effect::InsertReviewRound {
                round: ReviewRound {
                    id: round_id.clone(),
                    work_id: work_id.clone(),
                    target: target(object),
                    source: "reviewer".into(),
                    source_revision: None,
                    completeness: completeness.into(),
                    set_digest: format!("digest-{round_id}"),
                    generation: 1,
                    seq,
                },
                occurrences: source_keys
                    .iter()
                    .enumerate()
                    .map(|(i, key)| FindingOccurrence {
                        id: format!("{round_id}-occ-{i}"),
                        round_id: round_id.clone(),
                        source_key: (*key).into(),
                        content_digest: None,
                        severity: None,
                        position: None,
                        thread_id: None,
                    })
                    .collect(),
            })
        })
        .unwrap();
}

/// Record a judgment covering `round`'s exact `set_digest`.
fn judge_round(store: &SqliteStore, work_id: &WorkId, round_id: &str) {
    store
        .transact(|tx| {
            let round = tx
                .rounds_for_work(work_id)?
                .into_iter()
                .find(|r| r.id == round_id)
                .unwrap();
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertRoundJudgment {
                judgment: RoundJudgment {
                    id: format!("judg-{round_id}"),
                    work_id: work_id.clone(),
                    round_id: round_id.into(),
                    set_digest: round.set_digest,
                    decisions: vec![],
                    actor_id: "op".into(),
                    seq,
                },
            })
        })
        .unwrap();
}

fn pause_execution(store: &SqliteStore, execution_id: &ExecutionId, reason: &str) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some(reason.into());
            e.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
}

fn set_waiting(store: &SqliteStore, execution_id: &ExecutionId, reason: WaitingReason) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(reason);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
}

/// A finished (non-writer) Invocation on the Execution — the durable
/// proof this installation ran the Work.
fn record_done_invocation(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("inv-{}", execution_id.0)),
                    request_id: format!("{}:inv:0", execution_id.0),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    descendants: vec![],
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                },
            })
        })
        .unwrap();
}

fn record_unresolved_invocation(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("inv-{}", execution_id.0)),
                    request_id: format!("{}:inv:0", execution_id.0),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: None,
                    descendants: vec![],
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                },
            })
        })
        .unwrap();
}

fn ack_attention(store: &SqliteStore, work_id: &WorkId, attention_id: &str) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertAttentionAck {
                work_id: work_id.clone(),
                attention_id: attention_id.into(),
                actor_id: "op".into(),
            })
        })
        .unwrap();
}

fn attention_id_for(
    store: &SqliteStore,
    work_id: &WorkId,
    reason: &str,
    anchor: &str,
    execution_id: &ExecutionId,
) -> String {
    // The real projection embeds the Execution's current enqueued_seq
    // in the occurrence suffix; the census scopes acknowledgements to
    // that wait generation.
    let enqueued_seq = store
        .transact(|tx| Ok(tx.execution(execution_id)?.unwrap().enqueued_seq))
        .unwrap();
    format!(
        "att:{}:{}:{}:{}:{}:0",
        work_id.0, reason, anchor, execution_id.0, enqueued_seq
    )
}

fn unjudged_rounds(c: &AnomalyCensus) -> Vec<&JudgmentWaitItem> {
    c.judgment_waits
        .items
        .iter()
        .filter(|i| matches!(i, JudgmentWaitItem::UnjudgedRound { .. }))
        .collect()
}

fn orphans(c: &AnomalyCensus) -> &[OrphanedFinding] {
    &c.orphaned_findings.items
}

#[test]
fn empty_store_reports_every_detector() {
    let store = SqliteStore::in_memory(1).unwrap();
    let c = census(&store);
    for (name, section) in [
        ("judgment_waits", &c.judgment_waits.applicable),
        ("released_wait_blocked", &c.released_wait_blocked.applicable),
        ("orphaned_findings", &c.orphaned_findings.applicable),
        (
            "parked_selector_starvation",
            &c.parked_selector_starvation.applicable,
        ),
        ("source_gate_holds", &c.source_gate_holds.applicable),
    ] {
        assert!(*section, "{name} must be applicable");
    }
    assert!(!c.stale_spec_halts.applicable);
    assert!(
        c.stale_spec_halts
            .note
            .as_deref()
            .is_some_and(|n| n.contains("not_applicable_v0.1")),
        "stale spec-halts are reported explicitly, never dropped"
    );
    assert_eq!(c.judgment_waits.total, 0);
    assert_eq!(c.orphaned_findings.total, 0);
}

#[test]
fn unjudged_round_and_unacked_pause_surface_in_judgment_waits() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "c1");
    open_change(&store, &work_id, "sha-a", NOW);
    seal_review(&store, &work_id, "sha-a", &["f-1"], Some(&eid), NOW);
    pause_execution(&store, &eid, "repair: needs human");

    let c = census(&store);
    // The sealed round has no judgment coverage.
    let rounds = unjudged_rounds(&c);
    assert_eq!(rounds.len(), 1, "{c:?}");
    let JudgmentWaitItem::UnjudgedRound {
        occurrence_count,
        superseded,
        ..
    } = rounds[0]
    else {
        panic!()
    };
    assert_eq!(*occurrence_count, Some(1));
    assert!(!superseded);
    // The paused execution's paused_judgment attention is unacknowledged.
    assert!(
        c.judgment_waits.items.iter().any(|i| matches!(
            i,
            JudgmentWaitItem::UnackedPausedJudgment { execution_id, .. }
                if *execution_id == eid
        )),
        "unacked paused wait must surface: {c:?}"
    );
    // Same records feed the orphan detector: no judgment covers the
    // round, no writer is unresolved — but no invocation is recorded
    // either, so ownership is uncertain.
    let orphans = orphans(&c);
    assert_eq!(orphans.len(), 1, "{c:?}");
    assert_eq!(orphans[0].ownership, FindingOwnership::Uncertain);
}

#[test]
fn judged_round_leaves_no_wait_or_orphan() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "c2");
    open_change(&store, &work_id, "sha-a", NOW);
    seal_review(&store, &work_id, "sha-a", &["f-1"], Some(&eid), NOW);
    let round_id = store
        .transact(|tx| Ok(tx.rounds_for_work(&work_id)?.pop().unwrap().id))
        .unwrap();
    judge_round(&store, &work_id, &round_id);
    let c = census(&store);
    assert!(unjudged_rounds(&c).is_empty(), "{c:?}");
    assert!(orphans(&c).is_empty(), "{c:?}");
}

#[test]
fn released_wait_blocked_needs_ack_and_no_accepted_release() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "c3");
    pause_execution(&store, &eid, "repair: needs human");

    // Unacknowledged: detector 1 owns it, detector 2 does not.
    let c = census(&store);
    assert!(c.released_wait_blocked.items.is_empty(), "{c:?}");
    assert!(
        c.judgment_waits
            .items
            .iter()
            .any(|i| matches!(i, JudgmentWaitItem::UnackedPausedJudgment { .. })),
        "{c:?}"
    );

    // Acknowledged but no resume/retry: the released-wait parent is
    // blocked.
    let att = attention_id_for(
        &store,
        &work_id,
        "paused_judgment",
        "repair: needs human",
        &eid,
    );
    ack_attention(&store, &work_id, &att);
    let c = census(&store);
    assert_eq!(c.released_wait_blocked.items.len(), 1, "{c:?}");
    let item = &c.released_wait_blocked.items[0];
    assert_eq!(item.execution_id, eid);
    assert_eq!(item.acked_attention_ids, vec![att.clone()]);
    // The ack also clears detector 1's unacked wait.
    assert!(
        !c.judgment_waits
            .items
            .iter()
            .any(|i| matches!(i, JudgmentWaitItem::UnackedPausedJudgment { .. })),
        "{c:?}"
    );

    // An accepted resume records the release: the block clears.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "c3-resume".into(),
            body: CommandBody::Resume {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: eid.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let c = census(&store);
    assert!(c.released_wait_blocked.items.is_empty(), "{c:?}");
}

#[test]
fn rejected_resume_does_not_count_as_a_release() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "c3b");
    pause_execution(&store, &eid, "repair: needs human");
    let att = attention_id_for(
        &store,
        &work_id,
        "paused_judgment",
        "repair: needs human",
        &eid,
    );
    ack_attention(&store, &work_id, &att);
    // A resume against a stale work version is recorded but rejected —
    // the wait is still blocked, and the rejection stays visible.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "c3b-resume".into(),
            body: CommandBody::Resume {
                work_id: work_id.clone(),
                expected_work_version: 99,
                execution_id: eid.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Rejected { .. }), "{d:?}");
    let c = census(&store);
    assert_eq!(c.released_wait_blocked.items.len(), 1, "{c:?}");
    let item = &c.released_wait_blocked.items[0];
    assert!(
        item.release_commands
            .iter()
            .any(|r| r.kind == "resume" && !r.accepted),
        "the rejected attempt is evidence: {item:?}"
    );
}

#[test]
fn repause_is_a_new_wait_generation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "c3c");
    pause_execution(&store, &eid, "repair: needs human");

    // First generation: acknowledged, then an accepted resume requeues
    // the Execution under a new enqueued_seq.
    let att1 = attention_id_for(
        &store,
        &work_id,
        "paused_judgment",
        "repair: needs human",
        &eid,
    );
    ack_attention(&store, &work_id, &att1);
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "c3c-resume".into(),
            body: CommandBody::Resume {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: eid.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    // The Execution pauses again — a NEW wait generation. The old
    // acknowledgement must not clear it.
    pause_execution(&store, &eid, "repair: launch refused before spawn");
    let c = census(&store);
    assert!(
        c.judgment_waits.items.iter().any(|i| matches!(
            i,
            JudgmentWaitItem::UnackedPausedJudgment { execution_id, .. } if *execution_id == eid
        )),
        "the second pause is unacknowledged: {c:?}"
    );
    assert!(c.released_wait_blocked.items.is_empty(), "{c:?}");

    // Acknowledging the new wait clears detector 1 — but the accepted
    // resume released the FIRST pause (its committed_at is the old
    // generation's enqueued_seq), not this one, so detector 2 still
    // reports the blocked wait.
    let att2 = attention_id_for(
        &store,
        &work_id,
        "paused_judgment",
        "repair: launch refused before spawn",
        &eid,
    );
    ack_attention(&store, &work_id, &att2);
    let c = census(&store);
    assert_eq!(c.released_wait_blocked.items.len(), 1, "{c:?}");
    let item = &c.released_wait_blocked.items[0];
    assert_eq!(item.execution_id, eid);
    assert!(item.acked_attention_ids.contains(&att2), "{item:?}");
    assert!(
        !c.judgment_waits.items.iter().any(|i| matches!(
            i,
            JudgmentWaitItem::UnackedPausedJudgment { execution_id, .. } if *execution_id == eid
        )),
        "{c:?}"
    );
}

#[test]
fn orphaned_findings_classify_proven_foreign_uncertain() {
    let store = SqliteStore::in_memory(1).unwrap();

    // proven: this installation's host, a recorded finished
    // invocation, a complete uncovered round.
    let (w_proven, e_proven) = submit(&store, "own");
    open_change(&store, &w_proven, "sha-p", NOW);
    seal_review(&store, &w_proven, "sha-p", &["pf-1"], Some(&e_proven), NOW);
    record_done_invocation(&store, &e_proven);

    // foreign: same records, but the Work's binding host differs.
    let (w_foreign, e_foreign) = submit(&store, "foreign");
    open_change(&store, &w_foreign, "sha-f", NOW);
    seal_review(
        &store,
        &w_foreign,
        "sha-f",
        &["ff-1"],
        Some(&e_foreign),
        NOW,
    );
    record_done_invocation(&store, &e_foreign);
    store
        .transact(|tx| {
            let mut w = tx.work(&w_foreign)?.unwrap();
            let version = w.version;
            w.binding.repository =
                RepositoryRef::new(other_host(), RepositoryId::trusted("local-repo"));
            w.version += 1;
            tx.apply(&Effect::UpdateWork {
                work: w,
                expected: version,
            })
        })
        .unwrap();

    // uncertain (no invocations): own host but nothing proves this
    // installation ran the Work.
    let (w_noinv, e_noinv) = submit(&store, "noinv");
    open_change(&store, &w_noinv, "sha-n", NOW);
    seal_review(&store, &w_noinv, "sha-n", &["nf-1"], Some(&e_noinv), NOW);

    // uncertain (unreadable source): completeness is not `complete`.
    let (w_unread, _e_unread) = submit(&store, "unread");
    seal_round_raw(&store, &w_unread, "sha-u", "unavailable:timeout", &["uf-1"]);

    // not orphaned: an unresolved writer may still disposition them.
    let (w_live, e_live) = submit(&store, "live");
    open_change(&store, &w_live, "sha-l", NOW);
    seal_review(&store, &w_live, "sha-l", &["lf-1"], Some(&e_live), NOW);
    record_unresolved_invocation(&store, &e_live);

    let c = census(&store);
    let by_work: std::collections::HashMap<&str, FindingOwnership> = orphans(&c)
        .iter()
        .map(|o| (o.work_id.0.as_str(), o.ownership))
        .collect();
    assert_eq!(
        by_work.get(w_proven.0.as_str()),
        Some(&FindingOwnership::Proven)
    );
    assert_eq!(
        by_work.get(w_foreign.0.as_str()),
        Some(&FindingOwnership::Foreign)
    );
    assert_eq!(
        by_work.get(w_noinv.0.as_str()),
        Some(&FindingOwnership::Uncertain)
    );
    assert_eq!(
        by_work.get(w_unread.0.as_str()),
        Some(&FindingOwnership::Uncertain)
    );
    assert!(!by_work.contains_key(w_live.0.as_str()), "{c:?}");
}

#[test]
fn parked_selector_starvation_reports_only_aged_deferrals() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (_w1, e1) = submit(&store, "d1");
    let (_w2, e2) = submit(&store, "d2");
    store
        .transact(|tx| {
            tx.apply(&Effect::RecordDeferral {
                execution_id: e1.clone(),
                reason: "capacity_exhausted".into(),
                diagnostic: "no free capacity slot".into(),
            })?;
            // Age only e1's deferral past the threshold; e2's is fresh.
            for _ in 0..4 {
                tx.alloc_seq()?;
            }
            tx.apply(&Effect::RecordDeferral {
                execution_id: e2.clone(),
                reason: "resource_conflict".into(),
                diagnostic: "held".into(),
            })?;
            Ok(())
        })
        .unwrap();
    let bounds = CensusBounds {
        item_limit: 100,
        deferral_age_events: 2,
    };
    let c = census_with(&store, &bounds);
    let section = &c.parked_selector_starvation;
    assert_eq!(section.items.len(), 1, "{section:?}");
    assert_eq!(section.items[0].execution_id, e1);
    assert!(section.items[0].stale);
    assert!(section.items[0].age_events > bounds.deferral_age_events);
    assert_eq!(section.context.as_ref().unwrap()["deferrals_total"], 2);
    assert_eq!(section.context.as_ref().unwrap()["slot_limit"], 2);
}

#[test]
fn source_gate_holds_and_human_judgment_waits_surface() {
    let store = SqliteStore::in_memory(2).unwrap();
    // A Work with declared sources starts at needs_check.
    let issue = IssueRef {
        repository: repo(),
        issue: IssueId::trusted("42"),
        display_number: Some(42),
    };
    let (w_gated, _e_gated) = submit_with_sources(
        &store,
        "gate",
        vec![SourceSnapshot {
            issue,
            source_revision: "rev-1".into(),
            content_digest: "digest-1".into(),
        }],
    );
    let (w_hj, e_hj) = submit(&store, "hj");
    set_waiting(&store, &e_hj, WaitingReason::HumanJudgment);

    let c = census(&store);
    let items = &c.source_gate_holds.items;
    assert!(
        items.iter().any(|i| matches!(
            i,
            SourceGateItem::SourceGateHold {
                work_id,
                source_gate: SourceGate::NeedsCheck,
                ..
            } if *work_id == w_gated
        )),
        "{c:?}"
    );
    assert!(
        items.iter().any(|i| matches!(
            i,
            SourceGateItem::HumanJudgmentWait {
                work_id,
                execution_id,
                ..
            } if *work_id == w_hj && *execution_id == e_hj
        )),
        "{c:?}"
    );
}

#[test]
fn every_detector_is_individually_bounded() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "cap");
    record_done_invocation(&store, &eid);
    // Three uncovered rounds on three targets → three unjudged rounds
    // and three orphans.
    for object in ["sha-1", "sha-2", "sha-3"] {
        open_change(&store, &work_id, object, NOW);
        seal_review(&store, &work_id, object, &["f"], Some(&eid), NOW);
    }
    let bounds = CensusBounds {
        item_limit: 2,
        deferral_age_events: 256,
    };
    let c = census_with(&store, &bounds);
    assert_eq!(c.judgment_waits.limit, 2);
    assert_eq!(c.released_wait_blocked.limit, 2);
    assert_eq!(c.orphaned_findings.limit, 2);
    assert_eq!(c.parked_selector_starvation.limit, 2);
    assert_eq!(c.source_gate_holds.limit, 2);
    // judgment_waits carries 3 unjudged rounds (cap 2).
    assert_eq!(c.judgment_waits.items.len(), 2);
    assert_eq!(c.judgment_waits.total, 3);
    assert!(c.judgment_waits.truncated);
    // orphaned_findings carries 3 occurrences (cap 2).
    assert_eq!(c.orphaned_findings.items.len(), 2);
    assert_eq!(c.orphaned_findings.total, 3);
    assert!(c.orphaned_findings.truncated);
}

#[test]
fn superseded_rounds_are_flagged_not_hidden() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "sup");
    record_done_invocation(&store, &eid);
    open_change(&store, &work_id, "sha-same", NOW);
    // Two rounds on the SAME target: a changed occurrence set seals a
    // new round and supersedes the first — both stay reported, the
    // older flagged `superseded`.
    seal_review(&store, &work_id, "sha-same", &["old-f"], Some(&eid), NOW);
    seal_review(&store, &work_id, "sha-same", &["new-f"], Some(&eid), NOW);
    let c = census(&store);
    let rounds = unjudged_rounds(&c);
    assert_eq!(rounds.len(), 2, "{c:?}");
    let superseded: Vec<bool> = rounds
        .iter()
        .map(|i| match i {
            JudgmentWaitItem::UnjudgedRound { superseded, .. } => *superseded,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        superseded,
        vec![true, false],
        "the older same-target round is superseded"
    );
}

#[test]
fn unreadable_records_are_reported_never_dropped() {
    // File-backed store so a second connection can plant undecodable
    // rows the typed API can never write.
    let unique = format!(
        "taisan-census-unreadable-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let (work_id, eid) = submit(&store, "u1");
    open_change(&store, &work_id, "sha-u", NOW);
    seal_review(&store, &work_id, "sha-u", &["f-1"], Some(&eid), NOW);
    let round_id = store
        .transact(|tx| Ok(tx.rounds_for_work(&work_id)?.pop().unwrap().id))
        .unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        // A judgment row whose decisions payload is not JSON: the
        // round's coverage becomes unreadable, reported — never
        // silently treated as covered or clean.
        conn.execute(
            "INSERT INTO round_judgments (id, work_id, round_id, set_digest, \
             decisions, actor_id, seq) VALUES ('judg-bad', ?1, ?2, 'd', \
             'not-json', 'op', 999999)",
            rusqlite::params![work_id.0, round_id],
        )
        .unwrap();
        // A commands row whose canonical payload is undecodable.
        conn.execute(
            "INSERT INTO commands (origin, actor_kind, actor_id, \
             idempotency_key, payload_digest, canonical_payload, \
             command_id, decision, seq) VALUES ('cli', 'human', 'op', \
             'bad-key', 'd', X'FFFE', 'cmd-bad', \
             '{\"status\":\"accepted\"}', 999998)",
            [],
        )
        .unwrap();
    }
    let c = census(&store);
    let unreadable: Vec<&JudgmentWaitItem> = c
        .judgment_waits
        .items
        .iter()
        .filter(|i| {
            matches!(
                i,
                JudgmentWaitItem::UnreadableRecord { .. }
                    | JudgmentWaitItem::UnreadableCommand { .. }
            )
        })
        .collect();
    assert!(
        unreadable.iter().any(|i| matches!(
            i,
            JudgmentWaitItem::UnreadableRecord { record, .. } if record == "round_judgments"
        )),
        "corrupt judgment row must surface: {c:?}"
    );
    assert!(
        unreadable.iter().any(|i| matches!(
            i,
            JudgmentWaitItem::UnreadableCommand { command_id, .. } if command_id == "cmd-bad"
        )),
        "corrupt command payload must surface: {c:?}"
    );
    // The round's coverage is unprovable → still reported as an open
    // wait, not silently dropped.
    assert!(!unjudged_rounds(&c).is_empty(), "{c:?}");
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn census_is_read_only() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "ro");
    open_change(&store, &work_id, "sha-ro", NOW);
    seal_review(&store, &work_id, "sha-ro", &["f"], Some(&eid), NOW);
    pause_execution(&store, &eid, "repair: needs human");
    let before = store.transact(|tx| tx.last_seq()).unwrap();
    let commands_before = store
        .transact(|tx| Ok(tx.commands().unwrap().len()))
        .unwrap();
    let _ = census(&store);
    let after = store.transact(|tx| tx.last_seq()).unwrap();
    let commands_after = store
        .transact(|tx| Ok(tx.commands().unwrap().len()))
        .unwrap();
    assert_eq!(before, after, "census must not allocate sequence");
    assert_eq!(commands_before, commands_after, "census records nothing");
}
