//! The independent deterministic Worker (U4.5): a second implementation
//! passes the unchanged Worker contract suite the fixture worker
//! satisfies. `shared_worker_suite` runs the identical scenarios against
//! whatever binary is named — proving both implementations meet the
//! boundary — while worker-specific assertions (deterministic payloads,
//! refused host writes) pin the contract worker's own guarantees.
//!
//! Neither Worker publishes or controls the outer loop: they only
//! produce workspace-local changes and evidence the supervisor records.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::ports::{ProcessControlPort, ProcessEvidence, SignalOutcome, WorkerEnv};
use workloop_core::store::Store;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn contract_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-contract-worker")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-cw-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Distinct scratch namespace per tested binary — the two suite
/// invocations run in the same test process and must not share
/// directories.
fn suite_root(tag: &str, bin: &str) -> PathBuf {
    let name = Path::new(bin)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("worker");
    tmp_root(&format!("{name}-{tag}"))
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

fn spawn(bin: &str, args: &[&str], cwd: &Path) -> ProcessIdentity {
    PosixProcessControl
        .spawn(
            &std::iter::once(bin.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect::<Vec<_>>(),
            cwd,
            &WorkerEnv::default(),
        )
        .expect("spawn worker")
}

/// The unchanged Worker contract suite: same scenarios, same
/// assertions, whichever binary is under test.
fn shared_worker_suite(bin: &str) {
    let port = PosixProcessControl;

    // 1. clean-exit: exit 0, the declared artifact exists, and the run
    //    is deterministic — two runs over identical input produce
    //    byte-identical artifacts.
    let a = suite_root("suite-a", bin);
    let b = suite_root("suite-b", bin);
    for ws in [&a, &b] {
        std::fs::write(ws.join("request.json"), "{\"intent\":\"x\"}").unwrap();
        let leader = spawn(bin, &["clean-exit"], ws);
        wait_for(
            || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
            "clean exit",
        );
    }
    let art_a = std::fs::read(a.join("artifact.json")).expect("artifact a");
    let art_b = std::fs::read(b.join("artifact.json")).expect("artifact b");
    assert_eq!(
        art_a, art_b,
        "identical inputs must produce identical artifacts"
    );

    // 2. exit-no-artifact: clean exit with no produced artifact —
    //    nothing is invented.
    let ws = suite_root("suite-noart", bin);
    let leader = spawn(bin, &["exit-no-artifact"], &ws);
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "no-artifact exit",
    );
    assert!(!ws.join("artifact.json").exists());

    // 3. hang: the stop target — a verified signal stops it, a stale
    //    identity never authorizes the signal.
    let ws = suite_root("suite-hang", bin);
    let leader = spawn(bin, &["hang"], &ws);
    assert!(matches!(
        port.inspect(&leader),
        ProcessEvidence::Alive { .. }
    ));
    let stale = ProcessIdentity {
        started_at: "bogus".into(),
        ..leader.clone()
    };
    assert_eq!(
        port.signal_verified(&stale, libc::SIGKILL),
        SignalOutcome::SkippedStale
    );
    assert_eq!(
        port.signal_verified(&leader, libc::SIGKILL),
        SignalOutcome::Sent
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "hang stopped",
    );

    // 4. path-reopen: a live writer the descendant snapshot must cover —
    //    it keeps the workspace path open until proven stopped.
    let ws = suite_root("suite-reopen", bin);
    let leader = spawn(bin, &["path-reopen", "--interval-ms", "20"], &ws);
    wait_for(
        || ws.join("heartbeat.log").exists(),
        "path-reopen heartbeat",
    );
    let snapshot = port.snapshot_descendants(leader.pid).unwrap();
    for identity in &snapshot {
        port.signal_verified(identity, libc::SIGKILL);
    }
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "path-reopen stopped",
    );
}

#[test]
fn fixture_worker_passes_the_shared_suite() {
    shared_worker_suite(&fixture_bin());
}

#[test]
fn contract_worker_passes_the_shared_suite() {
    shared_worker_suite(&contract_bin());
}

/// The contract worker's artifact is a pure function of the request
/// bytes: a different request digest produces a different artifact —
/// evidence is bound to input, not replayed boilerplate.
#[test]
fn contract_worker_binds_artifacts_to_the_request() {
    let port = PosixProcessControl;
    let a = tmp_root("bind-a");
    let b = tmp_root("bind-b");
    std::fs::write(a.join("request.json"), "{\"intent\":\"x\"}").unwrap();
    std::fs::write(b.join("request.json"), "{\"intent\":\"y\"}").unwrap();
    let bin = contract_bin();
    for ws in [&a, &b] {
        let leader = spawn(&bin, &["clean-exit"], ws);
        wait_for(
            || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
            "clean exit",
        );
    }
    assert_ne!(
        std::fs::read(a.join("artifact.json")).unwrap(),
        std::fs::read(b.join("artifact.json")).unwrap(),
        "distinct requests produce distinct artifacts"
    );
}

/// Denied host writes: a `--file` escaping the managed workspace is
/// refused — reported, never executed. A Worker argument must never
/// reach outside the sandbox root.
#[test]
fn contract_worker_refuses_a_host_write() {
    let parent = tmp_root("deny");
    let ws = parent.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let leader = spawn(
        &contract_bin(),
        &["clean-exit", "--file", "../escape.txt"],
        &ws,
    );
    let port = PosixProcessControl;
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "refusal exit",
    );
    assert!(
        !parent.join("escape.txt").exists(),
        "no file is created outside the workspace"
    );
    assert!(
        !ws.join("../escape.txt").exists(),
        "no path traversal is honored"
    );
}

/// A committed symlink inside the workspace is a host write in
/// disguise: `--file` resolving through it would write outside the
/// sandbox root — reported and refused, never executed.
#[test]
fn contract_worker_refuses_a_write_through_a_workspace_symlink() {
    let parent = tmp_root("denylink");
    let ws = parent.join("ws");
    let outside = parent.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
    std::os::unix::fs::symlink(outside.join("escape.txt"), ws.join("target.txt")).unwrap();

    let port = PosixProcessControl;
    // A symlinked directory along the declared path.
    let leader = spawn(
        &contract_bin(),
        &["clean-exit", "--file", "link/planted.txt"],
        &ws,
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "refusal exit",
    );
    // A declared file that is itself a symlink.
    let leader = spawn(
        &contract_bin(),
        &["clean-exit", "--file", "target.txt"],
        &ws,
    );
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "refusal exit",
    );

    assert!(
        !outside.join("planted.txt").exists(),
        "nothing is written through a symlinked directory"
    );
    assert!(
        !outside.join("escape.txt").exists(),
        "a symlinked target file is never followed outside"
    );
}

/// The fixed-name heartbeat takes the same write boundary: a
/// committed `heartbeat.log` symlink is a host write — refused.
#[test]
fn contract_worker_reopen_refuses_a_symlinked_heartbeat() {
    let parent = tmp_root("denyreopen");
    let ws = parent.join("ws");
    let outside = parent.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(outside.join("heartbeat.log"), ws.join("heartbeat.log")).unwrap();

    let leader = spawn(&contract_bin(), &["reopen"], &ws);
    let port = PosixProcessControl;
    wait_for(
        || matches!(port.inspect(&leader), ProcessEvidence::Stopped { .. }),
        "refusal exit",
    );
    assert!(
        !outside.join("heartbeat.log").exists(),
        "the heartbeat is never written through the symlink"
    );
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("fixture")
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: RepositoryRef::new(host(), RepositoryId::trusted("repo")),
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
            host_write: false,
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

fn admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let req = CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "do the thing".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let d = handle_command(store, &config(), &req, NOW).unwrap();
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

fn cancel(store: &SqliteStore, work_id: &WorkId, key: &str) {
    let version = store
        .transact(|tx| tx.work(work_id))
        .unwrap()
        .map(|w| w.version)
        .expect("work exists");
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(
        matches!(d, CommandDecision::Accepted { .. }),
        "cancel accepted: {d:?}"
    );
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution")
}

/// Supervisor integration, unchanged: the contract worker is spawned,
/// supervised, and proven stopped through the same durable-intent path
/// every writer takes — the Worker never publishes or drives the loop.
#[test]
fn contract_worker_is_stopped_through_the_same_stop_proof() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("cw-stop");
    let (w, eid) = admit(&store, "cw-stop-1");
    let sup = Supervisor::new(
        PosixProcessControl,
        root.clone(),
        vec![contract_bin(), "hang".into()],
        config(),
    );
    let workspace = sup.workspace_of(&w);
    std::fs::create_dir_all(&workspace).unwrap();

    // Drive the pending request end-to-end: consume → spawn → Running.
    sup.run_once(&store, NOW).unwrap();
    assert_eq!(execution(&store, &eid).state, ExecutionState::Running);

    // Cancel reaches `cancelled` only after every recorded identity is
    // proven stopped — the same stop proof as any Worker.
    cancel(&store, &w, "cw-stop-2");
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        if execution(&store, &eid).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &eid).state, ExecutionState::Cancelled);
}
