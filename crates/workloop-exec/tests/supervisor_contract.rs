//! Contract tests for the supervisor ProcessControlPort and the launch /
//! stop / recovery paths over real processes (`docs/v0.1.md`: Launch,
//! Stop proof, Recovery rows).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::ports::{ProcessControlPort, ProcessEvidence, SignalOutcome, StopReceipt};
use workloop_core::store::{Effect, HostAction, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation, sweep,
};
use workloop_exec::supervisor::{PosixProcessControl, StopRecord, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn artifact(kind: &str) -> RequiredArtifact {
    RequiredArtifact {
        kind: kind.into(),
        path: kind.into(),
        expected_digest: None,
    }
}

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: host(),
                    actor: HostActorId::trusted("pub-agent"),
                },
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
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget {
            artifact_recovery_attempts: 3,
            artifact_recovery_seconds: 60,
            ..ExecutionBudget::default()
        },
        policy: WorkPolicy::default(),
    }
}

/// A `publish` stage effect is recorded only under an authorized
/// capability — the supervisor tests grant the fixture host the
/// publication capabilities once per store.
fn authorize(store: &SqliteStore) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
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
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
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

fn supervisor(mode: &str, root: PathBuf) -> Supervisor<PosixProcessControl> {
    Supervisor::new(
        PosixProcessControl,
        root,
        vec![fixture_bin(), mode.to_string()],
        config(),
    )
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution")
}

fn submit_and_admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    submit_and_admit_with(store, &config(), key)
}

fn submit_and_admit_with(
    store: &SqliteStore,
    cfg: &WorkloopConfig,
    key: &str,
) -> (WorkId, ExecutionId) {
    let d = handle_command(store, cfg, &submit(key), NOW).unwrap();
    let (work_id, eid) = match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id } => execution_id,
                other => panic!("unexpected effect {other:?}"),
            };
            (work_id.unwrap(), eid)
        }
        other => panic!("submit rejected: {other:?}"),
    };
    authorize(store);
    let admitted = store
        .transact(|tx| admit_queued(tx, cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, eid)
}

fn wait_for(mut pred: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn cancel(store: &SqliteStore, work_id: &WorkId, key: &str) {
    let version = store
        .transact(|tx| tx.work(work_id))
        .unwrap()
        .map(|w| w.version)
        .expect("work exists");
    let d = handle_command(
        store,
        &config(),
        &req(
            key,
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(
        matches!(d, CommandDecision::Accepted { .. }),
        "cancel: {d:?}"
    );
}

// ---- ProcessControlPort (#7) ----

#[test]
fn spawn_records_identity_in_own_process_group() {
    let root = tmp_root("spawn");
    let port = PosixProcessControl;
    let identity = port
        .spawn(&[fixture_bin(), "hang".into()], &root)
        .expect("spawn");
    assert_eq!(identity.pgid, Some(identity.pid));
    assert!(!identity.started_at.is_empty());
    assert!(matches!(
        port.inspect(&identity),
        ProcessEvidence::Alive { .. }
    ));
    port.signal_verified(&identity, libc::SIGKILL);
}

#[test]
fn signal_reverified_never_hits_a_reused_pid() {
    let root = tmp_root("reverify");
    let port = PosixProcessControl;
    let identity = port
        .spawn(&[fixture_bin(), "hang".into()], &root)
        .expect("spawn");
    // Corrupt the recorded verifier: a stale identity must not be signaled.
    let stale = ProcessIdentity {
        started_at: "bogus".into(),
        ..identity.clone()
    };
    assert_eq!(
        port.signal_verified(&stale, libc::SIGKILL),
        SignalOutcome::SkippedStale
    );
    assert!(matches!(
        port.inspect(&identity),
        ProcessEvidence::Alive { .. }
    ));
    // The real identity still verifies and is signaled.
    assert_eq!(
        port.signal_verified(&identity, libc::SIGKILL),
        SignalOutcome::Sent
    );
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "stopped after signal",
    );
}

#[test]
fn zombie_counts_as_stopped() {
    let root = tmp_root("zombie");
    let port = PosixProcessControl;
    let leader = port
        .spawn(&[fixture_bin(), "zombie".into()], &root)
        .expect("spawn");
    // The fixture holds an unreaped child; find it via the snapshot.
    wait_for(
        || {
            port.snapshot_descendants(leader.pid)
                .unwrap()
                .iter()
                .any(|i| i.pid != leader.pid)
        },
        "zombie child to appear",
    );
    let descendants = port.snapshot_descendants(leader.pid).unwrap();
    let zombie = descendants
        .iter()
        .find(|i| i.pid != leader.pid)
        .expect("a descendant");
    assert!(matches!(
        port.inspect(zombie),
        ProcessEvidence::Stopped { .. }
    ));
    port.signal_verified(&leader, libc::SIGKILL);
}

#[test]
fn reaped_identity_inspects_stopped_and_signals_already_gone() {
    let root = tmp_root("reaped");
    let port = PosixProcessControl;
    let identity = port
        .spawn(&[fixture_bin(), "hang".into()], &root)
        .expect("spawn");
    port.signal_verified(&identity, libc::SIGKILL);
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "stopped after signal",
    );
    // Reap until the pid leaves the process table: a definitively absent
    // pid is stop proof, and a further verified signal is a no-op.
    wait_for(
        || {
            let mut status = 0;
            let reaped =
                unsafe { libc::waitpid(identity.pid, &mut status, libc::WNOHANG) } == identity.pid;
            reaped
                || matches!(
                    port.signal_verified(&identity, libc::SIGKILL),
                    SignalOutcome::AlreadyGone
                )
        },
        "reaped child",
    );
    assert!(matches!(
        port.inspect(&identity),
        ProcessEvidence::Stopped { .. }
    ));
    assert_eq!(
        port.signal_verified(&identity, libc::SIGKILL),
        SignalOutcome::AlreadyGone
    );
}

#[test]
fn setsid_escapee_is_in_the_snapshot_and_signaled() {
    let root = tmp_root("setsid");
    let port = PosixProcessControl;
    let leader = port
        .spawn(
            &[
                fixture_bin(),
                "setsid-escape".into(),
                "--interval-ms".into(),
                "50".into(),
            ],
            &root,
        )
        .expect("spawn");
    wait_for(
        || root.join("heartbeat.log").exists(),
        "escaped writer heartbeat",
    );
    let snapshot = port.snapshot_descendants(leader.pid).unwrap();
    let escapee = snapshot
        .iter()
        .find(|i| i.pid != leader.pid)
        .expect("snapshot includes the escapee while leader lives");
    // It escaped the leader's group: a group signal cannot reach it.
    assert_ne!(escapee.pgid, leader.pgid);
    // Verified per-identity signaling does.
    assert_eq!(
        port.signal_verified(escapee, libc::SIGKILL),
        SignalOutcome::Sent
    );
    port.signal_verified(&leader, libc::SIGKILL);
    wait_for(
        || matches!(port.inspect(escapee), ProcessEvidence::Stopped { .. }),
        "escapee stopped",
    );
}

#[test]
fn persist_failure_sends_no_signal() {
    let root = tmp_root("nopersist");
    let port = PosixProcessControl;
    let sup = Supervisor::new(
        PosixProcessControl,
        root.clone(),
        vec![fixture_bin()],
        config(),
    );
    let leader = port
        .spawn(&[fixture_bin(), "hang".into()], &root)
        .expect("spawn");
    let invocation = Invocation {
        id: InvocationId("inv-test".into()),
        request_id: "req-test".into(),
        execution_id: ExecutionId("e-test".into()),
        role: InvocationRole::Coding,
        state: InvocationState::Running,
        process_identity: Some(leader.clone()),
        required_artifacts: vec![],
        evidence_refs: vec![],
        descendants: vec![],
    };
    let receipt = sup
        .stop_unless_persisted(&invocation, &leader, |_| {
            Err(workloop_core::store::StoreError::Internal {
                detail: "injected persist failure".into(),
            })
        })
        .unwrap();
    assert!(matches!(receipt, StopReceipt::Unknown { .. }));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        matches!(port.inspect(&leader), ProcessEvidence::Alive { .. }),
        "no signal may be sent when the intent is not durable"
    );
    port.signal_verified(&leader, libc::SIGKILL);
}

// ---- Launch (#9) ----

#[test]
fn crash_after_ready_replays_the_request_exactly_once() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("replay");
    let (_w, eid) = submit_and_admit(&store, "k-launch1");
    let sup = supervisor("hang", root);
    // Two consumption passes — as if the supervisor restarted after the
    // ready commit — must produce one Invocation and one writer.
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    let invocations = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap();
    assert_eq!(invocations.len(), 1, "at most one Invocation per request");
    assert!(invocations[0].process_identity.is_some());
    // Cleanup: cancel and drive the stop to proof.
    let work_id = invocations[0].execution_id.clone();
    let _ = work_id;
    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-launch2");
    for _ in 0..20 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
}

#[test]
fn crash_near_spawn_leaves_uncertain_with_no_second_writer() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("nearspawn");
    let (_w, eid) = submit_and_admit(&store, "k-near1");
    let sup = supervisor("hang", root);
    // Simulate: the request was consumed and an Invocation recorded, then
    // the supervisor died before identity capture — the spawn outcome is
    // simply not provable.
    let request = store
        .transact(|tx| {
            let reqs = tx.pending_requests()?;
            Ok(reqs.into_iter().next().expect("one request"))
        })
        .unwrap();
    store
        .transact(|tx| {
            let inv_id = tx.alloc_id("inv")?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(inv_id),
                    request_id: request.request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Prepared,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })
        .unwrap();
    sup.run_once(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Uncertain);
    assert_eq!(
        store
            .transact(|tx| tx.invocations_for_execution(&eid))
            .unwrap()
            .len(),
        1,
        "no second writer while the start is unproven"
    );
}

#[test]
fn live_process_is_rereported_running_after_a_lost_running_observation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("rerun");
    let (w, eid) = submit_and_admit(&store, "k-rerun1");
    let sup = supervisor("hang", root);
    let request = store
        .transact(|tx| {
            let reqs = tx.pending_requests()?;
            Ok(reqs.into_iter().next().expect("one request"))
        })
        .unwrap();
    // Simulate the crash window: the Invocation's running identity is
    // durable, but the supervisor died before the ProcessState::Running
    // observation committed — the Execution is still Ready.
    let port = PosixProcessControl;
    let workspace = sup.workspace_of(&w);
    std::fs::create_dir_all(&workspace).unwrap();
    let identity = port
        .spawn(&[fixture_bin(), "hang".into()], &workspace)
        .expect("spawn");
    store
        .transact(|tx| {
            let inv_id = tx.alloc_id("inv")?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(inv_id),
                    request_id: request.request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(identity.clone()),
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })
        .unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Ready);
    // The next scan inspects the live writer and must re-report Running.
    sup.run_once(&store, NOW).unwrap();
    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Running,
        "a verified-live writer moves Ready→running even after a lost report"
    );
    // A second pass must not disturb the settled state (the repeated
    // identical report is a store-level duplicate).
    sup.run_once(&store, NOW).unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Running);
    // Cleanup: stop the writer through the proven-stop path.
    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-rerun2");
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
}

#[test]
fn exited_writer_on_a_ready_execution_enters_reconciliation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("readyexit");
    let (w, eid) = submit_and_admit(&store, "k-re1");
    let sup = supervisor("clean-exit", root);
    let request = store
        .transact(|tx| {
            let reqs = tx.pending_requests()?;
            Ok(reqs.into_iter().next().expect("one request"))
        })
        .unwrap();
    // The writer ran to completion and exited before any Running report
    // was recorded — the same lost-report window, but the process is gone
    // by the time the supervisor scans again.
    let port = PosixProcessControl;
    let workspace = sup.workspace_of(&w);
    std::fs::create_dir_all(&workspace).unwrap();
    let identity = port
        .spawn(&[fixture_bin(), "clean-exit".into()], &workspace)
        .expect("spawn");
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "worker exit",
    );
    store
        .transact(|tx| {
            let inv_id = tx.alloc_id("inv")?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(inv_id),
                    request_id: request.request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(identity.clone()),
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })
        .unwrap();
    // The proven exit must not be ignored for a Ready Execution: the
    // Exited and artifact facts drive the normal reconciliation path
    // instead of stranding the Execution until the runtime sweep.
    sup.poll_invocations(&store, NOW).unwrap();
    let exited = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .any(|o| {
            matches!(
                &o.fact,
                ObservationFact::ProcessState {
                    state: ProcessStateFact::Exited,
                    ..
                }
            )
        });
    assert!(exited, "the proven exit is recorded as evidence");
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Waiting);
    assert_eq!(
        e.waiting_reason,
        Some(WaitingReason::HostWrite),
        "artifact evidence queued the publish intent"
    );
    assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 1);
    port.reap(identity.pid);
}

#[test]
fn launch_refused_pauses_and_releases_the_slot() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("refused");
    let (_w, eid) = submit_and_admit(&store, "k-ref1");
    let sup = Supervisor::new(
        PosixProcessControl,
        root,
        vec!["/nonexistent/workloop-worker".to_string()],
        config(),
    );
    sup.run_once(&store, NOW).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Paused);
    assert!(
        e.pause_reason.as_deref().unwrap_or("").contains("repair"),
        "repair pause recorded"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!(slots, 0, "slot released");
    assert_eq!(reservations, 2, "reservation kept while paused");
}

// ---- Stop proof / cancel path ----

#[test]
fn cancel_on_running_reaches_cancelled_only_after_stop_proof() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("cancelrun");
    let (_w, eid) = submit_and_admit(&store, "k-c1");
    let sup = supervisor("hang", root);
    sup.run_once(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-c2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    // Stop was requested, not proven: the slot and the write-resource
    // reservation stay held until the terminal commit.
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!((slots, reservations), (1, 2), "held while stopping");
    // First pass persists the intent and signals; the terminal commit only
    // lands after every recorded identity inspects as stopped.
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
    let intent = store
        .transact(|tx| {
            Ok(tx
                .invocations_for_execution(&eid)?
                .first()
                .map(|i| tx.stop_intent(&i.id))
                .transpose()?
                .flatten())
        })
        .unwrap();
    assert!(intent.is_some(), "stop intent was durable before signals");
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!(
        (slots, reservations),
        (0, 0),
        "slot and reservation release at the terminal commit, not before"
    );
}

// ---- Artifact recovery (#10) ----

#[test]
fn missing_artifact_runs_bounded_recovery_then_fails() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("recovery");
    let (work_id, eid) = submit_and_admit(&store, "k-rec1");
    let sup = supervisor("exit-no-artifact", root.clone());
    // Worker exits without the artifact → confirmed exit, then
    // waiting(artifact_recovery).
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        if e.state == ExecutionState::Waiting
            && e.waiting_reason == Some(WaitingReason::ArtifactRecovery)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Waiting);
    assert_eq!(
        e.waiting_reason,
        Some(WaitingReason::ArtifactRecovery),
        "bounded recovery entered after confirmed exit"
    );
    assert!(e.recovery_deadline_unix_seconds.is_some());
    // Bounded attempts are durable; the sweep fails the Execution at the
    // stored bound with WIP preserved.
    for _ in 0..4 {
        sup.run_once(&store, NOW).unwrap();
        sweep(&store, &config(), NOW).unwrap();
    }
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("artifact_recovery_exhausted")
    );
    assert!(
        root.join(format!("ws-{}", work_id.0)).exists(),
        "WIP preserved"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!(slots, 0);
    assert_eq!(reservations, 0, "slot and reservation released");
    // A retry claims the same workspace and branch.
    let d = handle_command(
        &store,
        &config(),
        &req(
            "k-rec2",
            CommandBody::Retry {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_execution_id: eid.clone(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }));
    let e2 = store
        .transact(|tx| tx.active_execution(&work_id))
        .unwrap()
        .unwrap();
    assert_eq!(e2.state, ExecutionState::Queued);
}

#[test]
fn recovery_artifact_in_the_workspace_root_is_not_truncated() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("recvroot");
    let (work_id, eid) = submit_and_admit(&store, "k-rr1");
    let sup = supervisor("exit-no-artifact", root);
    // A finished writer whose artifact recovery is in flight.
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-rr1".into()),
                    request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::ArtifactRecovery);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    // The artifact reappears at the workspace root itself: promoting it
    // must not copy the file onto itself and report a truncated file.
    let workspace = sup.workspace_of(&work_id);
    workloop_exec::supervisor::write_artifact(&workspace.join("artifact.json"), "payload").unwrap();
    sup.run_recovery(&store, NOW).unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifact.json")).unwrap(),
        "payload",
        "root artifact preserved, not self-truncated"
    );
    let present = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .any(|o| {
            matches!(
                &o.fact,
                ObservationFact::Artifact {
                    state: ArtifactFact::Present { .. },
                    ..
                }
            )
        });
    assert!(present, "verified artifact reported present");
}

#[test]
fn failed_artifact_promotion_is_not_reported_present() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        // Root ignores file modes; the copy-failure path needs EACCES.
        return;
    }
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("recvcopy");
    let (work_id, eid) = submit_and_admit(&store, "k-recopy1");
    let sup = supervisor("exit-no-artifact", root);
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-recopy".into()),
                    request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::ArtifactRecovery);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    // The artifact exists only under .checkpoints and cannot be read:
    // the promotion copy fails, and a failed copy must never be reported
    // as Present evidence.
    let workspace = sup.workspace_of(&work_id);
    let src = workspace.join(".checkpoints");
    std::fs::create_dir_all(&src).unwrap();
    let src_file = src.join("artifact.json");
    workloop_exec::supervisor::write_artifact(&src_file, "payload").unwrap();
    let mut perm = std::fs::metadata(&src_file).unwrap().permissions();
    perm.set_mode(0o000);
    std::fs::set_permissions(&src_file, perm).unwrap();
    sup.run_recovery(&store, NOW).unwrap();
    let present = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .any(|o| {
            matches!(
                &o.fact,
                ObservationFact::Artifact {
                    state: ArtifactFact::Present { .. },
                    ..
                }
            )
        });
    assert!(!present, "a failed promotion copy must not report Present");
    assert!(
        !workspace.join("artifact.json").exists(),
        "no partial artifact appears in the workspace root"
    );
    // Restore permissions so temp cleanup can remove the tree.
    let mut perm = std::fs::metadata(&src_file).unwrap().permissions();
    perm.set_mode(0o644);
    let _ = std::fs::set_permissions(&src_file, perm);
}

// ---- Clean exit drives the publish intent ----

#[test]
fn clean_exit_records_artifact_and_publish_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("clean");
    let (_w, eid) = submit_and_admit(&store, "k-cl1");
    let sup = supervisor("clean-exit", root);
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        if e.state == ExecutionState::Waiting && e.waiting_reason == Some(WaitingReason::HostWrite)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let e = execution(&store, &eid);
    assert_eq!(
        e.waiting_reason,
        Some(WaitingReason::HostWrite),
        "publish intent outstanding"
    );
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    assert_eq!(intents.len(), 1);
}

// ---- Stop settle when the writer is already (or never) resolved ----

#[test]
fn cancel_while_waiting_settles_once_no_writer_remains() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("waitcancel");
    let (w, eid) = submit_and_admit(&store, "k-w1");
    let sup = supervisor("clean-exit", root);
    // The writer exits cleanly: Exited -> Waiting(HostWrite) with the
    // Invocation already Done — no unresolved writer remains.
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        if e.state == ExecutionState::Waiting && e.waiting_reason == Some(WaitingReason::HostWrite)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Waiting);
    let intent = store
        .transact(|tx| tx.action_intents())
        .unwrap()
        .into_iter()
        .next()
        .expect("publish intent recorded");
    let action_id = intent.action_id.clone();
    let HostAction::PushRef {
        branch: intent_branch,
        produced: intent_produced,
    } = &intent.action
    else {
        panic!("the stage effect is a push_ref intent: {intent:?}")
    };
    cancel(&store, &w, "k-w2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    // Stop proof is immediate (no unresolved writer), but the pending
    // publish intent is an outstanding external effect: the terminal
    // cancellation waits for its reconciliation instead of releasing
    // the work under an outbox row the drain would still execute.
    sup.run_once(&store, NOW).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Stopping,
        "terminal settle defers on the outstanding publish intent"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!(
        (slots, reservations),
        (1, 2),
        "slot and writer-resource claims stay held while the effect is outstanding"
    );
    // The outbox reconciles the intent (the adapter published the ref
    // before cancellation was recorded). Only then may the deferred
    // terminal decision land — cancellation does not undo the write.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "test".into(),
                event_id: "host:action-1".into(),
            }),
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::HostActionResult {
                action_id,
                authenticated_actor: None,
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: repo(),
                    branch: intent_branch.clone(),
                    produced: intent_produced.clone(),
                    observed: intent_produced.clone(),
                    relation: RefRelation::Equal,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let e = execution(&store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Cancelled,
        "reconciled effect re-drives the deferred settle"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!((slots, reservations), (0, 0), "terminal release");
}

#[test]
fn cancel_on_ready_settles_without_ever_spawning() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("readycancel");
    let (w, eid) = submit_and_admit(&store, "k-rc1");
    let sup = supervisor("clean-exit", root);
    // Cancel between admission and the consumer seeing the request.
    cancel(&store, &w, "k-rc2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    sup.run_once(&store, NOW).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Cancelled, "stopping settles");
    // The durable request was consumed without ever spawning a writer.
    let invs = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap();
    assert_eq!(invs.len(), 1);
    assert!(
        !invs[0].state.is_unresolved_writer(),
        "invocation resolved: {:?}",
        invs[0].state
    );
    assert!(invs[0].process_identity.is_none(), "no process ever ran");
}

// ---- Store-level: an inserted Unknown observation marks uncertain ----

#[test]
fn unknown_inspection_maps_to_uncertain() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, eid) = submit_and_admit(&store, "k-u1");
    let root = tmp_root("unk");
    let sup = supervisor("hang", root);
    sup.consume_requests(&store, NOW).unwrap();
    let inv = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let obs = Observation {
        delivery: None,
        subject_work_id: Some(w.clone()),
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::ProcessState {
            invocation_id: inv.id.clone(),
            process_identity: String::new(),
            state: ProcessStateFact::Unknown,
        },
        observed_unix_seconds: NOW,
    };
    // Wait for running first so the transition applies.
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    handle_observation(&store, &config(), &obs, NOW).unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Uncertain);
    // Cleanup the real process.
    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-u2");
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state.is_terminal() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn failed_finish_reports_are_replayed_after_restart() {
    use std::cell::Cell;
    use workloop_core::store::{StoreError, TxView};

    struct FailsOnce<'a> {
        store: &'a SqliteStore,
        calls: Cell<usize>,
        fail_at: usize,
    }
    impl Store for FailsOnce<'_> {
        fn transact<R>(
            &self,
            f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
        ) -> Result<R, StoreError> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == self.fail_at {
                return Err(StoreError::Internal {
                    detail: "injected report failure".into(),
                });
            }
            self.store.transact(f)
        }
    }

    for fail_at in [4, 5, 6] {
        let store = SqliteStore::in_memory(1).unwrap();
        let root = tmp_root(&format!("finish-report-{fail_at}"));
        let (work_id, eid) = submit_and_admit(&store, "finish-report");
        let sup = supervisor("clean-exit", root.clone());
        sup.consume_requests(&store, NOW).unwrap();
        let invocation = store
            .transact(|tx| {
                let mut invocation = tx.invocations_for_execution(&eid)?.remove(0);
                invocation.required_artifacts.push(artifact("second.json"));
                tx.apply(&Effect::UpdateInvocation {
                    invocation: invocation.clone(),
                    expected_state: InvocationState::Running,
                })?;
                Ok(invocation)
            })
            .unwrap();
        let identity = invocation.process_identity.unwrap();
        wait_for(
            || {
                matches!(
                    PosixProcessControl.inspect(&identity),
                    ProcessEvidence::Stopped { .. }
                )
            },
            "worker exit before report failure",
        );
        std::fs::write(sup.workspace_of(&work_id).join("second.json"), "{}").unwrap();
        let failing = FailsOnce {
            store: &store,
            calls: Cell::new(0),
            fail_at,
        };
        assert!(sup.poll_invocations(&failing, NOW).is_err());
        assert_eq!(
            store
                .transact(|tx| tx.unresolved_invocations())
                .unwrap()
                .len(),
            1,
            "failure at transaction {fail_at} must retain the writer for evidence replay"
        );
        let restarted = supervisor("clean-exit", root);
        restarted.poll_invocations(&store, NOW + 1).unwrap();
        assert!(
            store
                .transact(|tx| tx.unresolved_invocations())
                .unwrap()
                .is_empty()
        );
        let facts = store
            .transact(|tx| tx.observations_for_execution(&eid))
            .unwrap();
        for required in ["artifact.json", "second.json"] {
            assert!(facts.iter().any(|obs| matches!(&obs.fact,
                ObservationFact::Artifact { required_artifact, state: ArtifactFact::Present { .. }, .. }
                    if required_artifact == required)));
        }
        assert!(facts.iter().any(|obs| matches!(
            &obs.fact,
            ObservationFact::ProcessState {
                state: ProcessStateFact::Exited,
                ..
            }
        )));
        assert_eq!(
            execution(&store, &eid).waiting_reason,
            Some(WaitingReason::HostWrite)
        );
        assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 1);
    }
}

#[test]
fn stale_or_unverified_leader_never_authorizes_a_group_signal() {
    use std::cell::Cell;
    use std::path::Path;
    use workloop_core::ports::SpawnError;

    struct UnverifiedLeader {
        identity: ProcessIdentity,
        condition: &'static str,
    }
    impl ProcessControlPort for UnverifiedLeader {
        fn spawn(&self, _: &[String], _: &Path) -> Result<ProcessIdentity, SpawnError> {
            unreachable!()
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            let mut identity = self.identity.clone();
            if self.condition == "reused" {
                identity.started_at = "replacement process".into();
            }
            Ok(vec![identity])
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
            if self.condition == "unknown" {
                ProcessEvidence::Unknown {
                    reason: "lookup failed".into(),
                }
            } else {
                ProcessEvidence::Stopped {
                    identity: identity.clone(),
                    exit_code: None,
                }
            }
        }
        fn signal_verified(&self, identity: &ProcessIdentity, _: i32) -> SignalOutcome {
            assert_eq!(
                identity, &self.identity,
                "a replacement must never be targeted"
            );
            SignalOutcome::AlreadyGone
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            panic!("unverified group must not be signaled")
        }
        fn reap(&self, _: i32) {}
    }

    for condition in ["reused", "stopped", "unknown"] {
        let identity = ProcessIdentity {
            pid: 42,
            started_at: "original".into(),
            pgid: Some(42),
        };
        let sup = Supervisor::new(
            UnverifiedLeader {
                identity: identity.clone(),
                condition,
            },
            PathBuf::from("/unused"),
            vec![],
            config(),
        );
        let invocation = Invocation {
            id: InvocationId("inv-stale".into()),
            request_id: "req-stale".into(),
            execution_id: ExecutionId("e-stale".into()),
            role: InvocationRole::Coding,
            state: InvocationState::Running,
            process_identity: Some(identity.clone()),
            required_artifacts: vec![],
            evidence_refs: vec![],
            descendants: vec![],
        };
        let persisted = Cell::new(false);
        let receipt = sup
            .stop_unless_persisted(&invocation, &identity, |_| {
                persisted.set(true);
                Ok(())
            })
            .unwrap();
        if condition == "reused" {
            assert!(matches!(receipt, StopReceipt::Unknown { .. }));
            assert!(
                !persisted.get(),
                "replacement descendants are not our stop snapshot"
            );
        } else {
            assert!(persisted.get());
        }
    }
}

#[test]
fn stop_receipts_distinguish_delivered_signals_from_stop_proof() {
    use std::path::Path;
    use workloop_core::ports::SpawnError;

    struct SignalPort(SignalOutcome);
    impl ProcessControlPort for SignalPort {
        fn spawn(&self, _: &[String], _: &Path) -> Result<ProcessIdentity, SpawnError> {
            unreachable!()
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
            ProcessEvidence::Alive {
                identity: identity.clone(),
            }
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            self.0.clone()
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            self.0.clone()
        }
        fn reap(&self, _: i32) {}
    }

    for (signal, expected) in [
        (SignalOutcome::Sent, StopReceipt::Requested),
        (SignalOutcome::AlreadyGone, StopReceipt::AlreadyStopped),
        (SignalOutcome::SkippedStale, StopReceipt::Requested),
        (
            SignalOutcome::Failed {
                reason: "permission denied".into(),
            },
            StopReceipt::Unknown {
                reason: "permission denied".into(),
            },
        ),
    ] {
        let store = SqliteStore::in_memory(1).unwrap();
        let (_, eid) = submit_and_admit(&store, "stop-receipt");
        let identity = ProcessIdentity {
            pid: 42,
            started_at: "original".into(),
            pgid: Some(42),
        };
        let invocation = Invocation {
            id: InvocationId("inv-receipt".into()),
            request_id: format!("{}:inv:0", eid.0),
            execution_id: eid,
            role: InvocationRole::Coding,
            state: InvocationState::Running,
            process_identity: Some(identity.clone()),
            required_artifacts: vec![],
            evidence_refs: vec![],
            descendants: vec![],
        };
        let sup = Supervisor::new(
            SignalPort(signal),
            PathBuf::from("/unused"),
            vec![],
            config(),
        );
        let first = sup.request_stop(&store, &invocation, &identity).unwrap();
        assert_eq!(first, expected);
        // The durable intent recorded above is what `drive_stops`
        // resumes from after a restart.
        assert!(
            store
                .transact(|tx| tx.stop_intent(&invocation.id))
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn repeated_liveness_transitions_recover_without_poll_observation_growth() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit_and_admit(&store, "liveness-transitions");
    let root = tmp_root("liveness-transitions");
    let sup = supervisor("hang", root.clone());
    sup.consume_requests(&store, NOW).unwrap();
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let mut recovered = vec![];
    let mut unchanged = vec![];
    for offset in 1..=2 {
        handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: Some(eid.clone()),
                fact: ObservationFact::ProcessState {
                    invocation_id: invocation.id.clone(),
                    process_identity: "probe unavailable".into(),
                    state: ProcessStateFact::Unknown,
                },
                observed_unix_seconds: NOW + offset,
            },
            NOW + offset,
        )
        .unwrap();
        assert_eq!(execution(&store, &eid).state, ExecutionState::Uncertain);
        let restarted = supervisor("hang", root.clone());
        restarted.poll_invocations(&store, NOW + offset).unwrap();
        recovered.push(execution(&store, &eid).state);
        let before = store
            .transact(|tx| tx.observations_for_execution(&eid))
            .unwrap()
            .len();
        restarted
            .poll_invocations(&store, NOW + offset + 10)
            .unwrap();
        let next_restart = supervisor("hang", root.clone());
        next_restart
            .poll_invocations(&store, NOW + offset + 20)
            .unwrap();
        let after = store
            .transact(|tx| tx.observations_for_execution(&eid))
            .unwrap()
            .len();
        unchanged.push((before, after));
    }
    cancel(&store, &work_id, "liveness-cancel");
    for _ in 0..40 {
        sup.run_once(&store, NOW + 100).unwrap();
        if execution(&store, &eid).state.is_terminal() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        recovered,
        vec![ExecutionState::Running, ExecutionState::Running]
    );
    assert!(
        unchanged.iter().all(|(before, after)| before == after),
        "{unchanged:?}"
    );
}

// ---- Stop proof (#16) ----

/// The durable stop intent a crashed supervisor would have left behind,
/// recorded through the same descendant snapshot and store commit
/// `request_stop` runs — split at the persistence boundary so the test
/// can hold the `kill -9` window open before any signal is sent.
fn persist_stop_intent(
    store: &SqliteStore,
    sup: &Supervisor<PosixProcessControl>,
    invocation: &Invocation,
    leader: &ProcessIdentity,
) -> StopIntent {
    match sup.record_stop_intent(store, invocation, leader).unwrap() {
        StopRecord::Recorded(intent) => intent,
        StopRecord::Unknown { reason } => panic!("stop intent not persisted: {reason}"),
    }
}

fn held_claims(store: &SqliteStore) -> (usize, usize) {
    store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap()
}

/// The ordering invariant, positive direction: while the persistence
/// boundary is still open the recorded leader is untouched — the first
/// signal can only be delivered after the intent is durable.
/// `persist_failure_sends_no_signal` covers the negative direction.
#[test]
fn stop_intent_is_durable_before_the_first_signal() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("intent-order");
    let port = PosixProcessControl;
    let sup = Supervisor::new(
        PosixProcessControl,
        root.clone(),
        vec![fixture_bin()],
        config(),
    );
    let leader = port
        .spawn(&[fixture_bin(), "hang".into()], &root)
        .expect("spawn");
    let invocation = Invocation {
        id: InvocationId("inv-order".into()),
        request_id: "req-order".into(),
        execution_id: ExecutionId("e-order".into()),
        role: InvocationRole::Coding,
        state: InvocationState::Running,
        process_identity: Some(leader.clone()),
        required_artifacts: vec![],
        evidence_refs: vec![],
        descendants: vec![],
    };
    let receipt = sup
        .stop_unless_persisted(&invocation, &leader, |mut intent| {
            assert!(
                !intent.identities.is_empty(),
                "the descendant snapshot is recorded with the intent"
            );
            match port.inspect(&leader) {
                // The boundary is open and no signal has been delivered.
                ProcessEvidence::Alive { .. } => {}
                other => panic!("a signal preceded the durable intent: {other:?}"),
            }
            // The durable write `request_stop` commits, not a stub.
            store.transact(|tx| {
                intent.recorded_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertStopIntent { intent })
            })
        })
        .unwrap();
    assert!(matches!(receipt, StopReceipt::Requested));
    // The row `drive_stops` resumes from is durable before the first
    // signal lands.
    let recorded = store
        .transact(|tx| tx.stop_intent(&invocation.id))
        .unwrap()
        .expect("the stop intent is durable in the store");
    assert!(
        recorded.identities.iter().any(|i| i.pid == leader.pid),
        "the recorded snapshot names the leader"
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "leader stopped after the intent was durable",
    );
    port.reap(leader.pid);
}

/// A `setsid`-escaped child survives the process-group signal; only the
/// descendant snapshot recorded with the stop intent names it, and the
/// Execution cannot reach `cancelled` until that identity is inspected.
#[test]
fn setsid_escapee_survives_the_group_signal_but_blocks_the_terminal_commit() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("setsid-stop");
    let (w, eid) = submit_and_admit(&store, "k-se1");
    let argv = vec![
        fixture_bin(),
        "setsid-escape".into(),
        // The leader lives ~20s so the stop intent's snapshot always finds
        // the escapee still parented under it.
        "--interval-ms".into(),
        "1000".into(),
    ];
    let sup = Supervisor::new(PosixProcessControl, root.clone(), argv.clone(), config());
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    wait_for(
        || sup.workspace_of(&w).join("heartbeat.log").exists(),
        "escapee heartbeat",
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let leader = invocation.process_identity.clone().unwrap();
    let port = PosixProcessControl;

    cancel(&store, &w, "k-se2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    assert_eq!(held_claims(&store), (1, 2));

    // The intent is durable before any signal — the same snapshot and
    // store commit `request_stop` runs, held open at the boundary so the
    // test can deliver the group signal itself. The production snapshot
    // names the escapee: it escaped the leader's process group, so only
    // this record covers it.
    let intent = persist_stop_intent(&store, &sup, &invocation, &leader);
    assert!(
        store
            .transact(|tx| tx.stop_intent(&invocation.id))
            .unwrap()
            .is_some(),
        "the stop intent is durable before the first signal"
    );
    let escapee = intent
        .identities
        .iter()
        .find(|i| i.pid != leader.pid)
        .expect("the snapshot names the setsid escapee")
        .clone();
    assert_ne!(
        escapee.pgid, leader.pgid,
        "the escapee left the leader's process group"
    );

    // A process-group signal alone kills the leader but not the escapee.
    // The group signal is authorized only while the recorded leader
    // verifies alive — the same gate `stop_unless_persisted` applies.
    assert!(matches!(
        port.inspect(&leader),
        ProcessEvidence::Alive { ref identity } if *identity == leader
    ));
    assert_eq!(
        port.signal_group(leader.pgid.expect("leader pgid"), libc::SIGKILL),
        SignalOutcome::Sent
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "leader stopped by the group signal",
    );
    assert!(
        matches!(port.inspect(&escapee), ProcessEvidence::Alive { .. }),
        "a setsid-escaped child survives a process-group signal"
    );

    // While the escapee is unproven, cancellation cannot settle — the
    // group-signal evidence covers the leader only. A supervisor restart
    // (kill -9 between intent and proof) resumes from the durable intent.
    drop(sup);
    let restarted = Supervisor::new(PosixProcessControl, root, argv, config());
    restarted.drive_stops(&store, NOW).unwrap();
    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Stopping,
        "the uninspected escapee keeps the Execution from settling"
    );
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "nothing releases while a recorded identity is unproven"
    );

    // The resumed pass signalled the escapee under re-verification; once
    // every recorded identity inspects stopped, the terminal commit lands.
    wait_for(
        || matches!(port.inspect(&escapee), ProcessEvidence::Stopped { .. }),
        "escapee stopped via the recorded identity",
    );
    for _ in 0..40 {
        restarted.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
    for identity in &intent.identities {
        assert!(
            matches!(port.inspect(identity), ProcessEvidence::Stopped { .. }),
            "every recorded identity inspected stopped at the terminal commit: {identity:?}"
        );
    }
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// A supervisor `kill -9` mid-stop — after the stop intent is persisted
/// and the first recorded identity is signaled, but before the rest are
/// processed: the restarted supervisor resumes signaling and inspection
/// from the durable intent alone — never re-deriving the tree and never
/// spawning a second writer.
#[test]
fn kill9_after_intent_persisted_resumes_from_the_durable_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("kill9-resume");
    let (w, eid) = submit_and_admit(&store, "k-k91");
    let argv = vec![
        fixture_bin(),
        "setsid-escape".into(),
        // The leader lives ~20s so the stop intent's snapshot always
        // catches the escapee.
        "--interval-ms".into(),
        "1000".into(),
    ];
    let sup = Supervisor::new(PosixProcessControl, root.clone(), argv.clone(), config());
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    wait_for(
        || sup.workspace_of(&w).join("heartbeat.log").exists(),
        "escapee heartbeat",
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let leader = invocation.process_identity.clone().unwrap();
    let port = PosixProcessControl;

    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-k92");
    // Mid-stop crash: the intent — with the full descendant snapshot the
    // production persist path records — is durable and the leader has
    // already been signaled when the supervisor dies; the escapee is
    // still unprocessed.
    let intent = persist_stop_intent(&store, &sup, &invocation, &leader);
    let escapee = intent
        .identities
        .iter()
        .find(|i| i.pid != leader.pid)
        .expect("the snapshot names the setsid escapee")
        .clone();
    assert_eq!(
        port.signal_verified(&leader, libc::SIGKILL),
        SignalOutcome::Sent,
        "the crashed supervisor had already signaled the first identity"
    );
    drop(sup);
    assert!(
        matches!(port.inspect(&escapee), ProcessEvidence::Alive { .. }),
        "killed mid-stop: the escapee is still unprocessed"
    );

    // The restarted supervisor holds no memory of the stop; the stop
    // pass must resume signaling and inspection from the durable intent
    // alone — never re-deriving the tree and never spawning a writer.
    let restarted = Supervisor::new(PosixProcessControl, root, argv, config());
    restarted.drive_stops(&store, NOW).unwrap();
    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Stopping,
        "the unproven escapee keeps the Execution from settling"
    );
    wait_for(
        || matches!(port.inspect(&escapee), ProcessEvidence::Stopped { .. }),
        "escapee stopped via the recorded identity",
    );
    for _ in 0..40 {
        restarted.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
    assert_eq!(
        store
            .transact(|tx| tx.invocations_for_execution(&eid))
            .unwrap()
            .len(),
        1,
        "no second writer while the recorded stop was unproven"
    );
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// An unreaped zombie counts as stopped, and the supervisor reaps the
/// children it spawned: after the terminal commit the leader's pid has
/// left the process table entirely rather than lingering as a zombie of
/// the supervisor.
#[test]
fn the_supervisor_reaps_the_children_it_spawned() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("reap-stop");
    let (_w, eid) = submit_and_admit(&store, "k-reap1");
    let sup = supervisor("zombie", root);
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let leader = invocation.process_identity.clone().unwrap();
    let port = PosixProcessControl;
    // The fixture holds an unreaped child; wait until it is recorded.
    wait_for(
        || {
            port.snapshot_descendants(leader.pid)
                .unwrap()
                .iter()
                .any(|i| i.pid != leader.pid)
        },
        "zombie child to appear",
    );

    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-reap2");
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);

    // The leader was our child: the supervisor reaped it, so its pid left
    // the process table instead of lingering as a supervisor-side zombie.
    // kill(pid, 0) still succeeds on a zombie — only the reap turns it
    // into ESRCH.
    wait_for(
        || {
            let rc = unsafe { libc::kill(leader.pid, 0) };
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        },
        "reaped leader left the process table",
    );
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// A worker's own `done` claim is evidence to assess, not a release
/// event: while no stop is in flight it yields neither a terminal state
/// nor any capacity or reservation release — it resolves the writer
/// record only, even while the process still runs. A worker never emits
/// a `ProcessState` fact itself; `Exited` is the supervisor's verified
/// report, and during a stop that report is the recorded proof the stop
/// settles on (see `an_exited_report_settles_a_requested_stop`). A
/// cancel on top of the claim still drives the recorded identity to
/// stop proof before the claims release — the resolved record alone is
/// never the proof.
#[test]
fn a_workers_done_claim_alone_releases_nothing() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("done-claim");
    let (w, eid) = submit_and_admit(&store, "k-dc1");
    let sup = supervisor("hang", root);
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let leader = invocation.process_identity.clone().unwrap();

    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "test".into(),
                event_id: "worker:done-claim".into(),
            }),
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::ProcessState {
                invocation_id: invocation.id.clone(),
                process_identity: format!("{}@{}", leader.pid, leader.started_at),
                state: ProcessStateFact::Exited,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let e = execution(&store, &eid);
    assert!(
        !e.state.is_terminal(),
        "a done claim alone is never a terminal state: {:?}",
        e.state
    );
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "a done claim alone releases no slot and no reservation"
    );
    assert_eq!(
        store
            .transact(|tx| tx.invocations_for_execution(&eid))
            .unwrap()
            .remove(0)
            .state,
        InvocationState::Done,
        "the claim resolves the writer record only — not the release"
    );
    // The gap the claim must not open: the resolved record is not stop
    // proof while the process is live, so a cancel still drives the
    // recorded identity to a real stop before the claims release.
    let port = PosixProcessControl;
    assert!(matches!(
        port.inspect(&leader),
        ProcessEvidence::Alive { .. }
    ));
    cancel(&store, &w, "k-dc2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
    assert!(
        matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "the live writer was signaled and proven before the release"
    );
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// `work`'s half of the stop-settle: during `stopping`, the reported
/// `Exited` is the recorded proof that the writer is gone — the
/// Execution settles to the requested outcome and releases the slot and
/// the reservation. The other half — the supervisor emits `Exited` only
/// after inspecting every recorded identity — is the port contract the
/// setsid/zombie/kill-9 tests pin; here the writer is actually stopped
/// and reaped first so the report models the verified path.
#[test]
fn an_exited_report_settles_a_requested_stop() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("exited-settle");
    let (w, eid) = submit_and_admit(&store, "k-es1");
    let sup = supervisor("hang", root);
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let leader = invocation.process_identity.clone().unwrap();
    let port = PosixProcessControl;

    cancel(&store, &w, "k-es2");
    let e = execution(&store, &eid);
    assert_eq!(e.state, ExecutionState::Stopping);
    assert_eq!(e.desired_action, Some(DesiredAction::Cancel));
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "the stop request alone releases nothing"
    );

    // The supervisor's half of the proof — signal, inspect, reap — runs
    // before `Exited` is ever reported.
    assert_eq!(
        port.signal_verified(&leader, libc::SIGKILL),
        SignalOutcome::Sent
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "writer stopped",
    );
    port.reap(leader.pid);
    drop(sup);

    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "test".into(),
                event_id: "sup:exited-settle".into(),
            }),
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::ProcessState {
                invocation_id: invocation.id.clone(),
                process_identity: format!("{}@{}", leader.pid, leader.started_at),
                state: ProcessStateFact::Exited,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();

    let e = execution(&store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Cancelled,
        "the verified exit report settles the requested cancel"
    );
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// A missing PID alone releases nothing: an unresolved writer with no
/// recorded identity cannot be signaled or proven, so the stop drives the
/// Execution to `uncertain` and the slot and reservation stay held.
#[test]
fn a_missing_pid_keeps_the_execution_uncertain_and_releases_nothing() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("missing-pid");
    let (w, eid) = submit_and_admit(&store, "k-mp1");
    let sup = supervisor("hang", root);
    // A consumed request whose invocation was recorded Prepared but never
    // gained a process identity — the missing-PID case.
    let request = store
        .transact(|tx| {
            let reqs = tx.pending_requests()?;
            Ok(reqs.into_iter().next().expect("one request"))
        })
        .unwrap();
    store
        .transact(|tx| {
            let inv_id = tx.alloc_id("inv")?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(inv_id),
                    request_id: request.request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Prepared,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })
        .unwrap();
    cancel(&store, &w, "k-mp2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    for _ in 0..3 {
        sup.drive_stops(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        assert_eq!(
            e.state,
            ExecutionState::Uncertain,
            "an unidentifiable writer keeps the stop unproven"
        );
        assert_eq!(e.desired_action, Some(DesiredAction::Cancel));
        assert_eq!(
            held_claims(&store),
            (1, 2),
            "a missing PID releases no slot and no reservation"
        );
    }
}

/// Heartbeat/deadline expiry releases nothing by itself: the runtime
/// bound expiring only requests a stop (desired `fail`); the slot and the
/// reservation stay held until the writer is proven stopped.
#[test]
fn a_runtime_deadline_expiry_requests_stop_and_releases_nothing() {
    let mut cfg = config();
    cfg.budget.runtime_limit_seconds = 60;
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("expiry-stop");
    let (_w, eid) = submit_and_admit_with(&store, &cfg, "k-exp1");
    let sup = Supervisor::new(
        PosixProcessControl,
        root,
        vec![fixture_bin(), "hang".into()],
        cfg.clone(),
    );
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );

    sweep(&store, &cfg, NOW + 61).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Stopping,
        "expiry requests a stop; it is not a release"
    );
    assert_eq!(e.desired_action, Some(DesiredAction::Fail));
    assert!(!e.state.is_terminal());
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "expiry alone releases no slot and no reservation"
    );
    // Stop proof completes the requested failure and only then releases.
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Failed {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Failed);
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}
