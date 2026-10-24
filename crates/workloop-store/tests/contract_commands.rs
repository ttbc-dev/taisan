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
    // Nothing recorded to judge against: an unverified head cannot be
    // accepted.
    assert_eq!(reject_reason(&judge("j2", "sha-x", 1)), RejectionCode::StalePrHead);
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
    // The new head has no recorded judgment yet — the gate stays shut.
    assert_eq!(
        reject_reason(&release("j6", "sha-y", 2)),
        RejectionCode::Ineligible
    );
    // A judgment on the current head lets release requeue the Execution.
    accepted(&judge("j7", "sha-y", 2));
    accepted(&release("j8", "sha-y", 3));
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);
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
        resources: vec![
            WriteResource::Branch {
                repository: "local/repo".into(),
                branch: format!("work/{}", work_id.0),
            },
            WriteResource::Workspace {
                workspace_id: format!("ws-{}", work_id.0),
            },
        ],
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
