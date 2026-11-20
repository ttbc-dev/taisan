//! Recovery-scheduling contract: the loop's phase-6 consumer fires due
//! `recovery_intents` exactly once, retires them on terminal evidence,
//! and never releases an explicit hold or guesses an unknown kind.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{QueryResult, attention};
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, RecoveryScan, WorkloopConfig, consume_due_recovery_intents, handle_command,
    handle_observation,
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
    schedule_at_generation(store, work_id, execution_id, kind, due_at, 1)
}

fn schedule_at_generation(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: Option<&ExecutionId>,
    kind: &str,
    due_at: i64,
    generation: u64,
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
                    generation,
                    evidence_basis: "test".into(),
                    state: RecoveryIntentState::Pending,
                    retirement_reason: None,
                    retirement_generation: None,
                    scheduled_seq,
                },
            })?;
            Ok(intent_id)
        })
        .unwrap()
}

/// Record one pending outbox host write for the Work, making
/// `projection::outstanding_actions` non-empty until it reconciles.
fn outstanding_host_write(store: &SqliteStore, work_id: &WorkId) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: format!("act-outstanding-{}", work_id.0),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::PushRef {
                        branch: format!("work/{}", work_id.0),
                        produced: CommitRevision::new(repo(), "artifact-digest").unwrap(),
                        expected_prior: None,
                    },
                    payload_digest: "test".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq: 0,
                },
            })
        })
        .unwrap();
}

fn reconcile_host_write(store: &SqliteStore, work_id: &WorkId) {
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateHostActionIntent {
                action_id: format!("act-outstanding-{}", work_id.0),
                status: HostActionStatus::Applied,
                authenticated_actor: Some(publisher()),
                effects: vec![],
            })
        })
        .unwrap();
}

fn consume(
    store: &SqliteStore,
    now: i64,
    deadline: Option<i64>,
    scope: Option<&WorkId>,
    limit: usize,
) -> RecoveryScan {
    let mut clock = || now;
    consume_due_recovery_intents(store, &config(), &mut clock, deadline, scope, limit).unwrap()
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

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn config_with_issue_read() -> WorkloopConfig {
    let mut c = config();
    c.declared_capabilities
        .insert(workloop_core::identity::HostCapability::IssueRead);
    c
}

fn submit_with_source(
    store: &SqliteStore,
    cfg: &WorkloopConfig,
    key: &str,
    source: SourceSnapshot,
) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        cfg,
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources: vec![source],
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

/// One authoritative issue read, delivered as an observation — the same
/// fact shape the run loop records from the host.
fn issue_read(
    store: &SqliteStore,
    cfg: &WorkloopConfig,
    work_id: &WorkId,
    number: u64,
    revision: &str,
    digest: &str,
    state: IssueState,
) {
    handle_observation(
        store,
        cfg,
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::Issue {
                issue: issue(number),
                source_revision: revision.into(),
                content_digest: digest.into(),
                state,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

fn pending_requeues(store: &SqliteStore, execution_id: &ExecutionId) -> usize {
    store
        .transact(|tx| {
            Ok(tx
                .recovery_intents()?
                .iter()
                .filter(|i| {
                    i.execution_id.as_ref() == Some(execution_id)
                        && i.state == RecoveryIntentState::Pending
                        && i.kind == "requeue"
                })
                .count())
        })
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
    assert_eq!(
        row.retirement_generation,
        Some(row.generation),
        "retirement records the evidence generation that invalidated it"
    );
}

#[test]
fn a_stale_retirement_generation_cannot_retire_a_newer_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-stale-retire");
    let intent_id =
        schedule_at_generation(&store, &work_id, Some(&execution_id), "requeue", NOW, 5);

    // A decision made from evidence older than the intent's scheduling
    // generation cannot retire it: the newer intent stays pending for
    // the pass that owns the current evidence.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("execution_terminal".into()),
                evidence_generation: Some(4),
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "{err}"
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending,
        "a stale decision never retires a newer intent"
    );

    // Evidence at or beyond the intent's generation retires it and the
    // generation is recorded.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("execution_terminal".into()),
                evidence_generation: Some(5),
            })
        })
        .unwrap();
    let row = intent(&store, &intent_id);
    assert_eq!(row.state, RecoveryIntentState::Retired);
    assert_eq!(row.retirement_generation, Some(5));

    // Retirement stays idempotent: a repeated retire is a no-op and does
    // not rewrite the recorded evidence generation.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("stale-repeat".into()),
                evidence_generation: Some(3),
            })
        })
        .unwrap();
    let row = intent(&store, &intent_id);
    assert_eq!(row.retirement_reason.as_deref(), Some("execution_terminal"));
    assert_eq!(row.retirement_generation, Some(5));
}

#[test]
fn a_retirement_older_than_the_current_work_generation_is_stale() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-stale-work");
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    // Newer Work evidence supersedes the decision's generation.
    store
        .transact(|tx| {
            let mut work = tx.work(&work_id)?.unwrap();
            let expected = work.version;
            work.version += 1;
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();

    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("execution_terminal".into()),
                evidence_generation: Some(1),
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "{err}"
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );

    // Evidence at the current Work generation retires it.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("execution_terminal".into()),
                evidence_generation: Some(2),
            })
        })
        .unwrap();
    let row = intent(&store, &intent_id);
    assert_eq!(row.state, RecoveryIntentState::Retired);
    assert_eq!(row.retirement_generation, Some(2));
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
fn a_progressing_execution_retires_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-active");
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    // The Execution is `queued` — resumed by another path while the
    // intent was pending. The requeue can never fire, so it is retired
    // rather than left pending to consume the per-tick budget forever.
    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.retired, 1);
    let row = intent(&store, &intent_id);
    assert_eq!(row.state, RecoveryIntentState::Retired);
    assert_eq!(
        row.retirement_reason.as_deref(),
        Some("execution_progressing"),
        "retirement records why the intent can never fire"
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Queued
    );
}

/// A pause still resolving keeps its intent: `stopping`/`uncertain`
/// heading for `paused` is exactly the state the requeue exists for.
#[test]
fn a_pause_still_resolving_holds_the_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-resolving");
    set_state(
        &store,
        &execution_id,
        ExecutionState::Queued,
        ExecutionState::Stopping,
    );
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.retired, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "execution_active")
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
}

/// #288: every `pause_for_repair` used to insert a fresh `requeue`
/// intent unconditionally. Repeated pause→resume cycles — here driven
/// through the real source-gate pause and re-admission — must keep at
/// most one pending `requeue` per Execution, parity with the `retry`
/// dedupe guard.
#[test]
fn repeated_repair_pauses_keep_one_pending_requeue_intent() {
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config_with_issue_read();
    let (work_id, eid) = submit_with_source(
        &store,
        &cfg,
        "k-dedupe",
        SourceSnapshot {
            issue: issue(1),
            source_revision: "rev-1".into(),
            content_digest: "digest-1".into(),
        },
    );
    // An unavailable authoritative read pauses the queued Execution for
    // repair — the first real `pause_for_repair` of the cycle.
    issue_read(
        &store,
        &cfg,
        &work_id,
        1,
        "rev-1",
        "digest-1",
        IssueState::Unavailable,
    );
    assert_eq!(execution(&store, &eid).state, ExecutionState::Paused);
    assert_eq!(pending_requeues(&store, &eid), 1);
    // Each cycle is a real resume→admit→pause: the Execution returns to
    // `queued`, admission re-reads the still-unavailable source gate and
    // pauses it again through `pause_for_repair`.
    for _ in 0..8 {
        set_state(&store, &eid, ExecutionState::Paused, ExecutionState::Queued);
        store
            .transact(|tx| workloop_core::work::admit_queued(tx, &cfg, &eid, NOW).map(|_| ()))
            .unwrap();
        assert_eq!(execution(&store, &eid).state, ExecutionState::Paused);
    }
    assert_eq!(
        pending_requeues(&store, &eid),
        1,
        "pause/resume cycles never accumulate pending requeue intents"
    );
}

/// #288 lost-wakeup interleaving: a pending `requeue` whose Execution
/// resumes and re-pauses *before* the intent is evaluated must still
/// fire — dedupe keeps the one pending intent and the firing guards
/// revalidate the live `paused` state, so the pause is never stranded.
#[test]
fn a_pending_requeue_survives_a_resume_and_fires_on_the_next_pause() {
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config_with_issue_read();
    let (work_id, eid) = submit_with_source(
        &store,
        &cfg,
        "k-interleave",
        SourceSnapshot {
            issue: issue(1),
            source_revision: "rev-1".into(),
            content_digest: "digest-1".into(),
        },
    );
    issue_read(
        &store,
        &cfg,
        &work_id,
        1,
        "rev-1",
        "digest-1",
        IssueState::Unavailable,
    );
    assert_eq!(execution(&store, &eid).state, ExecutionState::Paused);
    // Resume, then re-pause through the real admit path — all before
    // the recovery pass evaluates the intent.
    set_state(&store, &eid, ExecutionState::Paused, ExecutionState::Queued);
    store
        .transact(|tx| workloop_core::work::admit_queued(tx, &cfg, &eid, NOW).map(|_| ()))
        .unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Paused);
    assert_eq!(pending_requeues(&store, &eid), 1);
    // The source recovers before the recovery pass: the recorded
    // `SourceRead` prerequisite is met, so the surviving intent fires
    // against the new pause — consumed, and the Execution re-enters
    // the queue. The pause is never stranded.
    issue_read(
        &store,
        &cfg,
        &work_id,
        1,
        "rev-1",
        "digest-1",
        IssueState::Open,
    );
    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(execution(&store, &eid).state, ExecutionState::Queued);
    assert_eq!(pending_requeues(&store, &eid), 0);
}

/// #288: intents left pending by executions that already resumed used
/// to sit at the head of the due list forever, each consuming a slot of
/// the per-tick bound. They are now retired on evaluation — the dead
/// rows cost budget once, and live intents for other Works fire.
#[test]
fn stale_requeue_intents_retire_instead_of_starving_the_tick() {
    let store = SqliteStore::in_memory(24).unwrap();
    // The pre-fix accumulation shape: many pending requeue intents
    // whose Executions are already progressing again — they can never
    // fire. They are due earlier, so they sit at the head of the due
    // list on every pass.
    for i in 0..20 {
        let (w, e) = submit(&store, &format!("k-stale-{i}"));
        set_state(&store, &e, ExecutionState::Queued, ExecutionState::Running);
        schedule(&store, &w, Some(&e), "requeue", NOW - 1);
    }
    // Two live paused Works with due intents behind them in the queue.
    let (w1, e1) = submit(&store, "k-live-1");
    pause(&store, &e1, vec![]);
    let i1 = schedule(&store, &w1, Some(&e1), "requeue", NOW);
    let (w2, e2) = submit(&store, "k-live-2");
    pause(&store, &e2, vec![]);
    let i2 = schedule(&store, &w2, Some(&e2), "requeue", NOW);

    // Pass 1 evaluates the head of the due list: the 16 oldest stale
    // intents are retired, never held pending. Under the old behavior
    // they stayed pending and re-consumed this budget on every pass —
    // with 20 stale intents above the limit of 16, the live intents
    // behind them were never evaluated at all.
    let s1 = consume(&store, NOW, None, None, 16);
    assert_eq!(s1.retired, 16, "dead intents retire on first evaluation");
    // Pass 2 reaches the live intents: the remaining stale rows retire
    // and both live intents fire — dead intents never starve live
    // recovery again.
    let s2 = consume(&store, NOW, None, None, 16);
    assert_eq!(s2.retired, 4);
    assert_eq!(s2.consumed, 2);
    assert_eq!(intent(&store, &i1).state, RecoveryIntentState::Consumed);
    assert_eq!(intent(&store, &i2).state, RecoveryIntentState::Consumed);
    assert_eq!(execution(&store, &e1).state, ExecutionState::Queued);
    assert_eq!(execution(&store, &e2).state, ExecutionState::Queued);
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
fn an_exhausted_invocation_budget_defers_terminal_while_a_host_write_is_outstanding() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-budget-outstanding");
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
    outstanding_host_write(&store, &work_id);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.failed_terminal, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "outbox_reconciliation"),
        "the terminal settlement is held, not applied: {scan:?}"
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending,
        "an intent that was not applied stays pending"
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Paused,
        "a paused Execution is not terminally settled under an outstanding host write"
    );
    assert!(
        store
            .transact(|tx| tx.deferrals())
            .unwrap()
            .iter()
            .any(|d| d.execution_id == execution_id && d.reason == "outbox_reconciliation")
    );

    // Once the host write reconciles, the same intent settles terminal.
    reconcile_host_write(&store, &work_id);
    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.failed_terminal, 1);
    assert_eq!(scan.requeued, 0);
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Failed
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
}

#[test]
fn the_work_wide_time_ceiling_defers_terminal_while_a_host_write_is_outstanding() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (work_id, execution_id) = submit(&store, "k-time-outstanding");
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
    outstanding_host_write(&store, &work_id);
    let intent_id = schedule(&store, &work_id, Some(&execution_id), "requeue", NOW);

    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(scan.failed_terminal, 0);
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "outbox_reconciliation"),
        "the terminal settlement is held, not applied: {scan:?}"
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
    assert_eq!(
        execution(&store, &execution_id).state,
        ExecutionState::Paused
    );

    // Reconciled: the ceiling now settles the Execution terminal.
    reconcile_host_write(&store, &work_id);
    let scan = consume(&store, NOW, None, None, 16);
    assert_eq!(scan.failed_terminal, 1);
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
                evidence_generation: Some(1),
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateRecoveryIntent {
                intent_id: intent_id.clone(),
                state: RecoveryIntentState::Retired,
                retirement_reason: Some("terminal".into()),
                evidence_generation: Some(1),
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
                evidence_generation: None,
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
                evidence_generation: None,
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
    let scan = consume_due_recovery_intents(&injected, &config(), &mut clock, None, None, 16)
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
    let err =
        consume_due_recovery_intents(&injected, &config(), &mut clock, None, None, 16).unwrap_err();
    assert!(matches!(err, StoreError::Internal { .. }), "{err}");
    assert_eq!(
        intent(&store, "ri-1").state,
        RecoveryIntentState::Pending,
        "nothing fired from the failed pass"
    );
}

// ---- U5.6: production-scheduled intents and durable transport retry ----

/// The producing coding Invocation, done with its required artifact —
/// the publish chain's gate input.
fn recorded_invocation(eid: &ExecutionId, id: &str) -> Invocation {
    Invocation {
        id: InvocationId(id.into()),
        request_id: format!("{}:inv:0", eid.0),
        execution_id: eid.clone(),
        role: InvocationRole::Coding,
        state: InvocationState::Done,
        process_identity: None,
        required_artifacts: vec![RequiredArtifact {
            kind: "artifact.json".into(),
            path: "artifact.json".into(),
            expected_digest: Some("artifact-digest".into()),
        }],
        evidence_refs: vec![],
        descendants: vec![],
    }
}

/// The invariant lane's completed read-only check Invocation — the
/// publish gate requires it after the producing Invocation.
fn recorded_check_invocation(eid: &ExecutionId, id: &str) -> Invocation {
    Invocation {
        id: InvocationId(id.into()),
        request_id: format!("{}:inv:check", eid.0),
        execution_id: eid.clone(),
        role: InvocationRole::InvariantCheck,
        state: InvocationState::Done,
        process_identity: None,
        required_artifacts: vec![],
        evidence_refs: vec![],
        descendants: vec![],
    }
}

/// The check Invocation's proven `Exited{0}` — a `Done` record alone
/// is resolved-but-unproven.
fn check_exit_observation(eid: &ExecutionId, check_id: &str) -> Observation {
    Observation {
        delivery: None,
        subject_work_id: None,
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::ProcessState {
            invocation_id: InvocationId(check_id.into()),
            process_identity: String::new(),
            state: ProcessStateFact::Exited,
            exit_code: Some(0),
        },
        observed_unix_seconds: NOW,
    }
}

fn emit(store: &SqliteStore, work_id: &WorkId, execution_id: &ExecutionId, fact: ObservationFact) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(execution_id.clone()),
            fact,
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

fn pushes(store: &SqliteStore) -> Vec<HostActionIntent> {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .filter(|i| matches!(i.action, HostAction::PushRef { .. }))
                .collect::<Vec<_>>())
        })
        .unwrap()
}

fn retry_intents(store: &SqliteStore) -> Vec<RecoveryIntent> {
    store
        .transact(|tx| {
            Ok(tx
                .recovery_intents()?
                .into_iter()
                .filter(|i| i.kind == "retry")
                .collect::<Vec<_>>())
        })
        .unwrap()
}

/// Drive an Execution's publish chain to a recorded first push, then
/// fail it definitively — the production path that must persist a
/// `retry` intent in the deciding transaction.
fn drive_to_failed_push(store: &SqliteStore, work_id: &WorkId, execution_id: &ExecutionId) {
    set_state(
        store,
        execution_id,
        ExecutionState::Queued,
        ExecutionState::Waiting,
    );
    let coding = format!("{}-coding", execution_id.0);
    let check = format!("{}-check", execution_id.0);
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: recorded_invocation(execution_id, &coding),
            })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: recorded_check_invocation(execution_id, &check),
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: check_exit_observation(execution_id, &check),
            })?;
            Ok(())
        })
        .unwrap();
    emit(
        store,
        work_id,
        execution_id,
        ObservationFact::CapabilityReport {
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
        },
    );
    let before = pushes(store).len();
    emit(
        store,
        work_id,
        execution_id,
        ObservationFact::Artifact {
            invocation_id: InvocationId(coding.clone()),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present {
                digest: "artifact-digest".into(),
            },
        },
    );
    assert_eq!(
        pushes(store).len(),
        before + 1,
        "the artifact records this execution's push intent"
    );
    let latest = pushes(store).last().unwrap().action_id.clone();
    emit(
        store,
        work_id,
        execution_id,
        ObservationFact::HostActionResult {
            action_id: latest,
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Failed {
                reason: "declined".into(),
            },
            effects: vec![],
        },
    );
}

fn failed_push(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let (w, e1) = submit(store, key);
    drive_to_failed_push(store, &w, &e1);
    (w, e1)
}

/// Pause a `waiting` Execution with an explicit operator hold — the
/// precondition automatic recovery must never release.
fn hold(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("operator hold".into());
            e.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            e.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Waiting,
            })
        })
        .unwrap();
}

/// Fail a `waiting` Execution terminally — evidence that must retire
/// any recovery intent scheduled for it.
fn fail_execution(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| {
            let mut e = tx.execution(execution_id)?.unwrap();
            e.state = ExecutionState::Failed;
            e.terminal_code = Some("setup".into());
            e.outcome = Some(Outcome {
                state: ExecutionState::Failed,
                code: Some("setup".into()),
                detail: None,
                evidence_refs: vec![],
                recorded_seq: 0,
            });
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Waiting,
            })
        })
        .unwrap();
}

/// A file-backed store path under a fresh temp dir — reopen simulates
/// the restart between scheduling and firing.
fn file_db(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let unique = format!(
        "taisan-u56-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).unwrap();
    (root.clone(), root.join("store.db"))
}

/// The receipt persists the retry intent in the deciding transaction —
/// before any action can depend on it — and an unrelated drive of the
/// publish chain can never outrun the persisted wait.
#[test]
fn a_failed_receipt_persists_the_retry_intent_before_any_action() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = failed_push(&store, "r56-persist");
    let retries = retry_intents(&store);
    assert_eq!(retries.len(), 1, "exactly one retry intent was persisted");
    let retry = &retries[0];
    assert_eq!(retry.state, RecoveryIntentState::Pending);
    assert_eq!(
        retry.due_at_unix_seconds,
        NOW + 10,
        "the persisted due time is the policy schedule's first entry"
    );
    assert_eq!(retry.evidence_basis, "recovery.transport.max_retries");
    assert_eq!(retry.work_id, w);
    assert_eq!(retry.execution_id.as_ref(), Some(&e1));
    // Persisted before the action: the receipt itself recorded no
    // re-issue, and a later unrelated drive cannot outrun the wait.
    assert_eq!(pushes(&store).len(), 1);
    emit(
        &store,
        &w,
        &e1,
        ObservationFact::BranchState {
            repository: repo(),
            branch: format!("work/{}", w.0),
            state: BranchState::At {
                revision: rev("head-fresh"),
            },
        },
    );
    assert_eq!(
        pushes(&store).len(),
        1,
        "no re-issue while the retry intent is pending"
    );
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

/// The persisted backoff comes from the Work's configured schedule,
/// indexed by attempt — not from elapsed wall time.
#[test]
fn the_retry_backoff_is_persisted_from_the_works_policy() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit(&store, "r56-backoff");
    store
        .transact(|tx| {
            let mut work = tx.work(&w)?.unwrap();
            let expected = work.version;
            work.policy.transport_backoff_seconds = vec![7, 42];
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();
    drive_to_failed_push(&store, &w, &e1);
    let first = &retry_intents(&store)[0];
    assert_eq!(
        first.due_at_unix_seconds,
        NOW + 7,
        "attempt 1 waits the first schedule entry"
    );
    // A second definite failure indexes the next schedule entry —
    // measured from the deciding transaction's clock, not elapsed time.
    let scan = consume(&store, NOW + 7, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(pushes(&store).len(), 2);
    emit(
        &store,
        &w,
        &e1,
        ObservationFact::HostActionResult {
            action_id: pushes(&store)[1].action_id.clone(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Failed {
                reason: "declined".into(),
            },
            effects: vec![],
        },
    );
    let second = retry_intents(&store)
        .into_iter()
        .find(|i| i.state == RecoveryIntentState::Pending)
        .expect("the second failure persisted its own intent");
    assert_eq!(
        second.due_at_unix_seconds,
        NOW + 42,
        "attempt 2 waits the second schedule entry"
    );
}

/// A pending retry fires exactly once at `backoff_until`: not before,
/// and never twice.
#[test]
fn a_due_retry_fires_once_and_reissues_the_failed_push() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, e1) = failed_push(&store, "r56-once");
    let intent_id = retry_intents(&store)[0].intent_id.clone();

    // Not yet due: the intent stays pending, nothing re-issues.
    let early = consume(&store, NOW + 9, None, None, 16);
    assert_eq!(early.consumed, 0);
    assert_eq!(pushes(&store).len(), 1);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );

    // Due: the intent is consumed first, then the failed push re-issues.
    let scan = consume(&store, NOW + 10, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
    assert_eq!(
        pushes(&store).len(),
        2,
        "the consumed retry re-issued the push"
    );
    assert_eq!(
        execution(&store, &e1).state,
        ExecutionState::Waiting,
        "the Execution keeps waiting on the retried effect"
    );

    // A second pass cannot fire a consumed intent again.
    let again = consume(&store, NOW + 10, None, None, 16);
    assert_eq!(again.consumed, 0, "consumed is closed to further firing");
    assert_eq!(pushes(&store).len(), 2);
}

/// An explicit operator hold keeps a due retry intent pending — the
/// automatic pass never consumes it and never re-issues.
#[test]
fn an_explicit_hold_never_consumes_a_due_retry() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, e1) = failed_push(&store, "r56-hold");
    let intent_id = retry_intents(&store)[0].intent_id.clone();
    hold(&store, &e1);

    let scan = consume(&store, NOW + 10, None, None, 16);
    assert_eq!(scan.consumed, 0, "the hold is never released automatically");
    assert!(
        scan.held
            .iter()
            .any(|(id, reason)| id == &intent_id && reason == "operator_hold"),
        "{:?}",
        scan.held
    );
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending
    );
    assert_eq!(pushes(&store).len(), 1, "no re-issue under the hold");
}

/// A pending retry intent survives a restart between scheduling and
/// `backoff_until`, then fires exactly once.
#[test]
fn a_pending_retry_survives_restart_and_fires_once() {
    let (root, db) = file_db("restart-pending");
    let intent_id;
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        let (_w, _e1) = failed_push(&store, "r56-restart");
        intent_id = retry_intents(&store)[0].intent_id.clone();
        assert_eq!(pushes(&store).len(), 1);
    }
    // The restart: a fresh handle sees the same durable pending intent.
    let store = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Pending,
        "the pending intent survives the restart"
    );
    let scan = consume(&store, NOW + 10, None, None, 16);
    assert_eq!(scan.consumed, 1);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Consumed
    );
    assert_eq!(pushes(&store).len(), 2);
    drop(store);
    // And after another restart the consumed intent still cannot fire.
    let again = SqliteStore::open(&db, 1).unwrap();
    let rescan = consume(&again, NOW + 10, None, None, 16);
    assert_eq!(rescan.consumed, 0, "consumed never re-fires after restart");
    assert_eq!(pushes(&again).len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}

/// A retired intent stays retired across a restart — terminal evidence
/// that invalidated it is durable.
#[test]
fn a_retired_intent_stays_retired_across_restart() {
    let (root, db) = file_db("restart-retired");
    let intent_id;
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        let (_w, e1) = failed_push(&store, "r56-retire");
        intent_id = retry_intents(&store)[0].intent_id.clone();
        fail_execution(&store, &e1);
        let scan = consume(&store, NOW + 10, None, None, 16);
        assert_eq!(scan.retired, 1);
        assert_eq!(
            intent(&store, &intent_id).state,
            RecoveryIntentState::Retired
        );
    }
    // Reopen: the retired state is durable and closed — the intent
    // can never fire no matter how late the pass runs.
    let store = SqliteStore::open(&db, 1).unwrap();
    let scan = consume(&store, NOW + 10_000, None, None, 16);
    assert_eq!(scan.consumed, 0);
    assert_eq!(
        intent(&store, &intent_id).state,
        RecoveryIntentState::Retired,
        "retired survives the restart and never fires"
    );
    assert_eq!(pushes(&store).len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// The transport account is Work-wide: retries a predecessor spent
/// count against the successor Execution's budget — an attempt past
/// the Work's limit pauses for a human instead of scheduling another
/// automatic retry.
#[test]
fn transport_retries_span_successor_executions() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = failed_push(&store, "r56-span");
    store
        .transact(|tx| {
            let mut work = tx.work(&w)?.unwrap();
            let expected = work.version;
            work.policy.transport_max_retries = 1;
            tx.apply(&Effect::UpdateWork { work, expected })
        })
        .unwrap();
    fail_execution(&store, &e1);
    // The successor Execution on the same Work.
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "r56-span-e2".into(),
            body: CommandBody::Retry {
                work_id: w.clone(),
                expected_work_version: 1,
                expected_execution_id: e1.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        effect: CommandEffect::ExecutionCreated { execution_id: e2 },
        ..
    } = d
    else {
        panic!("successor execution: {d:?}")
    };
    drive_to_failed_push(&store, &w, &e2);
    // The predecessor's retry counts: attempt 2 exceeds the limit of 1.
    let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(
        usage.transport_retries_used, 2,
        "the transport account is Work-wide across Executions"
    );
    assert_eq!(
        execution(&store, &e2).state,
        ExecutionState::Stopping,
        "a spent budget pauses for a human, never retries"
    );
    assert_eq!(
        execution(&store, &e2).desired_action,
        Some(DesiredAction::Pause),
        "the recorded desire is the pause"
    );
    assert!(
        execution(&store, &e2)
            .repair_prerequisites
            .iter()
            .any(|p| matches!(p, RepairPrerequisite::OperatorDecision)),
        "exhaustion records the operator hold"
    );
    // The writer-stop proof settles the recorded pause (fixture
    // equivalent), then the pause's scheduled requeue stays pending
    // under the hold and the pause surfaces as Attention — never an
    // automatic retry.
    set_state(
        &store,
        &e2,
        ExecutionState::Stopping,
        ExecutionState::Paused,
    );
    let scan = consume(&store, NOW + 10, None, None, 16);
    assert_eq!(scan.consumed, 0);
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(
        items.iter().any(|a| a.reason == "paused_judgment"),
        "budget exhaustion surfaces Attention"
    );
}

/// No-progress is a signal only: it derives an Attention record and
/// never schedules a recovery intent, never stops, never retries.
#[test]
fn no_progress_raises_attention_only_and_never_schedules_a_retry() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit(&store, "r56-noprogress");
    set_state(&store, &e1, ExecutionState::Queued, ExecutionState::Ready);
    store
        .transact(|tx| {
            let mut usage = tx.work_usage(&w)?.unwrap_or_else(|| WorkUsage::new(&w));
            usage.first_dispatch_unix_seconds = Some(NOW - 3700);
            tx.apply(&Effect::UpdateWorkUsage { usage })
        })
        .unwrap();
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(
        items.iter().any(|a| a.reason == "no_progress"),
        "a stale progress clock surfaces Attention"
    );
    assert!(
        store
            .transact(|tx| tx.recovery_intents())
            .unwrap()
            .is_empty(),
        "no-progress never conjures an automatic retry"
    );
    let e = execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Ready, "no-progress never stops");
    assert_eq!(e.desired_action, None);
}

/// A progress record that cannot be classified — a timestamp without
/// its basis — surfaces as unknown: never progress, never death.
#[test]
fn corrupt_progress_surfaces_as_unknown_never_as_progress() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e1) = submit(&store, "r56-corrupt");
    set_state(&store, &e1, ExecutionState::Queued, ExecutionState::Ready);
    store
        .transact(|tx| {
            let mut usage = tx.work_usage(&w)?.unwrap_or_else(|| WorkUsage::new(&w));
            usage.first_dispatch_unix_seconds = Some(NOW - 100);
            // A timestamp without its recorded basis is unreadable.
            usage.last_progress_unix_seconds = Some(NOW - 10);
            tx.apply(&Effect::UpdateWorkUsage { usage })
        })
        .unwrap();
    let items = match store.transact(|tx| attention(tx, Some(&w), NOW)).unwrap() {
        QueryResult::Found { value, .. } => value,
        other => panic!("expected found, got {other:?}"),
    };
    assert!(
        items.iter().any(|a| a.reason == "progress_unknown"),
        "an unclassifiable progress record surfaces as unknown"
    );
    assert!(
        !items.iter().any(|a| a.reason == "no_progress"),
        "corrupt progress is never treated as a progress reading"
    );
    let e = execution(&store, &e1);
    assert_eq!(e.state, ExecutionState::Ready, "never treated as death");
}
