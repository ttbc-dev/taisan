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
use workloop_core::ports::{ProcessControlPort, SpawnError, WorkerEnv};
use workloop_core::store::Store;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker::{
    DEFAULT_ISOLATION_PROFILE_ID, FIXTURE_ISOLATION_PROFILE_ID, Unsupported, WorkerRequest,
    WorkerRequestExportState, WorkerRequestKind, export_request_id, isolation_profile,
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
        Some(id) => sup.with_isolation_profile(
            isolation_profile(id).expect("tests run under an approved profile id"),
        ),
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
    // Replay: a duplicated delivery of the same start plus full
    // supervision passes never spawn a second writer.
    sup.consume_requests(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
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
    let sup = supervisor(port, root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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
    assert_eq!(resolved.profile_id, FIXTURE_ISOLATION_PROFILE_ID);
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
    // The supervision pass that owns the lost-start window: it must
    // reconcile through the recorded export/invocation, never spawn.
    sup.poll_invocations(&store, NOW).unwrap();
    sup.run_once(&store, NOW + 1).unwrap();
    assert_eq!(
        port.spawn_calls(),
        1,
        "the lost start response never produces a second writer"
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Running,
        "the orphaned writer is found by its launch signature and re-bound"
    );
    let invocation = only_invocation(&store, &eid);
    let identity = invocation
        .process_identity
        .clone()
        .expect("the orphaned writer's identity is re-recorded");
    let export = export_for(&store, &invocation);
    assert_eq!(export.state, WorkerRequestExportState::Bound);
    assert_eq!(
        export.process_identity.as_deref(),
        Some(format!("{}@{}", identity.pid, identity.started_at).as_str()),
        "the re-bind lands the same request_id → identity binding a normal launch writes"
    );
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

/// The fault boundary #287 names: the consume commit landed
/// (`Prepared`, no identity, request consumed, export `Exported`) and
/// the launch commit never did. With no live process carrying the
/// launch signature, the supervisor must not wedge the Execution
/// `uncertain` forever — after the bounded window it proves absence,
/// abandons the launch, and releases the capacity slot.
#[test]
fn an_unidentified_launch_without_a_writer_is_abandoned_after_the_window() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-absent");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-6");
    sup.consume_requests(&store, NOW).unwrap();
    // The ambiguous spawn produced no live process: the scripted
    // writer already exited before the first resolution pass.
    port.exit_all(0);
    // Inside the bounded window the launch stays uncertain — a process
    // inside its fork-exec visibility gap is never declared absent.
    sup.poll_invocations(&store, NOW + 1).unwrap();
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain
    );
    assert!(only_invocation(&store, &eid).process_identity.is_none());
    // Dense polling never shortens the window: `uncertain_since` is
    // set on the transition into `uncertain` and repeated `Unknown`
    // reports do not refresh it, so the bound is measured from the
    // first lost launch, not the latest pass.
    sup.poll_invocations(&store, NOW + 15).unwrap();
    sup.poll_invocations(&store, NOW + 29).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // Past the window, proven absence abandons the launch — `abandoned`,
    // not `done`: nothing ran to finish — and the slot is released.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0, "proven absence releases the capacity slot");
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// A process-table read that fails is never absence proof: the launch
/// stays uncertain however long the window, and the next readable scan
/// resolves it normally.
#[test]
fn an_unreadable_process_table_keeps_the_launch_uncertain() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-scanfail");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (_w, eid) = submit_and_admit(&store, "k-shim-7");
    sup.consume_requests(&store, NOW).unwrap();
    port.fail_next_process_scan("ps table unavailable");
    // Even past the absence window a failed scan proves nothing.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Prepared);
    assert!(invocation.process_identity.is_none());
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain
    );
    // The next readable scan finds the orphaned writer and re-binds it.
    sup.poll_invocations(&store, NOW + 32).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert!(invocation.process_identity.is_some());
    assert_eq!(invocation.state, InvocationState::Running);
}

/// Two live processes carrying the same launch signature inside the
/// launch directory can never be told apart — the launch stays
/// uncertain rather than guessing which spawn produced which process.
#[test]
fn ambiguous_signature_matches_keep_the_launch_uncertain() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-ambiguous");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (w, eid) = submit_and_admit(&store, "k-shim-8");
    sup.consume_requests(&store, NOW).unwrap();
    // A second live process carries the identical argv inside the same
    // workspace — unrecorded, exactly like the orphan.
    let ws = sup.workspace_of(&w);
    port.spawn(&["fixture-worker".into()], &ws, &WorkerEnv::default())
        .expect("the ambiguous candidate spawns");
    sup.poll_invocations(&store, NOW + 31).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Prepared);
    assert!(invocation.process_identity.is_none());
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain,
        "ambiguity is never resolved by a guess"
    );
}

/// A signature match whose cwd cannot be read can be neither bound
/// nor ruled out: while one exists, an in-directory match might be a
/// foreign process and the unreadable one the real orphan — the
/// launch stays uncertain rather than adopt the wrong writer. Once
/// the cwd reads again the orphan is attributable and binds.
#[test]
fn an_unattributable_candidate_blocks_the_bind() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-cwd-none");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, None);
    let (w, eid) = submit_and_admit(&store, "k-shim-10");
    sup.consume_requests(&store, NOW).unwrap();
    // The orphan's cwd is unreadable (denied lsof) — and a second live
    // process carries the same argv inside the launch directory. The
    // in-directory match alone would look bindable; the unreadable
    // candidate makes every bind a guess.
    let orphan_pid = port.spawned().last().unwrap().pid;
    port.set_cwd_unreadable(orphan_pid, true);
    let ws = sup.workspace_of(&w);
    let foreign = port
        .spawn(&["fixture-worker".into()], &ws, &WorkerEnv::default())
        .expect("the in-directory candidate spawns");
    sup.poll_invocations(&store, NOW + 31).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Prepared);
    assert!(invocation.process_identity.is_none());
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain,
        "an unreadable candidate is never resolved by guessing"
    );
    // The foreign process exits and the orphan's cwd reads again: the
    // orphan is the single attributable match and binds normally.
    port.exit(foreign.pid, 0);
    port.set_cwd_unreadable(orphan_pid, false);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert!(invocation.process_identity.is_some());
    assert_eq!(invocation.state, InvocationState::Running);
}

/// A signature match the store already records — another Work's live
/// writer — is never re-bound as this launch's orphan, and never blocks
/// the orphan that IS this launch's.
#[test]
fn a_recorded_writer_is_never_rebound_as_the_orphan() {
    // Two capacity slots: both Works run concurrently.
    let store = SqliteStore::in_memory(2).unwrap();
    let root = tmp_root("unidentified-recorded");
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    let sup = supervisor(port.clone(), root, None);
    let (_w_a, eid_a) = submit_and_admit(&store, "k-shim-9a");
    sup.consume_requests(&store, NOW).unwrap();
    let (_w_b, eid_b) = submit_and_admit(&store, "k-shim-9b");
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(port.spawn_calls(), 2);
    let spawned = port.spawned();
    assert_eq!(spawned.len(), 2);
    sup.poll_invocations(&store, NOW).unwrap();
    // B's orphan — the second spawn — is the only unrecorded match in
    // B's workspace; A's recorded writer is excluded from consideration.
    let inv_b = only_invocation(&store, &eid_b);
    let identity_b = inv_b
        .process_identity
        .clone()
        .expect("B's orphan is re-bound");
    assert_eq!(
        identity_b.pid, spawned[1].pid,
        "the orphan, not the other Work's writer, is bound"
    );
    assert_ne!(identity_b.pid, spawned[0].pid);
    let inv_a = only_invocation(&store, &eid_a);
    assert_eq!(
        inv_a.process_identity.as_ref().map(|i| i.pid),
        Some(spawned[0].pid),
        "A's writer keeps its own recorded identity"
    );
    assert_eq!(inv_a.state, InvocationState::Running);
}

/// The scripted port's `spawned()` is the process-table view: a
/// `SpawnPlan::Unknown` outcome still records the leader identity —
/// the process exists even though its spawn response was lost.
/// `spawn_calls` stays the no-second-writer counter.
#[test]
fn scripted_unknown_plan_records_the_lost_identity() {
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture lost after spawn".into(),
    });
    let ws = tmp_root("scripted-unknown-spawned");
    assert!(matches!(
        port.spawn(&["fixture-worker".into()], &ws, &WorkerEnv::default()),
        Err(SpawnError::Unknown { .. })
    ));
    assert_eq!(
        port.spawned().len(),
        1,
        "the orphan is in the process table even though the response was lost"
    );
    assert_eq!(port.spawn_calls(), 1);
}
