//! Recovery-scheduling contract: the loop's phase-6 consumer fires due
//! `recovery_intents` exactly once, retires them on terminal evidence,
//! and never releases an explicit hold or guesses an unknown kind.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, RecoveryScan, WorkloopConfig, consume_due_recovery_intents, handle_command,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn config() -> WorkloopConfig {
    use std::collections::BTreeSet;
    use workloop_core::identity::{ActorRole, AdapterId, HostCapability, RoleBinding};
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred"),
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
        required_checks: vec![],
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

fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources: vec![],
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

fn pause(store: &SqliteStore, execution_id: &ExecutionId, prereqs: Vec<RepairPrerequisite>) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("test pause".into());
            e.repair_prerequisites = prereqs;
            e.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
}

fn set_state(
    store: &SqliteStore,
    execution_id: &ExecutionId,
    from: ExecutionState,
    to: ExecutionState,
) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = to;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: from,
            })
        })
        .unwrap();
}

fn schedule(
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
                    evidence_basis: "test".into(),
                    state: RecoveryIntentState::Pending,
                    retirement_reason: None,
                    scheduled_seq,
                },
            })?;
            Ok(intent_id)
        })
        .unwrap()
}

fn consume(
    store: &SqliteStore,
    now: i64,
    deadline: Option<i64>,
    scope: Option<&WorkId>,
    limit: usize,
) -> RecoveryScan {
    let mut clock = || now;
    consume_due_recovery_intents(store, &mut clock, deadline, scope, limit).unwrap()
}

fn intent(store: &SqliteStore, intent_id: &str) -> RecoveryIntent {
    store
        .transact(|tx| tx.recovery_intents())
        .unwrap()
        .into_iter()
        .find(|i| i.intent_id == intent_id)
        .expect("intent row")
}

fn execution(store: &SqliteStore, execution_id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(execution_id))
        .unwrap()
        .unwrap()
}

#[test]
fn due_requeue_requeues_a_paused_execution_and_consumes_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-requeue");
    pause(&store, &execution_id, vec![]);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(scan.requeued, 1);
    assert_eq!(scan.retired, 0);
    assert!(scan.held.is_empty());

    let e = execution(&store, &execution_id);
    assert_eq!(e.state, ExecutionState::Queued);
    assert!(e.pause_reason.is_none());
    assert!(e.repair_prerequisites.is_empty());
    assert!(e.paused_since_unix_seconds.is_none());

    let work = store.transact(|tx| tx.work(&work_id)).unwrap().unwrap();
    assert_eq!(work.version, 2, "the requeue bumps the Work version");

    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );

    // Firing is exactly once: a second pass consumes nothing.
    let again = consume(&store, NOW + 1, None, None, 16);
    assert_eq!(again.consumed, 0);
    assert_eq!(again.requeued, 0);
}

#[test]
fn a_future_intent_stays_pending() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-future");
    pause(&store, &execution_id, vec![]);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW + 100);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Paused
    );
}

#[test]
fn terminal_execution_evidence_retires_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-terminal");
    set_state(
        &store,
        &execution_id,
        ExecutionState::Queued,
        ExecutionState::Failed,
    );
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.retired, 1);
    assert_eq!(scan.consumed, 0);
    let row = intent(&store, &intent_id);
    assert_eq!(row.state, RecoveryIntentState::Retired);
    assert_eq!(
        row.retirement_reason.as_deref(),
        Some("execution_terminal"),
        "retirement records why the intent can never fire"
    );
}

#[test]
fn a_missing_or_unnamed_execution_retires_rather_than_guessing() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, _) = submit(&store, "k-missing");
    let missing = schedule(
        &store,
        &work_id,
        Some(&ExecutionId("e-absent".into())),
        "requeue",
        NOW,
    );
    let unnamed = schedule(&store, &work_id, None, "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.retired, 2);
    assert_eq!(
        intent(&store, &missing).retirement_reason.as_deref(),
        Some("execution_missing")
    );
    assert_eq!(
        intent(&store, &unnamed).retirement_reason.as_deref(),
        Some("no_execution")
    );
}

#[test]
fn an_unknown_kind_is_held_not_fired() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-kind");
    pause(&store, &execution_id, vec![]);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "observe", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "unsupported_kind")
    );
}

#[test]
fn an_operator_hold_is_never_released_automatically() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-hold");
    pause(
        &store,
        &execution_id,
        vec![RepairPrerequisite::OperatorDecision],
    );
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "operator_hold")
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Paused,
        "an explicit human hold survives an automatic pass"
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
}

#[test]
fn unmet_repair_prerequisites_hold_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-prereq");
    pause(
        &store,
        &execution_id,
        vec![RepairPrerequisite::SpecReapproval],
    );
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "prerequisites_unmet")
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
}

#[test]
fn an_already_progressing_execution_holds_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-active");
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "execution_active")
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Queued
    );
}

#[test]
fn the_tick_bound_leaves_the_remainder_pending() {
    let store = SqliteStore::in_memory(16).unwrap();
    let mut scheduled = Vec::new();
    for i in 0..18 {
        let (work_id, execution_id) = submit(&store, &format!("k-batch-{i}"));
        pause(&store, &execution_id, vec![]);
        scheduled.push(schedule(
            &store,
            &work_id,
            Some(&execution_id),
            "requeue",
            NOW,
        ));
    }

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 16, "one pass never exceeds its bound");
    let held: Vec<&String> = scan.held.iter().map(|(id, _)| id).collect();
    assert_eq!(scan.held.len(), 2);
    for (id, reason) in &scan.held {
        assert_eq!(reason, "recovery_limit", "{id}");
    }
    for id in &scheduled {
        let state = intent(&store, id).state;
        if held.contains(&id) {
            assert_eq!(state, RecoveryIntentState::Pending, "{id}");
        } else {
            assert_eq!(state, RecoveryIntentState::Consumed, "{id}");
        }
    }
}

#[test]
fn held_intents_count_against_the_tick_bound() {
    let store = SqliteStore::in_memory(4).unwrap();
    let mut scheduled = Vec::new();
    for i in 0..3 {
        let (work_id, execution_id) = submit(&store, &format!("k-held-{i}"));
        pause(&store, &execution_id, vec![]);
        // An unknown kind is never guessed: each due intent is held
        // after evaluation, and evaluation itself consumes the bound.
        scheduled.push(schedule(
            &store,
            &work_id,
            Some(&execution_id),
            "unknown",
            NOW,
        ));
    }

    let scan = consume(&store, NOW, None, None, 2);
    assert_eq!(scan.consumed, 0);
    assert_eq!(
        scan.held
            .iter()
            .filter(|(_, reason)| reason == "unsupported_kind")
            .count(),
        2,
        "only the tick bound's worth of intents is evaluated: {scan:?}"
    );
    assert_eq!(
        scan.held
            .iter()
            .filter(|(_, reason)| reason == "recovery_limit")
            .count(),
        1,
        "the intent past the bound is reported, not evaluated: {scan:?}"
    );
    for id in &scheduled {
        assert_eq!(
            intent(&store, id).state,
            RecoveryIntentState::Pending,
            "held intents stay pending"
        );
    }
}

#[test]
fn the_tick_deadline_defers_due_intents_to_the_next_pass() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-deadline");
    pause(&store, &execution_id, vec![]);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, Some(NOW), None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.deferred, vec![intent_id.clone()]);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Paused,
        "a deadline never partially fires an intent"
    );
}

#[test]
fn a_scoped_pass_never_touches_another_works_intent() {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w1, e1) = submit(&store, "k-scope-1");
    let (w2, e2) = submit(&store, "k-scope-2");
    pause(&store, &e1, vec![]);
    pause(&store, &e2, vec![]);
    let i1 = schedule(&store, &w1, Some(&e1), "requeue", NOW);
    let i2 = schedule(&store, &w2, Some(&e2), "requeue", NOW);

    let scan = consume(&store, NOW, None, Some(&w1), 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(intent(&store, &i1).state, RecoveryIntentState::Consumed);
    assert_eq!(intent(&store, &i2).state, RecoveryIntentState::Pending);
    assert_eq!(
        execution(&store, &e2).state,
        ExecutionState::Paused,
        "the scoped pass left the other Work untouched"
    );
}

#[test]
fn an_exhausted_invocation_budget_settles_terminal_instead_of_requeueing() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-budget");
    pause(&store, &execution_id, vec![]);
    store
        .transact(|tx| {
            let mut e = tx.execution(&execution_id)?.unwrap();
            e.budget.max_invocations = 0;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Paused,
            })
        })
        .unwrap();
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(scan.failed_terminal, 1);
    assert_eq!(scan.requeued, 0);
    let e = execution(&store, &execution_id);
    assert_eq!(e.state, ExecutionState::Failed);
    assert!(e.terminal_code.is_some(), "the spent bound is recorded");
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
}

#[test]
fn the_work_wide_time_ceiling_blocks_a_requeue() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-time");
    pause(&store, &execution_id, vec![]);
    let work = store.transact(|tx| tx.work(&work_id)).unwrap().unwrap();
    let limit = work.policy.automatic_time_limit_seconds;
    store
        .transact(|tx| {
            let mut usage = tx.work_usage(&work_id)?.unwrap();
            usage.first_dispatch_unix_seconds = Some(NOW - limit as i64 - 100);
            tx.apply(&Effect::UpdateWorkUsage { usage })
        })
        .unwrap();
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.failed_terminal, 1);
    assert_eq!(scan.requeued, 0);
    let e = execution(&store, &execution_id);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(e.terminal_code.as_deref(), Some("automatic_time_exhausted"));
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
}

#[test]
fn a_long_human_pause_does_not_count_against_the_automatic_time_ceiling() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-time-pause");
    pause(&store, &execution_id, vec![]);
    let work = store.transact(|tx| tx.work(&work_id)).unwrap().unwrap();
    let limit = work.policy.automatic_time_limit_seconds as i64;
    // The Work dispatched long ago, but almost all of the elapsed wall
    // time has been the current human/repair pause.
    store
        .transact(|tx| {
            let mut usage = tx.work_usage(&work_id)?.unwrap();
            usage.first_dispatch_unix_seconds = Some(NOW - limit - 200);
            tx.apply(&Effect::UpdateWorkUsage { usage })?;
            let mut execution = tx.execution(&execution_id)?.unwrap();
            execution.paused_since_unix_seconds = Some(NOW - limit - 150);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Paused,
            })
        })
        .unwrap();
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(
        scan.failed_terminal, 0,
        "human waiting is excluded from automatic time: {scan:?}"
    );
    assert_eq!(scan.requeued, 1);
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Queued
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
}

#[test]
fn intent_state_transitions_are_closed_and_idempotent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-lifecycle");
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    // Retire once, then retire again: idempotent, never a hard error.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("terminal".into()),
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("terminal".into()),
            })
        })
        .unwrap();

    // A retired intent can never be re-armed as pending (restart cannot
    // resurrect it), and a consumed one cannot be retired afterwards.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Pending,
                retirement_reason: None,
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "{err}"
    );

    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Consumed,
                retirement_reason: None,
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "{err}"
    );

    let row = intent(&store, &intent_id);
    assert_eq!(row.state, RecoveryIntentState::Retired);
    assert_eq!(row.retirement_reason.as_deref(), Some("terminal"));
}

/// A store wrapper that fails exactly ONE transaction — the Nth
/// `transact` call — with a chosen error, so a test can inject a
/// per-Work store conflict (or an infrastructure outage) at a precise
/// item boundary.
struct Injected<'a, S> {
    inner: &'a S,
    calls: std::cell::Cell<u32>,
    fail_at: u32,
    internal: bool,
}

impl<S: Store> Store for Injected<'_, S> {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n == self.fail_at {
            return Err(if self.internal {
                StoreError::Internal {
                    detail: "injected store outage".into(),
                }
            } else {
                StoreError::ConstraintConflict {
                    detail: "injected per-item conflict".into(),
                }
            });
        }
        self.inner.transact(f)
    }
}

#[test]
fn one_intents_conflict_is_recorded_against_its_work_and_the_rest_still_fire() {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w1, e1) = submit(&store, "k-conflict-1");
    let (w2, e2) = submit(&store, "k-conflict-2");
    pause(&store, &e1, vec![]);
    pause(&store, &e2, vec![]);
    let i1 = schedule(&store, &w1, Some(&e1), "requeue", NOW);
    let i2 = schedule(&store, &w2, Some(&e2), "requeue", NOW);

    // Call 1 lists the due intents; call 2 is the first intent's own
    // transaction — it fails with a store conflict, as a lost race
    // with another writer would.
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: false,
    };
    let mut clock = || NOW;
    let scan = consume_due_recovery_intents(&injected, &mut clock, None, None, 16)
        .expect("a per-intent conflict is not a phase error");

    assert_eq!(
        scan.consumed, 1,
        "the later independent intent still fires: {scan:?}"
    );
    assert_eq!(scan.requeued, 1);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &i1 && reason == "recovery_conflict"),
        "{:?}",
        scan.held
    );
    // The conflicted intent stays pending for the next pass, its
    // Work's Execution is untouched, and the conflict is recorded
    // against that Work instead of aborting the phase.
    assert_eq!(
        intent(&store, &i1).state,
        RecoveryIntentState::Pending,
        "the conflicted intent is never dropped"
    );
    assert_eq!(execution(&store, &e1).state, ExecutionState::Paused);
    let deferrals = store.transact(|tx| tx.deferrals()).unwrap();
    let recorded = deferrals
        .iter()
        .find(|d| d.execution_id == e1)
        .expect("the conflict is recorded against the Work");
    assert_eq!(recorded.reason, "recovery_conflict");
    // The other Work requeued normally.
    assert_eq!(intent(&store, &i2).state, RecoveryIntentState::Consumed);
    assert_eq!(execution(&store, &e2).state, ExecutionState::Queued);
}

#[test]
fn an_infrastructure_failure_still_fails_the_phase() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-outage");
    pause(&store, &execution_id, vec![]);
    schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    // An outage on the same transaction is NOT a per-item conflict:
    // the store itself is unreadable, so the phase fails and the pass
    // aborts instead of silently skipping Work.
    let injected = Injected {
        inner: &store,
        calls: std::cell::Cell::new(0),
        fail_at: 2,
        internal: true,
    };
    let mut clock = || NOW;
    let err = consume_due_recovery_intents(&injected, &mut clock, None, None, 16).unwrap_err();
    assert!(matches!(err, StoreError::Internal { .. }), "{err}");
    assert_eq!(
        intent(&store, "ri-1").state,
        RecoveryIntentState::Pending,
        "nothing fired from the failed pass"
    );
}
