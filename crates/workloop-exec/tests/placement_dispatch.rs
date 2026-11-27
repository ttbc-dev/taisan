//! Executor-placement dispatch contracts (#379, `docs/remote-executor.md`
//! §2.2): the recorded binding — never the current configuration —
//! decides where a request runs. A remote-bound request never crosses
//! the local launch path: without a wired driver it refuses as
//! `request.placement_unavailable` (the bound verdict — a named
//! Attention, never a Fault-retry and never a silent local launch),
//! and with one it crosses `RemoteDispatch` exactly once per request.
//! A fault inside the driver leaves the request pending for a later
//! pass; a remote-bound request that already produced an Invocation
//! defers to remote reconcile (#378) untouched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::placement::{BoundEndpoint, PlacementPolicy, PlacementTarget};
use workloop_core::store::{Effect, Store, StoreError, TxView};
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker_v2::WorkerRequestBody;
use workloop_exec::bodies::BodyStore;
use workloop_exec::remote_dispatch::{
    RemoteDispatch, RemoteDispatchError, RemoteDispatchOutcome, RemoteLaunch,
};
use workloop_exec::request_v2::{InvariantSetConfig, V2BudgetBounds, V2Dispatch};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_exec::workspace::RepositorySource;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-placement-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("fixture")
}

fn repo() -> RepositoryRef {
    RepositoryRef {
        host: host(),
        repository: RepositoryId::trusted("repo"),
    }
}

fn bound_endpoint() -> BoundEndpoint {
    BoundEndpoint {
        installation_id: "inst-fixture".into(),
        endpoint_id: "edge-1".into(),
        kind: "ssh".into(),
        host: "edge-1.internal".into(),
        port: 2222,
        expected_identity_digest: "ab".repeat(32),
        endpoint_credential_ref: "env:TAISAN_ENDPOINT_KEY".into(),
        coordinator_credential_ref: "env:TAISAN_COORDINATOR_KEY".into(),
        allowed_protocols: vec!["taisan.executor/1".into()],
    }
}

/// A policy whose Coding target is remote on the one static endpoint;
/// the digest is computed exactly as `executor_placement` does so the
/// admission-stamped binding carries the same anchor.
fn remote_coding_policy() -> PlacementPolicy {
    let mut policy = PlacementPolicy {
        endpoint: Some(bound_endpoint()),
        coding: PlacementTarget::Remote,
        ..PlacementPolicy::default()
    };
    policy.config_digest = policy.digest();
    policy
}

fn config(placement: PlacementPolicy) -> WorkloopConfig {
    WorkloopConfig {
        placement,
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
            HostCapability::IssueRead,
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

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A source repository with two commits: `base` (the spec-bound start)
/// and `newer` (HEAD — the Coding request's base object).
fn git_source(root: &Path) -> (PathBuf, String, String) {
    let dir = root.join("source");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("seed.txt"), "base").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("seed.txt"), "newer").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "newer"]);
    let newer = git(&dir, &["rev-parse", "HEAD"]);
    (dir, base, newer)
}

/// Admit an unsourced Work — its source gate settles `NotApplicable`
/// — under `cfg`, returning the first request id the admission minted.
/// The request's placement binding is the policy's stamp for the
/// Coding role, recorded on the durable row at admission.
fn admit(store: &SqliteStore, key: &str, cfg: &WorkloopConfig) -> (WorkId, ExecutionId, String) {
    let d = handle_command(
        store,
        cfg,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
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
        .transact(|tx| admit_queued(tx, cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
}

fn dispatch_profile(state_root: &Path, src: &Path) -> V2Dispatch {
    V2Dispatch {
        bodies: BodyStore::new(state_root.to_path_buf()),
        sources: vec![RepositorySource {
            repository: repo(),
            path: src.to_path_buf(),
        }],
        invariant_set: Some(InvariantSetConfig {
            id: "invariants-1".into(),
            revision: 1,
            items: vec!["no-todo".into(), "tests-pass".into()],
        }),
        budget: V2BudgetBounds {
            max_turns: 8,
            max_report_bytes: 1 << 20,
            max_log_bytes: 1 << 25,
            max_verification_commands: 4,
            max_continuation_refs: 4,
        },
        capabilities: vec!["artifact_json".into(), "process_control".into()],
        verification_egress_hosts: vec![],
    }
}

/// A v2 supervisor over the fixture wire; the remote driver is wired
/// by the test when the scenario needs one.
fn supervisor_v2(
    root: &Path,
    state_root: &Path,
    src: &Path,
    cfg: WorkloopConfig,
) -> Supervisor<PosixProcessControl> {
    let workspace_root = root.join("ws");
    std::fs::create_dir_all(&workspace_root).unwrap();
    let mut sup = Supervisor::new(
        PosixProcessControl,
        workspace_root,
        vec![
            fixture_bin(),
            "clean-exit".to_string(),
            "--wire".into(),
            "v2".into(),
        ],
        cfg,
    )
    .with_isolation_profile(
        workloop_core::worker::isolation_profile(
            workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
        )
        .unwrap(),
    )
    .with_v2_dispatch(dispatch_profile(state_root, src));
    let handoff_repo = root.join("handoff.git");
    git(root, &["init", "--bare", handoff_repo.to_str().unwrap()]);
    sup = sup.with_handoff(workloop_exec::handoff::HandoffLayout {
        repository: handoff_repo,
        workspace_root: root.join("ws"),
        out_root: root.join("out"),
        state_root: None,
        verification: None,
        verification_placement: PlacementTarget::Local,
        remote_verification_executor: None,
    });
    sup
}

fn pending_ids(store: &SqliteStore) -> Vec<String> {
    store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .map(|r| r.request_id)
        .collect()
}

fn request_of(store: &SqliteStore, request_id: &str) -> InvocationRequest {
    store
        .transact(|tx| tx.request(request_id))
        .unwrap()
        .expect("request row")
}

fn dispatch_refusals(store: &SqliteStore, work_id: &WorkId) -> Vec<(String, String)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::DispatchRefused { code, detail, .. } => {
                Some((code.clone(), detail.clone()))
            }
            _ => None,
        })
        .collect()
}

fn process_facts(store: &SqliteStore, work_id: &WorkId) -> Vec<(InvocationId, ProcessStateFact)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::ProcessState {
                invocation_id,
                state,
                ..
            } => Some((invocation_id.clone(), state.clone())),
            _ => None,
        })
        .collect()
}

/// The scripted remote driver: each `dispatch` consumes one scripted
/// outcome; `Commit` records the Invocation and consumed mark through
/// the erased `RemoteStore` transaction, exactly as the real driver
/// commits inside its own transact.
enum Scripted {
    Commit,
    Unavailable(&'static str),
    Fault(&'static str),
}

struct ScriptedRemoteDispatch {
    calls: AtomicUsize,
    last_body: Mutex<Option<String>>,
    outcome: Mutex<Scripted>,
}

impl RemoteDispatch for ScriptedRemoteDispatch {
    fn dispatch(
        &self,
        store: &dyn workloop_exec::remote_dispatch::RemoteStore,
        request: &InvocationRequest,
        invocation_id: &InvocationId,
        body: &WorkerRequestBody,
        _launch: &RemoteLaunch<'_>,
    ) -> Result<RemoteDispatchOutcome, RemoteDispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_body.lock().unwrap() =
            Some(String::from_utf8_lossy(&body.canonical_body).into_owned());
        let outcome = self.outcome.lock().unwrap();
        match &*outcome {
            Scripted::Commit => {
                store
                    .transact(&mut |tx: &mut dyn TxView| {
                        tx.apply(&Effect::InsertInvocation {
                            invocation: Invocation {
                                id: invocation_id.clone(),
                                request_id: request.request_id.clone(),
                                execution_id: request.execution_id.clone(),
                                role: request.role.clone(),
                                state: InvocationState::Running,
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
                    .map_err(|e: StoreError| RemoteDispatchError::Fault(e.to_string()))?;
                Ok(RemoteDispatchOutcome {
                    invocation_id: invocation_id.clone(),
                    process_identity: "remote:pid-42".into(),
                    bound_incarnation_id: "inc-7".into(),
                })
            }
            Scripted::Unavailable(detail) => Err(RemoteDispatchError::Unavailable {
                detail: detail.to_string(),
            }),
            Scripted::Fault(detail) => Err(RemoteDispatchError::Fault(detail.to_string())),
        }
    }
}

/// The admission stamp is the contract's root: a remote Coding
/// policy records the binding on the durable row — executor identity,
/// target, the self-contained endpoint descriptor, and the
/// schema-version/digest anchor — before any dispatch decision.
#[test]
fn admission_stamps_the_remote_binding_on_the_durable_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let policy = remote_coding_policy();
    let cfg = config(policy.clone());
    let (_w, _e, rid) = admit(&store, "bind", &cfg);

    let request = request_of(&store, &rid);
    let binding = request.placement.expect("placement binding recorded");
    assert_eq!(binding.target, PlacementTarget::Remote);
    assert_eq!(binding.executor, "edge-1");
    let endpoint = binding
        .endpoint
        .expect("remote binding carries the endpoint");
    assert_eq!(endpoint.endpoint_id, "edge-1");
    assert_eq!(endpoint.host, "edge-1.internal");
    assert_eq!(endpoint.port, 2222);
    assert_eq!(binding.schema_version, policy.schema_version);
    assert_eq!(binding.config_digest, policy.config_digest);
}

/// No driver wired is the bound verdict, not a Fault and never a
/// local launch: the refusal is typed `request.placement_unavailable`,
/// the request is consumed once, and no Invocation ever exists.
#[test]
fn remote_bound_request_without_driver_refuses_placement_unavailable() {
    let root = tmp_root("no-driver");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());
    let (w, _e, rid) = admit(&store, "no-driver", &cfg);
    let sup = supervisor_v2(&root, &state_root, &src, cfg);

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].0, "request.placement_unavailable");
    assert!(
        refusals[0].1.contains("no remote dispatch driver"),
        "unexpected refusal detail: {}",
        refusals[0].1
    );
    assert!(
        !pending_ids(&store).contains(&rid),
        "the refused request stays out of the pending set"
    );
    assert!(
        store
            .transact(|tx| tx.invocation_for_request(&rid))
            .unwrap()
            .is_none(),
        "a refused request never produced an Invocation"
    );
    // A second pass replays nothing — the consumed request is terminal.
    sup.consume_requests(&store, NOW + 1).unwrap();
    assert_eq!(dispatch_refusals(&store, &w).len(), 1);
}

/// Insert a pending request row directly — the shape a synthetic
/// scenario needs that admission cannot produce (e.g. a remote
/// binding whose endpoint descriptor is absent).
fn insert_request(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    request_id: &str,
    role: InvocationRole,
    placement: Option<workloop_core::placement::PlacementBinding>,
) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: request_id.into(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    role: role.clone(),
                    spec_revision: 1,
                    payload: serde_json::json!({}),
                    adapter_binding: None,
                    placement,
                },
            })?;
            let exec = tx.execution(execution_id)?.expect("execution exists");
            let mut next = exec.clone();
            next.invocations_used += 1;
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: exec.state.clone(),
            })?;
            Ok(())
        })
        .unwrap();
}

/// A remote-bound row whose endpoint descriptor is absent is a
/// corrupt record — it refuses with the same bound verdict, never a
/// local fallback.
#[test]
fn remote_bound_request_missing_endpoint_refuses() {
    let root = tmp_root("no-endpoint");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());

    // The corrupt row must scan while the Execution is still `ready` —
    // insert it before admission so its lower rowid orders it ahead of
    // the request admission mints. A remote-bound request scanned once
    // the Execution left `ready` is consumed under the same gate the
    // local paths share, without a verdict.
    let d = handle_command(
        &store,
        &cfg,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "no-endpoint".into(),
            body: CommandBody::Submit {
                intent: "intent-no-endpoint".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let (w, e) = match d {
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
    let binding = workloop_core::placement::PlacementBinding {
        executor: "edge-1".into(),
        target: PlacementTarget::Remote,
        endpoint: None,
        schema_version: 1,
        config_digest: "cd".repeat(16),
    };
    insert_request(
        &store,
        &w,
        &e,
        "corrupt-remote",
        InvocationRole::Coding,
        Some(binding),
    );
    let admitted = store
        .transact(|tx| admit_queued(tx, &cfg, &e, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");

    let port = std::sync::Arc::new(ScriptedRemoteDispatch {
        calls: AtomicUsize::new(0),
        last_body: Mutex::new(None),
        outcome: Mutex::new(Scripted::Commit),
    });
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(port.clone());
    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert!(
        refusals
            .iter()
            .any(|(code, detail)| code == "request.placement_unavailable"
                && detail.contains("no endpoint descriptor")),
        "expected the corrupt-row refusal, got {refusals:?}"
    );
}

/// The driver crosses the port exactly once per request: the
/// committed Invocation and the proven remote start report as
/// process evidence the same way a local spawn's does.
#[test]
fn remote_bound_request_dispatches_through_the_port_once() {
    let root = tmp_root("dispatch");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());
    let (w, _e, rid) = admit(&store, "dispatch", &cfg);
    let port = std::sync::Arc::new(ScriptedRemoteDispatch {
        calls: AtomicUsize::new(0),
        last_body: Mutex::new(None),
        outcome: Mutex::new(Scripted::Commit),
    });
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(port.clone());

    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert!(
        port.last_body.lock().unwrap().is_some(),
        "the port received the materialized bound-request body"
    );
    let invocation = store
        .transact(|tx| tx.invocation_for_request(&rid))
        .unwrap()
        .expect("the port's commit recorded the Invocation");
    let facts = process_facts(&store, &w);
    assert!(
        facts
            .iter()
            .any(|(id, state)| *id == invocation.id && *state == ProcessStateFact::Running),
        "the proven remote start reported as process evidence"
    );
    assert!(!pending_ids(&store).contains(&rid));
    assert!(dispatch_refusals(&store, &w).is_empty());

    // A second pass finds the recorded Invocation and defers to
    // remote reconcile — the port is never re-entered for a
    // consumed request.
    sup.consume_requests(&store, NOW + 1).unwrap();
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

/// `Unavailable` from the driver is the same terminal bound verdict
/// as an absent driver — the refusal names the endpoint's own detail.
#[test]
fn remote_unavailable_inside_the_driver_is_the_bound_refusal() {
    let root = tmp_root("unavailable");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());
    let (w, _e, rid) = admit(&store, "unavailable", &cfg);
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(
        std::sync::Arc::new(ScriptedRemoteDispatch {
            calls: AtomicUsize::new(0),
            last_body: Mutex::new(None),
            outcome: Mutex::new(Scripted::Unavailable(
                "session refused: endpoint unreachable",
            )),
        }),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].0, "request.placement_unavailable");
    assert!(refusals[0].1.contains("endpoint unreachable"));
    assert!(!pending_ids(&store).contains(&rid));
    assert!(
        store
            .transact(|tx| tx.invocation_for_request(&rid))
            .unwrap()
            .is_none()
    );
}

/// A coordinator-side `Fault` inside the driver is not a verdict: the
/// pass fails and the request stays pending so a later pass resumes
/// it under the same binding.
#[test]
fn remote_fault_leaves_the_request_pending() {
    let root = tmp_root("fault");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());
    let (w, _e, rid) = admit(&store, "fault", &cfg);
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(
        std::sync::Arc::new(ScriptedRemoteDispatch {
            calls: AtomicUsize::new(0),
            last_body: Mutex::new(None),
            outcome: Mutex::new(Scripted::Fault("store write lost")),
        }),
    );

    let result = sup.consume_requests(&store, NOW);
    assert!(result.is_err(), "a fault fails the pass");

    assert!(
        pending_ids(&store).contains(&rid),
        "a faulted dispatch leaves the request pending for the next pass"
    );
    assert!(dispatch_refusals(&store, &w).is_empty());
    assert!(
        store
            .transact(|tx| tx.invocation_for_request(&rid))
            .unwrap()
            .is_none()
    );
}

/// A remote-bound request that already produced an Invocation is
/// #378's reconcile territory: the pass consumes the row, enters no
/// port, and reports nothing new.
#[test]
fn remote_bound_request_with_an_invocation_defers_to_reconcile() {
    let root = tmp_root("reconcile");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    let cfg = config(remote_coding_policy());
    let (w, e, rid) = admit(&store, "reconcile", &cfg);

    // The recorded remote run: an Invocation row answering to the
    // request, exactly what the port's commit leaves behind.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-remote-1".into()),
                    request_id: rid.clone(),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            Ok(())
        })
        .unwrap();

    let port = std::sync::Arc::new(ScriptedRemoteDispatch {
        calls: AtomicUsize::new(0),
        last_body: Mutex::new(None),
        outcome: Mutex::new(Scripted::Commit),
    });
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(port.clone());
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(
        port.calls.load(Ordering::SeqCst),
        0,
        "a consumed request never re-enters the port"
    );
    assert!(!pending_ids(&store).contains(&rid));
    assert!(dispatch_refusals(&store, &w).is_empty());
    assert!(process_facts(&store, &w).is_empty());
}

/// A local-bound request never crosses the remote seam: the port is
/// not entered even when a driver is wired.
#[test]
fn local_bound_request_never_enters_the_remote_port() {
    let root = tmp_root("local");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _b, _n) = git_source(&root);
    // An all-remote-capable endpoint is configured but Coding stays
    // local — the local launch path is what the binding names.
    let mut policy = remote_coding_policy();
    policy.coding = PlacementTarget::Local;
    policy.config_digest = policy.digest();
    let cfg = config(policy);
    let (_w, _e, rid) = admit(&store, "local", &cfg);

    let request = request_of(&store, &rid);
    let binding = request.placement.as_ref().expect("binding");
    assert_eq!(binding.target, PlacementTarget::Local);
    assert_eq!(binding.executor, "local");

    let port = std::sync::Arc::new(ScriptedRemoteDispatch {
        calls: AtomicUsize::new(0),
        last_body: Mutex::new(None),
        outcome: Mutex::new(Scripted::Commit),
    });
    let sup = supervisor_v2(&root, &state_root, &src, cfg).with_remote_dispatch(port.clone());
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(
        port.calls.load(Ordering::SeqCst),
        0,
        "a local-bound request never enters the remote port"
    );
}

/// The binding is immutable against reload: a request admitted under
/// a remote policy stays remote-bound after the policy is swapped to
/// local-only — the durable row, not the current config, decides.
#[test]
fn a_recorded_binding_is_never_retargeted_by_reload() {
    let store = SqliteStore::in_memory(4).unwrap();
    let remote_cfg = config(remote_coding_policy());
    let (_w, _e, rid) = admit(&store, "reload", &remote_cfg);
    let before = request_of(&store, &rid).placement.expect("binding");

    // A later admission under a different policy writes only its own
    // row — the recorded binding is untouched.
    let local_cfg = config(PlacementPolicy::default());
    let (_w2, _e2, _rid2) = admit(&store, "reload-2", &local_cfg);
    let after = request_of(&store, &rid).placement.expect("binding");
    assert_eq!(before, after);
    assert_eq!(after.target, PlacementTarget::Remote);
}

/// The inspect contract (#379): `work detail` explains each durable
/// request's executor, role, liveness class, and refusal — and the
/// serialized view carries the endpoint's public descriptor only,
/// never a credential reference.
#[test]
fn work_detail_explains_executor_role_liveness_and_refusal() {
    let root = tmp_root("inspect");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(4).unwrap();
    let (src, _b, _n) = git_source(&root);
    let policy = remote_coding_policy();
    let cfg = config(policy.clone());
    let (w, _e, rid) = admit(&store, "inspect-refused", &cfg);

    // Refused: remote-bound with no driver wired.
    let sup = supervisor_v2(&root, &state_root, &src, cfg.clone());
    sup.consume_requests(&store, NOW).unwrap();

    // Launched: a second Work's remote-bound request commits through
    // the scripted port.
    let (w2, _e2, rid2) = admit(&store, "inspect-launched", &cfg);
    let sup2 = supervisor_v2(&root, &state_root, &src, cfg.clone()).with_remote_dispatch(
        std::sync::Arc::new(ScriptedRemoteDispatch {
            calls: AtomicUsize::new(0),
            last_body: Mutex::new(None),
            outcome: Mutex::new(Scripted::Commit),
        }),
    );
    sup2.consume_requests(&store, NOW).unwrap();

    // Pending: a third Work admitted but never scanned.
    let local_cfg = config(PlacementPolicy::default());
    let (w3, _e3, rid3) = admit(&store, "inspect-pending", &local_cfg);

    let view = |work_id: &WorkId| match store
        .transact(|tx| workloop_core::projection::work_detail(tx, work_id, NOW))
        .unwrap()
    {
        workloop_core::projection::QueryResult::Found { value, .. } => value,
        other => panic!("work detail not found: {other:?}"),
    };

    // Refused: executor identity, role, the endpoint descriptor, and
    // the closed refusal code — the named verdict an operator reads.
    let detail = view(&w);
    let refused = detail.executions[0]
        .requests
        .iter()
        .find(|r| r.request_id == rid)
        .expect("request view");
    assert_eq!(refused.role, InvocationRole::Coding);
    assert_eq!(refused.executor, "edge-1");
    let endpoint = refused.endpoint.as_ref().expect("endpoint view");
    assert_eq!(endpoint.endpoint_id, "edge-1");
    assert_eq!(endpoint.kind, "ssh");
    assert_eq!(endpoint.host, "edge-1.internal");
    assert_eq!(endpoint.port, 2222);
    assert_eq!(refused.liveness, "refused");
    assert_eq!(
        refused.refusal.as_deref(),
        Some("request.placement_unavailable")
    );
    assert_eq!(
        refused.placement_schema_version,
        Some(policy.schema_version)
    );
    assert_eq!(
        refused.placement_config_digest.as_deref(),
        Some(policy.config_digest.as_str())
    );

    // Launched: the recorded Invocation's identity rides the row.
    let detail2 = view(&w2);
    let launched = detail2.executions[0]
        .requests
        .iter()
        .find(|r| r.request_id == rid2)
        .expect("request view");
    assert_eq!(launched.liveness, "launched");
    assert!(launched.invocation_id.is_some());
    assert!(launched.invocation_state.is_some());
    assert!(launched.refusal.is_none());

    // Pending: the local binding under the implicit policy — `local`
    // executor, schema version 0, no endpoint descriptor.
    let detail3 = view(&w3);
    let pending = detail3.executions[0]
        .requests
        .iter()
        .find(|r| r.request_id == rid3)
        .expect("request view");
    assert_eq!(pending.liveness, "pending");
    assert_eq!(pending.executor, "local");
    assert!(pending.endpoint.is_none());
    assert_eq!(pending.placement_schema_version, Some(0));

    // Redaction: the serialized view names no credential reference —
    // an opaque provider ref is still configuration the inspect
    // surface never echoes.
    let serialized = serde_json::to_string(&detail).unwrap();
    assert!(!serialized.contains("TAISAN_ENDPOINT_KEY"));
    assert!(!serialized.contains("TAISAN_COORDINATOR_KEY"));
    assert!(!serialized.contains("endpoint_credential_ref"));
    assert!(!serialized.contains("coordinator_credential_ref"));
}
