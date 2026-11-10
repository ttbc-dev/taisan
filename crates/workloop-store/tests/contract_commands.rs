//! Contract tests: Commands, Admission, Reservations from docs/v0.1.md.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, dispatch_once, handle_command,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
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

fn target(head: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(head),
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
                credential_ref: "cred-publication".into(),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec!["ci".into()],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
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
            sources: vec![],
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

/// Record the persisted open+head evidence `judge_findings` and
/// `release_judgment` compare against: the applied `open_change_request`
/// intent, its `ChangeOpened` receipt inside a `HostActionResult`, then
/// the head observation.
fn seed_change(store: &SqliteStore, work_id: &WorkId, head: &str, observed_at: i64) {
    // Only an applied intent validates the receipt — tests that record
    // their own `act-open` intent keep their own status.
    let already = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .iter()
                .any(|i| i.action_id == "act-open"))
        })
        .unwrap();
    if !already {
        apply_effects(
            store,
            &[Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-open".into(),
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
                    created_seq: 0,
                },
            }],
        );
    }
    apply_effects(
        store,
        &[
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::HostActionResult {
                        action_id: "act-open".into(),
                        authenticated_actor: Some(publisher()),
                        status: HostActionStatus::Applied,
                        effects: vec![NormalizedEffectFact::ChangeOpened {
                            change: change(),
                            head: rev(head),
                            source_branch: format!("work/{}", work_id.0),
                            target_branch: "main".into(),
                        }],
                    },
                    observed_unix_seconds: observed_at,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::ChangeHead {
                        change: change(),
                        head: rev(head),
                    },
                    observed_unix_seconds: observed_at,
                },
            },
        ],
    );
}

/// Record only a fresh head observation for the already-opened change —
/// the adapter's `ChangeHead` for a moved head.
fn seed_head(store: &SqliteStore, work_id: &WorkId, head: &str, observed_at: i64) {
    apply_effects(
        store,
        &[Effect::InsertObservation {
            observation: Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::ChangeHead {
                    change: change(),
                    head: rev(head),
                },
                observed_unix_seconds: observed_at,
            },
        }],
    );
}

/// Deliver a complete review collection: seals the immutable round for
/// the given head and bumps the Work version.
fn seal_review(store: &SqliteStore, work_id: &WorkId, head: &str, source_keys: &[&str]) {
    workloop_core::work::handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::Review {
                target: target(head),
                source: "reviewer".into(),
                source_revision: None,
                completeness: Completeness::Complete,
                occurrences: source_keys
                    .iter()
                    .map(|key| OccurrenceInput {
                        source_key: key.to_string(),
                        content_digest: None,
                        severity: None,
                        position: None,
                        thread_id: None,
                    })
                    .collect(),
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

/// The occurrence ids of the Work's latest sealed round — the exact set
/// a `JudgeFindings` decision list must cover.
fn occurrence_ids(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| {
            let Some(round) = tx.latest_round(work_id)? else {
                return Ok(vec![]);
            };
            Ok(tx
                .occurrences(&round.id)?
                .into_iter()
                .map(|o| o.id)
                .collect::<Vec<_>>())
        })
        .unwrap()
}

/// A `JudgeFindings` request covering every occurrence of the current
/// sealed round with `choice`.
fn judge_request(
    store: &SqliteStore,
    key: &str,
    work_id: &WorkId,
    version: WorkVersion,
    head: &str,
    choice: FindingChoice,
) -> CommandRequest {
    let decisions = occurrence_ids(store, work_id)
        .into_iter()
        .map(|occurrence_id| FindingDecision {
            occurrence_id,
            choice: choice.clone(),
            rationale: match choice {
                FindingChoice::Accept | FindingChoice::Dismiss => Some("accepted".into()),
                _ => None,
            },
        })
        .collect();
    req(
        key,
        CommandBody::JudgeFindings {
            work_id: work_id.clone(),
            expected_work_version: version,
            expected_head: rev(head),
            decisions,
        },
    )
}

fn release_request(
    key: &str,
    work_id: &WorkId,
    version: WorkVersion,
    head: &str,
) -> CommandRequest {
    req(
        key,
        CommandBody::ReleaseJudgment {
            work_id: work_id.clone(),
            expected_work_version: version,
            expected_head: rev(head),
        },
    )
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
        repository: repo(),
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
            repository: repo(),
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
        spec_revision: 1,
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

    // Nothing observed yet: no change request exists, so the release is
    // source-unavailable rather than stale.
    let d = handle_command(
        &store,
        &config(),
        &release_request("j2", &work_id, 1, "h1"),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::SourceUnavailable);

    // The adapter recorded the head; the round must be sealed before a
    // judgment may bind to it.
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_request(&store, "j3", &work_id, 2, "h1", FindingChoice::Fix),
            NOW,
        )
        .unwrap(),
    );

    // A stale head cannot release the recorded judgment.
    let d = handle_command(
        &store,
        &config(),
        &release_request("j4", &work_id, 3, "h2"),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StaleHead);

    let d = handle_command(
        &store,
        &config(),
        &release_request("j5", &work_id, 3, "h1"),
        NOW,
    )
    .unwrap();
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
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);
    let judge = judge_request(&store, "jf2", &work_id, 2, "h1", FindingChoice::Fix);
    let decisions = match &judge.body {
        CommandBody::JudgeFindings { decisions, .. } => decisions.clone(),
        other => panic!("expected judge body, got {other:?}"),
    };
    let d = assert_replay_locked(&store, &judge, NOW, NOW);
    accepted(&d);
    // A re-decision would be StaleWorkVersion (now version 3) or collide on
    // the judgment's primary key.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 3, "replay must not bump to version 4");
    let recorded = store
        .transact(|tx| tx.latest_judgment(&work_id))
        .unwrap()
        .expect("judgment recorded");
    let round = store
        .transact(|tx| tx.latest_round(&work_id))
        .unwrap()
        .expect("round recorded");
    assert_eq!(
        recorded.round_id, round.id,
        "one judgment on the sealed round"
    );
    assert_eq!(
        serde_json::to_string(&recorded.decisions).unwrap(),
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
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_request(&store, "rj2", &work_id, 2, "h1", FindingChoice::Accept),
            NOW,
        )
        .unwrap(),
    );
    let release = release_request("rj3", &work_id, 3, "h1");
    let d = assert_replay_locked(&store, &release, NOW, NOW);
    accepted(&d);
    let committed_at = match &d {
        CommandDecision::Accepted { committed_at, .. } => *committed_at,
        other => panic!("expected accepted, got {other:?}"),
    };
    // Re-deciding now would be StaleWorkVersion (the Work is version 4) or
    // Ineligible (the Execution is no longer paused).
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 4, "replay must not bump to version 5");
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Queued);
    assert_eq!(
        e.enqueued_seq, committed_at,
        "replay must not requeue with a fresh sequence"
    );
}

// ---- Converge round 3: invocation-budget bounds + head provenance ----

fn budget_config(max_invocations: u32, max_review_rounds: u32) -> WorkloopConfig {
    config_with_budget(ExecutionBudget {
        max_invocations,
        max_review_rounds,
        ..ExecutionBudget::default()
    })
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

    let d = handle_command(
        &store,
        &config(),
        &req(
            "jn2",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_head: rev("h1"),
                decisions: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::SourceUnavailable);
    assert!(
        rejection_evidence(&d)
            .iter()
            .any(|e| e.contains(&work_id.0)),
        "evidence names the Work with no observed change"
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
    seed_change(&store, &work_id, "h1", NOW);

    let d = handle_command(
        &store,
        &config(),
        &req(
            "ja2",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_head: rev("h2"),
                decisions: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StaleHead);
    assert!(
        rejection_evidence(&d).iter().any(|e| e.contains("h1")),
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
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("jc1"), NOW).unwrap());
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);

    let d = handle_command(
        &store,
        &config(),
        &judge_request(&store, "jc2", &work_id, 2, "h1", FindingChoice::Fix),
        NOW,
    )
    .unwrap();
    accepted(&d);
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("work exists");
    assert_eq!(work.version, 3);
    let judgment = store
        .transact(|tx| tx.latest_judgment(&work_id))
        .unwrap()
        .expect("judgment recorded");
    let round = store
        .transact(|tx| tx.latest_round(&work_id))
        .unwrap()
        .expect("round recorded");
    assert_eq!(
        judgment.round_id, round.id,
        "the judgment binds the sealed round"
    );
    assert_eq!(
        round.target.head,
        rev("h1"),
        "the judgment's round carries the observed head"
    );
}

#[test]
fn release_rejects_head_changed_after_judgment() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("hc1"), NOW).unwrap());
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_request(&store, "hc2", &work_id, 2, "h1", FindingChoice::Fix),
            NOW,
        )
        .unwrap(),
    );
    // The change head moves after the judgment was accepted.
    seed_head(&store, &work_id, "h2", NOW + 10);
    force_state(&store, &e1, ExecutionState::Paused);

    let d = handle_command(
        &store,
        &config(),
        &release_request("hc3", &work_id, 3, "h1"),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StaleHead);
    assert!(
        rejection_evidence(&d).iter().any(|e| e.contains("h2")),
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
    assert_eq!(work.version, 3, "no version bump for the rejection");
}

#[test]
fn stale_head_observation_cannot_move_head_backward() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("sb1"), NOW).unwrap());
    // A delayed delivery for the older head arrives after the newer one.
    seed_change(&store, &work_id, "h2", NOW + 10);
    seed_head(&store, &work_id, "h1", NOW + 5);
    seal_review(&store, &work_id, "h2", &["f1"]);

    let d = handle_command(
        &store,
        &config(),
        &judge_request(&store, "sb2", &work_id, 2, "h2", FindingChoice::Fix),
        NOW + 10,
    )
    .unwrap();
    accepted(&d); // the newest observed head judges fine

    let d = handle_command(
        &store,
        &config(),
        &req(
            "sb3",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 3,
                expected_head: rev("h1"),
                decisions: vec![],
            },
        ),
        NOW + 15,
    )
    .unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::StaleHead);
    assert!(
        rejection_evidence(&d).iter().any(|e| e.contains("h2")),
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
                spec_revision: 1,
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
        repository: repo(),
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

// ---- Converge round 1: admission rechecks the judgment's head ----

/// `release_judgment` requeues E1 under the target it verified, but the
/// target can move while E1 waits in the queue. Dispatch admission must
/// recheck the recorded judgment against the currently derived target
/// before saving the Invocation request: a judgment on the old round
/// cannot authorize a worker on the new target, and the moved-target
/// state pauses for a fresh judgment without reserving capacity or
/// emitting a request.
#[test]
fn admission_refuses_to_honor_a_judgment_for_a_moved_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("ah1"), NOW).unwrap());
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["f1"]);

    // Human-gate cycle: pause, judge on h1, release — E1 requeues under the
    // judgment bound to the h1 round.
    force_state(&store, &e1, ExecutionState::Paused);
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_request(&store, "ah2", &work_id, 2, "h1", FindingChoice::Fix),
            NOW,
        )
        .unwrap(),
    );
    accepted(
        &handle_command(
            &store,
            &config(),
            &release_request("ah3", &work_id, 3, "h1"),
            NOW,
        )
        .unwrap(),
    );
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Queued);

    // The change head moves while E1 waits in the queue.
    seed_head(&store, &work_id, "h2", NOW + 10);

    // Dispatch must not authorize a worker on the stale h1 judgment.
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 20)?.is_none(),
                "a judgment for the old target cannot authorize admission"
            );
            Ok(())
        })
        .unwrap();
    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Paused,
        "a moved target re-pauses for a fresh judgment"
    );
    assert_eq!(
        e.repair_prerequisites,
        vec![RepairPrerequisite::OperatorDecision],
        "the pause is a human-decision repair, not an automatic retry"
    );
    let (slots, requests) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.pending_requests()?.len())))
        .unwrap();
    assert_eq!(slots, 0, "the stale-target pause reserves no capacity");
    assert_eq!(requests, 0, "no Invocation request is saved");

    // A fresh sealed round and judgment on h2 releases the gate; admission
    // then proceeds.
    seal_review(&store, &work_id, "h2", &["f2"]);
    accepted(
        &handle_command(
            &store,
            &config(),
            &judge_request(&store, "ah4", &work_id, 5, "h2", FindingChoice::Fix),
            NOW + 30,
        )
        .unwrap(),
    );
    accepted(
        &handle_command(
            &store,
            &config(),
            &release_request("ah5", &work_id, 6, "h2"),
            NOW + 30,
        )
        .unwrap(),
    );
    store
        .transact(|tx| {
            assert!(
                workloop_core::work::admit_queued(tx, &config(), &e1, NOW + 40)?.is_some(),
                "the current-target judgment admits"
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
        &[
            Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-1".into()),
                    request_id: consumed_id.clone(),
                    execution_id: e1.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Prepared,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            },
            Effect::MarkRequestConsumed {
                request_id: consumed_id.clone(),
            },
        ],
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
        &[
            Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-orphan".into()),
                    request_id: format!("{}:inv:ghost", e1.0),
                    execution_id: e1.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            },
            Effect::MarkRequestConsumed {
                request_id: format!("{}:inv:ghost", e1.0),
            },
        ],
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
                    repository: repo(),
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
                occurrence_id: "occ-other".into(),
                choice: FindingChoice::Wait,
                rationale: None,
            }),
            CommandBody::ReleaseJudgment { expected_head, .. } => *expected_head = rev("h2"),
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
        seed_change(&store, &work_id, "h1", NOW);
        seal_review(&store, &work_id, "h1", &["f1"]);
        let (original, mutant) = pair(judge_request(
            &store,
            "mx-judge",
            &work_id,
            2,
            "h1",
            FindingChoice::Fix,
        ));
        cases.push(("judge_findings", store, original, mutant));
    }
    {
        let store = SqliteStore::in_memory(1).unwrap();
        let (work_id, e1) = accepted_with_execution(
            &handle_command(&store, &config(), &submit("mx-rj-seed"), NOW).unwrap(),
        );
        force_state(&store, &e1, ExecutionState::Paused);
        seed_change(&store, &work_id, "h1", NOW);
        seal_review(&store, &work_id, "h1", &["f1"]);
        accepted(
            &handle_command(
                &store,
                &config(),
                &judge_request(
                    &store,
                    "mx-rj-judge",
                    &work_id,
                    2,
                    "h1",
                    FindingChoice::Accept,
                ),
                NOW,
            )
            .unwrap(),
        );
        let (original, mutant) = pair(release_request("mx-release-judgment", &work_id, 3, "h1"));
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

// ---- Head coverage: cancel versions, judgment replay surface,
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
    let judge = |key: &str, head: &str, version: u64| {
        handle_command(
            &store,
            &config(),
            &req(
                key,
                CommandBody::JudgeFindings {
                    work_id: work_id.clone(),
                    expected_work_version: version,
                    expected_head: rev(head),
                    decisions: occurrence_ids(&store, &work_id)
                        .into_iter()
                        .map(|occurrence_id| FindingDecision {
                            occurrence_id,
                            choice: FindingChoice::Accept,
                            rationale: Some("accepted".into()),
                        })
                        .collect(),
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
            &release_request(key, &work_id, version, head),
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
    seed_change(&store, &work_id, "sha-x", NOW);
    // A judgment for a different head than the recorded one is stale.
    assert_eq!(
        reject_reason(&judge("j3", "sha-old", 1)),
        RejectionCode::StaleHead
    );
    // The current-head judgment is recorded against the sealed round.
    seal_review(&store, &work_id, "sha-x", &["f1"]);
    accepted(&judge("j4", "sha-x", 2));
    force_state(&store, &e1, ExecutionState::Paused);
    // The head moved under the recorded judgment: releasing it is stale.
    seed_head(&store, &work_id, "sha-y", NOW + 10);
    assert_eq!(
        reject_reason(&release("j5", "sha-x", 3)),
        RejectionCode::StaleHead
    );
    // The new head has no sealed round yet — the recorded round is for the
    // old target, so the gate stays shut as stale.
    assert_eq!(
        reject_reason(&release("j6", "sha-y", 3)),
        RejectionCode::StaleHead
    );
    // A sealed round and judgment on the current head lets release
    // requeue the Execution.
    seal_review(&store, &work_id, "sha-y", &["f2"]);
    accepted(&judge("j7", "sha-y", 4));
    accepted(&release("j8", "sha-y", 5));
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
        descendants: vec![],
    }
}

fn launch_refused(store: &SqliteStore, eid: &ExecutionId, invocation_id: &InvocationId) {
    workloop_core::work::handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
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
    let mut invocation = recorded_invocation(&e1, "sc1-coding");
    invocation.state = InvocationState::Done;
    invocation.required_artifacts = vec![RequiredArtifact {
        kind: "artifact.json".into(),
        path: "artifact.json".into(),
        expected_digest: Some("artifact-digest".into()),
    }];
    apply_effects(
        &store,
        &[
            Effect::InsertInvocation { invocation },
            // The artifact evidence for the required artifact.
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: None,
                    subject_execution_id: Some(e1.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: InvocationId("sc1-coding".into()),
                        required_artifact: "artifact.json".into(),
                        state: ArtifactFact::Present {
                            digest: "artifact-digest".into(),
                        },
                    },
                    observed_unix_seconds: NOW,
                },
            },
        ],
    );
    // Completion requires the applied publish intents for the Work.
    for (action_id, action) in [
        (
            "act-sc1-push",
            HostAction::PushRef {
                branch: format!("work/{}", w.0),
                produced: rev("artifact-digest"),
            },
        ),
        (
            "act-sc1-open",
            HostAction::OpenChangeRequest {
                source_branch: format!("work/{}", w.0),
                target_branch: "main".into(),
                title: "sc1".into(),
                body: String::new(),
            },
        ),
    ] {
        apply_effects(
            &store,
            &[Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.into(),
                    work_id: w.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "test".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq: 0,
                },
            }],
        );
    }
    // A delivery that names only the Work: the fact type makes the
    // execution subject optional, and it must still be assessed.
    let emit = |fact: ObservationFact| {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: None,
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(ObservationFact::HostActionResult {
        action_id: "act-sc1-open".into(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::ChangeOpened {
            change: change(),
            head: rev("sha-1"),
            source_branch: format!("work/{}", w.0),
            target_branch: "main".into(),
        }],
    });
    emit(ObservationFact::ChangeHead {
        change: change(),
        head: rev("sha-1"),
    });
    emit(ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Success,
    });
    assert_eq!(
        read_execution(&store, &e1).state,
        ExecutionState::Waiting,
        "checks alone are not completion evidence"
    );
    emit(ObservationFact::Review {
        target: target("sha-1"),
        source: "rev".into(),
        source_revision: None,
        completeness: Completeness::Complete,
        occurrences: vec![],
    });
    assert_eq!(
        read_execution(&store, &e1).state,
        ExecutionState::Waiting,
        "the invariant lane still needs a positive assessment"
    );
    emit(ObservationFact::Assessment {
        target: target("sha-1"),
        set_digest: "invariants".into(),
        complete: true,
        items: vec![AssessmentItem {
            id: "inv-1".into(),
            verdict: AssessmentVerdict::Positive,
            refs: vec![],
        }],
    });
    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Succeeded,
        "work-scoped check + review + assessment evidence completes the Work"
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
    emit(ObservationFact::Assessment {
        target: target("sha-1"),
        set_digest: "invariants".into(),
        complete: true,
        items: vec![AssessmentItem {
            id: "inv-1".into(),
            verdict: AssessmentVerdict::Positive,
            refs: vec![],
        }],
    });
    assert_eq!(read_execution(&store, &e1).state, ExecutionState::Succeeded);
}

#[test]
fn completion_requires_artifacts_publication_and_resolved_writers() {
    for scenario in [
        "no_coding",
        "missing",
        "corrupt",
        "writer",
        "unpublished",
        "complete",
    ] {
        let store = SqliteStore::in_memory(1).unwrap();
        let (w, eid) = accepted_with_execution(
            &handle_command(&store, &config(), &submit(scenario), NOW).unwrap(),
        );
        force_state(&store, &eid, ExecutionState::Waiting);
        store
            .transact(|tx| {
                if scenario != "no_coding" {
                    let mut invocation = recorded_invocation(&eid, "coding");
                    invocation.state = if scenario == "writer" {
                        InvocationState::Running
                    } else {
                        InvocationState::Done
                    };
                    invocation.required_artifacts = vec![RequiredArtifact {
                        kind: "artifact.json".into(),
                        path: "artifact.json".into(),
                        expected_digest: None,
                    }];
                    tx.apply(&Effect::InsertInvocation { invocation })?;
                    if scenario != "missing" {
                        tx.apply(&Effect::InsertObservation {
                            observation: Observation {
                                delivery: None,
                                subject_work_id: Some(w.clone()),
                                subject_execution_id: Some(eid.clone()),
                                fact: ObservationFact::Artifact {
                                    invocation_id: InvocationId("coding".into()),
                                    required_artifact: "artifact.json".into(),
                                    state: if scenario == "corrupt" {
                                        ArtifactFact::Corrupt { reason: None }
                                    } else {
                                        ArtifactFact::Present {
                                            digest: "artifact-digest".into(),
                                        }
                                    },
                                },
                                observed_unix_seconds: NOW,
                            },
                        })?;
                    }
                }
                for (action_id, action, status) in [
                    (
                        "act-push",
                        HostAction::PushRef {
                            branch: format!("work/{}", w.0),
                            produced: rev("artifact-digest"),
                        },
                        HostActionStatus::Applied,
                    ),
                    (
                        "act-open",
                        HostAction::OpenChangeRequest {
                            source_branch: format!("work/{}", w.0),
                            target_branch: "main".into(),
                            title: "change".into(),
                            body: String::new(),
                        },
                        if scenario == "unpublished" {
                            HostActionStatus::Failed {
                                reason: "declined".into(),
                            }
                        } else {
                            HostActionStatus::Applied
                        },
                    ),
                ] {
                    tx.apply(&Effect::InsertHostActionIntent {
                        intent: HostActionIntent {
                            action_id: action_id.into(),
                            work_id: w.clone(),
                            host: host(),
                            repository: repo(),
                            role: ActorRole::Publication,
                            actor: publisher(),
                            action,
                            payload_digest: "test".into(),
                            status,
                            authenticated_actor: None,
                            effects: vec![],
                            created_seq: 0,
                        },
                    })?;
                }
                Ok(())
            })
            .unwrap();
        seed_change(&store, &w, "head", NOW);
        for fact in [
            ObservationFact::Check {
                change: change(),
                head: rev("head"),
                check_id: "ci".into(),
                producer: publisher(),
                conclusion: CheckConclusion::Success,
            },
            ObservationFact::Review {
                target: target("head"),
                source: "reviewer".into(),
                source_revision: None,
                completeness: Completeness::Complete,
                occurrences: vec![],
            },
            ObservationFact::Assessment {
                target: target("head"),
                set_digest: "invariants".into(),
                complete: true,
                items: vec![AssessmentItem {
                    id: "inv-1".into(),
                    verdict: AssessmentVerdict::Positive,
                    refs: vec![],
                }],
            },
        ] {
            workloop_core::work::handle_observation(
                &store,
                &config(),
                &Observation {
                    delivery: None,
                    subject_work_id: Some(w.clone()),
                    subject_execution_id: None,
                    fact,
                    observed_unix_seconds: NOW,
                },
                NOW,
            )
            .unwrap();
        }
        assert_eq!(
            read_execution(&store, &eid).state,
            if scenario == "complete" {
                ExecutionState::Succeeded
            } else {
                ExecutionState::Waiting
            },
            "{scenario}"
        );
    }
}

#[test]
fn dispatch_state_conflict_defers_only_the_conflicted_execution() {
    let root = std::env::temp_dir().join(format!(
        "workloop-dispatch-state-conflict-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 2).unwrap();
    let (_, first) = accepted_with_execution(
        &handle_command(&store, &config(), &submit("state-conflict-first"), NOW).unwrap(),
    );
    let (_, second) = accepted_with_execution(
        &handle_command(&store, &config(), &submit("state-conflict-second"), NOW).unwrap(),
    );
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_first_admission BEFORE UPDATE OF state ON executions
         WHEN OLD.enqueued_seq = (SELECT MIN(enqueued_seq) FROM executions)
          AND NEW.state = 'ready'
         BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    let admitted = store
        .transact(|tx| dispatch_once(tx, &config(), NOW))
        .expect("a guarded state conflict must not abort the dispatch scan");
    assert_eq!(admitted, vec![second.clone()]);
    assert_eq!(read_execution(&store, &first).state, ExecutionState::Queued);
    assert_eq!(read_execution(&store, &second).state, ExecutionState::Ready);
    store
        .transact(|tx| {
            assert_eq!(tx.slot_holders()?.len(), 1);
            assert_eq!(tx.reservations()?.len(), 2);
            assert_eq!(tx.pending_requests()?.len(), 1);
            let deferrals = tx.deferrals()?;
            assert_eq!(deferrals.len(), 1);
            assert_eq!(deferrals[0].execution_id, first);
            assert_eq!(deferrals[0].reason, "dispatch_conflict");
            Ok(())
        })
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER refuse_first_admission;")
        .unwrap();
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "workloop-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Regression (acceptance): a target-branch-only move makes the sealed
/// round's recorded target stale — the head did not move, the diff
/// target did, and a judgment against the old set is refused.
#[test]
fn target_branch_move_stales_the_sealed_round() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) = accepted_with_execution(
        &handle_command(&store, &config(), &submit("k-target-move"), NOW).unwrap(),
    );
    seed_change(&store, &work_id, "h1", NOW);
    seal_review(&store, &work_id, "h1", &["a"]);
    // The target branch advances under an unchanged head.
    apply_effects(
        &store,
        &[Effect::InsertObservation {
            observation: Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::BranchState {
                    repository: repo(),
                    branch: "main".into(),
                    state: BranchState::At {
                        revision: rev("main-2"),
                    },
                },
                observed_unix_seconds: NOW + 1,
            },
        }],
    );
    let decision = handle_command(
        &store,
        &config(),
        &judge_request(&store, "j-tm", &work_id, 2, "h1", FindingChoice::Fix),
        NOW + 2,
    )
    .unwrap();
    assert_eq!(
        reject_reason(&decision),
        RejectionCode::StaleHead,
        "a target-branch move stales the round's recorded target"
    );
}

/// Regression (acceptance): a `ChangeOpened` receipt for a different
/// repository is never adopted as the Work's change — repository
/// identity, not receipt shape, binds a change to the Work.
#[test]
fn foreign_repository_receipt_is_not_the_works_target() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _) = accepted_with_execution(
        &handle_command(&store, &config(), &submit("k-foreign"), NOW).unwrap(),
    );
    let foreign_repo = RepositoryRef::new(host(), RepositoryId::trusted("other-repo"));
    let foreign_change = ChangeRequestRef {
        repository: foreign_repo.clone(),
        change_request: ChangeRequestId::trusted("cr-9"),
        display_number: Some(9),
    };
    let foreign_head = CommitRevision::new(foreign_repo, "f-head").expect("valid revision");
    apply_effects(
        &store,
        &[
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::HostActionResult {
                        action_id: "act-foreign".into(),
                        authenticated_actor: Some(publisher()),
                        status: HostActionStatus::Applied,
                        effects: vec![NormalizedEffectFact::ChangeOpened {
                            change: foreign_change.clone(),
                            head: foreign_head.clone(),
                            source_branch: "work/other".into(),
                            target_branch: "main".into(),
                        }],
                    },
                    observed_unix_seconds: NOW,
                },
            },
            Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::ChangeHead {
                        change: foreign_change,
                        head: foreign_head,
                    },
                    observed_unix_seconds: NOW,
                },
            },
        ],
    );
    let decision = handle_command(
        &store,
        &config(),
        &judge_request(&store, "j-f", &work_id, 1, "f-head", FindingChoice::Fix),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(
        reject_reason(&decision),
        RejectionCode::SourceUnavailable,
        "a foreign-repository change is never the Work's target"
    );
}

/// Regression (acceptance): archive/new-store cutover — a pre-v1 file
/// that already carries user tables is refused, never upgraded in
/// place; the operator archives it and starts a new store.
#[test]
fn unversioned_store_file_is_archive_only() {
    let root = temp_root("cutover-legacy");
    let db = root.join("store.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE legacy_rows (id TEXT);")
            .unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { detail }) => {
            assert!(detail.contains("archive"), "detail: {detail}");
        }
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got an open store"),
    }
    // The refusal must not mutate the archive: journal mode stays the
    // file's own and no WAL sidecars appear.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "a refused archive must not be re-journaled");
    }
    assert!(
        !db.with_extension("db-wal").exists() && !db.with_extension("db-shm").exists(),
        "a refused open must not leave WAL sidecars"
    );
}

/// Regression (acceptance): a store file stamped with a newer format
/// version is refused rather than read leniently.
#[test]
fn a_newer_store_format_is_refused() {
    let root = temp_root("cutover-newer");
    let db = root.join("store.db");
    {
        let _store = SqliteStore::open(&db, 1).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99i64).unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { detail }) => {
            assert!(detail.contains("99"), "detail: {detail}");
        }
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got an open store"),
    }
}

/// Regression: a genuine v1 file upgrades to v2 in place — the new
/// tables and the invocations.descendants column appear and stamped
/// rows survive; a second open is a no-op because the migration is
/// idempotent.
#[test]
fn a_v1_store_upgrades_to_v2_preserving_rows() {
    let root = temp_root("upgrade-v1");
    let db = root.join("store.db");
    let request = submit("k-upgrade");
    let (work_id, _) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        accepted_with_execution(&handle_command(&store, &config(), &request, NOW).unwrap())
    };
    // Reshape the file into a real v1 store: drop the v2 additions and
    // the descendants column, then stamp user_version=1.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE artifact_handoffs;
             DROP TABLE verification_results;
             ALTER TABLE invocations DROP COLUMN descendants;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }
    let store = SqliteStore::open(&db, 1).unwrap();
    store
        .transact(|tx| {
            let work = tx.work(&work_id)?.expect("work row survives the upgrade");
            assert_eq!(work.version, 1);
            Ok(())
        })
        .unwrap();
    // A second open must not re-migrate — the schema is already v2.
    let _ = SqliteStore::open(&db, 1).unwrap();
}

/// Regression (acceptance): new-format records survive a reopen — the
/// durable Work row, the recorded decision, and the idempotent replay
/// are all intact on a fresh handle.
#[test]
fn reopened_store_round_trips_new_format_records() {
    let root = temp_root("reopen-integrity");
    let db = root.join("store.db");
    let request = submit("k-reopen");
    let (work_id, execution_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        accepted_with_execution(&handle_command(&store, &config(), &request, NOW).unwrap())
    };
    let store = SqliteStore::open(&db, 1).unwrap();
    // The same submission replays the recorded decision — no new Work,
    // no new Execution, no version bump.
    let replayed = handle_command(&store, &config(), &request, NOW + 10).unwrap();
    let (replayed_work, replayed_execution) = accepted_with_execution(&replayed);
    assert_eq!(replayed_work, work_id);
    assert_eq!(replayed_execution, execution_id);
    store
        .transact(|tx| {
            let work = tx.work(&work_id)?.expect("work row survives reopen");
            assert_eq!(work.version, 1);
            assert_eq!(work.binding.repository, repo());
            assert!(tx.spec(&work_id, None)?.is_some());
            Ok(())
        })
        .unwrap();
}

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn submit_with_sources(key: &str, sources: Vec<SourceSnapshot>) -> CommandRequest {
    req(
        key,
        CommandBody::Submit {
            intent: "do the thing".into(),
            sources,
            constraints: vec![],
            verification_argv: vec![],
        },
    )
}

fn config_with_issue_read() -> WorkloopConfig {
    let mut c = config();
    c.declared_capabilities.insert(HostCapability::IssueRead);
    c
}

/// Regression: a `DeliveryScope::Local` delivery has no host column, and
/// a NULL key dedupes nothing under SQLite UNIQUE semantics — the
/// dedupe index must still collapse a repeated (scope, source,
/// event_id) delivery.
#[test]
fn repeated_local_delivery_is_deduplicated() {
    let store = SqliteStore::in_memory(1).unwrap();
    let obs = |event_id: &str| Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "test".into(),
            event_id: event_id.into(),
        }),
        subject_work_id: None,
        subject_execution_id: None,
        fact: ObservationFact::ChangeHead {
            change: change(),
            head: rev("h-1"),
        },
        observed_unix_seconds: NOW,
    };
    assert_eq!(
        workloop_core::work::handle_observation(&store, &config(), &obs("ev-1"), NOW).unwrap(),
        IngestReceipt::Recorded
    );
    assert_eq!(
        workloop_core::work::handle_observation(&store, &config(), &obs("ev-1"), NOW).unwrap(),
        IngestReceipt::Duplicate,
        "the identical local delivery must dedupe, not insert twice"
    );
    // A different event id under the same scope and source is new data.
    assert_eq!(
        workloop_core::work::handle_observation(&store, &config(), &obs("ev-2"), NOW).unwrap(),
        IngestReceipt::Recorded
    );
}

/// Regression: a delivery that reuses (scope, source, event_id) with a
/// different fact is a producer conflict — rejected and visible, never
/// folded into the recorded row the way a replay is.
#[test]
fn delivery_conflict_with_different_content_is_rejected() {
    let store = SqliteStore::in_memory(1).unwrap();
    let obs = |head: &str| Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "test".into(),
            event_id: "ev-conflict".into(),
        }),
        subject_work_id: None,
        subject_execution_id: None,
        fact: ObservationFact::ChangeHead {
            change: change(),
            head: rev(head),
        },
        observed_unix_seconds: NOW,
    };
    assert_eq!(
        workloop_core::work::handle_observation(&store, &config(), &obs("h-1"), NOW).unwrap(),
        IngestReceipt::Recorded
    );
    match workloop_core::work::handle_observation(&store, &config(), &obs("h-2"), NOW).unwrap() {
        IngestReceipt::Rejected(_) => {}
        other => panic!("expected Rejected for conflicting content, got {other:?}"),
    }
    // The recorded row is still the first fact — nothing was folded in.
    store
        .transact(|tx| {
            let obs = tx.all_observations()?;
            assert_eq!(obs.len(), 1);
            assert!(
                matches!(&obs[0].fact, ObservationFact::ChangeHead { head, .. }
                    if head == &rev("h-1")),
                "stored fact: {:?}",
                obs[0].fact
            );
            Ok(())
        })
        .unwrap();
}

/// Regression (source gate): an unverified source defers admission, a
/// matching authoritative read unblocks it, and a changed read pauses
/// the Execution for repair — dispatch never runs on `NeedsCheck` or
/// `Changed` sources.
#[test]
fn source_gate_defers_admits_and_pauses_on_change() {
    let store = SqliteStore::in_memory(1).unwrap();
    let source = SourceSnapshot {
        issue: issue(9),
        source_revision: "rev-1".into(),
        content_digest: "digest-1".into(),
    };
    let d = handle_command(
        &store,
        &config_with_issue_read(),
        &submit_with_sources("k-gate", vec![source]),
        NOW,
    )
    .unwrap();
    let (work_id, eid) = accepted_with_execution(&d);
    // NeedsCheck: dispatch owes a source read and defers admission.
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            assert_eq!(
                tx.execution(&eid)?.unwrap().state,
                ExecutionState::Queued,
                "unverified source holds admission"
            );
            Ok(())
        })
        .unwrap();
    let read = |revision: &str, digest: &str, state: IssueState| Observation {
        delivery: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: None,
        fact: ObservationFact::Issue {
            issue: issue(9),
            source_revision: revision.into(),
            content_digest: digest.into(),
            state,
        },
        observed_unix_seconds: NOW,
    };
    // A matching read resolves the gate to Matches; dispatch admits.
    workloop_core::work::handle_observation(
        &store,
        &config_with_issue_read(),
        &read("rev-1", "digest-1", IssueState::Open),
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            assert_eq!(tx.execution(&eid)?.unwrap().state, ExecutionState::Ready);
            Ok(())
        })
        .unwrap();
    // A later read showing changed content stops the live writer for
    // repair — a live Execution never runs on a stale spec, and the pause
    // lands only after writer-stop proof (lifecycle.md).
    workloop_core::work::handle_observation(
        &store,
        &config_with_issue_read(),
        &read("rev-2", "digest-2", IssueState::Open),
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| {
            let e = tx.execution(&eid)?.unwrap();
            assert_eq!(
                e.state,
                ExecutionState::Stopping,
                "an active Execution stops the writer before pausing"
            );
            assert_eq!(e.desired_action, Some(DesiredAction::Pause));
            assert_eq!(
                e.repair_prerequisites,
                vec![RepairPrerequisite::SpecReapproval]
            );
            Ok(())
        })
        .unwrap();
    // Writer-stop proof settles the Execution to paused, releasing the
    // slot while keeping the recorded continuation.
    workloop_core::work::handle_observation(
        &store,
        &config_with_issue_read(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::ProcessState {
                invocation_id: InvocationId("inv-gate".into()),
                process_identity: "1@1".into(),
                state: ProcessStateFact::Exited,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| {
            let e = tx.execution(&eid)?.unwrap();
            assert_eq!(e.state, ExecutionState::Paused);
            assert_eq!(
                e.repair_prerequisites,
                vec![RepairPrerequisite::SpecReapproval]
            );
            assert!(
                e.pause_reason
                    .as_deref()
                    .is_some_and(|r| r.contains("source changed")),
                "the recorded repair reason survives the settle"
            );
            assert!(e.paused_since_unix_seconds.is_some());
            assert!(e.desired_action.is_none());
            assert_eq!(
                tx.slot_holders()?.len(),
                0,
                "the slot is released once writer safety is proven"
            );
            Ok(())
        })
        .unwrap();
}

/// Regression (acceptance): a capability the installation never
/// declared can never be satisfied — submit rejects, never degrades
/// the requirement.
#[test]
fn missing_declared_capability_rejects_submit() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut c = config();
    c.declared_capabilities
        .remove(&HostCapability::ReviewFindingsRead);
    let d = handle_command(&store, &c, &submit("k-cap"), NOW).unwrap();
    assert_eq!(reject_reason(&d), RejectionCode::UnsupportedCapability);
}

/// Regression (acceptance): one Work owns a scoped source issue — a
/// second submission overlapping a live claim is refused and names the
/// owning Work.
#[test]
fn claimed_source_issue_rejects_the_overlap() {
    let store = SqliteStore::in_memory(1).unwrap();
    let source = |n: u64| SourceSnapshot {
        issue: issue(n),
        source_revision: "rev-1".into(),
        content_digest: "digest-1".into(),
    };
    let first = handle_command(
        &store,
        &config_with_issue_read(),
        &submit_with_sources("k-own", vec![source(7)]),
        NOW,
    )
    .unwrap();
    let (owner, _) = accepted_with_execution(&first);
    let second = handle_command(
        &store,
        &config_with_issue_read(),
        &submit_with_sources("k-overlap", vec![source(7)]),
        NOW,
    )
    .unwrap();
    assert_eq!(reject_reason(&second), RejectionCode::Ineligible);
    match &second {
        CommandDecision::Rejected {
            current_work_id,
            evidence,
            ..
        } => {
            assert_eq!(current_work_id.as_ref(), Some(&owner));
            assert!(evidence.iter().any(|e| e.contains(&owner.0)));
        }
        _ => unreachable!(),
    }
    // The display number never participates in identity: naming the
    // same repository/issue id with a different or absent display number
    // still hits the existing claim.
    for (key, display) in [
        ("k-overlap-renumbered", Some(99)),
        ("k-overlap-nodisplay", None),
    ] {
        let renamed = SourceSnapshot {
            issue: IssueRef {
                repository: repo(),
                issue: IssueId::trusted("issue-7"),
                display_number: display,
            },
            source_revision: "rev-1".into(),
            content_digest: "digest-1".into(),
        };
        let d = handle_command(
            &store,
            &config_with_issue_read(),
            &submit_with_sources(key, vec![renamed]),
            NOW,
        )
        .unwrap();
        assert_eq!(
            reject_reason(&d),
            RejectionCode::Ineligible,
            "display_number {display:?} must not bypass the claim"
        );
    }
    // A different issue on the same repository is a distinct identity —
    // it submits cleanly.
    accepted(
        &handle_command(
            &store,
            &config_with_issue_read(),
            &submit_with_sources("k-other", vec![source(8)]),
            NOW,
        )
        .unwrap(),
    );
}

/// Regression: the publish chain advances on the receipt alone — an
/// applied `push_ref` receipt records the `open_change_request` intent
/// without the supervisor re-emitting artifact or process evidence.
#[test]
fn an_applied_push_receipt_records_the_next_stage_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("pc1"), NOW).unwrap());
    force_state(&store, &e1, ExecutionState::Waiting);
    let mut invocation = recorded_invocation(&e1, "pc1-coding");
    invocation.state = InvocationState::Done;
    invocation.required_artifacts = vec![RequiredArtifact {
        kind: "artifact.json".into(),
        path: "artifact.json".into(),
        expected_digest: Some("artifact-digest".into()),
    }];
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: invocation.clone(),
        }],
    );
    let emit = |fact: ObservationFact| {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: Some(e1.clone()),
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(ObservationFact::CapabilityReport {
        host: host(),
        capabilities: vec![
            CapabilityEntry {
                capability: HostCapability::PushRef,
                support: CapabilityState::Supported,
                authorization: CapabilityState::Authorized,
            },
            CapabilityEntry {
                capability: HostCapability::ChangeRequestCreate,
                support: CapabilityState::Supported,
                authorization: CapabilityState::Authorized,
            },
        ],
    });
    // Evidence lands: the stage-1 push intent is recorded.
    emit(ObservationFact::Artifact {
        invocation_id: invocation.id.clone(),
        required_artifact: "artifact.json".into(),
        state: ArtifactFact::Present {
            digest: "artifact-digest".into(),
        },
    });
    let push_id = store
        .transact(|tx| {
            let intents = tx.action_intents()?;
            let push = intents
                .iter()
                .find(|i| matches!(i.action, HostAction::PushRef { .. }))
                .expect("stage-1 push intent recorded");
            assert_eq!(push.status, HostActionStatus::Pending);
            assert!(
                !intents
                    .iter()
                    .any(|i| matches!(i.action, HostAction::OpenChangeRequest { .. })),
                "no open intent before the push lands"
            );
            Ok(push.action_id.clone())
        })
        .unwrap();
    // The applied receipt alone advances the chain to stage 2.
    emit(ObservationFact::HostActionResult {
        action_id: push_id,
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::RefPublished {
            repository: repo(),
            branch: format!("work/{}", w.0),
            produced: rev("artifact-digest"),
            observed: rev("artifact-digest"),
            relation: RefRelation::Equal,
        }],
    });
    store
        .transact(|tx| {
            let intents = tx.action_intents()?;
            let open = intents
                .iter()
                .find(|i| matches!(i.action, HostAction::OpenChangeRequest { .. }))
                .expect("the receipt recorded the open intent");
            assert_eq!(open.status, HostActionStatus::Pending);
            Ok(())
        })
        .unwrap();
}

/// Regression: the equivalent-failure streak signs the whole failing set.
/// Two failing required checks must still reach the limit — signing each
/// check separately would alternate the signature and never pause.
#[test]
fn equivalent_failures_pause_on_the_failing_set() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut c = config();
    c.required_checks = vec!["ci".into(), "lint".into()];
    let (w, e1) =
        accepted_with_execution(&handle_command(&store, &c, &submit("ef1"), NOW).unwrap());
    store
        .transact(|tx| {
            let mut work = tx.work(&w)?.unwrap();
            let expected = work.version;
            work.policy.equivalent_failure_limit = 2;
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();
    force_state(&store, &e1, ExecutionState::Waiting);
    let mut invocation = recorded_invocation(&e1, "ef1-coding");
    invocation.state = InvocationState::Done;
    apply_effects(&store, &[Effect::InsertInvocation { invocation }]);
    seed_change(&store, &w, "sha-1", NOW);
    let check = |check_id: &str, producer: &str| ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: check_id.into(),
        producer: HostActorRef {
            host: host(),
            actor: HostActorId::trusted(producer),
        },
        conclusion: CheckConclusion::Failure,
    };
    let emit = |fact: ObservationFact| {
        workloop_core::work::handle_observation(
            &store,
            &c,
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: Some(e1.clone()),
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(check("ci", "ci-agent"));
    emit(check("lint", "ci-agent"));
    assert_eq!(
        read_execution(&store, &e1).state,
        ExecutionState::Waiting,
        "one pass under the limit only counts the streak"
    );
    emit(check("ci", "ci-agent-2"));
    let e = read_execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Stopping,
        "the failing set recurring reaches the limit; the writer stops first"
    );
    assert_eq!(e.desired_action, Some(DesiredAction::Pause));
    // Writer-stop proof settles the pause and releases the slot.
    emit(ObservationFact::ProcessState {
        invocation_id: InvocationId("ef1-coding".into()),
        process_identity: "1@1".into(),
        state: ProcessStateFact::Exited,
    });
    let e = read_execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Paused);
    assert_eq!(
        e.repair_prerequisites,
        vec![RepairPrerequisite::OperatorDecision]
    );
}

/// Regression: `display_number` is presentation metadata — a source read
/// that omits or renumbers it still identifies the same issue and
/// resolves the gate.
#[test]
fn source_read_matches_regardless_of_display_number() {
    let store = SqliteStore::in_memory(1).unwrap();
    let source = SourceSnapshot {
        issue: issue(9),
        source_revision: "rev-1".into(),
        content_digest: "digest-1".into(),
    };
    let d = handle_command(
        &store,
        &config_with_issue_read(),
        &submit_with_sources("k-disp", vec![source]),
        NOW,
    )
    .unwrap();
    let (work_id, eid) = accepted_with_execution(&d);
    let mut renumbered = issue(9);
    renumbered.display_number = None;
    workloop_core::work::handle_observation(
        &store,
        &config_with_issue_read(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::Issue {
                issue: renumbered,
                source_revision: "rev-1".into(),
                content_digest: "digest-1".into(),
                state: IssueState::Open,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    store
        .transact(|tx| dispatch_once(tx, &config_with_issue_read(), NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            assert_eq!(
                tx.execution(&eid)?.unwrap().state,
                ExecutionState::Ready,
                "an absent display_number still identifies the declared issue"
            );
            Ok(())
        })
        .unwrap();
}

/// Regression: a verified open is a receipt whose action_id names a
/// recorded intent reconciled to `applied`. A receipt carrying
/// `ChangeOpened` under an unrecorded or failed action is an attempt,
/// never a diff target.
#[test]
fn unverified_open_receipt_cannot_create_a_diff_target() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, _e) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("uv1"), NOW).unwrap());
    // A receipt naming no recorded intent — fabricated evidence.
    workloop_core::work::handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: "act-fabricated".into(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::ChangeOpened {
                    change: change(),
                    head: rev("sha-fab"),
                    source_branch: format!("work/{}", w.0),
                    target_branch: "main".into(),
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    // A receipt naming an intent that reconciled to failed.
    store
        .transact(|tx| {
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-failed".into(),
                    work_id: w.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", w.0),
                        target_branch: "main".into(),
                        title: "seed".into(),
                        body: String::new(),
                    },
                    payload_digest: "test".into(),
                    status: HostActionStatus::Failed {
                        reason: "declined".into(),
                    },
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })
        })
        .unwrap();
    workloop_core::work::handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: "act-failed".into(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::ChangeOpened {
                    change: change(),
                    head: rev("sha-fab2"),
                    source_branch: format!("work/{}", w.0),
                    target_branch: "main".into(),
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let value = store
        .transact(|tx| workloop_core::projection::work_detail(tx, &w, NOW))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = value else {
        panic!("expected found")
    };
    assert!(
        value.current_change.is_none(),
        "unverified receipts never establish the diff target"
    );
    store
        .transact(|tx| {
            let failed = tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == "act-failed")
                .unwrap();
            assert!(
                matches!(failed.status, HostActionStatus::Failed { .. }),
                "a late applied claim cannot resurrect a failed intent"
            );
            Ok(())
        })
        .unwrap();
}

/// Regression: a failed push re-issues while retries remain, and the
/// applied retry — not the oldest intent — advances the chain to
/// `open_change_request` without recording a third push.
#[test]
fn an_applied_push_retry_advances_the_chain() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("pr1"), NOW).unwrap());
    store
        .transact(|tx| {
            let mut work = tx.work(&w)?.unwrap();
            let expected = work.version;
            work.policy.transport_max_retries = 3;
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();
    force_state(&store, &e1, ExecutionState::Waiting);
    let mut invocation = recorded_invocation(&e1, "pr1-coding");
    invocation.state = InvocationState::Done;
    invocation.required_artifacts = vec![RequiredArtifact {
        kind: "artifact.json".into(),
        path: "artifact.json".into(),
        expected_digest: Some("artifact-digest".into()),
    }];
    apply_effects(
        &store,
        &[Effect::InsertInvocation {
            invocation: invocation.clone(),
        }],
    );
    let emit = |fact: ObservationFact| {
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: Some(e1.clone()),
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(ObservationFact::CapabilityReport {
        host: host(),
        capabilities: vec![
            CapabilityEntry {
                capability: HostCapability::PushRef,
                support: CapabilityState::Supported,
                authorization: CapabilityState::Authorized,
            },
            CapabilityEntry {
                capability: HostCapability::ChangeRequestCreate,
                support: CapabilityState::Supported,
                authorization: CapabilityState::Authorized,
            },
        ],
    });
    emit(ObservationFact::Artifact {
        invocation_id: invocation.id.clone(),
        required_artifact: "artifact.json".into(),
        state: ArtifactFact::Present {
            digest: "artifact-digest".into(),
        },
    });
    let pushes = |store: &SqliteStore| {
        store
            .transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .filter(|i| matches!(i.action, HostAction::PushRef { .. }))
                    .collect::<Vec<_>>())
            })
            .unwrap()
    };
    assert_eq!(pushes(&store).len(), 1);
    // First push fails definitively: the receipt re-issues a retry.
    emit(ObservationFact::HostActionResult {
        action_id: pushes(&store)[0].action_id.clone(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Failed {
            reason: "declined".into(),
        },
        effects: vec![],
    });
    let retry = pushes(&store);
    assert_eq!(retry.len(), 2, "the failed push re-issues under the bound");
    // The retry's applied receipt advances the chain — the old failed
    // intent must not pin stage 1 open forever.
    emit(ObservationFact::HostActionResult {
        action_id: retry[1].action_id.clone(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::RefPublished {
            repository: repo(),
            branch: format!("work/{}", w.0),
            produced: rev("artifact-digest"),
            observed: rev("artifact-digest"),
            relation: RefRelation::Equal,
        }],
    });
    store
        .transact(|tx| {
            let intents = tx.action_intents()?;
            assert_eq!(
                intents
                    .iter()
                    .filter(|i| matches!(i.action, HostAction::PushRef { .. }))
                    .count(),
                2,
                "no third push once the retry landed"
            );
            let open = intents
                .iter()
                .find(|i| matches!(i.action, HostAction::OpenChangeRequest { .. }))
                .expect("the applied retry records the open intent");
            assert_eq!(open.status, HostActionStatus::Pending);
            Ok(())
        })
        .unwrap();
}

/// Regression: a `ChangeOpened` effect riding along on a NON-open
/// receipt (e.g. a verified push) cannot establish the diff target —
/// adoption requires an applied `open_change_request` intent whose
/// immutable payload the effect matches.
#[test]
fn a_smuggled_open_effect_in_a_push_receipt_creates_no_diff_target() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, _e) =
        accepted_with_execution(&handle_command(&store, &config(), &submit("sm1"), NOW).unwrap());
    apply_effects(
        &store,
        &[Effect::InsertHostActionIntent {
            intent: HostActionIntent {
                action_id: "act-smuggle".into(),
                work_id: w.clone(),
                host: host(),
                repository: repo(),
                role: ActorRole::Publication,
                actor: publisher(),
                action: HostAction::PushRef {
                    branch: format!("work/{}", w.0),
                    produced: rev("artifact-digest"),
                },
                payload_digest: "test".into(),
                status: HostActionStatus::Pending,
                authenticated_actor: None,
                effects: vec![],
                created_seq: 0,
            },
        }],
    );
    // The receipt legitimately verifies the push — and smuggles a
    // fully-formed ChangeOpened alongside it.
    workloop_core::work::handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: "act-smuggle".into(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Applied,
                effects: vec![
                    NormalizedEffectFact::RefPublished {
                        repository: repo(),
                        branch: format!("work/{}", w.0),
                        produced: rev("artifact-digest"),
                        observed: rev("artifact-digest"),
                        relation: RefRelation::Equal,
                    },
                    NormalizedEffectFact::ChangeOpened {
                        change: change(),
                        head: rev("h-smuggled"),
                        source_branch: format!("work/{}", w.0),
                        target_branch: "main".into(),
                    },
                ],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let value = store
        .transact(|tx| workloop_core::projection::work_detail(tx, &w, NOW))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = value else {
        panic!("expected found")
    };
    assert!(
        value.current_change.is_none(),
        "a non-open receipt's ChangeOpened must not establish the diff target"
    );
    // The push itself still reconciled applied — only the smuggled
    // effect is inadmissible.
    store
        .transact(|tx| {
            let push = tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == "act-smuggle")
                .unwrap();
            assert_eq!(push.status, HostActionStatus::Applied);
            Ok(())
        })
        .unwrap();
}

/// Regression: a store file stamped with the CURRENT format version but
/// holding a foreign schema is refused before any mutating pragma — the
/// version stamp alone is not proof of the recognized schema.
#[test]
fn a_versioned_file_without_the_recognized_schema_is_refused() {
    let root = temp_root("cutover-foreign-v1");
    let db = root.join("store.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE foreign_rows (id TEXT);")
            .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { detail }) => {
            assert!(detail.contains("schema"), "detail: {detail}");
        }
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got an open store"),
    }
    // The refusal must not mutate the foreign file: journal mode stays
    // the file's own and no WAL sidecars appear.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "a refused v1 file must not be re-journaled");
    }
    assert!(
        !db.with_extension("db-wal").exists() && !db.with_extension("db-shm").exists(),
        "a refused open must not leave WAL sidecars"
    );
}

/// Regression: a store created by the previous release — stamped v1,
/// full base schema, none of this slice's additive tables — opens and
/// is upgraded in place. Refusing it as "not the recognized schema"
/// would lock every existing installation out of its own store.
#[test]
fn a_previous_release_v1_store_is_upgraded_in_place() {
    let root = temp_root("cutover-previous-v1");
    let db = root.join("store.db");
    let (work_id, execution_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let request = submit("k-prev");
        let (work_id, execution_id) =
            accepted_with_execution(&handle_command(&store, &config(), &request, NOW).unwrap());
        (work_id, execution_id)
    };
    // Roll the file back to the previous release's table set and its
    // v1 stamp: every base table intact, the additive tables this
    // slice added dropped.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE run_lease; DROP TABLE recovery_intents; DROP TABLE body_catalog; \
             PRAGMA user_version=1;",
        )
        .unwrap();
    }
    // Open upgrades in place instead of refusing the file.
    let store = SqliteStore::open(&db, 1).expect("a previous-release v1 store opens");
    // Base rows survived the upgrade.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("base rows survive the upgrade");
    assert_eq!(work.id, work_id);
    let execution = store
        .transact(|tx| tx.execution(&execution_id))
        .unwrap()
        .expect("execution row survives the upgrade");
    assert_eq!(execution.state, ExecutionState::Queued);
    // The additive tables are installed and usable again (`open` itself
    // re-verifies the full expected table set before returning).
    assert!(store.body_state("no-such-body").unwrap().is_none());
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2, "the upgrade lands on the current format");
    }
    // Re-opening after the upgrade is a no-op, not a second migration.
    drop(store);
    let again = SqliteStore::open(&db, 1).expect("reopen after upgrade");
    assert!(again.body_state("no-such-body").unwrap().is_none());
}
