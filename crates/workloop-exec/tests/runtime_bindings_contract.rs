//! The worker-neutral runtime binding delivery contract
//! (docs/runtime-bindings.md, docs/v0.3.md §14.3–§14.4, issue #460):
//! the generated launch argv, mount plan, and environment every
//! contained managed-Worker launch receives — the versioned
//! `taisan.worker.bindings/1` provider document, the per-Invocation
//! staged runtime credential, the allowlist environment, and the
//! fail-closed CONNECT proxy — proven with dummy credentials and
//! fixture paths only.
//!
//! **Evidence boundary.** Everything here is code/fixture evidence:
//! it asserts the exact argv, binds, and environment the supervisor
//! *generates* — never that a real `bwrap` executed them. Kernel
//! enforcement (read-only binds denying writes, `--unshare-net`
//! leaving only loopback) is proven by the containment preflight
//! battery, which requires a real Linux host and is not claimed
//! here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::ports::WorkerEnv;
use workloop_core::work::{HostTemplate, WorkloopConfig};
use workloop_exec::boundary::{BoundaryConfig, RuntimeBoundary};
use workloop_exec::containment::{self, Gate, MountPlan};
use workloop_exec::scripted::ScriptedProcessControl;
use workloop_exec::supervisor::Supervisor;

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-bindings-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config() -> WorkloopConfig {
    let host = HostInstanceId::trusted("fixture");
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: RepositoryRef::new(host.clone(), RepositoryId::trusted("repo")),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host,
                    actor: HostActorId::trusted("pub-agent"),
                },
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::from([HostCapability::RepositoryRead]),
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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

fn boundary_config(dir: &Path) -> BoundaryConfig {
    BoundaryConfig {
        credential_ref: None,
        provider_config: None,
        model_api_hosts: vec![],
        runtime_dir: dir.join("runtime"),
        egress_dir: dir.join("egress"),
        relay_exe: PathBuf::from("/bin/sh"),
    }
}

/// A minimal valid `taisan.worker.bindings/1` document.
fn write_bindings(dir: &Path) -> PathBuf {
    let path = dir.join("worker-bindings.json");
    std::fs::write(
        &path,
        "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"model-a\",\"endpoint\":\"https://api.example\"}\n",
    )
    .unwrap();
    path
}

fn write_secret_file(dir: &Path, name: &str, value: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, value).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// Whether an argv bind source is the owner's home directory or an
/// ancestor of it — the contract refuses exactly that. A path
/// strictly beneath the home binds only the named path while the
/// tmpfs `/home` keeps the rest invisible, so the comparison is a
/// component-wise ancestor check, never a string prefix.
fn bind_source_is_home_or_ancestor(source: &str, home: &Path) -> bool {
    home.starts_with(source)
}

/// A contained probe launch through the real supervisor boundary
/// path: the same grant → mount plan → outer argv → contained env the
/// dispatch path produces.
fn contained_launch(
    dir: &Path,
    boundary: RuntimeBoundary,
    worker_argv: &[String],
) -> Result<(Vec<String>, WorkerEnv), String> {
    let sup = Supervisor::new(
        ScriptedProcessControl::new(),
        dir.join("workspaces"),
        worker_argv.to_vec(),
        config(),
    )
    .with_runtime_boundary(boundary);
    let worker = dir.join("worker-bin");
    std::fs::write(&worker, "#!/bin/sh\n").unwrap();
    let gitconfig = dir.join("gitconfig");
    std::fs::write(&gitconfig, "[user]\n\tname = t\n").unwrap();
    let gate = Gate::Ready {
        primitive: PathBuf::from("/bin/sh"),
        worker: Some(worker),
    };
    sup.contained_probe(&gate, worker_argv, None, &gitconfig, &WorkerEnv::default())
}

/// The credential is resolved and staged fresh for every grant: a
/// `file:` provider whose value rotates between two launches hands
/// the *latest* staged content to the next Invocation — never the
/// value an earlier grant cached.
#[test]
fn each_invocation_observes_the_latest_staged_credential() {
    let dir = tmp("latest");
    let source = write_secret_file(&dir, "token", "dummy-credential-v1");
    let mut cfg = boundary_config(&dir);
    cfg.credential_ref = Some(format!("file:{}", source.display()));
    let boundary = RuntimeBoundary::new(cfg);

    let first = boundary.ensure().unwrap();
    let staged = first.credential_file.expect("the first grant stages");
    assert_eq!(std::fs::read(&staged).unwrap(), b"dummy-credential-v1");

    // The operator rotates the credential in the provider's own
    // storage; the next grant — the next Invocation's launch —
    // observes the new value at the same mounted path.
    std::fs::write(&source, "dummy-credential-v2").unwrap();
    let second = boundary.ensure().unwrap();
    let staged2 = second.credential_file.expect("the second grant stages");
    assert_eq!(
        std::fs::read(&staged2).unwrap(),
        b"dummy-credential-v2",
        "a rotated provider value must reach the next grant"
    );
    drop(boundary);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Invalid or missing required bindings refuse the launch with a
/// bounded, typed diagnostic — never a partial mount and never an
/// echo of the document's content.
#[test]
fn invalid_or_missing_bindings_refuse_with_bounded_diagnostic() {
    let dir = tmp("refuse");
    for (name, content, expect) in [
        ("absent.json", "", "provider_config.unreadable"),
        (
            "malformed.json",
            "not json",
            "provider_config.contract_invalid",
        ),
        (
            "wrongversion.json",
            "{\"format_version\":\"taisan.worker.bindings/2\",\"model\":\"m\"}",
            "provider_config.contract_version",
        ),
        (
            "nomodel.json",
            "{\"format_version\":\"taisan.worker.bindings/1\"}",
            "'model' is required",
        ),
        (
            "embedded.json",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"dummy-secret-9\"}",
            "provider_config.contains_credential",
        ),
    ] {
        let path = dir.join(name);
        if name != "absent.json" {
            std::fs::write(&path, content).unwrap();
        }
        let mut cfg = boundary_config(&dir);
        cfg.provider_config = Some(path);
        cfg.credential_ref = None;
        if name == "embedded.json" {
            let source = write_secret_file(&dir, "tok", "dummy-secret-9");
            cfg.credential_ref = Some(format!("file:{}", source.display()));
        }
        let boundary = RuntimeBoundary::new(cfg);
        let err = boundary.ensure().unwrap_err();
        assert!(err.contains(expect), "{name}: {err} expected {expect}");
        assert!(err.len() < 240, "diagnostics stay bounded: {err}");
    }
    // A directory is not a contract document.
    let mut cfg = boundary_config(&dir);
    cfg.provider_config = Some(dir.clone());
    let err = RuntimeBoundary::new(cfg).ensure().unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every refusal unstages: a launch that cannot mount its bindings
/// leaves no staged credential file or staging sibling behind.
#[test]
fn a_refusal_leaves_no_staged_credential() {
    let dir = tmp("unstage");
    // SAFETY: test-only env mutation; the name is unique to this test.
    unsafe { std::env::set_var("TAISAN_BINDINGS_DUMMY_TOKEN", "dummy-secret-8") };
    let mut cfg = boundary_config(&dir);
    cfg.credential_ref = Some("env:TAISAN_BINDINGS_DUMMY_TOKEN".into());
    cfg.provider_config = Some(dir.join("absent.json"));
    let boundary = RuntimeBoundary::new(cfg);
    let err = boundary.ensure().unwrap_err();
    assert!(err.starts_with("provider_config."), "{err}");
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("runtime"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("runtime-auth"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "no staged credential survives a refusal"
    );
    unsafe { std::env::remove_var("TAISAN_BINDINGS_DUMMY_TOKEN") };
    let _ = std::fs::remove_dir_all(&dir);
}

/// The generated outer argv and environment for a fully-boundary
/// launch: the staged credential, the contract document, and the
/// proxy socket all bind read-only at their fixed sandbox paths; the
/// inner argv is relay-wrapped; the environment exports the relay
/// proxy URL on both spellings — and the credential *value* appears
/// in neither.
#[test]
fn generated_argv_matches_the_fixed_mount_surface() {
    let dir = tmp("surface");
    // SAFETY: test-only env mutation.
    unsafe { std::env::set_var("TAISAN_BINDINGS_DUMMY_CRED", "dummy-credential-marker") };
    let bindings = write_bindings(&dir);
    let mut cfg = boundary_config(&dir);
    cfg.credential_ref = Some("env:TAISAN_BINDINGS_DUMMY_CRED".into());
    cfg.provider_config = Some(bindings.clone());
    cfg.model_api_hosts = vec!["model.example".into()];
    let inner = vec!["/pkg/bin/managed-worker".to_string(), "run".to_string()];
    let (argv, env) = contained_launch(&dir, RuntimeBoundary::new(cfg), &inner).unwrap();
    let joined = argv.join("\n");

    for flag in ["--unshare-net", "--unshare-pid", "--unshare-ipc"] {
        assert!(joined.contains(flag), "namespace flag missing: {joined}");
    }
    // Every supervisor-owned grant binds read-only at its fixed path —
    // paired flag+dest on the same bind, never a global flag presence
    // an unrelated ro-bind could satisfy while a grant mounts writable.
    let binds: Vec<&[String]> = argv
        .windows(3)
        .filter(|w| w[0] == "--bind" || w[0] == "--ro-bind")
        .collect();
    for dest in [
        "/taisan/runtime-auth",
        "/taisan/provider-config",
        "/taisan/proxy.sock",
        "/taisan/gitconfig",
    ] {
        let mounts: Vec<&&[String]> = binds.iter().filter(|w| w[2] == dest).collect();
        assert!(!mounts.is_empty(), "missing mount {dest}:\n{joined}");
        assert!(
            mounts.iter().all(|w| w[0] == "--ro-bind"),
            "{dest} must bind read-only:\n{joined}"
        );
    }
    // The staged credential, contract document, and proxy socket each
    // bind their own host source read-only at the fixed sandbox
    // spelling — the source is paired with the flag and dest on the
    // same bind, not merely present somewhere in argv.
    let bind_at = |dest: &str| {
        binds
            .iter()
            .find(|w| w[2] == dest)
            .unwrap_or_else(|| panic!("missing bind for {dest}:\n{joined}"))
    };
    let auth = bind_at("/taisan/runtime-auth");
    assert_eq!(auth[0], "--ro-bind");
    assert!(
        auth[1].starts_with(dir.join("runtime").to_str().unwrap())
            && auth[1].contains("runtime-auth"),
        "the staged credential source must be the staged file: {auth:?}"
    );
    let provider = bind_at("/taisan/provider-config");
    assert_eq!(provider[0], "--ro-bind");
    assert_eq!(provider[1], bindings.to_str().unwrap());
    let sock = bind_at("/taisan/proxy.sock");
    assert_eq!(sock[0], "--ro-bind");
    assert!(sock[1].ends_with("worker.sock"), "{sock:?}");
    // The relay is the sandbox init serving the proxy to the worker
    // tree — argv leads with it.
    assert!(joined.contains("__sandbox-relay"), "{joined}");
    let env_map: std::collections::BTreeMap<_, _> = env.vars.iter().cloned().collect();
    assert_eq!(
        env_map["HTTPS_PROXY"], "http://127.0.0.1:18080",
        "the relay URL reaches the worker"
    );
    assert_eq!(env_map["https_proxy"], "http://127.0.0.1:18080");
    // The credential *value* never reaches argv or env.
    assert!(!joined.contains("dummy-credential-marker"), "{joined}");
    assert!(
        !env.vars
            .iter()
            .any(|(_, v)| v.contains("dummy-credential-marker")),
        "{:?}",
        env.vars
    );
    // Nothing exposes the host root, a container socket, or the
    // owner's home as a bind source.
    let sources: Vec<&str> = argv
        .windows(3)
        .filter(|w| w[0] == "--bind" || w[0] == "--ro-bind")
        .map(|w| w[1].as_str())
        .collect();
    assert!(
        !sources.contains(&"/"),
        "host root must never be bound:\n{joined}"
    );
    for forbidden in ["docker.sock", "containerd.sock"] {
        assert!(
            !sources.iter().any(|s| s.contains(forbidden)),
            "{forbidden} must never be bound:\n{joined}"
        );
    }
    let home = std::env::var("HOME").unwrap_or_default();
    assert!(
        home.is_empty()
            || !sources
                .iter()
                .any(|s| bind_source_is_home_or_ancestor(s, Path::new(&home))),
        "the owner's home must never be a bind source:\n{joined}"
    );
    unsafe { std::env::remove_var("TAISAN_BINDINGS_DUMMY_CRED") };
    let _ = std::fs::remove_dir_all(&dir);
}

/// The home-guard assertion in the mount-surface test mirrors the
/// contract exactly: only the home directory itself or an ancestor
/// of it is a forbidden source — a path strictly beneath it is
/// admissible, so a HOME that happens to prefix the fixture's temp
/// dir (e.g. HOME=/tmp) must not fail the suite.
#[test]
fn bind_source_home_ancestry_matches_the_contract() {
    let home = Path::new("/home/owner");
    for forbidden in ["/home/owner", "/home", "/"] {
        assert!(
            bind_source_is_home_or_ancestor(forbidden, home),
            "{forbidden} must be refused"
        );
    }
    for admissible in [
        "/home/owner/sub",
        "/home/owner2",
        "/tmp/workloop-bindings-fixture",
        "/var/folders/t",
    ] {
        assert!(
            !bind_source_is_home_or_ancestor(admissible, home),
            "{admissible} must stay admissible"
        );
    }
    // The reported environment: HOME is a strict prefix of the temp
    // dir the fixtures build under — every fixture source sits
    // strictly beneath it and stays admissible.
    let home = Path::new("/tmp");
    assert!(
        !bind_source_is_home_or_ancestor("/tmp/workloop-bindings-surface-1/worker-bin", home),
        "a source beneath HOME stays admissible"
    );
}

/// No allowlist means no proxy capability and no proxy environment:
/// no proxy.sock bind, no relay wrap, no *PROXY* variable — egress
/// stays fully denied by the unshared network namespace.
#[test]
fn no_allowlist_means_no_proxy_capability_or_env() {
    let dir = tmp("noegress");
    let mut cfg = boundary_config(&dir);
    cfg.provider_config = Some(write_bindings(&dir));
    // Empty model_api_hosts → no proxy at all.
    let inner = vec!["/pkg/bin/managed-worker".to_string()];
    let (argv, env) = contained_launch(&dir, RuntimeBoundary::new(cfg), &inner).unwrap();
    let joined = argv.join("\n");
    for absent in ["/taisan/proxy.sock", "__sandbox-relay"] {
        assert!(!joined.contains(absent), "{joined}");
    }
    assert!(
        !env.vars
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("https_proxy")
                || k.eq_ignore_ascii_case("http_proxy")),
        "{:?}",
        env.vars
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The environment sent to a managed Worker is an allowlist: only the
/// profile's passthrough names plus generated entries — generated
/// HOME/TMPDIR/gitconfig, the git transport/credential pins, commit
/// identity, declared CA settings — and the proxy pair when egress is
/// granted. No ambient supervisor variable arrives unlisted.
#[test]
fn the_contained_environment_is_an_allowlist() {
    let dir = tmp("env");
    // SAFETY: test-only env mutation; the marker must not pass through.
    unsafe { std::env::set_var("TAISAN_BINDINGS_AMBIENT", "ambient-marker") };
    let profile = workloop_core::worker::default_isolation_profile();
    let scratch = dir.join("scratch");
    let base = workloop_exec::isolation::prepare(&profile, &scratch).unwrap();
    let env = containment::contained_env(&base, Some(workloop_exec::egress::RELAY_PROXY_URL));
    let names: BTreeSet<_> = env.vars.iter().map(|(k, _)| k.as_str()).collect();
    // The whole surface: passthrough + generated + proxy.
    for expected in [
        "PATH",
        "HOME",
        "TMPDIR",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_TERMINAL_PROMPT",
        "GIT_ALLOW_PROTOCOL",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "HTTPS_PROXY",
        "https_proxy",
    ] {
        assert!(
            names.contains(expected),
            "missing generated variable {expected}"
        );
    }
    assert!(
        !names.contains("TAISAN_BINDINGS_AMBIENT"),
        "an ambient variable never arrives unlisted"
    );
    for denied in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "SSH_AUTH_SOCK",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        assert!(
            !names.contains(denied),
            "{denied} must never reach the sandbox"
        );
    }
    assert_eq!(
        env.vars
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v.as_str()),
        Some("/home")
    );
    assert_eq!(
        env.vars
            .iter()
            .find(|(k, _)| k == "TMPDIR")
            .map(|(_, v)| v.as_str()),
        Some("/tmp")
    );
    unsafe { std::env::remove_var("TAISAN_BINDINGS_AMBIENT") };
    let _ = std::fs::remove_dir_all(&dir);
}

/// A packaged managed Worker runtime — the executable and every
/// required runtime file the layout names — reaches the sandbox
/// through the narrow read-only binds: paths beneath the fixed system
/// prefixes ride the wholesale `/usr`-family binds, and every
/// absolute path the argv names outside them is bound read-only at
/// its own path. Nothing packaged ever binds read-write.
#[test]
fn packaged_runtime_files_stay_under_the_narrow_read_only_binds() {
    let dir = tmp("pkg");
    // The packaging example's layout shape: a fixed package root
    // holding the launcher, runtime dependencies, and package files.
    let pkg = dir.join("pkg");
    for member in [
        "bin/managed-worker",
        "dist/main.js",
        "node_modules/pkg/index.js",
        "runtime/agent-runtime.js",
    ] {
        let path = pkg.join(member);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "// packaged runtime file\n").unwrap();
    }
    // Every package path the launch names is a read-only own-path
    // bind — the contract never mounts a packaged file writable.
    let named: Vec<String> = [
        "bin/managed-worker",
        "dist/main.js",
        "runtime/agent-runtime.js",
    ]
    .iter()
    .map(|m| pkg.join(m).to_string_lossy().into_owned())
    .collect();
    let mut inner = vec![named[0].clone(), "run".to_string()];
    inner.extend(named.iter().skip(1).cloned());
    let worker = PathBuf::from(&named[0]);
    let binds = containment::extra_binds(&inner, Some(worker.as_path()));
    assert_eq!(binds.len(), 2, "every named package path binds: {binds:?}");
    let plan = MountPlan {
        workspace: None,
        request: None,
        input: None,
        out: None,
        gitconfig: None,
        runtime_auth: None,
        provider_config: None,
        proxy_sock: None,
        toolchain: None,
        caches: vec![],
        ca_bundles: vec![],
        extra_binds: binds,
        extra_rw_binds: vec![],
        supervised_binds: vec![],
        worker: Some(worker),
        mask_roots: vec![],
        masked_files: vec![],
    };
    assert!(
        containment::plan_denial(&plan).is_none(),
        "the packaged layout must not be denied"
    );
    let argv = containment::bwrap_args(Path::new("/bwrap"), &plan, &inner, "/tmp");
    let joined = argv.join("\n");
    // Package files appear only as --ro-bind sources, never --bind.
    let rw_sources: Vec<&str> = argv
        .windows(3)
        .filter(|w| w[0] == "--bind")
        .map(|w| w[1].as_str())
        .collect();
    for member in rw_sources {
        assert!(
            !member.starts_with(&*pkg.to_string_lossy()),
            "a packaged file bound writable: {member}\n{joined}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sources the contract never admits refuse the launch rather than
/// render: an argv-named host socket is denied by `plan_denial`
/// before the outer argv is built, and the owner's home directory
/// itself is never a bind source — `plan_denial` refuses the
/// spelling when it reaches the plan, while a home that covers a
/// fixed mount (`/tmp`, `/run`, …) or does not exist is filtered
/// out of the plan first. Either way the home is never bound.
#[test]
fn forbidden_bind_sources_refuse_the_launch() {
    let dir = tmp("forbidden");
    let sock = dir.join("runtime.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let inner = vec![
        "/pkg/bin/managed-worker".to_string(),
        sock.to_string_lossy().into_owned(),
    ];
    let err = contained_launch(&dir, RuntimeBoundary::new(boundary_config(&dir)), &inner)
        .expect_err("an argv-named host socket refuses");
    assert!(err.contains("not a regular file or directory"), "{err}");

    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return;
    };
    let home = PathBuf::from(home);
    let inner = vec![
        "/pkg/bin/managed-worker".to_string(),
        home.to_string_lossy().into_owned(),
    ];
    match contained_launch(&dir, RuntimeBoundary::new(boundary_config(&dir)), &inner) {
        Err(err) => assert!(err.contains("owner's home"), "{err}"),
        // The spelling never reached the plan — `extra_binds` drops a
        // home that covers a fixed mount (HOME=/tmp is a contemplated
        // environment) or that does not exist. The contract outcome
        // still holds: the rendered argv binds no source at or above
        // the home.
        Ok((argv, _)) => {
            let joined = argv.join("\n");
            // The oracle covers the env-HOME spelling the argv names;
            // the account-database home is never an argv element here
            // (its refusal is proven by the containment unit tests).
            let sources: Vec<&str> = argv
                .windows(3)
                .filter(|w| w[0] == "--bind" || w[0] == "--ro-bind")
                .map(|w| w[1].as_str())
                .collect();
            assert!(
                !sources
                    .iter()
                    .any(|s| bind_source_is_home_or_ancestor(s, &home)),
                "the owner's home must never be a bind source:\n{joined}"
            );
        }
    }
    // The filtering half is proven deterministically — the ambient
    // `match` only reaches its `Ok` branch when this environment's
    // HOME happens to cover a mount or not exist, which CI never
    // does. A covered-mount spelling is dropped before the plan for
    // every environment, on any host where it exists.
    let covered = vec!["/pkg/bin/managed-worker".to_string(), "/tmp".to_string()];
    assert!(
        !containment::extra_binds(&covered, None)
            .iter()
            .any(|p| p == Path::new("/tmp")),
        "a covered-mount argv element is filtered before the plan"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The documented packaging example stays a real layout: the worker
/// root sits beneath a bound system prefix and the example explains
/// the writable scratch areas.
#[test]
fn the_packaging_example_documents_the_bound_layout() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = root.join("docs/examples/managed-worker/README.md");
    let text = std::fs::read_to_string(&readme).expect("the packaging example must exist");
    for required in [
        "/usr/local/lib/taisan-managed-worker",
        "taisan.worker.bindings/1",
        "/taisan/runtime-auth",
        "/taisan/provider-config",
        "/taisan/out",
        "HOME",
        "TMPDIR",
    ] {
        assert!(
            text.contains(required),
            "the packaging example omits {required}"
        );
    }
    for layout_file in [
        "docs/examples/managed-worker/layout/bin/managed-worker",
        "docs/examples/managed-worker/worker-bindings.json",
    ] {
        assert!(
            root.join(layout_file).exists(),
            "the packaging example omits {layout_file}"
        );
    }
    // The example's contract document must satisfy the contract.
    let bindings = root.join("docs/examples/managed-worker/worker-bindings.json");
    workloop_exec::bindings::validate(&bindings, None)
        .expect("the example contract document must validate");
}
