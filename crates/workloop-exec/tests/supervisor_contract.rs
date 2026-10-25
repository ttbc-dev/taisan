//! Contract tests for the supervisor ProcessControlPort and the launch /
//! stop / recovery paths over real processes (`docs/v0.1.md`: Launch,
//! Stop proof, Recovery rows).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::{
    ActionResult, ArtifactFact, Observation, ObservationFact, ProcessStateFact,
};
use workloop_core::ports::{ProcessControlPort, ProcessEvidence, SignalOutcome, StopReceipt};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{
    WorkloopConfig, admit_queued, handle_command, handle_observation, sweep,
};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

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
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: false,
            network: false,
        },
        budget: ExecutionBudget {
            artifact_recovery_attempts: 3,
            artifact_recovery_seconds: 60,
            ..ExecutionBudget::default()
        },
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
    let d = handle_command(store, &config(), &submit(key), NOW).unwrap();
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
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW))
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
                .iter()
                .any(|i| i.pid != leader.pid)
        },
        "zombie child to appear",
    );
    let descendants = port.snapshot_descendants(leader.pid);
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
    let snapshot = port.snapshot_descendants(leader.pid);
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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
        Some(WaitingReason::GithubWrite),
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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
                    state: ArtifactFact::Present,
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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
                    state: ArtifactFact::Present,
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
        if e.state == ExecutionState::Waiting
            && e.waiting_reason == Some(WaitingReason::GithubWrite)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let e = execution(&store, &eid);
    assert_eq!(
        e.waiting_reason,
        Some(WaitingReason::GithubWrite),
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
    // The writer exits cleanly: Exited -> Waiting(GithubWrite) with the
    // Invocation already Done — no unresolved writer remains.
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        if e.state == ExecutionState::Waiting
            && e.waiting_reason == Some(WaitingReason::GithubWrite)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Waiting);
    let action_id = store
        .transact(|tx| tx.action_intents())
        .unwrap()
        .into_iter()
        .next()
        .expect("publish intent recorded")
        .action_id;
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
    // The outbox reconciles the intent (the fixture adapter opened the
    // PR before cancellation was recorded). Only then may the deferred
    // terminal decision land — cancellation does not undo the PR.
    handle_observation(
        &store,
        &config(),
        &Observation {
            source_event_id: Some("gh:pr-action-1".into()),
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::PrActionResult {
                action_id,
                action_kind: "publish_pr".into(),
                result: ActionResult::Applied,
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
        source_event_id: None,
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
