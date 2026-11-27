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
use workloop_core::observation::ObservationFact;
use workloop_core::ports::{ProbeOutcome, ProcessControlPort, SpawnError, WorkerEnv};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker::{
    DEFAULT_ISOLATION_PROFILE_ID, FIXTURE_ISOLATION_PROFILE_ID, IsolationProfile, Unsupported,
    WorkerRequest, WorkerRequestExportState, WorkerRequestKind, export_request_id,
    isolation_profile,
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// A lost CONTAINED launch leaves a writer running under the bwrap
/// outer argv — the bare bound argv never reaches the process table.
/// Re-binding must search the wrapped signature the launch actually
/// exec'd, or the orphan is missed and the launch is later declared
/// absent on directory occupancy alone.
#[test]
fn a_lost_contained_launch_rebinds_by_the_wrapped_signature() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("lost-contained");
    let port = ScriptedProcessControl::new();
    // Exactly the launch gate: the version probe plus the nine-probe
    // battery. Every later probe — `describe` and the recovery pass's
    // re-proof — falls through to the scripted auto-declaration.
    for _ in 0..10 {
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        }));
    }
    // Spawn happened; the response was lost — the process may exist.
    port.push_plan(SpawnPlan::Unknown {
        reason: "timeout between spawn and response".into(),
    });
    let sup = Supervisor::new(port.clone(), root, vec!["/bin/sh".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-lost-contained",
            env_passthrough: &[],
            sandbox_primitive: Some("sh"),
            ca_trust_paths: &[],
            commit_identity: None,
        });
    let (_w, eid) = submit_and_admit(&store, "k-lost-contained");
    sup.consume_requests(&store, NOW).unwrap();
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
        "the bwrap-wrapped orphan is found by the argv the spawn really exec'd"
    );
    let invocation = only_invocation(&store, &eid);
    assert!(
        invocation.process_identity.is_some(),
        "the orphaned writer's identity is re-recorded"
    );
}

/// The same lost contained launch, now with a runtime boundary
/// installed: the spawn's argv bound the grant's per-launch staged
/// credential (`runtime-auth.p<pid>.i<instance>.<seq>`) — a name the
/// resolve pass can never rebuild, because its own `ensure` stages
/// the NEXT seq (and a restarted supervisor stages under a new owner
/// tag). The signature comparison must treat the staging-shaped
/// token as canonical, or the orphan's real argv never compares
/// equal and the lost writer is never re-bound.
#[test]
fn a_lost_contained_launch_rebinds_past_the_rotating_staged_name() {
    use std::os::unix::fs::PermissionsExt;
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("lost-contained-auth");
    let port = ScriptedProcessControl::new();
    // Exactly the launch gate: the version probe plus the nine-probe
    // battery. Every later probe — `describe` and the recovery pass's
    // re-proof — falls through to the scripted auto-declaration.
    for _ in 0..10 {
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        }));
    }
    // Spawn happened; the response was lost — the process may exist.
    port.push_plan(SpawnPlan::Unknown {
        reason: "timeout between spawn and response".into(),
    });
    let secret = root.join("provider-token");
    std::fs::write(&secret, "dummy-credential").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let sup = Supervisor::new(port.clone(), root.clone(), vec!["/bin/sh".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-lost-contained-auth",
            env_passthrough: &[],
            sandbox_primitive: Some("sh"),
            ca_trust_paths: &[],
            commit_identity: None,
        })
        .with_runtime_boundary(workloop_exec::boundary::RuntimeBoundary::new(
            workloop_exec::boundary::BoundaryConfig {
                credential_ref: Some(format!("file:{}", secret.display())),
                provider_config: None,
                model_api_hosts: vec![],
                runtime_dir: root.join(".runtime"),
                egress_dir: root.join(".egress"),
                relay_exe: PathBuf::from("/bin/sh"),
            },
        ));
    let (_w, eid) = submit_and_admit(&store, "k-lost-contained-auth");
    sup.consume_requests(&store, NOW).unwrap();
    // The launch argv really did bind a per-grant staged name — the
    // token the resolve pass cannot rebuild verbatim.
    let spawned = port.spawned_commands();
    assert!(
        spawned[0].contains("runtime-auth.p"),
        "the contained launch binds the per-grant staged credential: {}",
        spawned[0]
    );
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
        "the orphan is re-bound though resolve staged a different credential name"
    );
    let invocation = only_invocation(&store, &eid);
    assert!(
        invocation.process_identity.is_some(),
        "the orphaned writer's identity is re-recorded"
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// A profile that declares a sandbox primitive the host cannot
/// resolve refuses the launch outright: the consumed request records
/// a `LaunchRefused` repair and no process ever exists. There is no
/// hygiene-only substitute for proven containment.
#[test]
fn an_unresolvable_containment_primitive_refuses_the_launch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("no-primitive");
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    let sup = Supervisor::new(port.clone(), root, vec!["fixture-worker".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-unresolvable-primitive",
            env_passthrough: &[],
            sandbox_primitive: Some("definitely-not-a-real-primitive-xyz"),
            ca_trust_paths: &[],
            commit_identity: None,
        });
    let (_w, eid) = submit_and_admit(&store, "k-shim-ns");
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(
        port.spawn_calls(),
        0,
        "no process exists without proven containment"
    );
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Abandoned);
    let refused = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .any(|o| matches!(o.fact, ObservationFact::LaunchRefused { .. }));
    assert!(
        refused,
        "the containment refusal is a recorded launch repair"
    );
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// Absence is not just "no argv signature match": a lost launch's
/// escaped descendant keeps the leader's working directory after
/// leaving the signature, so the launch directory must itself be
/// clear before `Exited`/`Abandoned` — otherwise a replacement writer
/// would run alongside the still-running escapee. A failed directory
/// scan fails closed the same way.
#[test]
fn an_escaped_descendant_in_the_launch_dir_blocks_the_absence_proof() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-escapee");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-7");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone, but its escaped descendant — a
    // different argv, the same launch directory — keeps running.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let escapee =
        port.adopt_child_with(leader, "fixture-worker escaped-writer --file heartbeat.log");
    sup.poll_invocations(&store, NOW + 1).unwrap();
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain
    );
    // Past the window the signature scan sees nothing, but the
    // directory still holds an unexplained process: no absence. A
    // failed directory scan fails closed identically.
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 31).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared,
        "an unexplained directory occupant is never declared absent"
    );
    assert!(only_invocation(&store, &eid).process_identity.is_none());
    // The escapee exiting clears the directory; the next pass proves
    // absence and abandons the launch.
    port.exit(escapee, 0);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0, "proven absence releases the capacity slot");
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// A process-table read that TRUNCATES is never absence proof either:
/// an escaped descendant that left the argv signature AND has an
/// unreadable cwd is invisible to both scans, so the directory
/// enumeration errs — an incomplete table is never "clear". When the
/// cwd reads again the occupant itself blocks; once it exits, absence
/// is proven and the slot releases.
#[test]
fn an_unreadable_directory_occupant_keeps_absence_unproven() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-dir-unreadable");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-12");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone; its escaped descendant — a different
    // argv, the same launch directory — keeps running, but the
    // platform cannot prove its cwd. A directory table that cannot
    // map it must never read as clear.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let escapee =
        port.adopt_child_with(leader, "fixture-worker escaped-writer --file heartbeat.log");
    port.set_cwd_unreadable(escapee, true);
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
        "an incomplete directory enumeration is never absence proof"
    );
    // The cwd reads again: the escapee is a visible unrecorded
    // occupant and keeps blocking absence on its own.
    port.set_cwd_unreadable(escapee, false);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // The escapee exits: the directory is provably clear and the
    // launch abandons, releasing the slot.
    port.exit(escapee, 0);
    sup.poll_invocations(&store, NOW + 33).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// The third absence check: a descendant that changed BOTH argv and
/// working directory is invisible to the signature and the directory
/// scans — but a launch group it never called `setsid` on still names
/// it. A live unrecorded member of the recorded launch group defeats
/// the clear verdict; only when it is gone is absence proven.
#[test]
fn an_escapee_keeping_the_launch_group_defeats_the_absence_proof() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-group-escapee");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-group");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone; its descendant changed argv AND cwd —
    // invisible to both earlier checks — but kept the process group:
    // the launch's recorded group still holds it.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let escapee = port.adopt_child_in_group(leader, "hidden-worker --quiet");
    let elsewhere = tmp_root("unidentified-elsewhere");
    port.set_cwd(escapee, &elsewhere);
    // Past the window the signature scan sees nothing and the
    // directory is clear, but the recorded launch group still holds a
    // live unrecorded member — no absence.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared,
        "a live group member that escaped both scans defeats the clear verdict"
    );
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Uncertain
    );
    // The escapee exiting leaves the group provably empty: the next
    // pass proves absence and abandons the launch.
    port.exit(escapee, 0);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0, "proven absence releases the capacity slot");
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// Consecutively incomplete absence scans do not hold a launch
/// `uncertain` forever: after the bounded count the supervisor reports
/// the distinct absence-unprovable outcome — the Invocation stays an
/// unresolved writer (a live process may still exist, so the slot is
/// never released) but the Execution is held for an operator decision
/// instead of silently retrying.
#[test]
fn consecutive_incomplete_scans_converge_to_absence_unprovable() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-incomplete");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-unprovable");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone; a live foreign process whose cwd can
    // never be mapped makes every directory scan incomplete — it is
    // not this launch's writer and never matches the signature, so
    // only the completeness gate sees it.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let foreign = port
        .spawn(
            &["foreign-daemon".into()],
            &tmp_root("unidentified-foreign"),
            &WorkerEnv::default(),
        )
        .unwrap()
        .pid;
    port.set_cwd_unreadable(foreign, true);
    // Within the bound the launch holds uncertain — an incomplete
    // scan proves nothing in either direction.
    for poll in 1..3 {
        sup.poll_invocations(&store, NOW + 30 + poll).unwrap();
        assert_eq!(
            only_invocation(&store, &eid).state,
            InvocationState::Prepared
        );
    }
    // The bounded count of consecutive incomplete scans converges to
    // the distinct outcome — never another silent hold.
    sup.poll_invocations(&store, NOW + 33).unwrap();
    let unprovable = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .any(|o| matches!(o.fact, ObservationFact::LaunchAbsenceUnprovable { .. }));
    assert!(unprovable, "the unprovable absence is a recorded outcome");
    let invocation = only_invocation(&store, &eid);
    assert_eq!(
        invocation.state,
        InvocationState::Unknown,
        "the launch converges to a distinct outcome, still an unresolved writer"
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.desired_action,
        Some(DesiredAction::Pause),
        "the unprovable absence is surfaced to the operator"
    );
    assert_eq!(
        e.repair_prerequisites,
        vec![RepairPrerequisite::OperatorDecision]
    );
    // The slot stays held — a live writer may still exist; nothing
    // was proven absent. The hold is stable while scans stay
    // incomplete: the escalation is surfaced once and further
    // incomplete scans keep the launch `unknown` without
    // re-escalating, but a later completed scan still resolves it.
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 1);
    for poll in 40..43 {
        sup.poll_invocations(&store, NOW + poll).unwrap();
    }
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown,
        "the converged outcome is stable while scans stay incomplete"
    );
    let escalations = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::LaunchAbsenceUnprovable { .. }))
        .count();
    assert_eq!(escalations, 1, "the distinct outcome is reported once");
}

/// The escalated `unknown` outcome is surfaced, not concluded: the
/// resolution loop keeps scanning the unidentified launch, so once
/// the obstruction clears a completed scan proves the absence and the
/// Execution settles under the recorded desired action — `paused` for
/// the operator decision the escalation asked for — and releases the
/// slot it held against the possible writer.
#[test]
fn an_escalated_unprovable_launch_recovers_when_absence_becomes_provable() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unprovable-recovers");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-recover");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    // An unmappable foreign cwd makes every absence scan incomplete —
    // three consecutive failures converge to the recorded outcome.
    let foreign = port.adopt_child_with(leader, "foreign-daemon");
    port.set_cwd(foreign, &tmp_root("unprovable-foreign"));
    port.set_cwd_unreadable(foreign, true);
    for poll in 1..=3 {
        sup.poll_invocations(&store, NOW + 30 + poll).unwrap();
    }
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown,
        "the bounded count surfaced the unprovable absence"
    );
    // The obstruction clears: the next scan completes, proves the
    // absence, and the escalated launch settles like any other
    // unidentified launch — `abandoned`, Execution `paused` under the
    // recorded desired action, slot released.
    port.set_cwd_unreadable(foreign, false);
    sup.poll_invocations(&store, NOW + 40).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    assert_eq!(e.desired_action, None);
    assert_eq!(
        e.repair_prerequisites,
        vec![RepairPrerequisite::OperatorDecision]
    );
    assert!(
        e.pause_reason
            .as_deref()
            .is_some_and(|r| r.contains("absence unprovable")),
        "the escalation record survives the recovery"
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0);
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// A writer that surfaces after the escalation is still resolved by
/// the ordinary rules: the completed scan re-binds it under the same
/// single-match-in-directory evidence a never-escalated launch uses,
/// and the recorded pause desire then drives the normal
/// signal-and-prove path — the found writer is stopped and the
/// Execution settles `paused`.
#[test]
fn an_escalated_launch_rebinds_a_late_appearing_writer() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unprovable-rebinds");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-rebind");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let foreign = port.adopt_child_with(leader, "foreign-daemon");
    port.set_cwd(foreign, &tmp_root("unprovable-rebind-foreign"));
    port.set_cwd_unreadable(foreign, true);
    for poll in 1..=3 {
        sup.poll_invocations(&store, NOW + 30 + poll).unwrap();
    }
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown
    );
    // The obstruction clears and the orphaned writer finally shows in
    // the signature scan: the single in-directory match is re-bound.
    port.set_cwd_unreadable(foreign, false);
    let orphan = port.adopt_child(leader);
    sup.poll_invocations(&store, NOW + 40).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Running);
    assert_eq!(
        invocation.process_identity.as_ref().map(|i| i.pid),
        Some(orphan),
        "the found writer is bound to the escalated launch"
    );
    // The recorded desired action still applies: the operator's pause
    // drives the normal signal-and-prove loop over the bound writer.
    sup.drive_stops(&store, NOW + 41).unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0);
    assert!(
        port.signals()
            .iter()
            .any(|s| s.pid == Some(orphan) || s.pgid == Some(orphan)),
        "the re-bound writer was signaled under the recorded pause"
    );
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// A recorded launch group whose owner slot was recycled is a foreign
/// family, not the lost writer's: `pgid` is the leader's pid, so a
/// live process there with a different start than the recorded
/// leader's proves the group re-formed after the leader died — its
/// members can never defeat the absence verdict.
#[test]
fn a_recycled_launch_group_owner_is_foreign_not_the_lost_writer() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-recycled-group");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-recycled");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    // The freed pid/pgid lands on an unrelated process — its own
    // group leader with a different start — and the recycled group
    // collects foreign members. None of them is the lost writer.
    let elsewhere = tmp_root("unidentified-recycled-elsewhere");
    port.recycle_as(leader, "foreign-daemon", &elsewhere);
    port.adopt_child_in_group(leader, "foreign-child");
    // Past the window the signature and directory scans complete and
    // find nothing of this launch's; the recycled group is verified
    // foreign at the owner slot, so its live members cannot hold
    // absence open — the launch abandons and the slot releases.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned,
        "a recycled group is foreign evidence, never the lost writer"
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0);
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// Only CONSECUTIVE incomplete scans count toward the bound: a
/// completed scan — even one that reports the launch occupied —
/// restarts the streak, so a transient unmappable process never
/// converges the outcome on its own.
#[test]
fn a_completed_scan_restarts_the_incomplete_scan_streak() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-streak");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-streak");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    // An escapee in the launch directory keeps every completed scan
    // Occupied — the launch is blocked on positive evidence.
    port.adopt_child_with(leader, "fixture-worker escaped-writer --file heartbeat.log");
    // Two incomplete scans — one short of the bound.
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 31).unwrap();
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // A completed scan (occupied, on the escapee) restarts the count.
    sup.poll_invocations(&store, NOW + 33).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // Two more incomplete scans — short of the bound again.
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 34).unwrap();
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 35).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared,
        "a completed scan restarted the streak — three non-consecutive gaps never converge"
    );
    // The third consecutive failure converges to the distinct outcome.
    port.fail_next_directory_scan("lsof unavailable");
    sup.poll_invocations(&store, NOW + 36).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown
    );
    // The escapee is still alive in the launch directory — nothing
    // was proven absent; the slot stays held.
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 1, "an unproven absence never releases the slot");
}

/// A scan that cannot run at all is the same unprovable pass as a
/// failed absence verdict: a signature scan that cannot enumerate the
/// process table counts toward the bounded escalation — the launch
/// never reaches the absence checks on such a pass, yet still
/// converges to `absence_unprovable` instead of holding `uncertain`
/// forever. A pass that finds unexplained life restarts the streak,
/// and once the table reads again the escalated launch resolves like
/// any other.
#[test]
fn failed_signature_scans_converge_and_evidence_restarts_the_streak() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-table-fail");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-tablefail");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    // Two unreadable process tables — one short of the bound.
    port.fail_next_process_scan("ps failed");
    sup.poll_invocations(&store, NOW + 31).unwrap();
    port.fail_next_process_scan("ps failed");
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // A completed scan that finds unexplained life — an unrecorded
    // signature match outside the launch directory — restarts the
    // count: this pass is uncertain on evidence, not on a gap.
    let lookalike = port.adopt_child(leader);
    port.set_cwd(lookalike, &tmp_root("unidentified-lookalike"));
    sup.poll_invocations(&store, NOW + 33).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared
    );
    // Three consecutive table failures now converge — had the
    // evidence pass not restarted the streak, the second failure
    // after it would already have been the third.
    port.fail_next_process_scan("ps failed");
    sup.poll_invocations(&store, NOW + 34).unwrap();
    port.fail_next_process_scan("ps failed");
    sup.poll_invocations(&store, NOW + 35).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared,
        "the evidence pass restarted the streak"
    );
    port.fail_next_process_scan("ps failed");
    sup.poll_invocations(&store, NOW + 36).unwrap();
    let escalations = store
        .transact(|tx| tx.observations_for_execution(&eid))
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::LaunchAbsenceUnprovable { .. }))
        .count();
    assert_eq!(escalations, 1, "the table failures converged the outcome");
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown,
        "the launch converges to the distinct outcome, still unresolved"
    );
    // The table reads again: with the lookalike gone the completed
    // scan proves the absence and the recoverable outcome settles —
    // `abandoned`, `paused`, slot released.
    port.exit(lookalike, 0);
    sup.poll_invocations(&store, NOW + 40).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0);
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// A signature match whose cwd cannot be mapped is a scan gap, not
/// just a bind blocker: consecutive unmappable-candidate passes
/// converge to `absence_unprovable` like any other unprovable scan —
/// and the recovery is the ordinary one, since clearing the flag
/// lets the same candidate re-bind as the writer.
#[test]
fn unmappable_candidate_scans_converge_then_rebind_the_writer() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-unmappable");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
    let (_w, eid) = submit_and_admit(&store, "k-shim-unmappable");
    sup.consume_requests(&store, NOW).unwrap();
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    // A live signature match in the launch directory whose cwd the
    // table cannot report: every scan it joins is unprovable.
    let candidate = port.adopt_child(leader);
    port.set_cwd_unreadable(candidate, true);
    for poll in 1..=3 {
        sup.poll_invocations(&store, NOW + 30 + poll).unwrap();
    }
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Unknown,
        "unmappable candidates converge to the bounded outcome"
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.desired_action, Some(DesiredAction::Pause));
    // The mapping reads again: the only in-directory match is bound
    // to the launch and the recorded pause drives it to a stop.
    port.set_cwd_unreadable(candidate, false);
    sup.poll_invocations(&store, NOW + 40).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert_eq!(invocation.state, InvocationState::Running);
    assert_eq!(
        invocation.process_identity.as_ref().map(|i| i.pid),
        Some(candidate)
    );
    sup.drive_stops(&store, NOW + 41).unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0);
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// A signature match outside the launch directory can be the real
/// writer after it chdir'd away — which makes the in-directory
/// lookalike unattributable. The same never-guess rule the unreadable
/// cwd guard enforces applies to a readable out-of-directory
/// candidate: the launch stays uncertain rather than adopt the wrong
/// writer. Once the out-of-directory candidate exits, the in-directory
/// orphan is the single candidate and binds normally.
#[test]
fn an_out_of_directory_candidate_blocks_the_bind() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-outdir");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(
        port.clone(),
        root.clone(),
        Some(FIXTURE_ISOLATION_PROFILE_ID),
    );
    let (_w, eid) = submit_and_admit(&store, "k-shim-11");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost writer carries the signature inside the launch
    // directory — and a second live, unrecorded process carries the
    // identical argv outside it: a foreign run, or the real writer
    // after a chdir. Either way the in-directory match might be the
    // wrong writer, so no bind is provable.
    let elsewhere = tmp_root("unidentified-outdir-elsewhere");
    let outsider = port
        .spawn(
            &["fixture-worker".into()],
            &elsewhere,
            &WorkerEnv::default(),
        )
        .expect("the out-of-directory candidate spawns");
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
        "a same-signature candidate outside the launch directory keeps \
         every bind a guess"
    );
    // The outsider exits: the orphan is the only unrecorded match,
    // attributable, and binds on the next pass.
    port.exit(outsider.pid, 0);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert!(invocation.process_identity.is_some());
    assert_eq!(invocation.state, InvocationState::Running);
}

/// A contained worker runs `--chdir /workspace` inside its mount
/// namespace, so its reported cwd is the sandbox mount — never the
/// host launch directory. The same single-match attribution rule
/// still re-binds it: `/workspace` inside the sandbox IS the launch's
/// workspace bind.
#[test]
fn a_contained_orphan_rebinds_on_its_sandbox_workspace_cwd() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-contained");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = supervisor(
        port.clone(),
        root.clone(),
        Some(FIXTURE_ISOLATION_PROFILE_ID),
    );
    let (_w, eid) = submit_and_admit(&store, "k-shim-contained");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost writer inside the sandbox: the signature matches and
    // its cwd reads as the sandbox workspace mount — `--chdir
    // /workspace` inside its own mount namespace, never the host
    // launch directory.
    let orphan_pid = port.spawned().last().unwrap().pid;
    port.set_cwd(orphan_pid, std::path::Path::new("/workspace"));
    sup.poll_invocations(&store, NOW + 31).unwrap();
    let invocation = only_invocation(&store, &eid);
    assert!(
        invocation.process_identity.is_some(),
        "the contained orphan re-binds on its sandbox workspace cwd"
    );
    assert_eq!(invocation.state, InvocationState::Running);
}

/// A contained launch's escaped descendant reports the sandbox mount
/// as its cwd — never the host launch directory. The port contract is
/// unscoped: `processes_in_directory` answers "cwd inside this dir"
/// alone, so the escapee is an occupant of `/workspace` and of no
/// host dir. The absence proof's launch scope scans the sandbox root
/// for a contained launch, so the same escapee still blocks absence —
/// the special case lives in the launch-scope judgment, not the port.
#[test]
fn a_sandbox_scoped_escapee_blocks_absence_without_leaking_the_port_contract() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-sandbox-escapee");
    let port = ScriptedProcessControl::new();
    // Exactly the launch gate: the version probe plus the nine-probe
    // battery. Every later probe — `describe` and the recovery pass's
    // re-proof — falls through to the scripted auto-declaration.
    for _ in 0..10 {
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        }));
    }
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = Supervisor::new(port.clone(), root, vec!["/bin/sh".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-sandbox-escapee",
            env_passthrough: &[],
            sandbox_primitive: Some("sh"),
            ca_trust_paths: &[],
            commit_identity: None,
        });
    let (w, eid) = submit_and_admit(&store, "k-shim-sandbox-escapee");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone; its escaped descendant keeps running
    // inside the sandbox — a different argv the signature scan cannot
    // see, and a cwd that reads the sandbox mount.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let escapee =
        port.adopt_child_with(leader, "fixture-worker escaped-writer --file heartbeat.log");
    port.set_cwd(escapee, std::path::Path::new("/workspace"));
    // Port contract: `/workspace` is inside no host dir — the escapee
    // is not an occupant of the launch workspace nor of any unrelated
    // directory; it IS an occupant of the sandbox root the contained
    // launch scope adds.
    let ws = sup.workspace_of(&w);
    assert!(
        !port
            .processes_in_directory(&ws)
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == escapee),
        "a sandbox cwd is never an occupant of the host launch dir"
    );
    let elsewhere = tmp_root("unidentified-sandbox-elsewhere");
    assert!(
        !port
            .processes_in_directory(&elsewhere)
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == escapee),
        "a sandbox cwd is never an occupant of an unrelated dir"
    );
    // A cwd spelled inside `/workspace` is never proven outside it.
    // Where no host inode backs the mount the genuine scan fails
    // closed on the same table the real port returns; where the
    // host mounts a real `/workspace` — the remote worker
    // containers do — the spelling resolves and the escapee is an
    // occupant of the scan root. Either verdict keeps absence
    // unproven; the namespace query surfaces the pid as data on
    // both host shapes.
    match port.processes_in_directory(std::path::Path::new("/workspace")) {
        Err(_) => {}
        Ok(occupants) => assert!(
            occupants.iter().any(|m| m.identity.pid == escapee),
            "a resolvable /workspace scan reports the contained escapee"
        ),
    }
    assert!(
        port.processes_in_namespace_dir(std::path::Path::new("/workspace"))
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == escapee),
        "the namespace query sees the contained escapee"
    );
    // Past the window the launch scope still holds an unexplained
    // process: the sandbox-root scan counts the escapee, so no absence.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Prepared,
        "a sandbox-scoped escapee keeps the absence unproven"
    );
    assert!(only_invocation(&store, &eid).process_identity.is_none());
    // The escapee exits: the whole launch scope is provably clear and
    // the launch abandons, releasing the slot.
    port.exit(escapee, 0);
    sup.poll_invocations(&store, NOW + 32).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0, "proven absence releases the capacity slot");
    assert_eq!(port.spawn_calls(), 1, "no second writer was ever started");
}

/// The sandbox-root scan must still reach the recorded-occupant
/// judgment when a contained worker's cwd cannot resolve on the
/// host: `/workspace` has no host inode, so the genuine scan fails
/// closed on the spelling — but the namespace query reports the
/// occupant as data, and a worker the store already records is not
/// unexplained. The remote lane runs several contained workers at
/// once: without the recorded judgment a live sibling worker would
/// hold an unidentified launch `Prepared` — and its slot — forever.
#[test]
fn a_recorded_sandbox_occupant_does_not_block_the_absence_proof() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("unidentified-sandbox-recorded");
    let port = ScriptedProcessControl::new();
    // Same contained-launch gate as the sandbox-escapee test: the
    // version probe plus the nine-probe battery; later probes fall
    // through to the scripted auto-declaration.
    for _ in 0..10 {
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        }));
    }
    port.push_plan(SpawnPlan::Unknown {
        reason: "crashed between spawn and identity capture".into(),
    });
    let sup = Supervisor::new(port.clone(), root, vec!["/bin/sh".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-sandbox-recorded",
            env_passthrough: &[],
            sandbox_primitive: Some("sh"),
            ca_trust_paths: &[],
            commit_identity: None,
        });
    let (_w, eid) = submit_and_admit(&store, "k-shim-sandbox-recorded");
    sup.consume_requests(&store, NOW).unwrap();
    // The lost leader is gone. A sibling contained invocation's
    // writer is still live at `/workspace` — the remote lane's
    // normal shape — and the store records its identity, so the
    // namespace scan's occupant is accounted for, not unexplained.
    let leader = port.spawned()[0].pid;
    port.exit(leader, 0);
    let sibling = port
        .spawn(
            &["other-contained-worker".into()],
            std::path::Path::new("/workspace"),
            &WorkerEnv::default(),
        )
        .expect("sibling spawn");
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-sibling".into()),
                    request_id: "req-sibling".into(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: Some(sibling.clone()),
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
    assert!(
        port.processes_in_namespace_dir(std::path::Path::new("/workspace"))
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == sibling.pid),
        "the namespace query reports the recorded sibling worker"
    );
    // Past the window the launch scope holds only recorded
    // occupants: absence is proven and the launch abandons instead
    // of wedging on a worker it was never meant to wait for.
    sup.poll_invocations(&store, NOW + 31).unwrap();
    assert_eq!(
        only_invocation(&store, &eid).state,
        InvocationState::Abandoned,
        "a recorded contained occupant must not hold absence open"
    );
    let slots = store.transact(|tx| Ok(tx.slot_holders()?.len())).unwrap();
    assert_eq!(slots, 0, "proven absence releases the capacity slot");
    // The only second spawn is the test's recorded sibling — the
    // supervisor never started a replacement writer.
    assert_eq!(port.spawn_calls(), 2);
}

/// A live process whose cwd the mechanism reports with the kernel's
/// ` (deleted)` marker — `<root>/gone (deleted)` — WAS inside the
/// scanned directory: judged raw the marked spelling never matches
/// `dir` component-wise where the marker lands on the boundary
/// component, so an unhandled marker is the forbidden outcome — a
/// silent skip that reads back as a clear table and authorizes a
/// replacement writer while the orphan still runs. Both directory
/// queries count the marked row as an occupant.
#[test]
fn a_deleted_marked_cwd_inside_the_scanned_dir_counts_as_an_occupant() {
    let port = ScriptedProcessControl::new();
    // The scans compare the canonical spelling — the mechanism's raw
    // report resolves symlinks (`/var` → `/private/var` on macOS).
    let root = std::fs::canonicalize(tmp_root("deleted-cwd-scan")).unwrap();
    let pid = port
        .spawn(&["writer".into()], &root, &WorkerEnv::default())
        .expect("spawn")
        .pid;
    // The modeled cwd is the mechanism's raw report: the link target
    // of a deleted `<root>/gone` still carries the marker.
    let gone = root.join("gone");
    port.set_cwd(pid, &PathBuf::from(format!("{} (deleted)", gone.display())));
    for occupants in [
        port.processes_in_directory(&root),
        port.processes_in_namespace_dir(&root),
    ] {
        assert!(
            occupants
                .expect("scan")
                .iter()
                .any(|m| m.identity.pid == pid),
            "a ` (deleted)` cwd inside the scanned dir is an occupant, never a silent skip"
        );
    }
    // The deleted-dir-is-scanned-dir case — the marker on the
    // boundary component, where the raw spelling could never match:
    // the process was in `dir` itself, so it still lands.
    port.set_cwd(pid, &PathBuf::from(format!("{} (deleted)", root.display())));
    for occupants in [
        port.processes_in_directory(&root),
        port.processes_in_namespace_dir(&root),
    ] {
        assert!(
            occupants
                .expect("scan")
                .iter()
                .any(|m| m.identity.pid == pid),
            "a deleted scanned dir is still an occupant spelling"
        );
    }
    // Membership is the pre-deletion spelling: a marked path outside
    // `dir` is no occupant.
    let elsewhere = std::fs::canonicalize(tmp_root("deleted-cwd-elsewhere")).unwrap();
    port.set_cwd(
        pid,
        &PathBuf::from(format!("{} (deleted)", elsewhere.display())),
    );
    assert!(
        !port
            .processes_in_directory(&root)
            .expect("scan")
            .iter()
            .any(|m| m.identity.pid == pid),
        "a marked cwd outside the scanned dir is not an occupant"
    );
}

/// A v1 Review launch mounts the managed workspace read-only: the
/// assessment reads produced state and a review process must never
/// mutate the artifact workspace — writers are the only roles that
/// mount it writable.
#[test]
fn a_v1_review_launch_mounts_the_workspace_read_only() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("review-ro");
    let (w, eid) = submit_and_admit(&store, "k-shim-review");
    // The execution keeps at most one unresolved invocation: record
    // the writer request the submit produced as already consumed by a
    // finished writer, so only the review request is left to start.
    let writer_request = format!("{}:inv:0", eid.0);
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-prev".into()),
                    request_id: writer_request.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: writer_request.clone(),
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: format!("{}:inv:7", eid.0),
                    work_id: w.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Review,
                    spec_revision: 1,
                    payload: serde_json::json!({}),
                    adapter_binding: None,
                },
            })
        })
        .unwrap();
    let dir = root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let bwrap = dir.join("fake-bwrap");
    std::fs::write(
        &bwrap,
        "#!/bin/sh\n\
         while [ $# -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\n\
         [ \"$1\" = \"--\" ] && shift\n\
         exec \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let worker = std::env::current_dir()
        .unwrap()
        .join("../../target/debug/workloop-fixture-worker")
        .canonicalize()
        .unwrap();
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    let sup = Supervisor::new(
        port.clone(),
        root.clone(),
        vec![worker.to_string_lossy().into_owned(), "clean-exit".into()],
        config(),
    )
    .with_isolation_profile(IsolationProfile {
        id: "test-review-contained",
        env_passthrough: &["PATH"],
        sandbox_primitive: Some(Box::leak(
            bwrap.to_string_lossy().into_owned().into_boxed_str(),
        )),
        ca_trust_paths: &[],
        commit_identity: None,
    });
    sup.consume_requests(&store, NOW).unwrap();
    let commands = port.spawned_commands();
    assert!(
        !commands.is_empty(),
        "the review launch (and its describe probe) spawn under bwrap"
    );
    let ws = sup.workspace_of(&w);
    // The describe probe already mounts the workspace read-only; the
    // regression was the launch itself mounting it writable. Every
    // workspace bind on this launch path is read-only.
    assert!(
        commands
            .iter()
            .any(|c| c.contains(&format!("--ro-bind {} /workspace", ws.display()))),
        "the review launch mounts the workspace read-only: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.contains(&format!("--bind {} /workspace", ws.display()))),
        "the review launch never mounts the workspace writable: {commands:?}"
    );
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
    let sup = supervisor(port.clone(), root, Some(FIXTURE_ISOLATION_PROFILE_ID));
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

/// §14.6 read-only regression: under a contained profile the mount
/// decision is per-role — the coding leg binds its workspace
/// writable, the invariant-check leg binds its snapshot read-only.
/// The recorded spawn argv carries the bwrap flags themselves:
/// `--bind` vs `--ro-bind` on `/workspace` is the boundary, not a
/// field on the request.
#[test]
fn a_contained_assessment_binds_the_workspace_read_only() {
    // Two capacity slots: the writer Execution and the check Execution
    // are both admitted `ready`.
    let store = SqliteStore::in_memory(2).unwrap();
    let root = tmp_root("contained-ro-mount");
    let port = ScriptedProcessControl::new();
    port.set_default_plan(SpawnPlan::Resident);
    // Exactly the writer launch's gate: the version probe plus the
    // nine-probe battery. Every later probe — each `describe` and the
    // check's own battery — falls through to the scripted
    // auto-declaration, whose `success: true` output passes a probe
    // and declares the adapter when the argv asks for one.
    for _ in 0..10 {
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        }));
    }
    let sup = Supervisor::new(port.clone(), root.clone(), vec!["/bin/sh".into()], config())
        .with_isolation_profile(IsolationProfile {
            id: "test-contained-ro-mount",
            env_passthrough: &[],
            sandbox_primitive: Some("sh"),
            ca_trust_paths: &[],
            commit_identity: None,
        });
    let (work, _eid) = submit_and_admit(&store, "k-ro-mount-writer");
    let (check_work, check_eid) = submit_and_admit(&store, "k-ro-mount-check");
    // The check launches only while its Execution stays `ready` — the
    // orchestrator re-issues it under `waiting → ready`, so it never
    // shares a pass with a just-spawned writer on the same Execution.
    // Retire its sibling's coding request so this pass carries a
    // writer launch and a check launch on separate Executions.
    let pending = store.transact(|tx| tx.pending_requests()).unwrap();
    let check_coding = pending
        .iter()
        .find(|request| request.execution_id == check_eid)
        .expect("the check Execution's coding request")
        .request_id
        .clone();
    store
        .transact(|tx| {
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: check_coding,
            })?;
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: format!("{}:inv:check", check_eid.0),
                    work_id: check_work.clone(),
                    execution_id: check_eid.clone(),
                    role: InvocationRole::InvariantCheck,
                    spec_revision: 1,
                    payload: serde_json::json!({"required_artifacts": []}),
                    adapter_binding: None,
                },
            })
        })
        .unwrap();
    sup.consume_requests(&store, NOW).unwrap();
    let commands = port.commands();
    assert_eq!(commands.len(), 2, "one writer launch plus the check");
    let ws = root.join(format!("ws-{}", work.0));
    assert!(
        commands[0].contains(&format!("--bind {} /workspace", ws.display())),
        "the writer binds its workspace writable: {}",
        commands[0]
    );
    assert!(
        commands[1].contains("--ro-bind") && commands[1].contains(" /workspace"),
        "the check's snapshot mounts read-only: {}",
        commands[1]
    );
    assert!(
        !commands[1].contains("--bind "),
        "no writable bind reaches the read-only check leg: {}",
        commands[1]
    );
}
