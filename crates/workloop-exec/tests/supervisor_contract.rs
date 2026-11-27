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
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, SignalOutcome, StopReceipt, WorkerEnv,
};
use workloop_core::projection::{self, WriterState};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
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
        placement: workloop_core::placement::PlacementPolicy::default(),
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
                credential_ref: CredentialRef::trusted("cred-publication"),
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

/// Contract launches are fixture-profile evidence: the reference
/// profile's bwrap boundary is proven by the containment suite, not
/// by these process-lifecycle tests.
fn fixture_sup<P: ProcessControlPort>(
    port: P,
    root: PathBuf,
    argv: Vec<String>,
    cfg: WorkloopConfig,
) -> Supervisor<P> {
    Supervisor::new(port, root, argv, cfg).with_isolation_profile(
        workloop_core::worker::isolation_profile(
            workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
        )
        .unwrap(),
    )
}

fn supervisor(mode: &str, root: PathBuf) -> Supervisor<PosixProcessControl> {
    fixture_sup(
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
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
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
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
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
        .spawn(
            &[fixture_bin(), "zombie".into()],
            &root,
            &WorkerEnv::default(),
        )
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
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
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

/// Signature enumeration is read-only: a zombie match is skipped, but
/// the scan must never reap one — the exit status belongs to the
/// invocation that owns the pid, and the owning path's `reap` still
/// reads it after a scan.
#[test]
fn signature_scan_never_reaps_a_zombie_match() {
    let root = tmp_root("scan-zombie");
    let port = PosixProcessControl;
    let signature = format!("{} clean-exit", fixture_bin());
    let identity = port
        .spawn(
            &[fixture_bin(), "clean-exit".into()],
            &root,
            &WorkerEnv::default(),
        )
        .expect("spawn");
    // The writer exits and stays an unreaped zombie of this test
    // process — `inspect` proves the stop without consuming it.
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "writer exited",
    );
    // A full signature scan runs over the live table; whether or not
    // the platform still shows the zombie's argv, the scan is
    // enumeration only — it cannot consume the exit status.
    let matches = port.matching_processes(&signature).expect("scan");
    assert!(
        !matches.iter().any(|m| m.identity.pid == identity.pid),
        "a zombie is never a live signature match"
    );
    assert_eq!(
        port.reap(identity.pid),
        Some(0),
        "the scan left the zombie's exit status for its owner"
    );
}

/// The directory scan is the absence proof's last handle: it finds a
/// process by working directory alone — the shape an escaped
/// descendant takes after leaving the launch argv signature.
#[test]
fn directory_scan_finds_an_occupant_by_cwd_alone() {
    let root = tmp_root("dir-scan");
    let port = PosixProcessControl;
    let identity = port
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
        .expect("spawn");
    let occupants = port.processes_in_directory(&root).expect("scan");
    assert!(
        occupants.iter().any(|m| m.identity.pid == identity.pid),
        "a live process inside the directory is reported by cwd alone"
    );
    // The scan is scoped to the directory: the same process is not
    // an occupant of an unrelated empty directory.
    let other = tmp_root("dir-scan-other");
    assert!(
        !port
            .processes_in_directory(&other)
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == identity.pid),
        "the scan is scoped to the directory"
    );
    port.signal_verified(&identity, libc::SIGKILL);
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "stopped after signal",
    );
}

/// A live writer whose working directory is deleted still reports a
/// cwd inside the scanned tree — `/proc/<pid>/cwd` (and Linux lsof)
/// mark the spelling ` (deleted)`; other mechanisms report the plain
/// unresolvable path. Either way the pid is already mapped, so the
/// scan must never read back clear: the genuine query errs or counts
/// the pid, the namespace query reports it.
#[test]
fn directory_scan_never_clears_a_deleted_cwd_occupant() {
    let port = PosixProcessControl;
    let root = std::fs::canonicalize(tmp_root("dir-scan-deleted")).unwrap();
    let gone = root.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    // `path-reopen` rewrites its heartbeat on an interval and ignores
    // the failure once the directory is gone — a live process holding
    // a deleted cwd. The first beat proves the eager startup cwd read
    // ran, so the deletion never kills the writer.
    let identity = port
        .spawn(
            &[
                fixture_bin(),
                "path-reopen".into(),
                "--interval-ms".into(),
                "50".into(),
            ],
            &gone,
            &WorkerEnv::default(),
        )
        .expect("spawn");
    wait_for(
        || gone.join("heartbeat.log").exists(),
        "writer heartbeat before cwd deletion",
    );
    std::fs::remove_dir_all(&gone).unwrap();
    // The genuine query never returns a table missing the pid —
    // `Err` fails the read closed; `Ok` must carry the occupant.
    for dir in [&root, &gone] {
        match port.processes_in_directory(dir) {
            Err(_) => {}
            Ok(occupants) => assert!(
                occupants.iter().any(|m| m.identity.pid == identity.pid),
                "a deleted-cwd occupant is never a silent skip into a clear table"
            ),
        }
        assert!(
            port.processes_in_namespace_dir(dir)
                .expect("scan")
                .iter()
                .any(|m| m.identity.pid == identity.pid),
            "the namespace query reports the deleted-cwd occupant"
        );
    }
    port.signal_verified(&identity, libc::SIGKILL);
    wait_for(
        || matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "stopped after signal",
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
            &WorkerEnv::default(),
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
    let sup = fixture_sup(
        PosixProcessControl,
        root.clone(),
        vec![fixture_bin()],
        config(),
    );
    let leader = port
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
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
fn scoped_supervision_launches_only_the_scoped_works_request() {
    let store = SqliteStore::in_memory(2).unwrap();
    let root = tmp_root("scoped");
    let (w1, e1) = submit_and_admit(&store, "k-scope1");
    let (w2, e2) = submit_and_admit(&store, "k-scope2");
    let sup = supervisor("hang", root);

    // A `--work` pass consumes only the named Work's durable request.
    sup.consume_requests_scoped(&store, NOW, Some(&w1), &mut || NOW, None)
        .unwrap();
    let inv1 = store
        .transact(|tx| tx.invocations_for_execution(&e1))
        .unwrap();
    let inv2 = store
        .transact(|tx| tx.invocations_for_execution(&e2))
        .unwrap();
    assert_eq!(inv1.len(), 1, "the scoped Work launched its writer");
    assert!(
        inv2.is_empty(),
        "a scoped pass never launches another Work's writer"
    );

    // Cleanup: settle both Executions with stop proof.
    cancel(&store, &w1, "k-scope3");
    cancel(&store, &w2, "k-scope4");
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &e1).state == ExecutionState::Cancelled
            && execution(&store, &e2).state == ExecutionState::Cancelled
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &e1).state, ExecutionState::Cancelled);
    assert_eq!(execution(&store, &e2).state, ExecutionState::Cancelled);
}

#[test]
fn a_tick_deadline_stops_supervision_before_starting_new_work() {
    let store = SqliteStore::in_memory(2).unwrap();
    let root = tmp_root("deadline");
    let (_w1, e1) = submit_and_admit(&store, "k-d1");
    let (_w2, e2) = submit_and_admit(&store, "k-d2");
    let sup = supervisor("hang", root);

    // Deadline already reached when the pass starts: nothing is
    // launched, polled, signalled or recovered, and the pass reports
    // that the deadline stopped it.
    let cut = sup
        .run_once_scoped(&store, NOW, None, &mut || NOW, Some(NOW))
        .unwrap();
    assert!(cut, "the pass reports the deadline stopped it");
    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        2,
        "no request was started"
    );
    assert!(
        store
            .transact(|tx| tx.invocations_for_execution(&e1))
            .unwrap()
            .is_empty()
    );

    // Deadline passes between two requests: the item loop checks before
    // every item, so the first request is consumed and the second never
    // starts. The first Execution is moved out of `Ready` first so its
    // consumption abandons instead of spawning — this test starts no
    // real writer.
    let mut first = execution(&store, &e1);
    first.state = ExecutionState::Stopping;
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateExecution {
                execution: first.clone(),
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let mut calls = 0u32;
    sup.consume_requests_scoped(
        &store,
        NOW,
        None,
        &mut || {
            calls += 1;
            if calls == 1 { NOW } else { NOW + 60 }
        },
        Some(NOW + 30),
    )
    .unwrap();
    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        1,
        "the request behind the deadline never started"
    );
    assert_eq!(
        store
            .transact(|tx| tx.invocations_for_execution(&e1))
            .unwrap()
            .len(),
        1,
        "the request before the deadline was consumed"
    );
    assert!(
        store
            .transact(|tx| tx.invocations_for_execution(&e2))
            .unwrap()
            .is_empty()
    );
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
        .spawn(
            &[fixture_bin(), "hang".into()],
            &workspace,
            &WorkerEnv::default(),
        )
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
        .spawn(
            &[fixture_bin(), "clean-exit".into()],
            &workspace,
            &WorkerEnv::default(),
        )
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
    // Reconciliation resolves straight into the next step: the
    // invariant lane is enabled, so the publish gate re-requests the
    // pre-publication check under the same Execution and reservation
    // (`waiting → ready`) before any host-write intent exists.
    assert_eq!(e.state, ExecutionState::Ready);
    let pending = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(pending.len(), 1, "exactly the check request is pending");
    assert_eq!(pending[0].role, InvocationRole::InvariantCheck);
    assert!(
        store.transact(|tx| tx.action_intents()).unwrap().is_empty(),
        "publication waits for the check's own Invocation evidence"
    );
    let _ = port.reap(identity.pid);
}

#[test]
fn launch_refused_pauses_and_releases_the_slot() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("refused");
    let (_w, eid) = submit_and_admit(&store, "k-ref1");
    let sup = fixture_sup(
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

/// A reparented escapee stays a recorded identity: refreshing the live
/// ppid snapshot must union with the durable descendant set, never
/// replace it — leaving the tree is not proof of stopping.
#[test]
fn refresh_unions_recorded_escapees_into_the_live_snapshot() {
    use std::path::Path;
    use workloop_core::ports::{SpawnError, WorkerEnv};

    struct LiveTree(Vec<ProcessIdentity>);
    impl ProcessControlPort for LiveTree {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn in the poll path")
        }
        // The escapee is absent: it reparented out of the leader's ppid
        // tree, so the live snapshot can no longer see it.
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(self.0.clone())
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, id: &ProcessIdentity) -> ProcessEvidence {
            ProcessEvidence::Alive {
                identity: id.clone(),
            }
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
    }

    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("refreshunion");
    let (_w, eid) = submit_and_admit(&store, "k-ru1");
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    let leader = ProcessIdentity {
        pid: 9_800_001,
        started_at: "leader start".into(),
        pgid: Some(9_800_001),
    };
    let live_child = ProcessIdentity {
        pid: 9_800_002,
        started_at: "child start".into(),
        pgid: Some(9_800_001),
    };
    let escapee = ProcessIdentity {
        pid: 9_800_003,
        started_at: "escapee start".into(),
        pgid: None,
    };
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-ru1".into()),
                    request_id: request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(leader.clone()),
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![escapee.clone()],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request_id.clone(),
            })
        })
        .unwrap();
    let sup = fixture_sup(LiveTree(vec![live_child.clone()]), root, vec![], config());
    sup.run_once(&store, NOW).unwrap();
    let descendants = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .into_iter()
        .next()
        .expect("invocation")
        .descendants;
    let pids: BTreeSet<i32> = descendants.iter().map(|i| i.pid).collect();
    assert_eq!(
        pids,
        BTreeSet::from([live_child.pid, escapee.pid]),
        "refresh unions the live tree with recorded escapees, never replacing them"
    );
}

/// The snapshot persisted by `request_stop` in the same pass must be part
/// of the stop proof. The escapee here is visible only to that fresh
/// snapshot — it `setsid`'d out of the group and the recorded descendant
/// list predates its spawn — so settling on the pre-stop identity set
/// would release capacity under a live writer.
#[test]
fn fresh_stop_snapshot_is_proven_before_settling() {
    use std::path::Path;
    use std::sync::Mutex;
    use workloop_core::ports::{SpawnError, WorkerEnv};

    struct FreshSnapshotEscapee {
        escapee: ProcessIdentity,
        inspected: Mutex<BTreeSet<i32>>,
    }
    impl ProcessControlPort for FreshSnapshotEscapee {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn in the stop path")
        }
        // The escapee is still a live child at stop time: the stop
        // intent's snapshot records it.
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![self.escapee.clone()])
        }
        // …but it left the process group, so enumeration cannot cover it.
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, id: &ProcessIdentity) -> ProcessEvidence {
            self.inspected.lock().unwrap().insert(id.pid);
            if id.pid == self.escapee.pid {
                ProcessEvidence::Alive {
                    identity: self.escapee.clone(),
                }
            } else {
                ProcessEvidence::Stopped {
                    identity: id.clone(),
                    exit_code: Some(0),
                }
            }
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
    }

    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("freshsnap");
    let (_w, eid) = submit_and_admit(&store, "k-fs1");
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    let leader = ProcessIdentity {
        pid: 9_900_001,
        started_at: "leader start".into(),
        pgid: Some(9_900_001),
    };
    let escapee = ProcessIdentity {
        pid: 9_900_002,
        started_at: "escapee start".into(),
        pgid: None,
    };
    // The writer is recorded Running; its durable descendant snapshot
    // predates the escapee's spawn, and the Execution is already Stopping.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-fs1".into()),
                    request_id: request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(leader.clone()),
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request_id.clone(),
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Stopping;
            e.desired_action = Some(DesiredAction::Cancel);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let sup = fixture_sup(
        FreshSnapshotEscapee {
            escapee: escapee.clone(),
            inspected: Mutex::new(BTreeSet::new()),
        },
        root,
        vec![],
        config(),
    );
    sup.drive_stops(&store, NOW).unwrap();
    assert!(
        sup.port.inspected.lock().unwrap().contains(&escapee.pid),
        "the just-persisted snapshot must be inspected before settling"
    );
    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Stopping,
        "a live writer the stale identity set missed must withhold the exit fact"
    );
}

/// The poll path owns the proof once an Execution leaves `Stopping`
/// (e.g. an Uncertain pass on a deferred Cancel): a `setsid` escapee
/// that only the durable intent still names must be inspected — and
/// re-signalled — before the exit fact can settle and release
/// capacity.
#[test]
fn an_intent_recorded_escapee_blocks_the_poll_path_exit_fact() {
    use std::path::Path;
    use std::sync::Mutex;
    use workloop_core::ports::{SpawnError, WorkerEnv};

    struct IntentEscapee {
        escapee: ProcessIdentity,
        signaled: Mutex<Vec<i32>>,
    }
    impl ProcessControlPort for IntentEscapee {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn in the poll path")
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        // The escapee `setsid`'d out of the group: enumeration cannot
        // cover it — only the durable intent does.
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, id: &ProcessIdentity) -> ProcessEvidence {
            if id.pid == self.escapee.pid {
                ProcessEvidence::Alive {
                    identity: self.escapee.clone(),
                }
            } else {
                ProcessEvidence::Stopped {
                    identity: id.clone(),
                    exit_code: Some(0),
                }
            }
        }
        fn signal_verified(&self, id: &ProcessIdentity, _: i32) -> SignalOutcome {
            self.signaled.lock().unwrap().push(id.pid);
            SignalOutcome::Sent
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
    }

    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("pollintent");
    let (_w, eid) = submit_and_admit(&store, "k-pi1");
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    let leader = ProcessIdentity {
        pid: 9_800_001,
        started_at: "leader start".into(),
        pgid: Some(9_800_001),
    };
    let escapee = ProcessIdentity {
        pid: 9_800_002,
        started_at: "escapee start".into(),
        pgid: None,
    };
    // The writer is recorded Running and the Execution left Stopping
    // for a live state — the poll path owns its exit fact now.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-pi1".into()),
                    request_id: request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(leader.clone()),
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request_id.clone(),
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Running;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })?;
            // The durable intent from the earlier stop drive names the
            // escapee — the only record that still covers it.
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertStopIntent {
                intent: StopIntent {
                    invocation_id: InvocationId("inv-pi1".into()),
                    identities: vec![leader.clone(), escapee.clone()],
                    recorded_seq: seq,
                },
            })
        })
        .unwrap();
    let sup = fixture_sup(
        IntentEscapee {
            escapee: escapee.clone(),
            signaled: Mutex::new(vec![]),
        },
        root,
        vec![],
        config(),
    );
    sup.poll_invocations(&store, NOW).unwrap();
    assert!(
        sup.port.signaled.lock().unwrap().contains(&escapee.pid),
        "the intent-recorded escapee must be re-signalled, not ignored"
    );
    let invocation = store
        .transact(|tx| tx.invocation_for_request(&request_id))
        .unwrap()
        .expect("invocation");
    assert_eq!(
        invocation.state,
        InvocationState::Running,
        "a live intent-recorded writer withholds the exit fact"
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

/// S-3a (#149): pre-publication invariant conformance is its own
/// durable Invocation — a separate `invariant_check` record with its
/// own id, request, lifecycle, and receipts — admitted `waiting →
/// ready` under the same Execution reservation after the producing
/// Invocation completes, and publication waits for its `Done` record.
/// The check is read-only: it declares no required artifacts.
#[test]
fn invariant_check_runs_as_its_own_invocation_before_publication() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("invcheck");
    let (_w, eid) = submit_and_admit(&store, "k-ic1");
    let sup = supervisor("clean-exit", root);
    for _ in 0..60 {
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
        "publication proceeded only after the check completed"
    );
    let invocations = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap();
    assert_eq!(invocations.len(), 2, "coding plus the check Invocation");
    let coding = &invocations[0];
    let check = &invocations[1];
    assert_eq!(coding.role, InvocationRole::Coding);
    assert_eq!(check.role, InvocationRole::InvariantCheck);
    assert_ne!(coding.id, check.id, "distinct invocation_id");
    assert_ne!(
        coding.request_id, check.request_id,
        "distinct durable request"
    );
    assert_eq!(check.state, InvocationState::Done);
    assert!(
        check.required_artifacts.is_empty(),
        "the read-only check declares no workspace artifact"
    );
    assert!(
        check.process_identity.is_some(),
        "the check carries its own process receipts"
    );
    // The publish intent exists only because the check completed: the
    // outbox holds exactly the one publication write.
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    assert_eq!(intents.len(), 1);
    assert!(matches!(intents[0].action, HostAction::PushRef { .. }));
}

/// S-3a (#149) review fix: the invariant check never launches in the
/// producing workspace. It runs against a per-Invocation snapshot copy,
/// so a check leg that writes can never mutate the state the producing
/// Invocation's artifact evidence describes. The worker below appends
/// its cwd to `whoami.txt` on every launch: one line proves only the
/// coding Invocation ran in `ws-<work>`; a second would mean the check
/// leg reached the producing workspace.
#[test]
fn invariant_check_runs_against_a_snapshot_never_the_producing_workspace() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("invcheck-snap");
    let script = root.join("cwd-writer.sh");
    // `describe` is delegated to the fixture worker so the adapter
    // declaration the probe records is the same one every other
    // contract test binds; every other launch records its cwd.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"describe\" ]; then\n    exec \"{}\" describe\nfi\nprintf '%s\\n' \"$PWD\" >> whoami.txt\nprintf '{{\"status\":\"done\"}}\\n' > artifact.json\n",
            fixture_bin()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (work, eid) = submit_and_admit(&store, "k-ic-snap");
    let sup = fixture_sup(
        PosixProcessControl,
        root.clone(),
        vec![script.to_string_lossy().into_owned()],
        config(),
    );
    for _ in 0..60 {
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
        "publication proceeded only after the check completed"
    );
    let invocations = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap();
    assert_eq!(invocations.len(), 2, "coding plus the check Invocation");
    let check = &invocations[1];
    assert_eq!(check.role, InvocationRole::InvariantCheck);
    // The durable export names the snapshot workspace — never the
    // producing one the artifact digest was recorded against.
    let export = store
        .transact(|tx| {
            tx.worker_request_export(&workloop_core::worker::export_request_id(&check.id))
        })
        .unwrap()
        .expect("the check's durable export");
    let ws = root.join(format!("ws-{}", work.0));
    assert_ne!(
        export.workspace_root,
        ws.to_string_lossy(),
        "the check never exports the producing workspace"
    );
    assert!(
        export.workspace_root.contains(".isolation"),
        "the check export names its snapshot workspace: {}",
        export.workspace_root
    );
    // Exactly one launch wrote into the producing workspace — the
    // coding Invocation's. The check's own write landed in the
    // snapshot copy and was reclaimed with the Invocation.
    let whoami = std::fs::read_to_string(ws.join("whoami.txt")).unwrap();
    let lines: Vec<&str> = whoami.lines().collect();
    // `$PWD` reports the physical path — canonicalize so a temp-dir
    // symlink (`/var` → `/private/var`) cannot skew the comparison.
    let ws_physical = ws.canonicalize().unwrap();
    assert_eq!(
        lines,
        vec![ws_physical.to_string_lossy().as_ref()],
        "only the producing writer ran in the workspace: {whoami}"
    );
    let snapshot = root.join(".isolation").join(format!(
        "check-{}",
        workloop_exec::isolation::sanitize_key(&check.id.0)
    ));
    assert!(
        !snapshot.exists(),
        "the check snapshot is reclaimed when the Invocation finishes"
    );
}

/// Inspection read and stop-request wiring (U5.3, #93): a stop request
/// reaches the supervised writer, and an inspection read reports the
/// recorded writer_state for the supervision record.
#[test]
fn a_stop_request_reaches_the_writer_and_inspection_reads_writer_state() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("u53-inspect");
    let port = PosixProcessControl;
    let sup = supervisor("hang", root);
    let (_w, eid) = submit_and_admit(&store, "k-u53-inspect");
    sup.run_once(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &eid).state == ExecutionState::Running,
        "running",
    );
    // An inspection read returns the recorded writer_state.
    let view = store
        .transact(|tx| projection::supervision(tx, &eid))
        .unwrap()
        .expect("supervision record");
    assert_eq!(view.writer_state, Some(WriterState::Running));
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let identity = invocation
        .process_identity
        .clone()
        .expect("spawned identity recorded");
    // A stop request reaches the writer: cancelling drives the stop
    // path, and only a verified stop settles the record.
    let e = execution(&store, &eid);
    cancel(&store, &e.work_id, "k-u53-inspect-cancel");
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
        matches!(port.inspect(&identity), ProcessEvidence::Stopped { .. }),
        "the stop request reached the writer"
    );
    // The inspection read returns the recorded writer_state after the
    // stop is verified.
    let view = store
        .transact(|tx| projection::supervision(tx, &eid))
        .unwrap()
        .expect("supervision record");
    assert_eq!(view.writer_state, Some(WriterState::Stopped));
}

/// Artifact recollection (U5.3, #93): the artifact set recorded for the
/// Execution matches exactly what the writer produced — digest for
/// digest, per required artifact.
#[test]
fn artifact_recollection_records_exactly_what_the_writer_produced() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("u53-artifact");
    let (w, eid) = submit_and_admit(&store, "k-u53-artifact");
    let sup = supervisor("clean-exit", root);
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        let e = execution(&store, &eid);
        if e.state == ExecutionState::Waiting {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Waiting);
    // What the writer produced, read back from its workspace.
    let produced =
        std::fs::read(sup.workspace_of(&w).join("artifact.json")).expect("produced artifact");
    let produced_digest = sha256_hex("artifact-v1", &produced);
    // The recorded artifact set for the Execution, per the recorded
    // observation evidence.
    let recorded: BTreeSet<String> = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::Artifact {
                state: ArtifactFact::Present { digest },
                ..
            } => Some(digest),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        BTreeSet::from([produced_digest]),
        "the recorded artifact set matches what the writer produced"
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let complete = store
        .transact(|tx| projection::artifacts_complete(tx, &eid, &invocation))
        .unwrap();
    assert!(complete, "every required artifact is recorded present");
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
        ..
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
            exit_code: None,
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
        // The replayed exit drives the same reconciliation: the
        // invariant lane re-requests the pre-publication check under
        // the same reservation, so the Execution is `ready` for that
        // request and no publish intent exists yet.
        //
        // The exit fact must carry the proven status: the injected
        // failure consumed the first finish attempt, and the status
        // survived because `inspect` peeks it without reaping — a
        // consumed-then-lost status would record `exit_code: None`.
        assert!(facts.iter().any(|obs| matches!(
            &obs.fact,
            ObservationFact::ProcessState {
                invocation_id,
                process_identity,
                state: ProcessStateFact::Exited,
                exit_code: Some(0),
            } if invocation_id == &invocation.id
                && process_identity
                    == &format!("{}@{}", identity.pid, identity.started_at)
        )));
        let e = execution(&store, &eid);
        assert_eq!(e.state, ExecutionState::Ready);
        let pending = store.transact(|tx| tx.pending_requests()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].role, InvocationRole::InvariantCheck);
        assert!(store.transact(|tx| tx.action_intents()).unwrap().is_empty());
    }
}

#[test]
fn stale_or_unverified_leader_never_authorizes_a_group_signal() {
    use std::cell::Cell;
    use std::path::Path;
    use workloop_core::ports::{SpawnError, WorkerEnv};

    struct UnverifiedLeader {
        identity: ProcessIdentity,
        condition: &'static str,
    }
    impl ProcessControlPort for UnverifiedLeader {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
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
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
    }

    for condition in ["reused", "stopped", "unknown"] {
        let identity = ProcessIdentity {
            pid: 42,
            started_at: "original".into(),
            pgid: Some(42),
        };
        let sup = fixture_sup(
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
    use workloop_core::ports::{SpawnError, WorkerEnv};

    struct SignalPort(SignalOutcome);
    impl ProcessControlPort for SignalPort {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
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
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
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
        let sup = fixture_sup(
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
                    exit_code: None,
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
    let sup = fixture_sup(
        PosixProcessControl,
        root.clone(),
        vec![fixture_bin()],
        config(),
    );
    let leader = port
        .spawn(
            &[fixture_bin(), "hang".into()],
            &root,
            &WorkerEnv::default(),
        )
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
    let _ = port.reap(leader.pid);
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
    let sup = fixture_sup(PosixProcessControl, root.clone(), argv.clone(), config());
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
    let restarted = fixture_sup(PosixProcessControl, root, argv, config());
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
    let sup = fixture_sup(PosixProcessControl, root.clone(), argv.clone(), config());
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
    let restarted = fixture_sup(PosixProcessControl, root, argv, config());
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
                exit_code: Some(0),
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
    let _ = port.reap(leader.pid);
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
                exit_code: Some(0),
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
    let sup = fixture_sup(
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

/// A crash at the stop boundary — durable intent written, supervisor
/// killed before any signal — holds the reservation and the capacity
/// slot through the restart until BOTH the writer's verified stop AND
/// required-effect reconciliation complete. Neither condition alone
/// releases.
#[test]
fn a_mid_stop_crash_holds_the_slot_until_stop_and_effects_reconcile() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("midstop-effect");
    let (w, eid) = submit_and_admit(&store, "k-ms1");
    let sup = supervisor("hang", root.clone());
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

    // An outstanding required effect: the terminal release must wait
    // for its reconciliation even after the writer is verified stopped.
    let action_id = "act-midstop".to_string();
    store
        .transact(|tx| {
            let cfg = config();
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: w.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: cfg.host.actors[0].actor.clone(),
                    action: HostAction::PushRef {
                        branch: format!("work/{}", w.0),
                        produced: CommitRevision::new(repo(), "0".repeat(40)).unwrap(),
                        expected_prior: None,
                    },
                    payload_digest: "digest".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })
        })
        .unwrap();

    cancel(&store, &w, "k-ms2");
    assert_eq!(execution(&store, &eid).state, ExecutionState::Stopping);
    assert_eq!(held_claims(&store), (1, 2));

    // The crash: the stop intent — with its descendant snapshot — is
    // durable, then the supervisor dies before any signal is sent.
    let _intent = persist_stop_intent(&store, &sup, &invocation, &leader);
    drop(sup);
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "the mid-stop crash holds the slot and the reservations"
    );

    // The restarted supervisor resumes from the durable intent and
    // drives the recorded identities to verified stop; the exit fact
    // it reports is the recorded proof.
    let restarted = supervisor("hang", root);
    for _ in 0..40 {
        restarted.drive_stops(&store, NOW).unwrap();
        if store
            .transact(|tx| projection::latest_process_fact(tx, &eid, &invocation.id))
            .unwrap()
            == Some(ProcessStateFact::Exited)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let record = store
        .transact(|tx| projection::supervision(tx, &eid))
        .unwrap()
        .unwrap();
    assert_eq!(
        record.writer_state,
        Some(WriterState::Stopped),
        "the verified stop is on record"
    );
    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Stopping,
        "verified stop alone does not release while an effect is outstanding"
    );
    assert_eq!(
        held_claims(&store),
        (1, 2),
        "the outstanding effect keeps the slot and reservations held"
    );

    // Reconciliation: the host reports a definite no-effect failure for
    // the recorded intent — only now does the terminal settle release.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "test".into(),
                event_id: "host:midstop-reconcile".into(),
            }),
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::HostActionResult {
                action_id,
                authenticated_actor: None,
                status: HostActionStatus::Failed {
                    reason: "host reports no write was made".into(),
                },
                effects: vec![],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();

    assert_eq!(
        execution(&store, &eid).state,
        ExecutionState::Cancelled,
        "verified stop plus reconciled effects settles the cancel"
    );
    assert_eq!(held_claims(&store), (0, 0), "terminal release");
}

/// A failed liveness read never implies a stopped writer: the Execution
/// turns `uncertain`, keeps slot and reservations, and no replacement
/// trial is spawned until the recorded identity verifies stopped — only
/// then may the slot release.
#[test]
fn a_failed_liveness_read_spawns_no_replacement_until_verified() {
    use std::cell::Cell;
    use std::path::Path;
    use workloop_core::ports::{SpawnError, WorkerEnv};

    // A port whose liveness read can be failed on demand: spawn records
    // an identity, then `inspect` reports `unknown` until reconciled.
    struct FlakyPort {
        fail: Cell<bool>,
        identity: ProcessIdentity,
    }
    impl ProcessControlPort for FlakyPort {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            Ok(self.identity.clone())
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(vec![])
        }
        fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
            if self.fail.get() {
                ProcessEvidence::Unknown {
                    reason: "liveness read failed".into(),
                }
            } else {
                ProcessEvidence::Stopped {
                    identity: identity.clone(),
                    exit_code: None,
                }
            }
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            SignalOutcome::AlreadyGone
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            SignalOutcome::AlreadyGone
        }
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            Ok(ProbeOutcome::Exited {
                stdout: br#"{"adapter":"workloop-fixture-worker",
                    "adapter_version":"1.0.0","runtime_version":"fixture-1.0.0"}"#
                    .to_vec(),
                success: true,
            })
        }
    }

    let store = SqliteStore::in_memory(1).unwrap();
    let (_w, eid) = submit_and_admit(&store, "k-fl1");
    let identity = ProcessIdentity {
        pid: 4242,
        started_at: "recorded".into(),
        pgid: Some(4242),
    };
    let sup = fixture_sup(
        FlakyPort {
            fail: Cell::new(false),
            identity,
        },
        tmp_root("flaky"),
        vec![fixture_bin(), "hang".into()],
        config(),
    );
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Running);

    // The liveness read now fails: the writer is unknown, never dead.
    sup.port.fail.set(true);
    for _ in 0..3 {
        sup.poll_invocations(&store, NOW).unwrap();
        assert_eq!(
            execution(&store, &eid).state,
            ExecutionState::Uncertain,
            "a failed liveness read leaves writer ownership unproven"
        );
        assert_eq!(
            held_claims(&store),
            (1, 2),
            "nothing releases on an unknown writer"
        );
        assert!(
            store
                .transact(|tx| tx.pending_requests())
                .unwrap()
                .is_empty(),
            "no replacement trial while the writer is unverified"
        );
        assert_eq!(
            store
                .transact(|tx| tx.invocations_for_execution(&eid))
                .unwrap()
                .len(),
            1,
            "no second writer is spawned while the writer is unverified"
        );
        let record = store
            .transact(|tx| projection::supervision(tx, &eid))
            .unwrap()
            .unwrap();
        assert_eq!(record.writer_state, Some(WriterState::Unknown));
        assert!(
            record.capacity_slot.is_some() && record.reservation_ids.len() == 2,
            "the supervision record shows the held slot and reservations"
        );
    }

    // Reconciliation: the recorded identity verifies stopped — only now
    // does the Execution settle (the recorded continue pauses it) and
    // the slot release; reservations stay held for the preserved WIP.
    sup.port.fail.set(false);
    sup.poll_invocations(&store, NOW).unwrap();
    let e = execution(&store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Paused,
        "the verified stop settles the recorded continue"
    );
    assert_eq!(
        held_claims(&store),
        (0, 2),
        "the slot releases only after verification; reservations keep the WIP"
    );
    let record = store
        .transact(|tx| projection::supervision(tx, &eid))
        .unwrap()
        .unwrap();
    assert_eq!(record.writer_state, Some(WriterState::Stopped));
    assert!(record.capacity_slot.is_none());
}
