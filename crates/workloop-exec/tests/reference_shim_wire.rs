//! Reference-adapter end-to-end leg (issue #366): the public
//! `taisan-worker-claude` Worker — the real runtime adapter driving a
//! coding-agent CLI — exercised through the real dispatch path, not
//! invoked directly. A scriptable fake `claude` executable stands in
//! for the actual runtime so the launches prove the full supervisor
//! surface: declaration probe and negotiation, durable request
//! materialization, the process boundary, bounded report collection,
//! and durable classification — with no model call and no credential.
//!
//! Evidence class: fixture evidence only — the fake runtime is test
//! infrastructure; these rows never claim live-provider behavior.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::ObservationFact;
use workloop_core::ports::ProcessControlPort;
use workloop_core::store::Store;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker_v2::ReportClassification;
use workloop_exec::bodies::BodyStore;
use workloop_exec::handoff::HandoffLayout;
use workloop_exec::request_v2::{InvariantSetConfig, V2BudgetBounds, V2Dispatch};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_exec::workspace::RepositorySource;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn shim_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/taisan-worker-claude")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-ref-shim-{tag}-{}-{}",
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

/// A scriptable fake `claude` runtime. `--version` answers the
/// describe/preflight probe; otherwise it emits the stream-json
/// result event baked into `FAKE_RESULT` / `FAKE_EVENT_JSON`, runs
/// the optional workspace actions, and exits with `exit`.
fn fake_runtime(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

const FAKE_SUCCESS: &str = r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = "--version" ]; then echo "fake-claude 1.0.0"; exit 0; fi
done
printf 'produced\n' > produced.txt
git add -A >/dev/null 2>&1
git -c user.email=fake@test -c user.name=fake commit -qm fake >/dev/null 2>&1
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":2,"duration_ms":10}'
exit 0
"#;

const FAKE_ERROR: &str = r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = "--version" ]; then echo "fake-claude 1.0.0"; exit 0; fi
done
printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider exploded"}'
exit 1
"#;

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

/// A source repository with two commits: `base` and `newer` (HEAD —
/// the Coding request's base object).
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

fn host() -> HostInstanceId {
    HostInstanceId::trusted("fixture")
}

fn repo() -> RepositoryRef {
    RepositoryRef {
        host: host(),
        repository: RepositoryId::trusted("repo"),
    }
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
            HostCapability::IssueRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget {
            // The request deadline anchors at the Execution's recorded
            // start (the test clock's NOW) — a real wall-clock runtime
            // must still see it in the future, so the limit spans
            // both clocks.
            runtime_limit_seconds: 100 * 365 * 24 * 3600,
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

fn admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId, String) {
    let cfg = config();
    let d = handle_command(
        store,
        &cfg,
        &CommandRequest {
            actor: actor(),
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
        .transact(|tx| admit_queued(tx, &cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
}

fn dispatch_profile(state_root: &Path, src: &Path) -> V2Dispatch {
    V2Dispatch {
        bodies: BodyStore::new(state_root.join("bodies")),
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
        capabilities: vec!["invocation_transport.argv-v2".into(), "report_json".into()],
        verification_egress_hosts: vec![],
    }
}

fn layout(root: &Path) -> HandoffLayout {
    let bare = root.join("handoff.git");
    git(root, &["init", "--bare", bare.to_str().unwrap()]);
    HandoffLayout {
        repository: bare,
        workspace_root: root.join("ws"),
        out_root: root.join("out"),
        state_root: None,
        verification: None,
        verification_placement: workloop_core::placement::PlacementTarget::Local,
        remote_verification_executor: None,
    }
}

/// A v2 supervisor bound to the reference shim argv — the fixture
/// profile: these launches prove the wire path, not the bwrap
/// boundary.
fn shim_sup(
    root: &Path,
    state_root: &Path,
    src: &Path,
    argv: Vec<String>,
) -> Supervisor<PosixProcessControl> {
    let handoff = layout(root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    Supervisor::new(
        PosixProcessControl,
        handoff.workspace_root.clone(),
        argv,
        config(),
    )
    .with_isolation_profile(
        workloop_core::worker::isolation_profile(
            workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
        )
        .unwrap(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(state_root, src))
}

fn request(store: &SqliteStore, execution: &ExecutionId) -> InvocationRequest {
    store
        .transact(|tx| tx.request(&format!("{}:inv:0", execution.0)))
        .unwrap()
        .expect("durable request")
}

fn invocation_state(store: &SqliteStore, request_id: &str) -> Option<InvocationState> {
    store
        .transact(|tx| tx.invocation_for_request(request_id))
        .unwrap()
        .map(|i| i.state)
}

fn drive_to_done<P: ProcessControlPort>(sup: &Supervisor<P>, store: &SqliteStore, rid: &str) {
    for _ in 0..200 {
        sup.run_once(store, NOW).unwrap();
        if invocation_state(store, rid) == Some(InvocationState::Done) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("invocation {rid} did not resolve Done");
}

fn report_classifications(
    store: &SqliteStore,
    execution_id: &ExecutionId,
) -> Vec<ReportClassification> {
    store
        .transact(|tx| tx.observations_for_execution(execution_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::WorkerReport { classification, .. } => Some(classification),
            _ => None,
        })
        .collect()
}

fn launch_refusals(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::LaunchRefused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

// ----------------------------------------------------------------------

/// The reference adapter passes the complete real launch path: the
/// supervisor probes `describe`, negotiates the declaration, records
/// the adapter binding, materializes the request and inputs, spawns
/// `run`, collects the bounded report — and the produced workspace
/// commit arrives as the claimed head.
#[test]
fn shim_completes_a_coding_invocation_through_the_real_launch_path() {
    let root = tmp_root("coding");
    let (src, _base, _newer) = git_source(&root);
    let state = root.join("state");
    let store = SqliteStore::in_memory(4).unwrap();
    let (work_id, eid, rid) = admit(&store, "shim-coding");
    let _ = work_id;
    let fake = fake_runtime(&root, "fake-claude", FAKE_SUCCESS);
    let argv = vec![
        shim_bin(),
        "--runtime".into(),
        fake.to_string_lossy().into_owned(),
    ];
    let sup = shim_sup(&root, &state, &src, argv);
    drive_to_done(&sup, &store, &rid);

    // The real `describe` bound this exact adapter identity to the
    // durable request — the negotiation surface, not a name we chose.
    let binding = request(&store, &eid)
        .adapter_binding
        .expect("adapter binding recorded");
    assert_eq!(binding.adapter.as_str(), "taisan-worker-claude");

    // The report the shim wrote classified `success` through the
    // supervisor's own envelope validation — the produced commit is a
    // claim the handoff can now verify independently.
    let classes = report_classifications(&store, &eid);
    assert!(
        classes
            .iter()
            .any(|c| matches!(c, ReportClassification::Success)),
        "expected a success-classified report, got {classes:?}"
    );
}

/// The same wire path refuses the launch fail-closed when the bound
/// runtime cannot answer `describe`: no invocation, no spawned writer,
/// a typed refusal on the durable record.
#[test]
fn shim_launch_is_refused_when_the_runtime_is_missing() {
    let root = tmp_root("missing");
    let (src, _base, _newer) = git_source(&root);
    let state = root.join("state");
    let store = SqliteStore::in_memory(4).unwrap();
    let (work_id, eid, rid) = admit(&store, "shim-missing");
    let _ = eid;
    let argv = vec![
        shim_bin(),
        "--runtime".into(),
        "/no/such/claude-binary".into(),
    ];
    let sup = shim_sup(&root, &state, &src, argv);
    for _ in 0..50 {
        sup.run_once(&store, NOW).unwrap();
        if !launch_refusals(&store, &work_id).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let refusals = launch_refusals(&store, &work_id);
    assert!(
        refusals
            .iter()
            .any(|r| r.starts_with("adapter.declaration_unavailable")),
        "expected adapter.declaration_unavailable, got {refusals:?}"
    );
    // The Invocation row records the launch intent; a refused
    // declaration must never have spawned a writer process.
    let spawned = store
        .transact(|tx| tx.invocation_for_request(&rid))
        .unwrap()
        .map(|i| i.process_identity.is_some())
        .unwrap_or(false);
    assert!(!spawned, "a refused declaration must never spawn a writer");
}

/// A runtime that reports an execution error arrives as the contract's
/// `unavailable` class through the same bounded ingestion — never as
/// a positive result.
#[test]
fn shim_maps_runtime_failure_to_unavailable_through_the_supervisor() {
    let root = tmp_root("rt-fail");
    let (src, _base, _newer) = git_source(&root);
    let state = root.join("state");
    let store = SqliteStore::in_memory(4).unwrap();
    let (work_id, eid, rid) = admit(&store, "shim-rtfail");
    let _ = work_id;
    let fake = fake_runtime(&root, "fake-claude-error", FAKE_ERROR);
    let argv = vec![
        shim_bin(),
        "--runtime".into(),
        fake.to_string_lossy().into_owned(),
    ];
    let sup = shim_sup(&root, &state, &src, argv);
    drive_to_done(&sup, &store, &rid);

    let classes = report_classifications(&store, &eid);
    assert!(
        classes
            .iter()
            .any(|c| matches!(c, ReportClassification::Unavailable)),
        "expected an unavailable-classified report, got {classes:?}"
    );
}
