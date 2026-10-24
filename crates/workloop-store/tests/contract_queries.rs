//! Projection Query contract tests (`docs/v0.1.md`: `work_list`,
//! `work_detail`, `attention`, `capacity`, `command_result`). Queries are
//! read-only derived state — they never refresh GitHub or start a worker.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::*;
use workloop_core::projection::*;
use workloop_core::store::*;
use workloop_core::work::{WorkloopConfig, admit_queued, dispatch_once, handle_command, sweep};
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

fn submit(store: &SqliteStore, key: &str, repo: &str, number: u64) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                issue_refs: vec![GitHubIssueRef {
                    repository: repo.into(),
                    number,
                }],
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

fn found<T>(r: QueryResult<T>) -> T {
    match r {
        QueryResult::Found { value, .. } => value,
        QueryResult::NotFound => panic!("expected found, got not_found"),
        QueryResult::Forbidden => panic!("expected found, got forbidden"),
        QueryResult::StoreUnavailable => panic!("expected found, got store_unavailable"),
    }
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store.transact(|tx| tx.execution(id)).unwrap().unwrap()
}

#[test]
fn work_list_phases_derive_from_records() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (_w1, _e1) = submit(&store, "q1", "local/repo", 1);
    let (_w2, e2) = submit(&store, "q2", "local/repo", 2);
    let view = found(store.transact(|tx| work_list(tx, NOW)).unwrap());
    assert_eq!(view.items.len(), 2);
    assert!(view.items.iter().all(|i| i.phase == WorkPhase::Queued));
    // Admitting the second flips its derived phase without a state column.
    store
        .transact(|tx| admit_queued(tx, &config(), &e2, NOW).map(|_| ()))
        .unwrap();
    let view = found(store.transact(|tx| work_list(tx, NOW)).unwrap());
    let item = view
        .items
        .iter()
        .find(|i| i.phase == WorkPhase::Implementing);
    assert!(item.is_some(), "admitted work derives implementing");
}

#[test]
fn work_detail_binds_findings_to_the_current_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q3", "local/repo", 7);
    let obs = |fact| Observation {
        source_event_id: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact,
        observed_unix_seconds: NOW,
    };
    for fact in [
        ObservationFact::PrHead {
            repository: "local/repo".into(),
            number: 7,
            head_sha: "sha-old".into(),
        },
        ObservationFact::PrHead {
            repository: "local/repo".into(),
            number: 7,
            head_sha: "sha-new".into(),
        },
        ObservationFact::PrReview {
            repository: "local/repo".into(),
            head_sha: "sha-old".into(),
            reviewer: "stale-rev".into(),
            verdict: ReviewVerdict::ChangesRequested,
        },
        ObservationFact::PrReview {
            repository: "local/repo".into(),
            head_sha: "sha-new".into(),
            reviewer: "rev".into(),
            verdict: ReviewVerdict::ChangesRequested,
        },
    ] {
        workloop_core::work::handle_observation(&store, &config(), &obs(fact), NOW).unwrap();
    }
    let QueryResult::Found { value, sources, .. } =
        store.transact(|tx| work_detail(tx, &work_id, NOW)).unwrap()
    else {
        panic!("expected found")
    };
    let (pr_ref, head) = value.current_pr.as_ref().unwrap();
    assert_eq!(pr_ref.number, 7);
    assert_eq!(head.as_deref(), Some("sha-new"));
    // Only the current-head finding is actionable.
    assert_eq!(value.findings.len(), 1);
    assert!(value.findings[0].contains("sha-new"));
    // The review source was read for this head → known.
    assert!(
        sources
            .iter()
            .any(|s| s.source == "github:review" && s.availability == Availability::Known)
    );
    let missing = store
        .transact(|tx| work_detail(tx, &WorkId("w-nope".into()), NOW))
        .unwrap();
    assert!(matches!(missing, QueryResult::NotFound));
}

#[test]
fn work_detail_reports_the_terminal_outcome() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, "q20", "local/repo", 61);
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q20-cancel".into(),
            body: CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let view = found(store.transact(|tx| work_detail(tx, &work_id, NOW)).unwrap());
    let outcome = view.outcome.expect("terminal outcome surfaced in detail");
    assert_eq!(outcome.state, ExecutionState::Cancelled);
    assert_eq!(outcome.code.as_deref(), Some("cancelled"));
}

#[test]
fn attention_versions_and_acknowledgement() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "q4", "local/repo", 9);
    // Park the execution paused → paused_judgment attention.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair: needs human".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].reason, "paused_judgment");
    let att_id = items[0].attention_id.clone();

    // Acknowledging hides only that version.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q4-ack".into(),
            body: CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: att_id.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "acknowledged attention is hidden"
    );

    // A changed condition resurfaces under a new version id.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.pause_reason = Some("repair: different problem".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Paused,
            })
        })
        .unwrap();
    let items = found(store.transact(|tx| attention(tx, None, NOW)).unwrap());
    assert_eq!(items.len(), 1);
    assert_ne!(items[0].attention_id, att_id, "new version resurfaces");
}

#[test]
fn uncertain_beyond_threshold_surfaces_attention() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, eid) = submit(&store, "q5", "local/repo", 11);
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    // Drop into uncertain just now — below the 15-minute threshold.
    let obs = Observation {
        source_event_id: None,
        subject_work_id: None,
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::ProcessState {
            invocation_id: InvocationId("inv-x".into()),
            process_identity: String::new(),
            state: ProcessStateFact::Unknown,
        },
        observed_unix_seconds: NOW,
    };
    workloop_core::work::handle_observation(&store, &config(), &obs, NOW).unwrap();
    assert!(
        found(store.transact(|tx| attention(tx, None, NOW)).unwrap()).is_empty(),
        "below threshold: no attention"
    );
    let later = NOW + 901;
    let items = found(store.transact(|tx| attention(tx, None, later)).unwrap());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].reason, "uncertain_beyond_threshold");
    assert_eq!(
        items[0].investigation_threshold_seconds,
        Some(15 * 60),
        "threshold recorded"
    );
    assert!(!items[0].next_action.is_empty());
}

#[test]
fn capacity_explains_every_slot_reservation_and_deferral() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w1, e1) = submit(&store, "q6", "local/repo", 21);
    let (_w2, e2) = submit(&store, "q7", "local/repo", 22);
    // One free slot: e1 admitted, e2 deferred — the deferral must not
    // block the queue's explanation.
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW).map(|_| ()))
        .unwrap();
    let view = found(store.transact(|tx| capacity(tx)).unwrap());
    assert_eq!(view.used, 1);
    assert_eq!(view.limit, 1);
    assert_eq!(view.holders[0].execution_id, e1);
    assert!(view.reservations.len() >= 2, "workspace+branch held");
    assert!(
        view.reservations
            .iter()
            .all(|r| matches!(r.holder, ReservationHolder::Execution { .. }))
    );
    assert_eq!(view.deferred.len(), 1);
    assert_eq!(view.deferred[0].execution_id, e2);
    assert_eq!(view.deferred[0].reason, "capacity_exhausted");

    // An external claim with an expiry is visible in the same view.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "q8-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![WriteResource::Workspace {
                    workspace_id: "shared-ws".into(),
                }],
                ttl_seconds: 300,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let view = found(store.transact(|tx| capacity(tx)).unwrap());
    let external = view
        .reservations
        .iter()
        .find(|r| r.resource_key == "workspace:shared-ws")
        .expect("external reservation listed");
    assert!(matches!(
        external.holder,
        ReservationHolder::External { .. }
    ));
    assert_eq!(external.expires_at_unix_seconds, Some(NOW + 300));
}

#[test]
fn command_result_recovers_a_lost_response_by_id_and_replay_key() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request = CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: "q9-submit".into(),
        body: CommandBody::Submit {
            intent: "x".into(),
            issue_refs: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let decision = handle_command(&store, &config(), &request, NOW).unwrap();
    let command_id = decision.command_id().clone();

    let by_id = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::Id {
                    command_id: command_id.clone(),
                },
            )
        })
        .unwrap();
    assert!(matches!(by_id, QueryResult::Found { .. }));

    let by_key = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: Origin("cli".into()),
                    actor: actor(),
                    idempotency_key: "q9-submit".into(),
                },
            )
        })
        .unwrap();
    let QueryResult::Found { value, .. } = by_key else {
        panic!("replay key must recover the decision")
    };
    assert_eq!(value.command_id(), &command_id);

    let missing = store
        .transact(|tx| {
            command_result(
                tx,
                &CommandResultLookup::Id {
                    command_id: CommandId("cmd-nope".into()),
                },
            )
        })
        .unwrap();
    assert!(matches!(missing, QueryResult::NotFound));
}

#[test]
fn runtime_limit_is_a_committed_stopping_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, eid) = submit(&store, "q10", "local/repo", 31);
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Ready);
    let over = NOW + (6 * 3600) + 1;
    sweep(&store, &config(), over).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Stopping);
    assert_eq!(e.desired_action, Some(DesiredAction::Fail));
    // Queue time does not consume the runtime budget: a queued execution
    // at the same clock is untouched.
    let (_w2, e2) = submit(&store, "q11", "local/repo", 32);
    sweep(&store, &config(), over).unwrap();
    assert_eq!(execution(&store, &e2).state, ExecutionState::Queued);
}

#[test]
fn deferred_head_does_not_block_later_items() {
    let store = SqliteStore::in_memory(2).unwrap();
    // w1 wants the same branch the external actor holds; w2 is free.
    let (_w1, e1) = submit(&store, "q12", "local/repo", 41);
    let (_w2, e2) = submit(&store, "q13", "other/repo", 42);
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "other-actor".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "q14-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![
                    WriteResource::Workspace {
                        workspace_id: "ws-w-1".into(),
                    },
                    WriteResource::Branch {
                        repository: "local/repo".into(),
                        branch: "work/w-1".into(),
                    },
                ],
                ttl_seconds: 600,
            },
        },
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW).map(|_| ()))
        .unwrap();
    assert_eq!(
        execution(&store, &e1).state,
        ExecutionState::Queued,
        "conflicting head stays queued"
    );
    assert_eq!(
        execution(&store, &e2).state,
        ExecutionState::Ready,
        "later independent item was admitted past the deferred head"
    );
}

#[test]
fn expired_external_claim_forfeits_exclusion() {
    let store = SqliteStore::in_memory(2).unwrap();
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "ext".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "q15-claim".into(),
            body: CommandBody::ClaimResources {
                resources: vec![WriteResource::Branch {
                    repository: "local/repo".into(),
                    branch: "work/w-1".into(),
                }],
                ttl_seconds: 60,
            },
        },
        NOW,
    )
    .unwrap();
    let (_w, eid) = submit(&store, "q16", "local/repo", 51);
    // The claim's TTL lapsed: dispatch reclaims the resource and admits.
    store
        .transact(|tx| dispatch_once(tx, &config(), NOW + 61).map(|_| ()))
        .unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Ready);
}
