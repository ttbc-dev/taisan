//! Acceptance matrix for the execution loop's automatic recovery
//! (U5.7): every case LP-19..LP-27 from `docs/execution-loop.md`
//! "Planned verification suite" runs here against a real SqliteStore,
//! asserts the exact resulting state and evidence records, and
//! exercises the automatic recovery mechanism — recovery-intent firing
//! (loop phase 6), the observation-driven transition path, and
//! Attention derivation. No case substitutes a manual `retry` Command
//! for a required automatic branch. Crash scenarios are exercised
//! from their durable post-crash state (an intent persisted before the
//! crash boundary, a stopping Execution, a pending host action),
//! seeded through the store — the same prior-history pattern prior
//! slices established; no case kills a process mid-phase. The dispatch
//! deadline gate itself is covered by U5.3's
//! `run_loop::tests::past_the_deadline_the_dispatch_pass_emits_no_new_execution`.
//!
//! Claims boundary: fixture/contract results only. No live host,
//! Worker, or E2E scenario was run; live claims require separately
//! authorized live evidence.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{QueryResult, attention};
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, WorkloopConfig, consume_due_recovery_intents, handle_command, handle_observation,
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
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
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

fn accepted_with_execution(d: CommandDecision) -> (WorkId, ExecutionId) {
    match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => execution_id.clone(),
                other => panic!("expected an execution effect, got {other:?}"),
            };
            (work_id.expect("work_id"), eid)
        }
        other => panic!("expected accepted with execution, got {other:?}"),
    }
}

fn submit_work(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    accepted_with_execution(handle_command(store, &config(), &submit(key), NOW).unwrap())
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution exists")
}

fn set_state(
    store: &SqliteStore,
    execution_id: &ExecutionId,
    from: ExecutionState,
    to: ExecutionState,
    pause_prereqs: Option<Vec<RepairPrerequisite>>,
) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = to.clone();
            if let Some(prereqs) = pause_prereqs {
                e.pause_reason = Some("matrix fixture pause".into());
                e.repair_prerequisites = prereqs;
                e.paused_since_unix_seconds = Some(NOW);
            }
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: from,
            })
        })
        .unwrap();
}

fn seed_usage(store: &SqliteStore, work_id: &WorkId, update: &dyn Fn(&mut WorkUsage)) {
    store
        .transact(|tx| {
            let mut usage = tx
                .work_usage(work_id)?
                .unwrap_or_else(|| WorkUsage::new(work_id));
            update(&mut usage);
            tx.apply(&Effect::UpdateWorkUsage { usage })
        })
        .unwrap();
}

fn seed_intent(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: Option<&ExecutionId>,
    kind: &str,
    due_at: i64,
) -> String {
    store
        .transact(|tx| {
            let intent_id = tx.alloc_id("ri")?;
            let scheduled_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertRecoveryIntent {
                intent: RecoveryIntent {
                    intent_id: intent_id.clone(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.cloned(),
                    kind: kind.into(),
                    due_at_unix_seconds: due_at,
                    generation: 1,
                    evidence_basis: "fault matrix fixture".into(),
                    state: RecoveryIntentState::Pending,
                    retirement_reason: None,
                    scheduled_seq,
                },
            })?;
            Ok(intent_id)
        })
        .unwrap()
}

fn intent(store: &SqliteStore, intent_id: &str) -> RecoveryIntent {
    store
        .transact(|tx| tx.recovery_intents())
        .unwrap()
        .into_iter()
        .find(|i| i.intent_id == intent_id)
        .expect("intent row")
}

fn consume(store: &SqliteStore, now: i64) -> workloop_core::work::RecoveryScan {
    let mut clock = || now;
    consume_due_recovery_intents(store, &mut clock, None, None, 16).unwrap()
}

fn observe(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: Option<&ExecutionId>,
    fact: ObservationFact,
) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.cloned(),
            fact,
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

// ---- LP-20 / F12: repeated equivalent failure ----

fn lp_20_equivalent_failure_pauses_at_the_limit() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut cfg = config();
    cfg.required_checks = vec!["ci".into(), "lint".into()];
    let (w, e1) =
        accepted_with_execution(handle_command(&store, &cfg, &submit("lp20"), NOW).unwrap());
    store
        .transact(|tx| {
            let mut work = tx.work(&w)?.unwrap();
            let expected = work.version;
            work.policy.equivalent_failure_limit = 2;
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Waiting,
        None,
    );
    seed_usage(&store, &w, &|u| {
        u.first_dispatch_unix_seconds = Some(NOW - 100);
    });
    seed_change(&store, &w, "sha-1", NOW);

    let fail_check = |check_id: &str, producer: &str| ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: check_id.into(),
        producer: HostActorRef {
            host: host(),
            actor: HostActorId::trusted(producer),
        },
        conclusion: CheckConclusion::Failure,
    };
    observe(&store, &w, Some(&e1), fail_check("ci", "ci-agent"));
    // A non-equivalent failure resets the streak: the failing set is
    // now ci+lint, a different classifier key than before.
    observe(&store, &w, Some(&e1), fail_check("lint", "lint-agent"));
    let reset = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(reset.equivalent_failure_streak, 1);
    assert_eq!(
        reset.equivalent_signature.as_deref(),
        Some("checks:ci+lint:failure"),
        "a non-equivalent failure resets the streak and the signature"
    );
    // Identical recurring failure (same failing set) reaches the limit.
    observe(&store, &w, Some(&e1), fail_check("ci", "ci-agent-2"));
    let at_limit = execution(&store, &e1);
    assert_eq!(
        at_limit.state,
        ExecutionState::Stopping,
        "the failing set recurring reaches the limit; the writer stops first"
    );
    assert_eq!(at_limit.desired_action, Some(DesiredAction::Pause));

    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::ProcessState {
            invocation_id: InvocationId("lp20-coding".into()),
            process_identity: "1@1".into(),
            state: ProcessStateFact::Exited,
            exit_code: Some(0),
        },
    );
    let paused = execution(&store, &e1);
    assert_eq!(paused.state, ExecutionState::Paused);
    assert_eq!(
        paused.repair_prerequisites,
        vec![RepairPrerequisite::OperatorDecision],
        "at the limit the automatic path hands off to a human"
    );

    let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(usage.equivalent_failure_streak, 2);
    assert_eq!(
        usage.equivalent_signature.as_deref(),
        Some("checks:ci+lint:failure")
    );
    let decisions = store.transact(|tx| tx.recovery_decisions(&w)).unwrap();
    assert!(
        decisions
            .iter()
            .any(|d| d.classification == "equivalent_failure"),
        "the equivalent-failure handoff records a recovery decision: {decisions:?}"
    );
}

// ---- LP-21 / F13: terminal evidence retires a pending intent ----

fn lp_21_terminal_evidence_retires_the_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp21");
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Paused,
        Some(vec![]),
    );
    let intent_id = seed_intent(&store, &w, Some(&e1), "requeue", NOW);

    // Terminal remote/local evidence wins before the intent fires:
    // the Execution is settled terminal by an earlier path.
    set_state(
        &store,
        &e1,
        ExecutionState::Paused,
        ExecutionState::Failed,
        None,
    );
    let scan = consume(&store, NOW);
    assert_eq!(scan.retired, 1);
    assert_eq!(scan.requeued, 0);
    let recorded = intent(&store, &intent_id);
    assert_eq!(recorded.state, RecoveryIntentState::Retired);
    assert_eq!(
        recorded.retirement_reason.as_deref(),
        Some("execution_terminal")
    );

    // Restart cannot resurrect the retired intent.
    let again = consume(&store, NOW + 1);
    assert_eq!(again.consumed, 0);
    assert_eq!(again.retired, 0);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Retired
    );
}

// ---- LP-22 / F14: prolonged uncertainty ----

fn lp_22_prolonged_uncertainty_surfaces_attention_without_settling() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp22");
    store
        .transact(|tx| {
            let mut e = tx.execution(&e1)?.unwrap();
            let threshold = e.budget.uncertain_threshold_seconds as i64;
            e.state = ExecutionState::Uncertain;
            e.desired_action = Some(DesiredAction::Continue);
            e.uncertain_since_unix_seconds = Some(NOW - threshold - 1);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();

    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    let item = items
        .iter()
        .find(|a| a.reason == "uncertain_beyond_threshold")
        .expect("uncertainty beyond the threshold surfaces as Attention");
    assert_eq!(
        item.investigation_threshold_seconds,
        Some(e_budget_threshold(&store, &e1))
    );

    // No automatic settlement, no automatic release, ownership held.
    let e = execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Uncertain);
    assert_eq!(e.desired_action, Some(DesiredAction::Continue));
}

fn e_budget_threshold(store: &SqliteStore, id: &ExecutionId) -> u64 {
    execution(store, id).budget.uncertain_threshold_seconds
}

// ---- LP-23 / F15: work-wide automatic-time ceiling ----

fn lp_23_automatic_time_exhaustion_hands_off_not_requeues() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp23");
    let limit = WorkPolicy::default().automatic_time_limit_seconds as i64;
    seed_usage(&store, &w, &|u| {
        u.first_dispatch_unix_seconds = Some(NOW - limit - 60);
        u.human_wait_seconds_total = 0;
    });
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Paused,
        Some(vec![]),
    );
    let _intent_id = seed_intent(&store, &w, Some(&e1), "requeue", NOW);

    let scan = consume(&store, NOW);
    assert_eq!(scan.requeued, 0, "no requeue past the ceiling");
    assert_eq!(scan.failed_terminal, 1);
    assert_eq!(
        scan.consumed, 1,
        "the intent is consumed terminal, not re-armed"
    );
    let e = execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(
        e.outcome.as_ref().and_then(|o| o.code.as_deref()),
        Some("automatic_time_exhausted")
    );
}

// ---- LP-19 / F11 + LP-24: stalled execution and no-progress ----

fn lp_19_fake_liveness_does_not_reset_the_progress_clock() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp19");
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Ready,
        None,
    );
    seed_usage(&store, &w, &|u| {
        u.first_dispatch_unix_seconds = Some(NOW - 3700);
        u.last_progress_unix_seconds = Some(NOW - 3600);
    });
    seed_change(&store, &w, "sha-1", NOW);

    // The warning derivs from durable records only; nothing stops.
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    let item = items
        .iter()
        .find(|a| a.reason == "no_progress")
        .expect("a Work with no material progress past the threshold surfaces Attention");
    assert_eq!(item.investigation_threshold_seconds, Some(1800));
    let e = execution(&store, &e1);
    assert_eq!(
        e.state,
        ExecutionState::Ready,
        "no-progress never authorizes a stop"
    );
    assert_eq!(e.desired_action, None);

    // Fake liveness: a new commit and a failing check are not progress.
    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::ChangeHead {
            change: change(),
            head: rev("sha-2"),
        },
    );
    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::Check {
            change: change(),
            head: rev("sha-2"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Failure,
        },
    );
    let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(
        usage.last_progress_unix_seconds,
        Some(NOW - 3600),
        "fake liveness never rearms the progress clock"
    );
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(
        items.iter().any(|a| a.reason == "no_progress"),
        "a failing check on a current head is still not progress; the Attention persists"
    );

    // Material progress (a passing required check) rearms and clears.
    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::Check {
            change: change(),
            head: rev("sha-2"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(usage.last_progress_unix_seconds, Some(NOW));
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(
        !items.iter().any(|a| a.reason == "no_progress"),
        "material progress clears the no-progress Attention automatically"
    );
    assert_eq!(execution(&store, &e1).state, ExecutionState::Ready);
}

// ---- LP-24: no-progress threshold boundaries and auto-clear ----

fn lp_24_no_progress_threshold_and_auto_clear() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp24");
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Ready,
        None,
    );
    seed_usage(&store, &w, &|u| {
        u.first_dispatch_unix_seconds = Some(NOW - 100);
        u.last_progress_unix_seconds = Some(NOW - 1799);
    });
    let below = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(!below.iter().any(|a| a.reason == "no_progress"));

    seed_usage(&store, &w, &|u| {
        u.last_progress_unix_seconds = Some(NOW - 1800);
    });
    let at = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(at.iter().any(|a| a.reason == "no_progress"));

    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::Artifact {
            invocation_id: InvocationId("lp24-coding".into()),
            required_artifact: "patch".into(),
            state: ArtifactFact::Present {
                digest: "digest-1".into(),
            },
        },
    );
    let after = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(!after.iter().any(|a| a.reason == "no_progress"));
}

// ---- LP-25: recovery_intents lifecycle ----

fn lp_25_persisted_schedule_fires_exactly_once() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp25a");
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Paused,
        Some(vec![]),
    );
    let intent_id = seed_intent(&store, &w, Some(&e1), "requeue", NOW);

    // Crash after scheduling: the pending intent survives; the next
    // automatic pass consumes it exactly once.
    let scan = consume(&store, NOW);
    assert_eq!(scan.consumed, 1);
    assert_eq!(scan.requeued, 1);
    assert_eq!(execution(&store, &e1).state, ExecutionState::Queued);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
    let again = consume(&store, NOW + 1);
    assert_eq!(again.consumed, 0);
    assert_eq!(execution(&store, &e1).state, ExecutionState::Queued);

    // Concurrent manual intervention wins the race: a Resume Command
    // before the pass leaves the intent holding, never double-fired.
    let store2 = SqliteStore::in_memory(1).unwrap();
    let (w2, e2) = submit_work(&store2, "lp25b");
    set_state(
        &store2,
        &e2,
        ExecutionState::Queued,
        ExecutionState::Paused,
        Some(vec![]),
    );
    let intent_id2 = seed_intent(&store2, &w2, Some(&e2), "requeue", NOW);
    let resume = handle_command(
        &store2,
        &config(),
        &req(
            "lp25-manual-resume",
            CommandBody::Resume {
                work_id: w2.clone(),
                expected_work_version: 1,
                execution_id: e2.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(matches!(resume, CommandDecision::Accepted { .. }));
    let scan2 = consume(&store2, NOW);
    assert_eq!(scan2.consumed, 0);
    assert_eq!(scan2.requeued, 0);
    assert_eq!(
        scan2.held,
        vec![(intent_id2.clone(), "execution_active".into())]
    );
    assert_eq!(
        intent(&store2, &intent_id2).state,
        RecoveryIntentState::Pending
    );

    // A pending intent whose Execution goes terminal is retired, not
    // fired: retirement is durable and idempotent across restarts.
    set_state(
        &store2,
        &e2,
        ExecutionState::Queued,
        ExecutionState::Failed,
        None,
    );
    let scan3 = consume(&store2, NOW + 2);
    assert_eq!(scan3.consumed, 0);
    assert_eq!(scan3.retired, 1);
    let retired = intent(&store2, &intent_id2);
    assert_eq!(retired.state, RecoveryIntentState::Retired);
    assert_eq!(
        retired.retirement_reason.as_deref(),
        Some("execution_terminal")
    );
    let scan4 = consume(&store2, NOW + 3);
    assert_eq!(scan4.retired, 0, "a retired intent never fires again");
}

// ---- LP-26: equivalent-failure suppression ----

#[test]
fn lp_26_a_suppressed_intent_is_recorded_not_consumed() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp26");
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Paused,
        Some(vec![]),
    );
    seed_usage(&store, &w, &|u| {
        u.equivalent_failure_streak = 2;
        u.equivalent_signature = Some("checks:ci:failure".into());
    });
    let intent_id = seed_intent(&store, &w, Some(&e1), "requeue", NOW);

    let scan = consume(&store, NOW);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.requeued, 0);
    assert_eq!(
        scan.held,
        vec![(intent_id.clone(), "equivalent_failure_suppressed".into())],
        "the suppressed automatic requeue is recorded, never consumed"
    );
    let recorded = intent(&store, &intent_id);
    assert_eq!(recorded.state, RecoveryIntentState::Pending);
    assert_eq!(execution(&store, &e1).state, ExecutionState::Paused);
    let deferrals = store.transact(|tx| tx.deferrals()).unwrap();
    let deferral = deferrals
        .iter()
        .find(|d| d.execution_id == e1)
        .expect("the suppression is durably recorded against the Execution");
    assert_eq!(deferral.reason, "equivalent_failure_suppressed");
    assert!(
        deferral.diagnostic.contains("policy limit"),
        "{}",
        deferral.diagnostic
    );

    // A later outcome clears the stale suppression explanation: the
    // failure budget resets and the Execution holds for a human
    // decision instead — the capacity Query must not keep showing the
    // old stall reason.
    set_state(
        &store,
        &e1,
        ExecutionState::Paused,
        ExecutionState::Paused,
        Some(vec![RepairPrerequisite::OperatorDecision]),
    );
    seed_usage(&store, &w, &|u| {
        u.equivalent_failure_streak = 0;
        u.equivalent_signature = None;
    });
    let scan = consume(&store, NOW + 1);
    assert_eq!(scan.held, vec![(intent_id.clone(), "operator_hold".into())]);
    let deferrals = store.transact(|tx| tx.deferrals()).unwrap();
    assert!(
        deferrals
            .iter()
            .all(|d| !(d.execution_id == e1 && d.reason == "equivalent_failure_suppressed")),
        "the stale suppression explanation is cleared once the outcome changes: {deferrals:?}"
    );
}

// ---- LP-27: timeout recovery sequence ordering ----

fn lp_27_timeout_recovery_sequence_orders_stop_reconcile_requeue() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "lp27");

    // Crash at the stop boundary: an unverified stop holds everything —
    // no requeue, ownership retained.
    store
        .transact(|tx| {
            let mut e = tx.execution(&e1)?.unwrap();
            e.state = ExecutionState::Stopping;
            e.desired_action = Some(DesiredAction::Pause);
            e.pause_reason = Some("matrix fixture pause".into());
            e.repair_prerequisites = vec![RepairPrerequisite::EffectResolution {
                action_id: "act-lp27-open".into(),
            }];
            e.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-lp27-open".into(),
                    work_id: w.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: "work/lp27".into(),
                        target_branch: "main".into(),
                        title: "fixture".into(),
                        body: String::new(),
                    },
                    payload_digest: "fixture".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq: 1,
                },
            })
        })
        .unwrap();
    let intent_id = seed_intent(&store, &w, Some(&e1), "requeue", NOW);
    let scan = consume(&store, NOW);
    assert_eq!(scan.requeued, 0);
    assert_eq!(
        execution(&store, &e1).state,
        ExecutionState::Stopping,
        "no requeue before a proven stop; ownership and reservations stand"
    );

    // Proven stop: the witnessed exit settles the pause through the
    // observation path — no manual Command.
    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::ProcessState {
            invocation_id: InvocationId("lp27-coding".into()),
            process_identity: "1@1".into(),
            state: ProcessStateFact::Exited,
            exit_code: Some(0),
        },
    );
    assert_eq!(execution(&store, &e1).state, ExecutionState::Paused);

    // Reconciliation before requeue: the still-unknown effect holds the
    // intent with a visible reason.
    let scan = consume(&store, NOW);
    assert_eq!(scan.requeued, 0);
    assert_eq!(
        scan.held,
        vec![(intent_id.clone(), "prerequisites_unmet".into())],
        "the unresolved effect holds the automatic requeue"
    );

    // The effect acquires a verified result through the normal
    // observation path; the next pass fires exactly once.
    observe(
        &store,
        &w,
        Some(&e1),
        ObservationFact::HostActionResult {
            action_id: "act-lp27-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev("sha-lp27"),
                source_branch: "work/lp27".into(),
                target_branch: "main".into(),
            }],
        },
    );
    let scan = consume(&store, NOW);
    assert_eq!(scan.requeued, 1);
    assert_eq!(execution(&store, &e1).state, ExecutionState::Queued);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
}

// ---- Attention derivation: queries over durable records only ----

#[test]
fn attention_derivation_reflects_durable_record_mutations() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit_work(&store, "att-1");

    // Mutating the durable record (state + outcome) changes the derived
    // Attention with no Command-side cache in between: every read is a
    // fresh derivation from the store.
    set_state(
        &store,
        &e1,
        ExecutionState::Queued,
        ExecutionState::Failed,
        None,
    );
    let derived =
        |store: &SqliteStore| match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
            QueryResult::Found { value, .. } => value,
            other => panic!("expected found, got {other:?}"),
        };
    let items = derived(&store);
    assert!(
        items.iter().any(|a| a.reason == "confirmed_failure"),
        "a Failed Execution derives confirmed_failure from durable records"
    );

    // Acknowledge exactly the derivable version, then re-derive: the
    // acknowledged version clears; the underlying record is untouched.
    let version = items
        .iter()
        .find(|a| a.reason == "confirmed_failure")
        .unwrap()
        .attention_id
        .clone();
    let ack = handle_command(
        &store,
        &config(),
        &req(
            "att-ack",
            CommandBody::Acknowledge {
                work_id: w.clone(),
                expected_work_version: 1,
                attention_id: version,
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(matches!(ack, CommandDecision::Accepted { .. }));
    assert!(
        !derived(&store)
            .iter()
            .any(|a| a.reason == "confirmed_failure"),
        "the acknowledged version clears from the derived set"
    );

    // Mutating a different durable record re-derives a different
    // Attention: a stale progress clock surfaces no_progress while the
    // acknowledged confirmed_failure stays hidden.
    seed_usage(&store, &w, &|u| {
        u.first_dispatch_unix_seconds = Some(NOW - 3700);
        u.last_progress_unix_seconds = Some(NOW - 3600);
    });
    store
        .transact(|tx| {
            let mut e = tx.execution(&e1)?.unwrap();
            e.state = ExecutionState::Ready;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Failed,
            })
        })
        .unwrap();
    let items = derived(&store);
    assert!(
        items.iter().any(|a| a.reason == "no_progress"),
        "the mutated usage record derives no_progress"
    );
    assert!(
        !items.iter().any(|a| a.reason == "confirmed_failure"),
        "records never seen reappear with stale reasons"
    );
}

// ---- The parameterized matrix: every LP case, no uncovered row ----

#[test]
fn fault_matrix_lp_19_through_27() {
    // (case, runner) — each runner asserts the exact resulting state
    // and evidence records for its LP entry from
    // docs/execution-loop.md "Planned verification suite".
    let cases: &[(&str, fn())] = &[
        (
            "LP-19",
            lp_19_fake_liveness_does_not_reset_the_progress_clock,
        ),
        ("LP-20", lp_20_equivalent_failure_pauses_at_the_limit),
        ("LP-21", lp_21_terminal_evidence_retires_the_intent),
        (
            "LP-22",
            lp_22_prolonged_uncertainty_surfaces_attention_without_settling,
        ),
        (
            "LP-23",
            lp_23_automatic_time_exhaustion_hands_off_not_requeues,
        ),
        ("LP-24", lp_24_no_progress_threshold_and_auto_clear),
        ("LP-25", lp_25_persisted_schedule_fires_exactly_once),
        ("LP-26", lp_26_a_suppressed_intent_is_recorded_not_consumed),
        (
            "LP-27",
            lp_27_timeout_recovery_sequence_orders_stop_reconcile_requeue,
        ),
    ];
    let expected = [
        "LP-19", "LP-20", "LP-21", "LP-22", "LP-23", "LP-24", "LP-25", "LP-26", "LP-27",
    ];
    let covered: Vec<&str> = cases.iter().map(|(id, _)| *id).collect();
    assert_eq!(covered, expected, "the matrix has no uncovered case");
    for (id, run) in cases {
        eprintln!("fault matrix: {id}");
        run();
    }
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .unwrap();
}

/// Seed the published change + head the equivalence assessment needs:
/// an applied open intent and its receipt plus a head observation.
fn seed_change(store: &SqliteStore, work_id: &WorkId, head: &str, observed_at: i64) {
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
