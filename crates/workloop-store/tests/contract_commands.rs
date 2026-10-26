//! Contract tests: Commands, Admission, Reservations from docs/v0.1.md.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::{
    CheckConclusion, Observation, ObservationFact, Outcome, ReviewVerdict,
};
use workloop_core::store::{Effect, PrAction, PrActionIntent, PrActionStatus, Store};
use workloop_core::work::{WorkloopConfig, dispatch_once, handle_command};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn config() -> WorkloopConfig {
    WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
    }
}

fn config_with_budget(budget: ExecutionBudget) -> WorkloopConfig {
    WorkloopConfig { budget, ..config() }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit(key: &str) -> CommandRequest {
    req(
        key,
        CommandBody::Submit {
            intent: "do the thing".into(),
            issue_refs: vec![GitHubIssueRef {
                repository: "local/repo".into(),
                number: 1,
            }],
            constraints: vec![],
            verification_argv: vec![],
        },
    )
}

fn accepted(d: &CommandDecision) {
    match d {
        CommandDecision::Accepted { .. } => {}
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn accepted_with_execution(d: &CommandDecision) -> (WorkId, ExecutionId) {
    match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => execution_id.clone(),
                other => panic!("expected an execution effect, got {other:?}"),
            };
            (work_id.clone().expect("work_id"), eid)
        }
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn reject_reason(d: &CommandDecision) -> RejectionCode {
    match d {
        CommandDecision::Rejected { reason, .. } => reason.clone(),
        other => panic!("expected rejected, got {other:?}"),
    }
}

/// Apply raw effects to set up states commands can't reach yet (e.g. a
/// failed Execution) — the fixture equivalent of prior history.
fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("setup effects apply");
}

fn read_execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .expect("read")
        .expect("execution exists")
}

/// Move an Execution to a target state through the store's guarded update.
fn force_state(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    let from = read_execution(store, id).state;
    let mut e = read_execution(store, id);
    e.state = to.clone();
    if to.is_terminal() {
        e.terminal_code = Some("setup".into());
        e.outcome = Some(Outcome {
            state: to.clone(),
            code: Some("setup".into()),
            detail: None,
            evidence_refs: vec![],
            recorded_seq: 0,
        });
    }
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

/// Record the persisted PR-head evidence `judge_findings` and
/// `release_judgment` must compare against — the adapter's Observation.
fn seed_pr_head(store: &SqliteStore, work_id: &WorkId, sha: &str, observed_at: i64) {
    apply_effects(
        store,
        &[Effect::InsertObservation {
            observation: Observation {
                source_event_id: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::PrHead {
                    repository: "local/repo".into(),
                    number: 1,
                    head_sha: sha.into(),
                },
                observed_unix_seconds: observed_at,
            },
        }],
    );
}

/// Same evidence recorded against one of the Work's Executions instead of
/// the Work itself; `latest_pr_head` must resolve it either way.
fn seed_pr_head_for_execution(store: &SqliteStore, execution_id: &ExecutionId, sha: &str) {
    apply_effects(
        store,
        &[Effect::InsertObservation {
            observation: Observation {
                source_event_id: None,
                subject_work_id: None,
                subject_execution_id: Some(execution_id.clone()),
                fact: ObservationFact::PrHead {
                    repository: "local/repo".into(),
                    number: 1,
                    head_sha: sha.into(),
                },
                observed_unix_seconds: NOW,
            },
        }],
    );
}

/// Drive one Command through `handle_command` twice and prove the second
/// call is served from the stored decision: identical decision JSON, no new
/// event sequence, and the same decision recoverable via `command_result`.
/// `now` is the original attempt's clock; `replay_now` lets a test replay
/// after time has passed (e.g. past a claim's TTL midpoint).
fn assert_replay_locked(
    store: &SqliteStore,
    request: &CommandRequest,
    now: i64,
    replay_now: i64,
) -> CommandDecision {
    let first = handle_command(store, &config(), request, now).expect("first handle_command");
    let first_json = serde_json::to_string(&first).expect("decision serializes");
    let seq_before = store.transact(|tx| tx.last_seq()).expect("read last_seq");
    let second =
        handle_command(store, &config(), request, replay_now).expect("replay handle_command");
    assert_eq!(
        first_json,
        serde_json::to_string(&second).expect("decision serializes"),
        "replay returns the saved decision"
    );
    let seq_after = store.transact(|tx| tx.last_seq()).expect("read last_seq");
    assert_eq!(
        seq_before, seq_after,
        "replay allocates no new sequence and applies no effect"
    );
    let recovered = store
        .transact(|tx| tx.command_decision(first.command_id()))
        .expect("command_result read")
        .expect("command_result recovers the decision");
    assert_eq!(
        first_json,
        serde_json::to_string(&recovered).expect("decision serializes"),
        "command_result returns the same decision"
    );
    first
}

// ---- Commands: idempotency ----

#[test]
fn submit_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let d = assert_replay_locked(&store, &submit("k1"), NOW, NOW);
    accepted(&d);
}

#[test]
fn same_key_different_payload_is_idempotency_conflict() {
    let store = SqliteStore::in_memory(1).unwrap();
    handle_command(&store, &config(), &submit("k2"), NOW).unwrap();
    let mut other = submit("k2");
    if let CommandBody::Submit { intent, .. } = &mut other.body {
        *intent = "different intent".into();
    }
    let d = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::IdempotencyConflict);
}

// ---- Commands: submit/retry/resume/cancel ----

#[test]
fn submit_creates_work_and_queued_execution() {
    let store = SqliteStore::in_memory(1).unwrap();
    let d = handle_command(&store, &config(), &submit("s1"), NOW).unwrap();
    let (work_id, eid) = accepted_with_execution(&d);
    let e = read_execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Queued);
    assert_eq!(e.work_id, work_id);
    assert_eq!(e.ordinal, 0);
}

#[test]
fn retry_requires_confirmed_failure() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("r1"), NOW).unwrap());
    // E1 is queued, not failed.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "r2",
            CommandBody::Retry {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_execution_id: e1.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::Ineligible);

    // After a confirmed failure, retry creates the next queued Execution.
    force_state(&store, &e1, ExecutionState::Failed);
    let d = handle_command(
        &store,
        &config(),
        &req(
            "r3",
            CommandBody::Retry {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_execution_id: e1.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    let (_, e2) = accepted_with_execution(&d);
    let e2 = read_execution(&store, &e2);
    assert_eq!(e2.state, ExecutionState::Queued);
    assert_eq!(e2.ordinal, 1);
}

#[test]
fn stale_work_version_is_rejected() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("v1"), NOW).unwrap());
    let d = handle_command(
        &store,
        &config(),
        &req(
            "v2",
            CommandBody::Resume {
                work_id,
                expected_work_version: 99,
                execution_id: e1,
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StaleWorkVersion);
}

#[test]
fn resume_requeues_paused_and_rejects_others() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("p1"), NOW).unwrap());
    // queued → ineligible
    let d = handle_command(
        &store,
        &config(),
        &req(
            "p2",
            CommandBody::Resume {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: e1.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::Ineligible);
    // paused → requeued with a new enqueue sequence, budget preserved
    force_state(&store, &e1, ExecutionState::Paused);
    let before = read_execution(&store, &e1).enqueued_seq;
    let d = handle_command(
        &store,
        &config(),
        &req(
            "p3",
            CommandBody::Resume {
                work_id,
                expected_work_version: 1,
                execution_id: e1.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Queued);
    assert!(e.enqueued_seq > before);
}

#[test]
fn cancel_queued_is_terminal_and_second_cancel_is_noop() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("c1"), NOW).unwrap());
    let d = handle_command(
        &store,
        &config(),
        &req(
            "c2",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Cancelled);
    // Second cancel with a different key: accepted no-op.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "c3",
            CommandBody::Cancel {
                work_id,
                expected_work_version: 2,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);
}

// ---- Reservations ----

fn branch_res() -> WriteResource {
    WriteResource::Branch {
        repository: "local/repo".into(),
        branch: "wip/main".into(),
    }
}

#[test]
fn claim_conflict_returns_holder_and_expiry_frees() {
    let store = SqliteStore::in_memory(1).unwrap();
    let claim = CommandBody::ClaimResources {
        resources: vec![branch_res()],
        ttl_seconds: 60,
    };
    accepted(&handle_command(&store, &config(), &req("cl1", claim.clone()), NOW).unwrap());

    // Another actor conflicts and sees the holder.
    let mut other_actor = req("cl2", claim.clone());
    other_actor.actor.external_id = "someone-else".into();
    let d = handle_command(&store, &config(), &other_actor, NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::ResourceConflict);

    // Same actor renews.
    accepted(&handle_command(&store, &config(), &req("cl3", claim.clone()), NOW + 30).unwrap());

    // After expiry a different actor can claim.
    let mut late = req("cl4", claim);
    late.actor.external_id = "someone-else".into();
    accepted(&handle_command(&store, &config(), &late, NOW + 120).unwrap());
}

#[test]
fn release_only_frees_the_claimers_rows() {
    let store = SqliteStore::in_memory(1).unwrap();
    let claim = CommandBody::ClaimResources {
        resources: vec![branch_res()],
        ttl_seconds: 60,
    };
    accepted(&handle_command(&store, &config(), &req("rc1", claim), NOW).unwrap());

    let release = CommandBody::ReleaseResources {
        resources: vec![branch_res()],
    };
    // Another actor cannot release it.
    let mut other = req("rc2", release.clone());
    other.actor.external_id = "someone-else".into();
    let d = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::Unauthorized);

    // The claimer can.
    accepted(&handle_command(&store, &config(), &req("rc3", release), NOW).unwrap());
}

// ---- Admission ----

#[test]
fn dispatch_admits_oldest_eligible_and_reserves_atomically() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("a1"), NOW).unwrap());

    store
        .transact(|tx| {
            let admitted = workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?;
            assert!(admitted.is_some());
            Ok(())
        })
        .unwrap();

    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Ready);
    // Slot, reservation, and request all exist after the same commit.
    let (slots, reservations, requests) = store
        .transact(|tx| {
            Ok((
                tx.slot_holders()?.len(),
                tx.reservations()?.len(),
                tx.pending_requests()?.len(),
            ))
        })
        .unwrap();
    assert_eq!(slots, 1);
    assert_eq!(reservations, 2, "workspace + branch");
    assert_eq!(requests, 1);
}

#[test]
fn queued_execution_defers_when_capacity_is_full() {
    let store = SqliteStore::in_memory(0).unwrap(); // no slots at all
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("q1"), NOW).unwrap());
    store
        .transact(|tx| {
            let admitted = workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?;
            assert!(admitted.is_none());
            Ok(())
        })
        .unwrap();
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
    let deferrals = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(deferrals, 0);
}

#[test]
fn earlier_external_claim_defers_queued_execution() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("x1"), NOW).unwrap());
    // An external writer claims the work branch before dispatch.
    let claim = CommandBody::ClaimResources {
        resources: vec![WriteResource::Branch {
            repository: "local/repo".into(),
            branch: format!("work/{}", work_id.0),
        }],
        ttl_seconds: 300,
    };
    accepted(&handle_command(&store, &config(), &req("x2", claim), NOW).unwrap());

    store
        .transact(|tx| {
            let admitted = workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?;
            assert!(admitted.is_none(), "external claim wins over queued");
            Ok(())
        })
        .unwrap();
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
}

#[test]
fn dispatch_conflict_defers_that_item_and_later_items_still_admit() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (w1, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("dc1"), NOW).unwrap());
    let (_, e2) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("dc2"), NOW).unwrap());

    // Seed a request row with the exact ID the first admission will
    // mint (`{execution}:inv:{invocations_used}`), so the mid-admission
    // InsertRequest trips the uniqueness constraint — the shape of a
    // lost request-ID race with another process.
    let seeded = InvocationRequest {
        request_id: format!("{}:inv:0", e1.0),
        work_id: w1,
        execution_id: e1.clone(),
        role: InvocationRole::Coding,
        payload: serde_json::json!({}),
    };
    apply_effects(&store, &[Effect::InsertRequest { request: seeded }]);

    let admitted = store
        .transact(|tx| dispatch_once(tx, &config(), NOW))
        .expect("a dispatch-time conflict is not a tick error");
    assert!(
        !admitted.contains(&e1),
        "the conflicted item is not admitted: {admitted:?}"
    );
    assert!(
        admitted.contains(&e2),
        "later independent queued items still admit: {admitted:?}"
    );

    // The failed admission left no partial effects: e1 stays queued
    // (its queued→ready update and slot claim rolled back) with a
    // retryable diagnostic; e2 holds the only slot and request.
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
    assert_eq!(read_execution(&store, &e2).state, ExecutionState::Ready);
    let (slots, deferrals) = store
        .transact(|tx| {
            Ok((
                tx.slot_holders()?.len(),
                tx.deferrals()?
                    .into_iter()
                    .find(|d| d.execution_id == e1)
                    .expect("conflicted item is deferred"),
            ))
        })
        .unwrap();
    assert_eq!(slots, 1, "only the later item claims a slot");
    assert_eq!(deferrals.reason, "dispatch_conflict");
}

// ---- Regression: review findings on slice-0 decision core ----

#[test]
fn conflicting_payload_replays_the_conflict_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    handle_command(&store, &config(), &submit("f1"), NOW).unwrap();
    let mut other = submit("f1");
    if let CommandBody::Submit { intent, .. } = &mut other.body {
        *intent = "different intent".into();
    }
    let first = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(reject_reason(&first), RejectionCode::IdempotencyConflict);
    // The same conflicting payload replays the stored conflict decision.
    let second = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(reject_reason(&second), RejectionCode::IdempotencyConflict);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "conflict replay returns the stored decision"
    );
}

#[test]
fn external_claim_names_only_branch_pairs() {
    let store = SqliteStore::in_memory(1).unwrap();
    let ws = WriteResource::Workspace {
        workspace_id: "ws-w-1".into(),
    };
    let d = handle_command(
        &store,
        &config(),
        &req(
            "w1",
            CommandBody::ClaimResources {
                resources: vec![ws.clone()],
                ttl_seconds: 60,
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::InvalidRequest);
    let d = handle_command(
        &store,
        &config(),
        &req(
            "w2",
            CommandBody::ReleaseResources {
                resources: vec![ws],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::InvalidRequest);
}

#[test]
fn cancel_noop_reports_the_current_version() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("n1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Stopping);
    let d = handle_command(
        &store,
        &config(),
        &req(
            "n2",
            CommandBody::Cancel {
                work_id,
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Accepted { work_version, .. } => {
            assert_eq!(
                work_version,
                Some(1),
                "an accepted no-op carries no new version"
            );
        }
        other => panic!("expected accepted, got {other:?}"),
    }
}

#[test]
fn release_judgment_requires_a_matching_recorded_judgment() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("j1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Paused);
    let release = |head: &str, version: u64| CommandBody::ReleaseJudgment {
        work_id: work_id.clone(),
        expected_work_version: version,
        expected_pr_head: head.into(),
    };

    // Nothing recorded yet.
    let d = handle_command(&store, &config(), &req("j2", release("h1", 1)), NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::Ineligible);

    // The adapter must record the head before a judgment may bind to it.
    seed_pr_head(&store, &work_id, "h1", NOW);
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "j3",
                CommandBody::JudgeFindings {
                    work_id: work_id.clone(),
                    expected_work_version: 1,
                    expected_pr_head: "h1".into(),
                    decisions: vec![FindingDecision {
                        finding_id: "f1".into(),
                        choice: FindingChoice::Fix,
                    }],
                },
            ),
            NOW,
        )
        .unwrap(),
    );

    // A stale head cannot release the recorded judgment.
    let d = handle_command(&store, &config(), &req("j4", release("h2", 2)), NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StalePrHead);

    let d = handle_command(&store, &config(), &req("j5", release("h1", 2)), NOW).unwrap();
    accepted(&d);
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
}

#[test]
fn resume_readmission_keeps_reservations_sequence_and_budget_anchor() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("m1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    let first = read_execution(&store, &e1);

    // Pause per the lifecycle contract: release the slot, keep reservations.
    force_state(&store, &e1, ExecutionState::Paused);
    apply_effects(
        &store,
        &[Effect::ReleaseSlot {
            execution_id: e1.clone(),
        }],
    );
    let d = handle_command(
        &store,
        &config(),
        &req(
            "m2",
            CommandBody::Resume {
                work_id,
                expected_work_version: 1,
                execution_id: e1.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);

    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 10)?.is_some(),
                "own retained reservations must not conflict"
            );
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(e.invocations_used, 2);
    assert_eq!(
        e.started_seq, first.started_seq,
        "the first dispatch anchors the runtime budget"
    );
    assert_eq!(e.started_unix_seconds, first.started_unix_seconds);

    let (reservations, request_ids) = store
        .transact(|tx| {
            Ok((
                tx.reservations()?.len(),
                tx.pending_requests()?
                    .into_iter()
                    .map(|r| r.request_id)
                    .collect::<Vec<_>>(),
            ))
        })
        .unwrap();
    assert_eq!(reservations, 2, "workspace + branch, no duplicates");
    assert_eq!(
        request_ids,
        vec![format!("{}:inv:0", e1.0), format!("{}:inv:1", e1.0),]
    );
}

// ---- Converge round 2: shadow-review regression coverage ----

#[test]
fn workspace_claim_cannot_block_dispatch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ws1"), NOW).unwrap());
    // An external actor names the Work's own managed workspace key.
    let ws = WriteResource::Workspace {
        workspace_id: format!("ws-{}", work_id.0),
    };
    let d = handle_command(
        &store,
        &config(),
        &req(
            "ws2",
            CommandBody::ClaimResources {
                resources: vec![ws.clone()],
                ttl_seconds: 60,
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::InvalidRequest);
    let d = handle_command(
        &store,
        &config(),
        &req(
            "ws3",
            CommandBody::ReleaseResources {
                resources: vec![ws],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::InvalidRequest);
    assert!(
        store.transact(|tx| tx.reservations()).unwrap().is_empty(),
        "no external claim may hold a workspace: key"
    );

    // Dispatch is not blocked: the Execution reserves both keys itself.
    store
        .transact(|tx| {
            let admitted = workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?;
            assert!(admitted.is_some(), "workspace squat cannot defer dispatch");
            Ok(())
        })
        .unwrap();
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Ready);
    let (reservations, requests) = store
        .transact(|tx| Ok((tx.reservations()?.len(), tx.pending_requests()?.len())))
        .unwrap();
    assert_eq!(
        reservations, 2,
        "workspace + branch, both held by the Execution"
    );
    assert_eq!(requests, 1);
}

#[test]
fn resume_replays_and_readmits_once() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("rp1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    let first_dispatch = read_execution(&store, &e1);
    force_state(&store, &e1, ExecutionState::Paused);
    apply_effects(
        &store,
        &[Effect::ReleaseSlot {
            execution_id: e1.clone(),
        }],
    );

    let resume = req(
        "rp2",
        CommandBody::Resume {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: e1.clone(),
        },
    );
    let d = assert_replay_locked(&store, &resume, NOW, NOW);
    accepted(&d);
    let committed_at = match &d {
        CommandDecision::Accepted { committed_at, .. } => *committed_at,
        other => panic!("expected accepted, got {other:?}"),
    };

    // The replay must not re-decide against the now-stale version 1.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "replay must not bump the version again");
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Queued);
    assert_eq!(
        e.enqueued_seq, committed_at,
        "replay must not requeue with a fresh sequence"
    );

    // Re-admission reuses the retained reservations instead of conflicting.
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 10)?.is_some(),
                "own retained reservations must not conflict"
            );
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(e.invocations_used, 2);
    assert_eq!(
        e.started_seq, first_dispatch.started_seq,
        "the first dispatch anchors the runtime budget"
    );
    assert_eq!(e.started_unix_seconds, first_dispatch.started_unix_seconds);

    let (reservations, request_ids) = store
        .transact(|tx| {
            Ok((
                tx.reservations()?.len(),
                tx.pending_requests()?
                    .into_iter()
                    .map(|r| r.request_id)
                    .collect::<Vec<_>>(),
            ))
        })
        .unwrap();
    assert_eq!(reservations, 2, "workspace + branch, no duplicates");
    assert_eq!(
        request_ids,
        vec![format!("{}:inv:0", e1.0), format!("{}:inv:1", e1.0)]
    );
}

#[test]
fn retry_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("tr1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Failed);
    let retry = req(
        "tr2",
        CommandBody::Retry {
            work_id: work_id.clone(),
            expected_work_version: 1,
            expected_execution_id: e1.clone(),
        },
    );
    let d = assert_replay_locked(&store, &retry, NOW, NOW);
    accepted(&d);
    // A re-decision would be StaleWorkVersion (the Work is now version 2)
    // or would create a third Execution.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "replay must not bump the version again");
    let ordinals = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap()
        .into_iter()
        .map(|e| e.ordinal)
        .collect::<Vec<_>>();
    assert_eq!(ordinals, vec![0, 1], "no third Execution on replay");
}

#[test]
fn cancel_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ca1"), NOW).unwrap());
    let cancel = req(
        "ca2",
        CommandBody::Cancel {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: None,
        },
    );
    let d = assert_replay_locked(&store, &cancel, NOW, NOW);
    accepted(&d);
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Cancelled);
    // A re-decision would be StaleWorkVersion or a second version bump.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "replay must not bump the version again");
}

#[test]
fn claim_resources_replays_without_renewing() {
    let store = SqliteStore::in_memory(1).unwrap();
    let claim = req(
        "cpr1",
        CommandBody::ClaimResources {
            resources: vec![branch_res()],
            ttl_seconds: 60,
        },
    );
    // Replay after the TTL midpoint: re-deciding would renew to NOW + 90.
    let d = assert_replay_locked(&store, &claim, NOW, NOW + 30);
    accepted(&d);
    let reservations = store.transact(|tx| tx.reservations()).unwrap();
    assert_eq!(reservations.len(), 1, "one reservation row survives replay");
    assert_eq!(
        reservations[0].expires_at_unix_seconds,
        Some(NOW + 60),
        "replay must not renew the TTL"
    );
}

#[test]
fn release_resources_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let claim = CommandBody::ClaimResources {
        resources: vec![branch_res()],
        ttl_seconds: 60,
    };
    accepted(&handle_command(&store, &config(), &req("rrs1", claim), NOW).unwrap());
    let release = req(
        "rrs2",
        CommandBody::ReleaseResources {
            resources: vec![branch_res()],
        },
    );
    let d = assert_replay_locked(&store, &release, NOW, NOW);
    match &d {
        CommandDecision::Accepted {
            effect: CommandEffect::ResourcesReleased,
            ..
        } => {}
        other => panic!("expected ResourcesReleased, got {other:?}"),
    }
    assert!(
        store.transact(|tx| tx.reservations()).unwrap().is_empty(),
        "replay must not resurrect the reservation"
    );
}

#[test]
fn acknowledge_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ak1"), NOW).unwrap());
    // `acknowledge` only accepts an Attention version the saved state
    // currently derives; a paused Execution derives the paused-judgment id.
    force_state(&store, &e1, ExecutionState::Paused);
    let ack = req(
        "ak2",
        CommandBody::Acknowledge {
            work_id: work_id.clone(),
            expected_work_version: 1,
            attention_id: store
                .transact(|tx| {
                    let workloop_core::projection::QueryResult::Found { value, .. } =
                        workloop_core::projection::work_detail(tx, &work_id, NOW)?
                    else {
                        panic!("work must exist");
                    };
                    Ok(value.attention[0].attention_id.clone())
                })
                .unwrap(),
        },
    );
    let d = assert_replay_locked(&store, &ack, NOW, NOW);
    accepted(&d);
    // The sequence stability asserted inside the helper proves the ack row
    // was not re-applied; the Work carries no new version either.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 1, "acknowledge never bumps the Work version");
}

#[test]
fn judge_findings_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("jf1"), NOW).unwrap());
    seed_pr_head(&store, &work_id, "h1", NOW);
    let decisions = vec![FindingDecision {
        finding_id: "f1".into(),
        choice: FindingChoice::Fix,
    }];
    let judge = req(
        "jf2",
        CommandBody::JudgeFindings {
            work_id: work_id.clone(),
            expected_work_version: 1,
            expected_pr_head: "h1".into(),
            decisions: decisions.clone(),
        },
    );
    let d = assert_replay_locked(&store, &judge, NOW, NOW);
    accepted(&d);
    // A re-decision would be StaleWorkVersion (now version 2) or collide on
    // the judgment's (work_id, expected_pr_head) primary key.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "replay must not bump to version 3");
    let (head, recorded) = store
        .transact(|tx| tx.latest_judgment(&work_id))
        .unwrap()
        .expect("judgment recorded");
    assert_eq!(head, "h1");
    assert_eq!(
        serde_json::to_string(&recorded).unwrap(),
        serde_json::to_string(&decisions).unwrap(),
        "one judgment with the original decisions"
    );
}

#[test]
fn release_judgment_replays_the_same_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("rj1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Paused);
    seed_pr_head(&store, &work_id, "h1", NOW);
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "rj2",
                CommandBody::JudgeFindings {
                    work_id: work_id.clone(),
                    expected_work_version: 1,
                    expected_pr_head: "h1".into(),
                    decisions: vec![FindingDecision {
                        finding_id: "f1".into(),
                        choice: FindingChoice::Accept,
                    }],
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    let release = req(
        "rj3",
        CommandBody::ReleaseJudgment {
            work_id: work_id.clone(),
            expected_work_version: 2,
            expected_pr_head: "h1".into(),
        },
    );
    let d = assert_replay_locked(&store, &release, NOW, NOW);
    accepted(&d);
    let committed_at = match &d {
        CommandDecision::Accepted { committed_at, .. } => *committed_at,
        other => panic!("expected accepted, got {other:?}"),
    };
    // Re-deciding now would be StaleWorkVersion (the Work is version 3) or
    // Ineligible (the Execution is no longer paused).
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 3, "replay must not bump to version 4");
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Queued);
    assert_eq!(
        e.enqueued_seq, committed_at,
        "replay must not requeue with a fresh sequence"
    );
}

// ---- Converge round 3: invocation-budget bounds + PR-head provenance ----

fn budget_config(max_invocations: u32, max_review_rounds: u32) -> WorkloopConfig {
    config_with_budget(ExecutionBudget {
        max_invocations,
        max_review_rounds,
        ..ExecutionBudget::default()
    })
}

fn judge_req(key: &str, work_id: &WorkId, version: u64, head: &str) -> CommandRequest {
    req(
        key,
        CommandBody::JudgeFindings {
            work_id: work_id.clone(),
            expected_work_version: version,
            expected_pr_head: head.into(),
            decisions: vec![FindingDecision {
                finding_id: "f1".into(),
                choice: FindingChoice::Fix,
            }],
        },
    )
}

fn rejection_evidence(d: &CommandDecision) -> Vec<String> {
    match d {
        CommandDecision::Rejected { evidence, .. } => evidence.clone(),
        other => panic!("expected rejected, got {other:?}"),
    }
}

#[test]
fn admission_never_exceeds_max_invocations() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = budget_config(1, 5);
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config, &submit("b1"), NOW).unwrap());

    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config, &e1, NOW)?.is_some(),
                "first admission is within budget"
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(read_execution(&store, &e1).invocations_used, 1);

    // Back to the queue as a resume/retry sweep would leave it; the budget
    // is now spent and the next admission must fail terminally.
    force_state(&store, &e1, ExecutionState::Queued);
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config, &e1, NOW + 10)?.is_none(),
                "budget-spent admission must not emit a second request"
            );
            Ok(())
        })
        .unwrap();

    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("invocation_budget_exhausted")
    );
    assert_eq!(e.invocations_used, 1, "no counter increment past the bound");
    let outcome = e.outcome.expect("budget failure records an Outcome");
    assert_eq!(outcome.state, ExecutionState::Failed);
    assert_eq!(outcome.code.as_deref(), Some("invocation_budget_exhausted"));

    let (slots, reservations, requests) = store
        .transact(|tx| {
            Ok((
                tx.slot_holders()?.len(),
                tx.reservations()?.len(),
                tx.pending_requests()?.len(),
            ))
        })
        .unwrap();
    assert_eq!(slots, 0, "the slot is released");
    assert_eq!(reservations, 0, "self-held reservations are released");
    assert_eq!(requests, 1, "exactly one request was ever inserted");
}

#[test]
fn resume_fails_when_invocation_budget_exhausted() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = budget_config(1, 5);
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config, &submit("rb1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config, &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    force_state(&store, &e1, ExecutionState::Paused);
    apply_effects(
        &store,
        &[Effect::ReleaseSlot {
            execution_id: e1.clone(),
        }],
    );

    let resume = req(
        "rb2",
        CommandBody::Resume {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: e1.clone(),
        },
    );
    let d = assert_replay_locked(&store, &resume, NOW, NOW);
    match &d {
        CommandDecision::Accepted { work_version, .. } => {
            assert_eq!(*work_version, Some(2), "the Command commits one bump");
        }
        other => panic!("expected accepted, got {other:?}"),
    }

    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Failed,
        "a spent budget must fail, not requeue"
    );
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("invocation_budget_exhausted")
    );
    assert!(e.outcome.is_some(), "terminal code plus recorded Outcome");
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "replay must not bump the version again");
    let reservations = store.transact(|tx| tx.reservations()).unwrap();
    assert!(
        reservations.is_empty(),
        "the failed resume releases the Execution's reservations"
    );
}

#[test]
fn review_admission_never_exceeds_max_review_rounds() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = budget_config(3, 1);
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config, &submit("rv1"), NOW).unwrap());

    store
        .transact(|tx| {
            let admitted = workloop_core::work::admit_queued_role(
                tx,
                &config,
                &e1,
                InvocationRole::Review,
                NOW,
            )?;
            assert!(admitted.is_some(), "first review round is within budget");
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Ready);
    assert_eq!(
        e.invocations_used, 1,
        "a review admission consumes an invocation"
    );
    assert_eq!(e.review_rounds_used, 1, "and one review round");
    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].role, InvocationRole::Review);
    assert_eq!(requests[0].request_id, format!("{}:inv:0", e1.0));

    force_state(&store, &e1, ExecutionState::Queued);
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued_role(
                    tx,
                    &config,
                    &e1,
                    InvocationRole::Review,
                    NOW + 10,
                )?
                .is_none(),
                "the spent review bound must fail admission"
            );
            Ok(())
        })
        .unwrap();

    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(e.terminal_code.as_deref(), Some("review_budget_exhausted"));
    assert_eq!(e.invocations_used, 1, "invocation budget still has room");
    assert_eq!(e.review_rounds_used, 1, "no round counted past the bound");
    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(requests.len(), 1, "no second review request");
}

#[test]
fn judge_rejects_head_with_no_observation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("jn1"), NOW).unwrap());

    let d = handle_command(&store, &config(), &judge_req("jn2", &work_id, 1, "h1"), NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::SourceUnavailable);
    assert!(
        rejection_evidence(&d)
            .iter()
            .any(|e| e.contains(&work_id.0)),
        "evidence names the Work with no observed head"
    );
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 1, "an unverifiable head bumps nothing");
    assert!(
        store
            .transact(|tx| tx.latest_judgment(&work_id))
            .unwrap()
            .is_none(),
        "no judgment row is written"
    );
}

#[test]
fn judge_rejects_arbitrary_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ja1"), NOW).unwrap());
    seed_pr_head(&store, &work_id, "h1", NOW);

    let d = handle_command(&store, &config(), &judge_req("ja2", &work_id, 1, "h2"), NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StalePrHead);
    assert_eq!(
        rejection_evidence(&d),
        vec!["h1".to_string()],
        "evidence names the observed head"
    );
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 1, "a stale-head judgment bumps nothing");
    assert!(
        store
            .transact(|tx| tx.latest_judgment(&work_id))
            .unwrap()
            .is_none(),
        "no judgment row is written"
    );
}

#[test]
fn judge_accepts_the_observed_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("jc1"), NOW).unwrap());
    // The adapter recorded the head against the Execution; the provenance
    // read must resolve it through the Work either way.
    seed_pr_head_for_execution(&store, &e1, "h1");

    let d = handle_command(&store, &config(), &judge_req("jc2", &work_id, 1, "h1"), NOW).unwrap();
    accepted(&d);
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2);
    let (head, _) = store
        .transact(|tx| tx.latest_judgment(&work_id))
        .unwrap()
        .expect("judgment recorded");
    assert_eq!(head, "h1", "the judgment carries the observed head");
}

#[test]
fn release_rejects_head_changed_after_judgment() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("hc1"), NOW).unwrap());
    seed_pr_head(&store, &work_id, "h1", NOW);
    accepted(
        &handle_command(&store, &config(), &judge_req("hc2", &work_id, 1, "h1"), NOW).unwrap(),
    );
    // The PR moves after the judgment was accepted.
    seed_pr_head(&store, &work_id, "h2", NOW + 10);
    force_state(&store, &e1, ExecutionState::Paused);

    let d = handle_command(
        &store,
        &config(),
        &req(
            "hc3",
            CommandBody::ReleaseJudgment {
                work_id: work_id.clone(),
                expected_work_version: 2,
                expected_pr_head: "h1".into(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StalePrHead);
    assert_eq!(
        rejection_evidence(&d),
        vec!["h2".to_string()],
        "evidence names the head that moved"
    );
    assert_eq!(
        read_execution(&store, &e1).state,
        ExecutionState::Paused,
        "a stale-head release never requeues"
    );
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 2, "no version bump for the rejection");
}

#[test]
fn stale_head_observation_cannot_move_head_backward() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("sb1"), NOW).unwrap());
    // A delayed delivery for the older head arrives after the newer one.
    seed_pr_head(&store, &work_id, "h2", NOW + 10);
    seed_pr_head(&store, &work_id, "h1", NOW + 5);

    let d = handle_command(
        &store,
        &config(),
        &judge_req("sb2", &work_id, 1, "h2"),
        NOW + 10,
    )
    .unwrap();
    accepted(&d); // the newest observed head judges fine

    let d = handle_command(
        &store,
        &config(),
        &judge_req("sb3", &work_id, 2, "h1"),
        NOW + 15,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StalePrHead);
    assert_eq!(
        rejection_evidence(&d),
        vec!["h2".to_string()],
        "the delayed h1 observation never becomes the current head"
    );
}

#[test]
fn admission_rolls_back_on_request_persistence_failure() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("pf1"), NOW).unwrap());
    // Occupy the request ID admission will derive from invocations_used = 0.
    apply_effects(
        &store,
        &[Effect::InsertRequest {
            request: InvocationRequest {
                request_id: format!("{}:inv:0", e1.0),
                work_id: work_id.clone(),
                execution_id: e1.clone(),
                role: InvocationRole::Coding,
                payload: serde_json::json!({}),
            },
        }],
    );

    let result = store.transact(|tx| {
        workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?;
        Ok(())
    });
    assert!(
        result.is_err(),
        "a conflicting request ID aborts the admission transaction"
    );

    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Queued, "nothing committed");
    assert_eq!(
        e.invocations_used, 0,
        "no counter committed without the request"
    );
    let (slots, reservations, requests) = store
        .transact(|tx| {
            Ok((
                tx.slot_holders()?.len(),
                tx.reservations()?.len(),
                tx.pending_requests()?,
            ))
        })
        .unwrap();
    assert_eq!(slots, 0, "no slot claim committed");
    assert_eq!(reservations, 0, "no reservation committed");
    assert_eq!(requests.len(), 1, "only the pre-inserted row remains");
    assert_eq!(requests[0].request_id, format!("{}:inv:0", e1.0));
}

// ---- Converge round 1: terminal cancel releases retained reservations ----

/// A resumed Execution re-enters `queued` still holding its workspace and
/// branch rows (lifecycle: reservations are held while nonterminal and
/// released in the terminal commit). Cancelling it from `queued` must
/// release them in the same commit, or the keys strand under a cancelled
/// owner for the lifetime of the store.
#[test]
fn cancel_resumed_queued_execution_releases_retained_reservations() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("cr1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some(),
                "admission reserves workspace + branch"
            );
            Ok(())
        })
        .unwrap();
    // Pause per the lifecycle contract: release the slot, keep reservations.
    force_state(&store, &e1, ExecutionState::Paused);
    apply_effects(
        &store,
        &[Effect::ReleaseSlot {
            execution_id: e1.clone(),
        }],
    );
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "cr2",
                CommandBody::Resume {
                    work_id: work_id.clone(),
                    expected_work_version: 1,
                    execution_id: e1.clone(),
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
    assert_eq!(
        store.transact(|tx| tx.reservations()).unwrap().len(),
        2,
        "the resumed Execution still holds workspace + branch"
    );

    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "cr3",
                CommandBody::Cancel {
                    work_id,
                    expected_work_version: 2,
                    execution_id: None,
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Cancelled);
    assert!(
        store.transact(|tx| tx.reservations()).unwrap().is_empty(),
        "the terminal cancel releases the retained write-resource reservations"
    );
}

/// Cancelling a never-admitted `queued` Execution emits a holder-scoped
/// release for reservations it does not hold; an external claim on the same
/// branch key must survive untouched.
#[test]
fn cancel_from_queued_leaves_foreign_claims_alone() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("cr4"), NOW).unwrap());
    let work_branch = WriteResource::Branch {
        repository: "local/repo".into(),
        branch: format!("work/{}", work_id.0),
    };
    let mut claim = req(
        "cr5",
        CommandBody::ClaimResources {
            resources: vec![work_branch],
            ttl_seconds: 600,
        },
    );
    claim.actor.external_id = "outsider".into();
    accepted(&handle_command(&store, &config(), &claim, NOW).unwrap());
    assert_eq!(
        store.transact(|tx| tx.reservations()).unwrap().len(),
        1,
        "the external claim holds the work branch while the Execution is queued"
    );

    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "cr6",
                CommandBody::Cancel {
                    work_id,
                    expected_work_version: 1,
                    execution_id: None,
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Cancelled);
    let reservations = store.transact(|tx| tx.reservations()).unwrap();
    assert_eq!(reservations.len(), 1, "the foreign claim is not deleted");
    assert!(
        reservations[0].holder.eq(&ReservationHolder::External {
            actor_id: "outsider".into()
        }),
        "the surviving row is still the external claim"
    );
}

// ---- Converge round 1: admission rechecks the judgment's PR head ----

/// `release_judgment` requeues E1 under the head it verified, but the head
/// can move while E1 waits in the queue. Dispatch admission must recheck the
/// recorded judgment against the currently observed head before saving the
/// Invocation request: a judgment for the old head cannot authorize a worker
/// on the new head, and the moved-head state pauses for a fresh judgment
/// without reserving capacity or emitting a request.
#[test]
fn admission_refuses_to_honor_a_judgment_for_a_moved_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ah1"), NOW).unwrap());
    seed_pr_head(&store, &work_id, "h1", NOW);

    // Human-gate cycle: pause, judge on h1, release — E1 requeues under the
    // judgment bound to h1.
    force_state(&store, &e1, ExecutionState::Paused);
    accepted(
        &handle_command(&store, &config(), &judge_req("ah2", &work_id, 1, "h1"), NOW).unwrap(),
    );
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "ah3",
                CommandBody::ReleaseJudgment {
                    work_id: work_id.clone(),
                    expected_work_version: 2,
                    expected_pr_head: "h1".into(),
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);

    // The PR head moves while E1 waits in the queue.
    seed_pr_head(&store, &work_id, "h2", NOW + 10);

    // Dispatch must not authorize a worker on the stale h1 judgment.
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 20)?.is_none(),
                "a judgment for the old head cannot authorize admission"
            );
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Paused,
        "a moved head re-pauses for a fresh judgment"
    );
    let reason = e.pause_reason.expect("pause reason recorded");
    assert!(
        reason.contains("h1") && reason.contains("h2"),
        "the pause names both the decided and observed heads, got: {reason}"
    );
    let (slots, requests) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.pending_requests()?.len())))
        .unwrap();
    assert_eq!(slots, 0, "the stale-head pause reserves no capacity");
    assert_eq!(requests, 0, "no Invocation request is saved");

    // A fresh judgment on h2 releases the gate; admission then proceeds.
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_req("ah4", &work_id, 3, "h2"),
            NOW + 30,
        )
        .unwrap(),
    );
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "ah5",
                CommandBody::ReleaseJudgment {
                    work_id: work_id.clone(),
                    expected_work_version: 4,
                    expected_pr_head: "h2".into(),
                },
            ),
            NOW + 30,
        )
        .unwrap(),
    );
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 40)?.is_some(),
                "the current-head judgment admits"
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Ready);
    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        1,
        "exactly one request is saved after the fresh judgment"
    );
}

// ---- Converge round 2: recording the Invocation consumes its request ----

fn pending_request_ids(store: &SqliteStore) -> Vec<String> {
    store
        .transact(|tx| {
            Ok(tx
                .pending_requests()?
                .into_iter()
                .map(|r| r.request_id)
                .collect::<Vec<_>>())
        })
        .unwrap()
}

/// `pending_requests` is the durable handoff `execution` scans on wake and
/// after restart. Recording the Invocation for a request is its consumption,
/// so that request must leave the pending list — and only it — keeping a
/// fresh request distinguishable from one already started.
#[test]
fn recording_an_invocation_consumes_only_its_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("pc1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    let consumed_id = format!("{}:inv:0", e1.0);
    assert_eq!(pending_request_ids(&store), vec![consumed_id.clone()]);

    // `execution` records the Invocation before launch — the consumption.
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: Invocation {
                id: InvocationId("inv-1".into()),
                request_id: consumed_id.clone(),
                execution_id: e1.clone(),
                role: InvocationRole::Coding,
                state: InvocationState::Prepared,
                process_identity: None,
                required_artifacts: vec![],
                evidence_refs: vec![],
            },
        }],
    );
    assert!(
        pending_request_ids(&store).is_empty(),
        "a request whose Invocation was recorded must not stay pending"
    );
    assert_eq!(
        store
            .transact(|tx| tx.invocations_for_execution(&e1))
            .unwrap()
            .len(),
        1,
        "the recorded Invocation survives for reconciliation by request ID"
    );

    // The next admission saves a fresh request; the handoff must offer it
    // while still withholding the consumed one.
    force_state(&store, &e1, ExecutionState::Queued);
    apply_effects(
        &store,
        &[Effect::ReleaseSlot {
            execution_id: e1.clone(),
        }],
    );
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 10)?.is_some());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        pending_request_ids(&store),
        vec![format!("{}:inv:1", e1.0)],
        "only the fresh request is pending; the consumed one stays consumed"
    );
}

/// The consumption mark matches by request ID; an Invocation whose request
/// row is absent must still record (no aborted write) and must not disturb
/// the request that is genuinely pending.
#[test]
fn recording_an_invocation_without_a_request_row_leaves_pending_alone() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("pc2"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: Invocation {
                id: InvocationId("inv-orphan".into()),
                request_id: format!("{}:inv:ghost", e1.0),
                execution_id: e1.clone(),
                role: InvocationRole::Coding,
                state: InvocationState::Done,
                process_identity: None,
                required_artifacts: vec![],
                evidence_refs: vec![],
            },
        }],
    );
    assert_eq!(
        pending_request_ids(&store),
        vec![format!("{}:inv:0", e1.0)],
        "the unmatched Invocation leaves the real request pending"
    );
}

// ---- docs/v0.1.md Commands row: payload-change conflict for every kind ----

/// The Commands matrix says "Idempotent replay of every accepted Command; a
/// payload change under the same key yields `idempotency_conflict`". Replay
/// of the accepted decision is covered per kind above; this proves the
/// payload-change half for every `CommandBody` kind — the digest is compared
/// before any Command-specific validation, so a mutated body under a stored
/// key must reject as `idempotency_conflict` without reaching `decide`, and
/// the conflicting payload itself replays its stored conflict decision.
#[test]
fn every_command_kind_conflicts_on_a_payload_change_under_the_same_key() {
    /// A distinguishing payload edit per Command kind.
    fn mutate(body: &mut CommandBody) {
        match body {
            CommandBody::Submit { intent, .. } => *intent = "different intent".into(),
            CommandBody::ClaimResources { ttl_seconds, .. } => *ttl_seconds += 1,
            CommandBody::ReleaseResources { resources } => {
                resources.push(WriteResource::Branch {
                    repository: "local/repo".into(),
                    branch: "wip/other".into(),
                });
            }
            CommandBody::Retry {
                expected_work_version,
                ..
            }
            | CommandBody::Resume {
                expected_work_version,
                ..
            }
            | CommandBody::Cancel {
                expected_work_version,
                ..
            } => *expected_work_version = 99,
            CommandBody::Acknowledge { attention_id, .. } => *attention_id = "att-other".into(),
            CommandBody::JudgeFindings { decisions, .. } => decisions.push(FindingDecision {
                finding_id: "f-other".into(),
                choice: FindingChoice::Wait,
            }),
            CommandBody::ReleaseJudgment {
                expected_pr_head, ..
            } => *expected_pr_head = "h2".into(),
        }
    }

    let pair = |original: CommandRequest| {
        let mut mutant = original.clone();
        mutate(&mut mutant.body);
        (original, mutant)
    };

    // One case per CommandBody kind: a store where the original is Accepted.
    let mut cases: Vec<(&str, SqliteStore, CommandRequest, CommandRequest)> = Vec::new();
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (original, mutant) = pair(submit("mx-submit"));
        cases.push(("submit", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (original, mutant) = pair(req(
            "mx-claim",
            CommandBody::ClaimResources {
                resources: vec![branch_res()],
                ttl_seconds: 60,
            },
        ));
        cases.push(("claim_resources", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        accepted(
            &handle_command(
                &store,
                &config(),
                &req(
                    "mx-release-seed",
                    CommandBody::ClaimResources {
                        resources: vec![branch_res()],
                        ttl_seconds: 60,
                    },
                ),
                NOW,
            )
            .unwrap(),
        );
        let (original, mutant) = pair(req(
            "mx-release",
            CommandBody::ReleaseResources {
                resources: vec![branch_res()],
            },
        ));
        cases.push(("release_resources", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, e1) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-retry-seed"), NOW).unwrap(),
        );
        force_state(&store, &e1, ExecutionState::Failed);
        let (original, mutant) = pair(req(
            "mx-retry",
            CommandBody::Retry {
                work_id,
                expected_work_version: 1,
                expected_execution_id: e1,
            },
        ));
        cases.push(("retry", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, e1) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-resume-seed"), NOW).unwrap(),
        );
        force_state(&store, &e1, ExecutionState::Paused);
        let (original, mutant) = pair(req(
            "mx-resume",
            CommandBody::Resume {
                work_id,
                expected_work_version: 1,
                execution_id: e1,
            },
        ));
        cases.push(("resume", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, _) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-cancel-seed"), NOW).unwrap(),
        );
        let (original, mutant) = pair(req(
            "mx-cancel",
            CommandBody::Cancel {
                work_id,
                expected_work_version: 1,
                execution_id: None,
            },
        ));
        cases.push(("cancel", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, e1) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-ack-seed"), NOW).unwrap(),
        );
        // A paused Execution derives an Attention id the original may name.
        force_state(&store, &e1, ExecutionState::Paused);
        let (original, mutant) = pair(req(
            "mx-ack",
            CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: store
                    .transact(|tx| {
                        let workloop_core::projection::QueryResult::Found { value, .. } =
                            workloop_core::projection::work_detail(tx, &work_id, NOW)?
                        else {
                            panic!("work must exist");
                        };
                        Ok(value.attention[0].attention_id.clone())
                    })
                    .unwrap(),
            },
        ));
        cases.push(("acknowledge", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, _) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-judge-seed"), NOW).unwrap(),
        );
        seed_pr_head(&store, &work_id, "h1", NOW);
        let (original, mutant) = pair(req(
            "mx-judge",
            CommandBody::JudgeFindings {
                work_id,
                expected_work_version: 1,
                expected_pr_head: "h1".into(),
                decisions: vec![FindingDecision {
                    finding_id: "f1".into(),
                    choice: FindingChoice::Fix,
                }],
            },
        ));
        cases.push(("judge_findings", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, e1) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-rj-seed"), NOW).unwrap(),
        );
        force_state(&store, &e1, ExecutionState::Paused);
        seed_pr_head(&store, &work_id, "h1", NOW);
        accepted(
            &handle_command(
                &store,
                &config(),
                &req(
                    "mx-rj-judge",
                    CommandBody::JudgeFindings {
                        work_id: work_id.clone(),
                        expected_work_version: 1,
                        expected_pr_head: "h1".into(),
                        decisions: vec![FindingDecision {
                            finding_id: "f1".into(),
                            choice: FindingChoice::Accept,
                        }],
                    },
                ),
                NOW,
            )
            .unwrap(),
        );
        let (original, mutant) = pair(req(
            "mx-release-judgment",
            CommandBody::ReleaseJudgment {
                work_id,
                expected_work_version: 2,
                expected_pr_head: "h1".into(),
            },
        ));
        cases.push(("release_judgment", store, original, mutant));
    }
    assert_eq!(cases.len(), 9, "one case per CommandBody kind");

    for (label, store, original, mutant) in cases {
        let first = handle_command(&store, &config(), &original, NOW).unwrap();
        assert!(
            matches!(first, CommandDecision::Accepted { .. }),
            "{label}: the original must be accepted, got {first:?}"
        );
        let seq_before = store.transact(|tx| tx.last_seq()).expect("read last_seq");
        let conflict = handle_command(&store, &config(), &mutant, NOW).unwrap();
        assert_eq!(
            reject_reason(&conflict),
            RejectionCode::IdempotencyConflict,
            "{label}"
        );
        let seq_after = store.transact(|tx| tx.last_seq()).expect("read last_seq");
        assert_eq!(
            seq_before, seq_after,
            "{label}: a conflict rejection allocates no new sequence"
        );
        let replay = handle_command(&store, &config(), &mutant, NOW).unwrap();
        assert_eq!(
            serde_json::to_string(&conflict).unwrap(),
            serde_json::to_string(&replay).unwrap(),
            "{label}: the conflicting payload replays its stored conflict decision"
        );
    }
}

// ---- PR-head coverage: cancel versions, judgment replay surface,
// launch refusal, and work-scoped completion evidence ----

#[test]
fn cancel_noop_reports_the_stored_version() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("n1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    // ready → stopping bumps the version once.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "n2",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Stopping);
    // A repeated cancel while already stopping is an accepted no-op: it
    // reports the stored version, never one the commit did not produce.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "n3",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 2,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    match &d {
        CommandDecision::Accepted { work_version, .. } => {
            assert_eq!(*work_version, Some(2));
        }
        other => panic!("expected accepted no-op, got {other:?}"),
    }
    let stored = store.transact(|tx| tx.work(&work_id)).unwrap().unwrap();
    assert_eq!(stored.version, 2, "a no-op produced no version bump");
}
#[test]
fn cancel_queued_releases_the_resumed_executions_reservations() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("n4"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    // Pause retains the workspace/branch claims; resume requeues the
    // Execution still holding them.
    force_state(&store, &e1, ExecutionState::Paused);
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "n5",
                CommandBody::Resume {
                    work_id: work_id.clone(),
                    expected_work_version: 1,
                    execution_id: e1.clone(),
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
    // Cancelling the requeued Execution must release its retained claims —
    // execution-held reservations have no TTL.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "n6",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 2,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    accepted(&d);
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Cancelled);
    let reservations = store.transact(|tx| tx.reservations()).unwrap();
    assert!(
        reservations.is_empty(),
        "cancelled execution released its reservations"
    );
    let slots = store.transact(|tx| tx.slot_holders()).unwrap();
    assert!(
        slots.is_empty(),
        "cancelled execution released its capacity slot"
    );
}
#[test]
fn judgments_bind_to_the_recorded_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("j1"), NOW).unwrap());
    let emit_head = |head: &str| {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                source_event_id: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: Some(e1.clone()),
                fact: ObservationFact::PrHead {
                    repository: "local/repo".into(),
                    number: 1,
                    head_sha: head.into(),
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    let judge = |key: &str, head: &str, version: u64| {
        handle_command(
            &store,
            &config(),
            &req(
                key,
                CommandBody::JudgeFindings {
                    work_id: work_id.clone(),
                    expected_work_version: version,
                    expected_pr_head: head.into(),
                    decisions: vec![FindingDecision {
                        finding_id: "f1".into(),
                        choice: FindingChoice::Accept,
                    }],
                },
            ),
            NOW,
        )
        .unwrap()
    };
    let release = |key: &str, head: &str, version: u64| {
        handle_command(
            &store,
            &config(),
            &req(
                key,
                CommandBody::ReleaseJudgment {
                    work_id: work_id.clone(),
                    expected_work_version: version,
                    expected_pr_head: head.into(),
                },
            ),
            NOW,
        )
        .unwrap()
    };
    // Nothing observed to judge against: an unverifiable head is a
    // source-unavailable rejection, not a stale one.
    assert_eq!(
        reject_reason(&judge("j2", "sha-x", 1)),
        RejectionCode::SourceUnavailable
    );
    emit_head("sha-x");
    // A judgment for a different head than the recorded one is stale.
    assert_eq!(
        reject_reason(&judge("j3", "sha-old", 1)),
        RejectionCode::StalePrHead
    );
    // The current-head judgment is recorded.
    accepted(&judge("j4", "sha-x", 1));
    force_state(&store, &e1, ExecutionState::Paused);
    // The head moved under the recorded judgment: releasing it is stale.
    emit_head("sha-y");
    assert_eq!(
        reject_reason(&release("j5", "sha-x", 2)),
        RejectionCode::StalePrHead
    );
    // The new head has no recorded judgment — the recorded judgment is for
    // the old head, so the gate stays shut as stale.
    assert_eq!(
        reject_reason(&release("j6", "sha-y", 2)),
        RejectionCode::StalePrHead
    );
    // A judgment on the current head lets release requeue the Execution.
    accepted(&judge("j7", "sha-y", 2));
    accepted(&release("j8", "sha-y", 3));
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
}
#[test]
fn repeated_idempotency_conflict_replays_the_saved_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    handle_command(&store, &config(), &submit("k9"), NOW).unwrap();
    let mut other = submit("k9");
    if let CommandBody::Submit { intent, .. } = &mut other.body {
        *intent = "different intent".into();
    }
    let first = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(reject_reason(&first), RejectionCode::IdempotencyConflict);
    // The identical conflict delivered again must replay the durable
    // conflict decision, not hit the decision-table uniqueness index.
    let second = handle_command(&store, &config(), &other, NOW).unwrap();
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}
#[test]
fn resumed_execution_reuses_its_own_reservations() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("rr1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    // A paused Execution retains its workspace/branch claims and its
    // invocation counter; resume requeues the same Execution.
    force_state(&store, &e1, ExecutionState::Paused);
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "rr2",
                CommandBody::Resume {
                    work_id,
                    expected_work_version: 1,
                    execution_id: e1.clone(),
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some(),
                "self-held reservations must not defer re-admission"
            );
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Ready);
    assert_eq!(e.invocations_used, 2);
    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(requests.len(), 2, "one durable request per attempt");
    assert_ne!(requests[0].request_id, requests[1].request_id);
}
#[test]
fn invocation_budget_exhaustion_is_a_committed_failure() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut cfg = config();
    cfg.budget.max_invocations = 1;
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &cfg, &submit("b1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &cfg, &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    force_state(&store, &e1, ExecutionState::Paused);
    accepted(
        &handle_command(
            &store,
            &cfg,
            &req(
                "b2",
                CommandBody::Resume {
                    work_id,
                    expected_work_version: 1,
                    execution_id: e1.clone(),
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &cfg, &e1, NOW)?.is_none());
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("invocation_budget_exhausted")
    );
}
// ---- Observations: launch refusal (docs/lifecycle.md: `ready` → paused) ----

fn recorded_invocation(eid: &ExecutionId, id: &str) -> Invocation {
    Invocation {
        id: InvocationId(id.into()),
        request_id: format!("{}:inv:0", eid.0),
        execution_id: eid.clone(),
        role: InvocationRole::Coding,
        state: InvocationState::Prepared,
        process_identity: None,
        required_artifacts: vec![],
        evidence_refs: vec![],
    }
}

fn launch_refused(store: &SqliteStore, eid: &ExecutionId, invocation_id: &InvocationId) {
    workloop_core::work::handle_observation(
        store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: None,
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::LaunchRefused {
                invocation_id: invocation_id.clone(),
                reason: "spawn refused before any process existed".into(),
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}
#[test]
fn launch_refused_parks_a_ready_execution_and_resolves_the_invocation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("lr1"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    let inv = recorded_invocation(&e1, "inv-lr1");
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: inv.clone(),
        }],
    );
    launch_refused(&store, &e1, &inv.id);

    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Paused);
    assert_eq!(
        e.pause_reason.as_deref(),
        Some("repair: launch refused before spawn")
    );
    let (holders, reservations, invs) = store
        .transact(|tx| {
            Ok((
                tx.slot_holders()?.len(),
                tx.reservations()?.len(),
                tx.invocations_for_execution(&e1)?,
            ))
        })
        .unwrap();
    assert_eq!(holders, 0, "the slot is released for repair");
    assert_eq!(
        reservations, 2,
        "writer reservations stay held while paused"
    );
    assert_eq!(invs[0].state, InvocationState::Abandoned, "no process ran");
}

#[test]
fn late_launch_refusal_never_overwrites_a_stopping_execution() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("lr2"), NOW).unwrap());
    store
        .transact(|tx| {
            assert!(workloop_core::work::admit_queued(tx, &config(), &e1, NOW)?.is_some());
            Ok(())
        })
        .unwrap();
    let inv = recorded_invocation(&e1, "inv-lr2");
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: inv.clone(),
        }],
    );
    // Cancel wins the race with the spawn attempt.
    accepted(
        &handle_command(
            &store,
            &config(),
            &req(
                "lr2-cancel",
                CommandBody::Cancel {
                    work_id: w.clone(),
                    expected_work_version: 1,
                    execution_id: None,
                },
            ),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Stopping);

    launch_refused(&store, &e1, &inv.id);

    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Stopping,
        "a late refusal must not park a stopping Execution in paused"
    );
    assert_eq!(e.desired_action, Some(DesiredAction::Cancel));
    let (holders, invs) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.invocations_for_execution(&e1)?)))
        .unwrap();
    assert_eq!(holders, 1, "the slot stays held until the stop settles");
    assert_eq!(
        invs[0].state,
        InvocationState::Abandoned,
        "the refusal still resolves the never-started writer"
    );
}

#[test]
fn late_launch_refusal_never_resurrects_a_terminal_execution() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("lr3"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Cancelled);

    launch_refused(&store, &e1, &InvocationId("inv-lr3".into()));

    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Cancelled,
        "a terminal Execution is not resurrected to paused"
    );
    assert_eq!(e.terminal_code.as_deref(), Some("setup"));
    assert!(e.outcome.is_some(), "the recorded outcome survives");
}
#[test]
fn work_scoped_check_and_review_evidence_completes_the_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("sc1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Waiting);
    // Completion requires the applied publish intent for the Work.
    apply_effects(
        &store,
        &[Effect::InsertPrActionIntent {
            intent: PrActionIntent {
                action_id: "act-sc1".into(),
                work_id: w.clone(),
                repository: "local/repo".into(),
                action: PrAction::PublishPr {
                    branch: format!("work/{}", w.0),
                    title: "sc1".into(),
                    body: String::new(),
                },
                status: PrActionStatus::Applied,
            },
        }],
    );
    // A delivery that names only the Work: the fact type makes the
    // execution subject optional, and it must still be assessed.
    let emit = |fact: ObservationFact| {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                source_event_id: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: None,
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(ObservationFact::PrHead {
        repository: "local/repo".into(),
        number: 1,
        head_sha: "sha-1".into(),
    });
    emit(ObservationFact::PrChecks {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        conclusion: CheckConclusion::Success,
    });
    assert_eq!(
        read_execution(&store, &e1).state,
        ExecutionState::Waiting,
        "checks alone are not completion evidence"
    );
    emit(ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        reviewer: "rev".into(),
        verdict: ReviewVerdict::Approve,
    });
    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Succeeded,
        "work-scoped check + review evidence completes the Work"
    );
    let outcome = e.outcome.expect("completion records an outcome");
    assert_eq!(outcome.code.as_deref(), Some("succeeded"));
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("sha-1")),
        "the verified head is preserved: {:?}",
        outcome.detail
    );
    // Further work-scoped deliveries against the terminal Work are inert.
    emit(ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        reviewer: "rev2".into(),
        verdict: ReviewVerdict::Approve,
    });
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Succeeded);
}
