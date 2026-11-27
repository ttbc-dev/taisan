//! U4.6a — adapter-swap semantics. The supervisor binds a Worker adapter
//! to a durable request before launch; a later `worker_argv` change applies
//! only to requests that were not yet bound. WS-20 swaps between the
//! fixture and independent contract Workers in both directions; WS-21
//! proves a runtime version that differs from the stored declaration is a
//! configuration repair with no worker start.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::ObservationFact;
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, SignalOutcome, SpawnError, WorkerEnv,
};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_exec::adapter;
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
    let dir = std::env::temp_dir().join(format!("workloop-swap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Contract launches are fixture-profile evidence: the reference
/// profile's bwrap boundary is proven by the containment suite, not
/// by these adapter-swap tests.
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

fn host() -> HostInstanceId {
    HostInstanceId::trusted("swap-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("swap-repo"))
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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
            host_write: false,
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

fn submit_and_admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let request = CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
        origin: Origin("adapter-swap".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "adapter swap".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let decision = handle_command(store, &config(), &request, NOW).unwrap();
    let (work_id, execution_id) = match decision {
        CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("submit rejected: {other:?}"),
    };
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &execution_id, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, execution_id)
}

fn request(store: &SqliteStore, execution: &ExecutionId) -> InvocationRequest {
    store
        .transact(|tx| tx.request(&format!("{}:inv:0", execution.0)))
        .unwrap()
        .expect("request")
}

fn invocations(store: &SqliteStore, execution: &ExecutionId) -> Vec<Invocation> {
    store
        .transact(|tx| tx.invocations_for_execution(execution))
        .unwrap()
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution")
}

fn cancel(store: &SqliteStore, work_id: &WorkId, key: &str) {
    let version = store
        .transact(|tx| tx.work(work_id))
        .unwrap()
        .map(|work| work.version)
        .expect("work exists");
    let decision = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("adapter-swap".into()),
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
    assert!(matches!(decision, CommandDecision::Accepted { .. }));
}

fn drive_until(
    sup: &Supervisor<impl ProcessControlPort>,
    store: &SqliteStore,
    pred: impl Fn() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        sup.run_once(store, NOW).unwrap();
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out driving supervisor");
}

#[test]
fn ws_20_worker_argv_swap_binds_only_new_requests_in_each_direction() {
    let store = SqliteStore::in_memory(4).unwrap();
    let root = tmp_root("ws20");
    let mut sup = fixture_sup(
        PosixProcessControl,
        root,
        vec![fixture_bin(), "hang".into()],
        config(),
    );

    // A request created and dispatched under the fixture adapter.
    let (work_a, exec_a) = submit_and_admit(&store, "ws20-a");
    sup.consume_requests(&store, NOW).unwrap();
    let binding_a = request(&store, &exec_a)
        .adapter_binding
        .expect("request A binds an adapter");
    assert_eq!(binding_a.adapter.as_str(), "workloop-fixture-worker");
    let invocation_a = invocations(&store, &exec_a)
        .into_iter()
        .next()
        .expect("invocation A");
    assert_eq!(invocation_a.state, InvocationState::Running);
    let identity_a = invocation_a.process_identity.clone().expect("writer A");

    // Swap to the independent contract Worker, then create B.
    sup.worker_argv = vec![contract_bin(), "hang".into()];
    let (work_b, exec_b) = submit_and_admit(&store, "ws20-b");
    sup.consume_requests(&store, NOW).unwrap();
    let binding_b = request(&store, &exec_b)
        .adapter_binding
        .expect("request B binds an adapter");
    assert_eq!(binding_b.adapter.as_str(), "workloop-contract-worker");
    assert_ne!(
        binding_a.capability_digest, binding_b.capability_digest,
        "a different adapter declares a different capability surface"
    );

    // A running Invocation keeps its recorded binding; only the request
    // created after the swap selected the new adapter.
    assert_eq!(
        request(&store, &exec_a).adapter_binding.as_ref(),
        Some(&binding_a),
        "the pre-swap request is not migrated"
    );
    assert_eq!(
        invocations(&store, &exec_a)[0].process_identity.as_ref(),
        Some(&identity_a),
        "the running Invocation keeps its recorded writer identity"
    );

    // Swap back to the fixture adapter for the reverse direction.
    sup.worker_argv = vec![fixture_bin(), "hang".into()];
    let (work_c, exec_c) = submit_and_admit(&store, "ws20-c");
    sup.consume_requests(&store, NOW).unwrap();
    let binding_c = request(&store, &exec_c)
        .adapter_binding
        .expect("request C binds an adapter");
    assert_eq!(binding_c.adapter.as_str(), "workloop-fixture-worker");
    assert_eq!(
        request(&store, &exec_b).adapter_binding.as_ref(),
        Some(&binding_b),
        "the request created during the swap keeps its adapter"
    );

    // Command, queue and budget semantics are independent of the adapter:
    // every Execution took the same admission shape and the same budget.
    for exec in [&exec_a, &exec_b, &exec_c] {
        let e = execution(&store, exec);
        assert_eq!(e.invocations_used, 1);
        assert_eq!(
            serde_json::to_value(&e.budget).unwrap(),
            serde_json::to_value(&config().budget).unwrap(),
            "adapter selection does not change the recorded budget"
        );
        assert_eq!(invocations(&store, exec)[0].role, InvocationRole::Coding);
    }
    assert!(
        execution(&store, &exec_a).enqueued_seq < execution(&store, &exec_b).enqueued_seq
            && execution(&store, &exec_b).enqueued_seq < execution(&store, &exec_c).enqueued_seq,
        "queue order is the committed admission order regardless of adapter"
    );

    // The same cancel decision settles identically under either adapter.
    cancel(&store, &work_a, "ws20-cancel-a");
    cancel(&store, &work_b, "ws20-cancel-b");
    cancel(&store, &work_c, "ws20-cancel-c");
    drive_until(&sup, &store, || {
        [&exec_a, &exec_b, &exec_c]
            .iter()
            .all(|exec| execution(&store, exec).state == ExecutionState::Cancelled)
    });
    for exec in [&exec_a, &exec_b, &exec_c] {
        assert_eq!(execution(&store, exec).state, ExecutionState::Cancelled);
        assert_eq!(
            execution(&store, exec).terminal_code.as_deref(),
            Some("cancelled")
        );
    }
}

#[test]
fn ws_20_request_binding_survives_a_store_reopen() {
    let dir = tmp_root("reopen");
    let db = dir.join("store.db");
    let (exec, binding, identity) = {
        let store = SqliteStore::open(&db, 2).unwrap();
        let (_work, exec) = submit_and_admit(&store, "ws20-reopen");
        let sup = fixture_sup(
            PosixProcessControl,
            dir.join("ws"),
            vec![fixture_bin(), "hang".into()],
            config(),
        );
        sup.consume_requests(&store, NOW).unwrap();
        let binding = request(&store, &exec)
            .adapter_binding
            .expect("request binding recorded");
        let identity = invocations(&store, &exec)[0]
            .process_identity
            .clone()
            .expect("writer identity");
        (exec, binding, identity)
    };

    let store = SqliteStore::open(&db, 2).expect("v3 store reopens");
    assert_eq!(
        request(&store, &exec).adapter_binding,
        Some(binding),
        "the adapter binding is durable, not an in-process cache"
    );
    unsafe { libc::kill(identity.pid, libc::SIGKILL) };
}

/// Counts spawn attempts so WS-21 can assert not merely "no writer is
/// running" but "no start was attempted".
#[derive(Clone)]
struct CountingPosix {
    calls: std::rc::Rc<std::cell::Cell<usize>>,
}

impl ProcessControlPort for CountingPosix {
    fn spawn(
        &self,
        argv: &[String],
        cwd: &std::path::Path,
        env: &WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError> {
        self.calls.set(self.calls.get() + 1);
        PosixProcessControl.spawn(argv, cwd, env)
    }
    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {
        PosixProcessControl.snapshot_descendants(leader_pid)
    }
    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String> {
        PosixProcessControl.group_members(pgid)
    }
    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        PosixProcessControl.inspect(identity)
    }
    fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome {
        PosixProcessControl.signal_verified(identity, signal)
    }
    fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome {
        PosixProcessControl.signal_group(pgid, signal)
    }
    fn reap(&self, pid: i32) -> Option<i32> {
        PosixProcessControl.reap(pid)
    }
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &std::path::Path,
        env: &WorkerEnv,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        PosixProcessControl.run_probe(argv, cwd, env, timeout)
    }
}

#[test]
fn ws_21_runtime_version_mismatch_is_configuration_repair_with_no_start() {
    let store = SqliteStore::in_memory(2).unwrap();
    let root = tmp_root("ws21");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let port = CountingPosix {
        calls: calls.clone(),
    };
    let mut sup = fixture_sup(
        port,
        root.clone(),
        vec![fixture_bin(), "hang".into()],
        config(),
    );

    let (_work_a, exec_a) = submit_and_admit(&store, "ws21-a");
    sup.consume_requests(&store, NOW).unwrap();
    let stored = request(&store, &exec_a)
        .adapter_binding
        .expect("request A binds the declared runtime");
    assert_eq!(stored.runtime_version, "fixture-1.0.0");
    assert_eq!(calls.get(), 1, "the declared runtime started once");

    // Request B recorded the declaration this argv made when it bound;
    // by the time its slot comes up the same argv reports a newer
    // runtime (the worker binary was upgraded in place). The stored
    // binding is immutable, so the drift is a configuration repair —
    // not a silent upgrade — and authorizes no start.
    let upgraded_argv = vec![
        fixture_bin(),
        "hang".into(),
        "--runtime-version".into(),
        "fixture-2.0.0".into(),
    ];
    let declared_at_bind = adapter::observe_worker_declaration(
        &PosixProcessControl,
        &[fixture_bin(), "hang".into()],
        &root,
        &WorkerEnv::default(),
    )
    .expect("the fixture declares its runtime");
    let (_work_b, exec_b) = submit_and_admit(&store, "ws21-b");
    let request_b = request(&store, &exec_b).request_id.clone();
    store
        .transact(|tx| {
            tx.apply(&Effect::BindRequestAdapter {
                request_id: request_b,
                binding: declared_at_bind.binding(&upgraded_argv),
            })
        })
        .unwrap();
    sup.worker_argv = upgraded_argv;
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(
        calls.get(),
        1,
        "a runtime version mismatch must not start a worker"
    );
    assert_eq!(
        request(&store, &exec_b)
            .adapter_binding
            .map(|binding| binding.runtime_version),
        Some("fixture-1.0.0".to_string()),
        "the refused request keeps its recorded binding"
    );
    let inv_b = invocations(&store, &exec_b);
    assert_eq!(inv_b.len(), 1);
    assert_eq!(inv_b[0].state, InvocationState::Abandoned);
    assert!(
        inv_b[0].process_identity.is_none(),
        "no worker was recorded"
    );
    assert_eq!(execution(&store, &exec_b).state, ExecutionState::Paused);
    let observations = store
        .transact(|tx| tx.observations_for_execution(&exec_b))
        .unwrap();
    assert!(
        observations.iter().any(|observation| matches!(
            &observation.fact,
            ObservationFact::LaunchRefused { reason, .. }
                if reason.contains("runtime.version_mismatch")
        )),
        "the retained evidence names the configuration repair: {observations:?}"
    );

    // The refused request is terminal for its binding: the operator
    // re-issues the work under a new request, which binds the upgraded
    // declaration and starts with it.
    let (_work_c, exec_c) = submit_and_admit(&store, "ws21-c");
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(
        calls.get(),
        2,
        "the re-issued request binds the new runtime and starts"
    );
    assert_eq!(
        request(&store, &exec_c)
            .adapter_binding
            .map(|binding| binding.runtime_version),
        Some("fixture-2.0.0".to_string()),
        "the re-issued request binds the upgraded declaration"
    );
    assert_eq!(
        invocations(&store, &exec_c)[0].state,
        InvocationState::Running
    );

    // Clean up the hanging writers.
    for exec in [&exec_a, &exec_c] {
        if let Some(identity) = invocations(&store, exec)[0].process_identity.clone() {
            unsafe { libc::kill(identity.pid, libc::SIGKILL) };
        }
    }
}
