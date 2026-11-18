//! Recovery contract tests for intake (#208): `recover_intake`
//! against durable runs whose recorded owners are live, dead, or
//! uninspectable — owner-proof gating, ledger reconciliation of
//! pending/unknown candidates, and idempotent re-runs.

use std::process::Command;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{Actor, ActorKind, Origin};
use workloop_core::identity::{
    ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability, HostInstanceId,
    RepositoryId, RepositoryRef, RoleBinding,
};
use workloop_core::store::{
    Effect, IntakeCandidate, IntakeCandidateStatus, IntakeRun, IntakeRunState, IntakeTrigger, Store,
};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_exec::supervisor::PosixProcessControl;
use workloop_intake::{RecoveryOutcome, recover_intake};
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
        lanes: Default::default(),
        required_checks: vec![],
        allowed_effects: workloop_core::domain::AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: workloop_core::domain::ExecutionBudget::default(),
        policy: workloop_core::domain::WorkPolicy::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

/// A spawned-then-reaped child: its pid is provably absent from the
/// process table, so owner inspection reports `Stopped`.
fn dead_owner() -> (i32, String) {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    let identity = PosixProcessControl::identity_of(pid).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    (identity.pid, identity.started_at)
}

/// This test process — provably alive for the whole test.
fn live_owner() -> (i32, String) {
    let identity = PosixProcessControl::identity_of(std::process::id() as i32).unwrap();
    (identity.pid, identity.started_at)
}

fn run_record(
    run_id: &str,
    state: IntakeRunState,
    owner_pid: i32,
    owner_started_at: &str,
) -> IntakeRun {
    IntakeRun {
        run_id: run_id.into(),
        trigger: IntakeTrigger::ScheduledInterval,
        selector_id: AdapterId::trusted("test-sel"),
        selector_version: 1,
        policy_revision: 7,
        settings_revision: None,
        state,
        detail_code: None,
        request_payload: b"{}".to_vec(),
        request_digest: "d".repeat(64),
        owner_pid,
        owner_started_at: owner_started_at.into(),
        owner_nonce: format!("nonce-{run_id}"),
        created_unix_seconds: NOW,
        finished_unix_seconds: None,
        recorded_seq: 1,
    }
}

fn insert_run(store: &SqliteStore, run: IntakeRun) {
    store
        .transact(|tx| tx.apply(&Effect::InsertIntakeRun { run }))
        .unwrap();
}

fn candidate(
    run_id: &str,
    ordinal: u32,
    key: &str,
    status: IntakeCandidateStatus,
) -> IntakeCandidate {
    IntakeCandidate {
        run_id: run_id.into(),
        ordinal,
        proposal_key: format!("prop-{key}"),
        replay_key: format!("proposal/test-sel:{key}"),
        payload: br#"{"schema_version":1}"#.to_vec(),
        payload_digest: format!("payload-{key}"),
        status,
        status_reason: None,
        command_id: None,
        work_id: None,
        committed_seq: None,
        recorded_seq: 2,
    }
}

fn admit_batch(store: &SqliteStore, run_id: &str, candidates: Vec<IntakeCandidate>) {
    store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: run_id.into(),
                candidates,
            })
        })
        .unwrap();
}

/// A replay key shaped like a real proposal's, 64 lowercase hex.
fn replay_key(tag: &str) -> String {
    let digest = workloop_selector::proposal_key(
        "test-sel",
        1,
        7,
        &[],
        &workloop_selector::TaskInput {
            intent: format!("intent-{tag}"),
            constraints: vec![],
            verification_argv: vec![],
        },
        &[],
    );
    format!("proposal/test-sel:{digest}")
}

/// Persist an ordinary accepted submit under the selector origin and
/// replay identity — exactly what a mid-run crash leaves behind.
fn seed_decision(
    store: &SqliteStore,
    cfg: &WorkloopConfig,
    actor: &Actor,
    replay_key: &str,
) -> (String, Option<String>) {
    let d = handle_command(
        store,
        cfg,
        &CommandRequest {
            actor: actor.clone(),
            origin: Origin("selector".into()),
            idempotency_key: replay_key.into(),
            body: CommandBody::Submit {
                intent: format!("seeded-{replay_key}"),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        command_id,
        work_id,
        ..
    } = d
    else {
        panic!("seeded submit must accept: {d:?}");
    };
    (command_id.0.clone(), work_id.map(|w| w.0.clone()))
}

fn candidate_with_key(
    run_id: &str,
    ordinal: u32,
    replay_key: &str,
    status: IntakeCandidateStatus,
) -> IntakeCandidate {
    IntakeCandidate {
        replay_key: replay_key.into(),
        proposal_key: replay_key.trim_start_matches("proposal/test-sel:").into(),
        ..candidate(run_id, ordinal, "x", status)
    }
}

fn run_state(store: &SqliteStore, run_id: &str) -> Option<IntakeRunState> {
    store
        .transact_deferred(|tx| Ok(tx.intake_run(run_id)?.map(|r| r.state)))
        .unwrap()
}

fn candidate_row(store: &SqliteStore, run_id: &str, ordinal: u32) -> IntakeCandidate {
    store
        .transact_deferred(|tx| tx.intake_candidates(run_id))
        .unwrap()
        .into_iter()
        .find(|c| c.ordinal == ordinal)
        .unwrap()
}

#[test]
fn recovery_without_an_active_run_is_a_noop() {
    let store = SqliteStore::in_memory(1).unwrap();
    let report = recover_intake(&store, &actor(), NOW).unwrap();
    assert!(matches!(report.outcome, RecoveryOutcome::NoActiveRun));
    assert_eq!(report.active_run, None);
    assert_eq!(report.candidates_reconciled, 0);
}

#[test]
fn a_live_owner_retains_the_run_untouched() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (pid, started) = live_owner();
    insert_run(
        &store,
        run_record("r-live", IntakeRunState::Open, pid, &started),
    );
    let report = recover_intake(&store, &actor(), NOW).unwrap();
    assert!(matches!(report.outcome, RecoveryOutcome::OwnerAlive));
    assert_eq!(report.active_run.as_deref(), Some("r-live"));
    assert_eq!(run_state(&store, "r-live"), Some(IntakeRunState::Open));
}

#[test]
fn an_uninspectable_owner_retains_the_run_untouched() {
    let store = SqliteStore::in_memory(1).unwrap();
    // pid -1 makes `ps` fail: liveness is unprovable, so exclusion
    // is retained rather than reclaimed.
    insert_run(
        &store,
        run_record("r-unknown", IntakeRunState::Open, -1, "bogus-start"),
    );
    let report = recover_intake(&store, &actor(), NOW).unwrap();
    assert!(matches!(report.outcome, RecoveryOutcome::OwnerUnknown));
    assert_eq!(run_state(&store, "r-unknown"), Some(IntakeRunState::Open));
}

#[test]
fn a_dead_owner_open_run_interrupts_without_a_validated_batch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let act = actor();
    let (pid, started) = dead_owner();
    insert_run(
        &store,
        run_record("r-open", IntakeRunState::Open, pid, &started),
    );
    let report = recover_intake(&store, &act, NOW).unwrap();
    match report.outcome {
        RecoveryOutcome::Recovered { final_state } => {
            assert_eq!(final_state, IntakeRunState::Interrupted)
        }
        other => panic!("expected Recovered, got {other:?}"),
    }
    let run = store
        .transact_deferred(|tx| tx.intake_run("r-open"))
        .unwrap()
        .unwrap();
    assert_eq!(run.state, IntakeRunState::Interrupted);
    assert_eq!(run.detail_code.as_deref(), Some("no_conforming_response"));
    assert_eq!(run.finished_unix_seconds, Some(NOW));
}

#[test]
fn recovery_reconciles_candidates_against_the_ledger() {
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config();
    let act = actor();
    let (pid, started) = dead_owner();

    // An `admitting` run whose batch committed before the crash:
    // candidate 0 has a saved accepted decision, candidate 1 was
    // mid-submit (unknown), candidate 2 never reached the ledger.
    let key_decided = replay_key("decided");
    let (command_id, work_id) = seed_decision(&store, &cfg, &act, &key_decided);
    insert_run(
        &store,
        run_record("r-mid", IntakeRunState::Open, pid, &started),
    );
    admit_batch(
        &store,
        "r-mid",
        vec![
            candidate_with_key("r-mid", 0, &key_decided, IntakeCandidateStatus::Pending),
            candidate_with_key(
                "r-mid",
                1,
                &replay_key("unknown"),
                IntakeCandidateStatus::Pending,
            ),
            candidate_with_key(
                "r-mid",
                2,
                &replay_key("lost"),
                IntakeCandidateStatus::Pending,
            ),
        ],
    );
    // Candidate 1's submit outcome was undetermined when the owner
    // died — the `unknown` status the coordinator records for it.
    let c1_row = candidate_row(&store, "r-mid", 1);
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Unknown,
                    status_reason: Some("submit_outcome_unknown".into()),
                    ..c1_row
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    let report = recover_intake(&store, &act, NOW).unwrap();
    match report.outcome {
        RecoveryOutcome::Recovered { final_state } => {
            assert_eq!(final_state, IntakeRunState::Interrupted)
        }
        other => panic!("expected Recovered, got {other:?}"),
    }
    assert_eq!(report.candidates_reconciled, 3);

    // Found decision → replayed with the original links.
    let c0 = candidate_row(&store, "r-mid", 0);
    assert_eq!(c0.status, IntakeCandidateStatus::Replayed);
    assert_eq!(
        c0.command_id.as_ref().map(|c| c.0.as_str()),
        Some(command_id.as_str())
    );
    assert_eq!(
        c0.work_id.as_ref().map(|w| w.0.as_str()),
        work_id.as_deref()
    );
    assert!(c0.committed_seq.is_some());

    // No saved decision → interrupted, never silently accepted.
    for ordinal in [1, 2] {
        let c = candidate_row(&store, "r-mid", ordinal);
        assert_eq!(c.status, IntakeCandidateStatus::Interrupted);
        assert_eq!(c.status_reason.as_deref(), Some("no_saved_decision"));
        assert!(c.command_id.is_none());
        assert!(c.work_id.is_none());
    }
}

#[test]
fn an_admitting_run_with_all_candidates_decided_closes_completed() {
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config();
    let act = actor();
    let (pid, started) = dead_owner();

    // Candidate 0 already reached `accepted` before the crash; its
    // saved decision exists. Candidate 1 is pending but its decision
    // also committed — recovery links it as replayed.
    let key0 = replay_key("done");
    let (command0, work0) = seed_decision(&store, &cfg, &act, &key0);
    let key1 = replay_key("linked");
    let (command1, work1) = seed_decision(&store, &cfg, &act, &key1);
    insert_run(
        &store,
        run_record("r-done", IntakeRunState::Open, pid, &started),
    );
    let c0 = candidate_with_key("r-done", 0, &key0, IntakeCandidateStatus::Pending);
    let c1 = candidate_with_key("r-done", 1, &key1, IntakeCandidateStatus::Pending);
    admit_batch(&store, "r-done", vec![c0, c1]);
    // Candidate 0 resolved to `accepted` before the crash — the same
    // guarded transition the coordinator itself writes.
    let c0_row = candidate_row(&store, "r-done", 0);
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Accepted,
                    command_id: Some(workloop_core::command::CommandId(command0.clone())),
                    work_id: work0.clone().map(workloop_core::domain::WorkId),
                    committed_seq: Some(1),
                    ..c0_row
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    let report = recover_intake(&store, &act, NOW).unwrap();
    match report.outcome {
        RecoveryOutcome::Recovered { final_state } => {
            assert_eq!(final_state, IntakeRunState::Completed)
        }
        other => panic!("expected Recovered/Completed, got {other:?}"),
    }
    // A run recovered to `completed` carries no failure diagnostic —
    // `detail_code` is reserved for failed/interrupted/skipped runs.
    let detail = store
        .transact_deferred(|tx| Ok(tx.intake_run("r-done")?.and_then(|r| r.detail_code)))
        .unwrap();
    assert_eq!(detail, None);
    let c1 = candidate_row(&store, "r-done", 1);
    assert_eq!(c1.status, IntakeCandidateStatus::Replayed);
    assert_eq!(
        c1.command_id.as_ref().map(|c| c.0.as_str()),
        Some(command1.as_str())
    );
    assert_eq!(c1.work_id.as_ref().map(|w| w.0.as_str()), work1.as_deref());
    // The already-decided candidate is untouched.
    let c0 = candidate_row(&store, "r-done", 0);
    assert_eq!(c0.status, IntakeCandidateStatus::Accepted);
}

#[test]
fn recovery_is_idempotent_and_finishes_interrupted_run_leftovers() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let act = actor();
    let (pid, started) = dead_owner();

    // First pass: dead owner, open run, one decided + one lost
    // candidate.
    let key = replay_key("leftover");
    seed_decision(&store, &cfg, &act, &key);
    insert_run(
        &store,
        run_record("r-left", IntakeRunState::Open, pid, &started),
    );
    admit_batch(
        &store,
        "r-left",
        vec![
            candidate_with_key("r-left", 0, &key, IntakeCandidateStatus::Pending),
            candidate_with_key(
                "r-left",
                1,
                &replay_key("orphan"),
                IntakeCandidateStatus::Pending,
            ),
        ],
    );
    let first = recover_intake(&store, &act, NOW).unwrap();
    assert!(matches!(
        first.outcome,
        RecoveryOutcome::Recovered {
            final_state: IntakeRunState::Interrupted
        }
    ));

    // Second pass: the run is terminal, so no active run exists — but
    // a hypothetical leftover would still be swept. Assert the pass
    // is clean and changes nothing.
    let second = recover_intake(&store, &act, NOW + 1).unwrap();
    assert!(matches!(second.outcome, RecoveryOutcome::NoActiveRun));
    assert_eq!(second.candidates_reconciled, 0);
    assert_eq!(
        run_state(&store, "r-left"),
        Some(IntakeRunState::Interrupted)
    );
}

#[test]
fn the_sweep_finishes_candidates_under_an_interrupted_run() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let act = actor();

    // A previous recovery died mid-reconcile: the run is already
    // `interrupted` but a candidate is still `pending`. The next
    // pass — with no active run — must finish it against the ledger.
    let key = replay_key("swept");
    let (command_id, work_id) = seed_decision(&store, &cfg, &act, &key);
    insert_run(
        &store,
        run_record("r-swept", IntakeRunState::Open, -1, "gone"),
    );
    admit_batch(
        &store,
        "r-swept",
        vec![candidate_with_key(
            "r-swept",
            0,
            &key,
            IntakeCandidateStatus::Pending,
        )],
    );
    // Interrupt the run the way a partial recovery leaves it.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-swept".into(),
                state: IntakeRunState::Interrupted,
                detail_code: Some("owner_lost".into()),
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap();

    let report = recover_intake(&store, &act, NOW).unwrap();
    assert!(matches!(report.outcome, RecoveryOutcome::NoActiveRun));
    assert_eq!(report.candidates_reconciled, 1);
    let c = candidate_row(&store, "r-swept", 0);
    assert_eq!(c.status, IntakeCandidateStatus::Replayed);
    assert_eq!(
        c.command_id.as_ref().map(|c| c.0.as_str()),
        Some(command_id.as_str())
    );
    assert_eq!(c.work_id.as_ref().map(|w| w.0.as_str()), work_id.as_deref());
}

#[test]
fn an_admitting_run_with_a_preexisting_interrupted_candidate_stays_interrupted() {
    // An earlier recovery can die between reconciling candidates and
    // closing the run, leaving an `interrupted` candidate under an
    // `admitting` run. A later pass must not close that run
    // `completed` just because this pass interrupted nothing itself.
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config();
    let act = actor();
    let (pid, started) = dead_owner();

    let key1 = replay_key("linked");
    let (command1, work1) = seed_decision(&store, &cfg, &act, &key1);
    insert_run(
        &store,
        run_record("r-partial", IntakeRunState::Open, pid, &started),
    );
    let c0 = candidate_with_key(
        "r-partial",
        0,
        &replay_key("gone"),
        IntakeCandidateStatus::Pending,
    );
    let c1 = candidate_with_key("r-partial", 1, &key1, IntakeCandidateStatus::Pending);
    admit_batch(&store, "r-partial", vec![c0, c1]);
    // The earlier partial recovery already marked candidate 0
    // interrupted; candidate 1 still waits for this pass.
    let c0_row = candidate_row(&store, "r-partial", 0);
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Interrupted,
                    status_reason: Some("no_saved_decision".into()),
                    ..c0_row
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    let report = recover_intake(&store, &act, NOW).unwrap();
    match report.outcome {
        RecoveryOutcome::Recovered { final_state } => {
            assert_eq!(final_state, IntakeRunState::Interrupted)
        }
        other => panic!("expected Recovered/Interrupted, got {other:?}"),
    }
    let detail = store
        .transact_deferred(|tx| Ok(tx.intake_run("r-partial")?.and_then(|r| r.detail_code)))
        .unwrap();
    assert_eq!(detail.as_deref(), Some("owner_lost"));
    // The decided candidate still reconciles normally.
    let c1 = candidate_row(&store, "r-partial", 1);
    assert_eq!(c1.status, IntakeCandidateStatus::Replayed);
    assert_eq!(
        c1.command_id.as_ref().map(|c| c.0.as_str()),
        Some(command1.as_str())
    );
    assert_eq!(c1.work_id.as_ref().map(|w| w.0.as_str()), work1.as_deref());
    // The pre-existing interrupted candidate is untouched.
    let c0 = candidate_row(&store, "r-partial", 0);
    assert_eq!(c0.status, IntakeCandidateStatus::Interrupted);
}

#[test]
fn concurrent_recovery_passes_do_not_report_a_store_fault() {
    // Two recovery passes (e.g. the run driver's tick and a manual
    // `taisan intake`) can observe the same dead-owner run. The
    // loser's guarded close conflicts — recovery is idempotent, so
    // both callers must see success, never a store error.
    let store = SqliteStore::in_memory(2).unwrap();
    let cfg = config();
    let act = actor();
    let (pid, started) = dead_owner();

    let key = replay_key("raced");
    seed_decision(&store, &cfg, &act, &key);
    insert_run(
        &store,
        run_record("r-raced", IntakeRunState::Open, pid, &started),
    );
    admit_batch(
        &store,
        "r-raced",
        vec![candidate_with_key(
            "r-raced",
            0,
            &key,
            IntakeCandidateStatus::Pending,
        )],
    );

    let store_ref = &store;
    let act_ref = &act;
    let results: Vec<_> = std::thread::scope(|scope| {
        let a = scope.spawn(|| recover_intake(store_ref, act_ref, NOW));
        let b = scope.spawn(|| recover_intake(store_ref, act_ref, NOW));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    for result in results {
        assert!(result.is_ok(), "concurrent recovery failed: {result:?}");
    }
    // The run reached a terminal state exactly once.
    assert_eq!(
        run_state(&store, "r-raced"),
        Some(IntakeRunState::Completed)
    );
}
