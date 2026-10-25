//! Contract tests: Commands, Admission, Reservations from docs/v0.1.md.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::{Observation, ObservationFact, Outcome};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{WorkloopConfig, handle_command};
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
    let first = handle_command(&store, &config(), &submit("k1"), NOW).unwrap();
    let second = handle_command(&store, &config(), &submit("k1"), NOW).unwrap();
    let first_json = serde_json::to_string(&first).unwrap();
    let second_json = serde_json::to_string(&second).unwrap();
    assert_eq!(first_json, second_json, "replay returns the saved decision");
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
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ak1"), NOW).unwrap());
    let ack = req(
        "ak2",
        CommandBody::Acknowledge {
            work_id: work_id.clone(),
            expected_work_version: 1,
            attention_id: "att-1".into(),
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
