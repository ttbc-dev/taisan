//! Contract tests for the Worker permission boundary (U2.3, #109):
//! every worker launch and `describe` probe runs under the environment
//! the approved isolation profile builds — never the supervisor's
//! inherited environment — so no host credential (GitHub tokens, SSH
//! agent, git credential helpers, the owner's `~`) can reach the
//! worker, and an isolation configuration outside the approved
//! registry fails closed before any process exists.
//!
//! These are fixture/contract claims only: the profile's namespace,
//! egress, and runtime-credential machinery is the U2.4/U4 slice, and
//! live-host evidence requires separate authorization.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::ObservationFact;
use workloop_core::ports::WorkerEnv;
use workloop_core::store::Store;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker::isolation_profile;
use workloop_exec::isolation;
use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-isolation-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Contract launches are fixture-profile evidence: the reference
/// profile's bwrap boundary is proven by the containment suite, not
/// by these environment-hygiene tests.
fn fixture_sup<P: workloop_core::ports::ProcessControlPort>(
    port: P,
    root: PathBuf,
    argv: Vec<String>,
    cfg: WorkloopConfig,
) -> Supervisor<P> {
    Supervisor::new(port, root, argv, cfg).with_isolation_profile(
        isolation_profile(workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID).unwrap(),
    )
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

fn req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
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

fn request(store: &SqliteStore, execution: &ExecutionId) -> InvocationRequest {
    store
        .transact(|tx| tx.request(&format!("{}:inv:0", execution.0)))
        .unwrap()
        .expect("request")
}

fn observations(store: &SqliteStore, execution: &ExecutionId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_execution(execution))
        .unwrap()
        .into_iter()
        .filter_map(|observation| match observation.fact {
            ObservationFact::LaunchRefused { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
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

/// Scratch-tree names under `dir` whose name starts with `prefix` —
/// empty when the directory does not exist yet.
fn read_tree_names(dir: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(prefix))
                .collect()
        })
        .unwrap_or_default()
}

/// Names the profile passes through from the supervisor plus the names
/// the isolation layer generates — the child's whole closed set.
const ALLOWED_ENV_KEYS: &[&str] = &[
    "PATH",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "HOME",
    "TMPDIR",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_GLOBAL",
    "GIT_TERMINAL_PROMPT",
    "GIT_ALLOW_PROTOCOL",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
];

/// Credential and ambient-config names a host session commonly exports;
/// none of them may appear in a worker's environment.
const DENIED_ENV_KEYS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_CONFIG_DIR",
    "GH_ENTERPRISE_TOKEN",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "XDG_CONFIG_HOME",
    "TAISAN_CONFIG",
    "AWS_PROFILE",
    "KUBECONFIG",
];

fn env_map(env: &WorkerEnv) -> BTreeMap<String, String> {
    env.vars.iter().cloned().collect()
}

fn assert_scrubbed(env: &WorkerEnv, scratch_prefix: &Path) {
    let map = env_map(env);
    for denied in DENIED_ENV_KEYS {
        assert!(
            !map.contains_key(*denied),
            "{denied} must not reach a worker"
        );
    }
    for key in map.keys() {
        assert!(
            ALLOWED_ENV_KEYS.contains(&key.as_str()),
            "unexpected name {key} in the worker environment"
        );
    }
    let home = PathBuf::from(map.get("HOME").expect("HOME is generated"));
    assert!(
        home.starts_with(scratch_prefix) && home.ends_with("home"),
        "HOME must resolve inside the isolation scratch, got {}",
        home.display()
    );
    let gitconfig = PathBuf::from(map.get("GIT_CONFIG_GLOBAL").expect("generated"));
    assert!(
        gitconfig.starts_with(scratch_prefix),
        "GIT_CONFIG_GLOBAL must resolve inside the isolation scratch"
    );
}

/// A real spawned worker's environment contains only the allowlisted
/// names — never the supervisor's inherited credentials (INV-001,
/// INV-002). The fixture's `env-report` writes exactly what the kernel
/// gave the child, read back from the workspace.
#[test]
fn a_spawned_worker_runs_under_an_allowlisted_environment() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("spawnenv");
    let sup: Supervisor<PosixProcessControl> = fixture_sup(
        PosixProcessControl,
        root.clone(),
        vec![
            fixture_bin(),
            "env-report".into(),
            "--file".into(),
            "env.json".into(),
        ],
        config(),
    );
    let (work_id, _eid) = submit_and_admit(&store, "k-iso-1");
    sup.consume_requests(&store, NOW).unwrap();

    let workspace = sup.workspace_of(&work_id);
    let report = workspace.join("env.json");
    wait_for(|| report.exists(), "env-report artifact");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    let vars: BTreeMap<String, String> = value["vars"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();

    // The closed set: everything present was passed through or
    // generated — nothing inherited.
    for key in vars.keys() {
        assert!(
            ALLOWED_ENV_KEYS.contains(&key.as_str()),
            "unexpected name {key} in the spawned worker's environment"
        );
    }
    for denied in DENIED_ENV_KEYS {
        assert!(
            !vars.contains_key(*denied),
            "{denied} must not reach a worker"
        );
    }
    assert!(vars.contains_key("PATH"), "PATH passes through");

    // HOME resolves inside the installation-owned scratch — the real
    // owner's ~/.config/gh, ~/.ssh, ~/.git-credentials, and ~/.gitconfig
    // are unreachable through it (INV-002).
    let scratch = isolation::scratch_root(&root);
    let home = PathBuf::from(vars.get("HOME").expect("HOME is generated"));
    assert!(home.starts_with(&scratch), "HOME must be scratch-scoped");
    let owner_home = std::env::var("HOME").unwrap_or_default();
    assert_ne!(
        home.to_string_lossy(),
        owner_home,
        "the worker's HOME is never the owner's home"
    );
    for name in [
        ".config/gh",
        ".ssh",
        ".git-credentials",
        ".gitconfig",
        ".netrc",
    ] {
        assert!(
            !home.join(name).exists(),
            "the scratch HOME carries no host credential file {name}"
        );
    }
}

/// The `describe` probe and the launch see the same scrubbed
/// environment: a scripted port records each `WorkerEnv` the supervisor
/// passed, and every recorded env is the allowlisted shape (INV-006,
/// INV-001).
#[test]
fn the_describe_probe_runs_under_the_scrubbed_environment() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("probeenv");
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-2");
    sup.consume_requests(&store, NOW).unwrap();

    let request_id = request(&store, &eid).request_id;
    let probe_envs = port.probe_envs();
    assert_eq!(probe_envs.len(), 1, "exactly one describe probe ran");
    assert_scrubbed(
        &probe_envs[0],
        &isolation::scratch_root(&root)
            .join(format!("probe-{}", isolation::sanitize_key(&request_id))),
    );

    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    let spawn_envs = port.spawn_envs();
    assert_eq!(spawn_envs.len(), 1, "exactly one launch ran");
    assert_scrubbed(
        &spawn_envs[0],
        &isolation::scratch_root(&root)
            .join(format!("inv-{}", isolation::sanitize_key(&invocation.id.0))),
    );
}

/// The generated gitconfig is effective: git under the worker's
/// environment denies every remote transport and every credential
/// helper, and `GIT_TERMINAL_PROMPT=0` turns the remaining prompt into
/// a failure — `git credential fill` yields nothing (INV-003). The
/// file-local transport the handoff uses stays allowed. Because
/// repository-local `.git/config` outranks `GIT_CONFIG_GLOBAL`, the
/// same holds inside a controlled repository whose own config plants
/// the overrides: the command-scope `GIT_ALLOW_PROTOCOL=file` and
/// empty `credential.helper` pins beat anything the workspace writes
/// into `.git/config`.
#[test]
fn git_hygiene_denies_remote_transports_and_credential_helpers() {
    let scratch = tmp_root("githygiene").join("env");
    let env = isolation::prepare(
        &workloop_core::worker::default_isolation_profile(),
        &scratch,
    )
    .unwrap();
    let map = env_map(&env);

    let gitconfig = PathBuf::from(&map["GIT_CONFIG_GLOBAL"]);
    let body = std::fs::read_to_string(&gitconfig).unwrap();
    assert!(body.contains("helper ="), "{body}");
    assert!(body.contains("allow = never"), "{body}");
    assert!(body.contains("allow = always"), "{body}");

    let here = std::env::current_dir().expect("cwd");
    let run_git_in = |cwd: &Path, args: &[&str]| {
        std::process::Command::new("git")
            .env_clear()
            .envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run git")
    };
    let run_git = |args: &[&str]| run_git_in(&here, args);
    // A remote fetch is refused locally — before any network or
    // credential lookup runs.
    let out = run_git(&["ls-remote", "https://example.invalid/repo.git"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("transport 'https' not allowed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // No credential helper answers: `git credential fill` fails rather
    // than minting a repository-host credential.
    let mut fill = std::process::Command::new("git")
        .env_clear()
        .envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .args(["credential", "fill"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run git credential fill");
    use std::io::Write;
    fill.stdin
        .as_mut()
        .unwrap()
        .write_all(b"protocol=https\nhost=github.com\n\n")
        .unwrap();
    let out = fill.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "credential fill must not produce a credential"
    );
    // The file transport the handoff repository uses stays allowed.
    let local = tmp_root("githygiene-local").join("r.git");
    let out = run_git(&["init", "--bare", local.to_str().unwrap()]);
    assert!(out.status.success());
    let out = run_git(&["ls-remote", local.to_str().unwrap()]);
    assert!(out.status.success(), "file transport stays allowed");

    // A controlled repository whose own `.git/config` plants exactly
    // the overrides a workspace could try: a credential helper that
    // would drop a marker, and per-scheme transport grants. None of
    // them may take effect under the launch environment.
    let planted = tmp_root("githygiene-planted");
    let out = run_git(&["init", "-q", planted.to_str().unwrap()]);
    assert!(out.status.success());
    let marker = planted.join("helper-ran");
    let helper = format!("!touch {}", marker.display());
    for (key, value) in [
        ("credential.helper", helper.as_str()),
        ("protocol.https.allow", "always"),
        ("protocol.ssh.allow", "always"),
    ] {
        let out = run_git_in(&planted, &["config", key, value]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = run_git_in(&planted, &["ls-remote", "https://example.invalid/repo.git"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("transport 'https' not allowed"),
        "a repo-local protocol grant must not reopen https: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run_git_in(&planted, &["ls-remote", "ssh://example.invalid/repo.git"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("transport 'ssh' not allowed"),
        "a repo-local protocol grant must not reopen ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut fill = std::process::Command::new("git")
        .env_clear()
        .envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(&planted)
        .args(["credential", "fill"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run git credential fill");
    fill.stdin
        .as_mut()
        .unwrap()
        .write_all(b"protocol=https\nhost=github.com\n\n")
        .unwrap();
    let out = fill.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "credential fill must not produce a credential"
    );
    assert!(
        !marker.exists(),
        "the repo-local credential helper must never run"
    );
    // The local transport stays allowed inside the planted repository
    // too — the handoff path is unaffected by the pins.
    let planted_local = planted.join("r.git");
    let out = run_git_in(
        &planted,
        &["init", "-q", "--bare", planted_local.to_str().unwrap()],
    );
    assert!(out.status.success());
    let out = run_git_in(&planted, &["ls-remote", planted_local.to_str().unwrap()]);
    assert!(out.status.success(), "file transport stays allowed");

    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(&planted);
}

/// An isolation profile name outside the approved registry resolves to
/// nothing — config validation and any resolution at the launch
/// boundary refuse it rather than degrading to unprofiled execution
/// (INV-004).
#[test]
fn an_unknown_isolation_profile_id_fails_closed() {
    assert!(isolation_profile("taisan-reference-v0.1").is_some());
    assert!(isolation_profile("fixture-worker-v0.1").is_some());
    for id in ["bogus-profile", "none", "", "taisan-reference-v0.2"] {
        assert!(
            isolation_profile(id).is_none(),
            "unapproved profile '{id}' must not resolve"
        );
    }
}

/// A workspace root whose *launch* environment cannot be created
/// refuses the launch through the launch path's `refuse_launch` — the
/// describe probe ran under a working scratch first, so the refusal is
/// the supervisor's launch branch, not the probe's fail-closed
/// selection: the Invocation is abandoned, no worker process is
/// started, and the request is terminal for its binding (INV-005).
#[test]
fn a_failed_environment_preparation_refuses_the_launch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("envfail");
    // The first Invocation on a fresh store is `inv-1`; its launch
    // scratch key is `inv-inv-1` (the `inv-` scratch prefix plus the
    // Invocation id). That tree is occupied by a regular file while
    // the `probe-*` tree stays preparable, so only the launch
    // preparation fails.
    let iso = root.join(".isolation");
    std::fs::create_dir_all(&iso).unwrap();
    std::fs::write(iso.join("inv-inv-1"), "occupied").unwrap();
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-3");
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(
        port.probe_envs().len(),
        1,
        "the describe probe ran under the working scratch"
    );
    assert_eq!(
        port.spawn_calls(),
        0,
        "no worker process started under a broken launch environment"
    );
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(
        invocation.id.0, "inv-1",
        "the occupied scratch key inv-inv-1 belongs to this Invocation"
    );
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let reasons = observations(&store, &eid);
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("isolation.environment_preparation_failed")),
        "the retained evidence names the refusal: {reasons:?}"
    );

    // The refusal is durable: a later pass does not retry a launch
    // under a degraded environment.
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(port.spawn_calls(), 0);
}

/// A workspace root whose isolation scratch cannot even be created
/// fails closed at the describe probe: no probe runs, nothing binds,
/// and the Invocation is abandoned with the same recorded
/// `LaunchRefused` evidence (INV-005).
#[test]
fn a_failed_probe_environment_fails_closed_before_binding() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("probefail");
    // `.isolation` occupied by a regular file: no scratch under it —
    // probe or launch — can be prepared.
    std::fs::write(root.join(".isolation"), "occupied").unwrap();
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-5");
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(
        port.probe_envs().len(),
        0,
        "no probe ran under a broken isolation root"
    );
    assert_eq!(port.spawn_calls(), 0, "no worker process started");
    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let reasons = observations(&store, &eid);
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("isolation.environment_preparation_failed")),
        "the retained evidence names the refusal: {reasons:?}"
    );
}

/// Scratch trees are reclaimed with their lifecycle: the probe's tree
/// as soon as the declaration returns, the writer's tree when the
/// Invocation is proven finished — `.isolation` never accumulates one
/// dead tree per request, and nothing outside a live probe or writer
/// holds worker-writable `HOME`/`TMPDIR` storage.
#[test]
fn isolation_scratch_trees_are_reclaimed_with_their_lifecycle() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("scratchrelease");
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-4");
    sup.consume_requests(&store, NOW).unwrap();

    let iso = isolation::scratch_root(&root);
    assert!(
        read_tree_names(&iso, "probe-").is_empty(),
        "the probe scratch is reclaimed as soon as describe returns"
    );
    assert_eq!(
        read_tree_names(&iso, "inv-").len(),
        1,
        "the running writer keeps its scratch tree"
    );

    port.exit_all(0);
    sup.poll_invocations(&store, NOW).unwrap();

    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(invocation.state, InvocationState::Done);
    let leftovers = read_tree_names(&iso, "");
    assert!(
        leftovers.is_empty(),
        "a finished Invocation leaves no scratch tree behind: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Spawn outcomes decide the launch scratch: a refused spawn starts no
/// process, so its fully built tree is reclaimed with the refusal.
#[test]
fn a_refused_spawn_reclaims_the_launch_scratch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("spawnrefused");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Refused {
        reason: "no slot".into(),
    });
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-6");
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(port.spawn_calls(), 1);
    let iso = isolation::scratch_root(&root);
    assert!(
        read_tree_names(&iso, "probe-").is_empty(),
        "the probe scratch is reclaimed"
    );
    assert!(
        !iso.join("inv-inv-1").exists(),
        "a refused spawn's launch scratch is reclaimed — no process exists"
    );
    let refused = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(refused.state, InvocationState::Abandoned);
    let reasons = observations(&store, &eid);
    assert!(
        reasons.iter().any(|r| r.contains("no slot")),
        "the refusal reason is recorded: {reasons:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An unknown spawn outcome keeps the launch scratch: the process may
/// be live under that `HOME`/`TMPDIR`, and reclaiming it would yank
/// the writer's filesystem out from under it.
#[test]
fn an_unknown_spawn_outcome_retains_the_launch_scratch() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("spawnunknown");
    let port = ScriptedProcessControl::new();
    port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, eid) = submit_and_admit(&store, "k-iso-7");
    sup.consume_requests(&store, NOW).unwrap();

    assert_eq!(port.spawn_calls(), 1);
    let iso = isolation::scratch_root(&root);
    assert!(
        iso.join("inv-inv-1").exists(),
        "an unknown spawn outcome keeps its launch scratch"
    );
    let unknown = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(
        unknown.state,
        InvocationState::Prepared,
        "an unknown start outcome is never resolved by a guess"
    );
    assert!(unknown.process_identity.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// A launch stopped through `drive_stops` — the cancel/fail/pause path
/// the poll pass skips — releases its scratch tree with the proven
/// exit, exactly like the poll path: `.isolation` never keeps an
/// `inv-*` tree for a writer already proven stopped (#174).
#[test]
fn a_stop_driven_launch_reclaims_its_scratch_tree() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("stoprelease");
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (work_id, eid) = submit_and_admit(&store, "k-iso-stop");
    sup.consume_requests(&store, NOW).unwrap();

    let iso = isolation::scratch_root(&root);
    assert_eq!(
        read_tree_names(&iso, "inv-").len(),
        1,
        "the running writer keeps its scratch tree"
    );

    // Cancel drives the Execution into `stopping`; the poll path skips
    // it, so `drive_stops` owns the exit fact — and the launch
    // scratch's reclamation.
    let version = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .map(|w| w.version)
        .expect("work exists");
    let d = handle_command(
        &store,
        &config(),
        &req(
            "k-iso-stop-cancel",
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
    sup.drive_stops(&store, NOW).unwrap();

    let invocation = store
        .transact(|tx| tx.invocations_for_execution(&eid))
        .unwrap()
        .remove(0);
    assert_eq!(invocation.state, InvocationState::Done);
    let leftovers = read_tree_names(&iso, "inv-");
    assert!(
        leftovers.is_empty(),
        "a stop-driven launch leaves no scratch tree behind: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `.isolation` orphans are bounded once per supervisor pass: scratch
/// trees no unresolved Invocation still owns — a crashed launch's
/// `inv-*` leftover, an interrupted probe's `probe-*` tree — are
/// reclaimed, while a live writer's tree and anything outside the
/// owned prefixes are never touched (#174).
#[test]
fn isolation_scratch_orphans_are_swept_but_live_trees_are_kept() {
    let store = SqliteStore::in_memory(1).unwrap();
    let root = tmp_root("orphansweep");
    let port = ScriptedProcessControl::new();
    let sup = fixture_sup(
        port.clone(),
        root.clone(),
        vec!["fixture-worker".into()],
        config(),
    );
    let (_work_id, _eid) = submit_and_admit(&store, "k-iso-orphan");
    sup.consume_requests(&store, NOW).unwrap();

    let iso = isolation::scratch_root(&root);
    // Orphan trees a crashed pass could leave: an `inv-*` key no
    // Invocation owns and a `probe-*` tree whose declaration never
    // returned. A foreign entry is never the sweep's to touch.
    for orphan in ["inv-inv-999", "probe-dead-request"] {
        let dir = iso.join(orphan);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(dir.join("gitconfig"), "x").unwrap();
    }
    let foreign = iso.join("not-ours");
    std::fs::create_dir_all(&foreign).unwrap();

    sup.run_once(&store, NOW).unwrap();

    assert!(
        !iso.join("inv-inv-999").exists(),
        "an orphaned launch scratch is reclaimed"
    );
    assert!(
        !iso.join("probe-dead-request").exists(),
        "an interrupted probe's scratch is reclaimed"
    );
    assert!(
        iso.join("inv-inv-1").exists(),
        "the live writer's scratch is kept"
    );
    assert!(foreign.exists(), "foreign entries are never touched");
    let _ = std::fs::remove_dir_all(&root);
}
