//! Contract tests for the reference WorkerRequest slice (U4.2, #98):
//! the durable export written before launch, the deterministic
//! `request_id`, the launch binding to the spawned process handle, and
//! the `resume` refusal. Scripted port outcomes model the launch
//! faults no argv can produce; the durable assertions are always read
//! back from the store, never from provider internals.

use std::collections::BTreeSet;
use std::path::PathBuf;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::Store;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker::{
    DEFAULT_ISOLATION_PROFILE_ID, Unsupported, WorkerRequest, WorkerRequestExportState,
    WorkerRequestKind, export_request_id,
};
use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};
use workloop_exec::supervisor::Supervisor;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-shim-{tag}-{}", std::process::id()));
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

fn submit_and_admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &req(
            key,
            CommandBody::Submit {
                intent: "do the thing".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        ),
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
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, eid)
}

fn supervisor(
    port: ScriptedProcessControl,
    root: PathBuf,
    profile: Option<&str>,
) -> Supervisor<ScriptedProcessControl> {
    let sup = Supervisor::new(port, root, vec!["fixture-worker".into()], config());
    match profile {
        Some(profile) => sup.with_isolation_profile_id(profile),
        None => sup,
    }
}

fn only_invocation(store: &SqliteStore, eid: &ExecutionId) -> Invocation {
    store
        .transact(|tx| tx.invocations_for_execution(eid))
        .unwrap()
        .remove(0)
}

fn export_for(
    store: &SqliteStore,
    invocation: &Invocation,
) -> workloop_core::worker::WorkerRequestExport {
    store
        .transact(|tx| tx.worker_request_export(&export_request_id(&invocation.id)))
        .unwrap()
        .expect("the export is durable")
}

#[test]
fn the_export_is_durable_before_a_faulted_launch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("export-first");
    let port = ScriptedProcessControl::new();
    // The launch faulted before any process existed.
    port.push_plan(SpawnPlan::Refused {
        reason: "preflight refused".into(),
    });
    let sup = supervisor(port, root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-1");
    sup.consume_requests(&store, NOW).unwrap();
    // The Invocation exists (prepared, abandoned after the refusal).
    let invocation = only_invocation(&store, &eid);
    let export = export_for(&store, &invocation);
    assert_eq!(export.invocation_id, invocation.id);
    assert_eq!(export.request_id, format!("worker:{}", invocation.id.0));
    assert_eq!(export.profile_id, DEFAULT_ISOLATION_PROFILE_ID);
    assert!(!export.workspace_root.is_empty());
    assert_eq!(export.state, WorkerRequestExportState::Exported);
    assert_eq!(export.process_identity, None);
    assert_eq!(export.digest.len(), 64);
}

#[test]
fn a_resume_request_is_refused_with_unsupported_reason() {
    let refused = WorkerRequest::build(
        InvocationId("inv-resume".into()),
        WorkerRequestKind::Resume,
        "profile-a",
        "/srv/ws",
    )
    .expect_err("resume is refused");
    assert!(
        matches!(&refused, Unsupported { reason } if reason.contains("resume")),
        "the refusal is Unsupported{{reason}}: {refused:?}"
    );
}

#[test]
fn a_duplicate_start_for_one_invocation_spawns_exactly_one_writer() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("duplicate");
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    let sup = supervisor(port.clone(), root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-2");
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(
        port.spawn_calls(),
        1,
        "a duplicate start for one invocation_id resolves the existing writer"
    );
    assert_eq!(port.spawned().len(), 1);
    let invocation = only_invocation(&store, &eid);
    assert!(invocation.process_identity.is_some());
    // The export identity is the deterministic binding for this launch.
    let export = export_for(&store, &invocation);
    assert_eq!(export.request_id, export_request_id(&invocation.id));
    assert_eq!(export.state, WorkerRequestExportState::Bound);
}

#[test]
fn the_launch_binds_request_id_to_the_spawned_handle_under_the_named_profile() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("bind");
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    let sup = supervisor(port, root, Some("profile-audit-42"));
    let (_w, eid) = submit_and_admit(&store, "k-shim-3");
    sup.consume_requests(&store, NOW).unwrap();
    let invocation = only_invocation(&store, &eid);
    let identity = invocation
        .process_identity
        .clone()
        .expect("the launch produced a handle");
    let export = export_for(&store, &invocation);
    assert_eq!(export.state, WorkerRequestExportState::Bound);
    assert_eq!(
        export.process_identity.as_deref(),
        Some(format!("{}@{}", identity.pid, identity.started_at).as_str()),
        "the binding names the spawned handle"
    );
    // Reconciliation resolves the spawned process through the recorded
    // request_id binding.
    let resolved = store
        .transact(|tx| tx.worker_request_export(&export.request_id))
        .unwrap()
        .expect("the binding is durable");
    assert_eq!(resolved.invocation_id, invocation.id);
    assert_eq!(resolved.profile_id, "profile-audit-42");
}

#[test]
fn a_lost_start_response_reconciles_to_one_writer_on_the_next_pass() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("lost-start");
    let port = ScriptedProcessControl::new();
    // Spawn happened; the response was lost — the process may exist.
    port.push_plan(SpawnPlan::Unknown {
        reason: "timeout between spawn and response".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-4");
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(
        port.spawn_calls(),
        1,
        "the lost start response never produces a second writer"
    );
    let invocation = only_invocation(&store, &eid);
    assert!(invocation.process_identity.is_none());
    let export = export_for(&store, &invocation);
    assert_eq!(export.state, WorkerRequestExportState::Exported);
    assert_eq!(export.process_identity, None);
}

#[test]
fn an_unbound_export_holds_the_invocation_uncertain() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unbound-holds");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-5");
    sup.consume_requests(&store, NOW).unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(port.spawn_calls(), 1);
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Uncertain,
        "an unbound export holds the writer uncertain, never released"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!(
        (slots, reservations),
        (1, 2),
        "capacity and reservation stay held while the writer is unresolved"
    );
    let invocation = only_invocation(&store, &eid);
    let export = export_for(&store, &invocation);
    assert_eq!(export.state, WorkerRequestExportState::Exported);
}
