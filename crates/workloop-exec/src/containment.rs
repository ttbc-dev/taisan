//! Mount, PID, IPC, and network namespace containment for launches
//! whose approved isolation profile declares a sandbox primitive
//! (`docs/v0.3.md` §14.3–§14.6).
//!
//! The supervisor stays the boundary builder: for a profile that
//! names `bwrap`, every `describe` probe and every Invocation launch —
//! v1 or v2 — runs as `bwrap <flags> -- <worker argv>` inside fresh
//! user/PID/IPC/network namespaces with a fixed mount set. The
//! environment is rewritten to the in-sandbox paths (`/workspace`,
//! `/taisan/*`, tmpfs `HOME`/`TMPDIR`); host credential paths never
//! enter because they were never mounted.
//!
//! Enforcement is proven, not declared: the first launch under an
//! identity runs a bounded negative-probe battery — read-only binds
//! deny writes, unmounted host paths are invisible, tmpfs scratch is
//! writable, the PID namespace kills escaped descendants, the network
//! namespace exposes loopback only, and the worker binary resolves —
//! and the result persists as `taisan.containment-preflight/1`
//! evidence keyed by profile, kernel, primitive version, and worker
//! argv. A later launch reuses the evidence only while the identity
//! matches; a host/kernel/runtime/profile change re-proves. A failed
//! or absent proof refuses the launch — there is no hygiene-only
//! substitute (`docs/v0.3.md` §14, OP-1/OPS-01/OPS-02).
//!
//! `bwrap` is a Linux primitive: on a host without it (or without
//! unprivileged user namespaces) the gate refuses every launch whose
//! profile requires containment. The contract-harness profile
//! declares no primitive and is never wrapped — its launches remain
//! test evidence only.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use workloop_core::domain::sha256_hex;
use workloop_core::ports::{ProbeOutcome, ProcessControlPort, SpawnError, WorkerEnv};
use workloop_core::worker::IsolationProfile;

use crate::isolation;

/// The persisted preflight-evidence format.
pub const EVIDENCE_FORMAT: &str = "taisan.containment-preflight/1";

/// In-sandbox paths (`docs/v0.3.md` §14.3). The wire request already
/// names `/workspace` as the workspace mount; the rest are the
/// per-Invocation and generated-config surfaces.
pub const SANDBOX_WORKSPACE: &str = "/workspace";
pub const SANDBOX_OUT: &str = "/taisan/out";
pub const SANDBOX_REQUEST: &str = "/taisan/request.json";
pub const SANDBOX_INPUT: &str = "/taisan/input";
pub const SANDBOX_GITCONFIG: &str = "/taisan/gitconfig";

/// The fixed read-only system binds every contained launch gets —
/// the runtime and toolchain surface, never an owner home, another
/// workspace, the store, the handoff repository, or installation
/// config (`docs/v0.3.md` §14.3). Only directories that exist on the
/// host are bound; a merged-usr layout binds `/bin`/`/sbin` as the
/// symlinks they are.
const SYSTEM_BINDS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt"];

/// How long a failed proof suppresses re-probing: an operator fixes
/// the environment, the next window re-proves. Successful evidence
/// has no expiry of its own — the identity fields are the expiry.
const EVIDENCE_RETRY_SECONDS: i64 = 300;

/// Per-probe bound: every negative probe is a tiny shell program; a
/// wedged sandbox answers `Bounded` and fails the battery.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What one contained launch binds into the sandbox. Every field is a
/// host path the supervisor owns; the sandbox side is fixed by the
/// constants above.
#[derive(Debug, Clone)]
pub struct MountPlan {
    /// The managed workspace → `/workspace`; the flag is the role's
    /// read-only grant. `None` leaves the workspace unmounted (a
    /// `describe` probe whose workspace does not exist yet).
    pub workspace: Option<(PathBuf, bool)>,
    /// The canonical request file → `/taisan/request.json`, read-only.
    pub request: Option<PathBuf>,
    /// Materialized input bodies → `/taisan/input`, read-only.
    pub input: Option<PathBuf>,
    /// The Invocation-owned output dir → `/taisan/out`, writable.
    pub out: Option<PathBuf>,
    /// Generated git config → `/taisan/gitconfig`, read-only.
    pub gitconfig: Option<PathBuf>,
    /// Absolute-path argv elements that exist on the host, bound
    /// read-only at their own path — a `python3 /opt/worker.py`
    /// launch keeps its script reachable. This can only ever expose
    /// what the uncontained launch already could read; it never adds
    /// a path the worker argv did not name.
    pub extra_binds: Vec<PathBuf>,
    /// The resolved host path of the worker executable, bound
    /// read-only at its own absolute path.
    pub worker: PathBuf,
}

/// The gate's answer for one launch argv.
#[derive(Debug, Clone)]
pub enum Gate {
    /// The profile declares no sandbox primitive — launches run under
    /// the hygiene environment unchanged (the contract-harness
    /// profile; test evidence only, never a production boundary).
    Uncontained,
    /// The primitive resolved and the recorded negative-probe
    /// evidence matches this launch's identity — freshly proven or
    /// replayed.
    Ready {
        /// The resolved `bwrap` path the outer argv leads with.
        primitive: PathBuf,
        /// The resolved worker executable path the mount plan binds.
        worker: PathBuf,
    },
}

/// Why the gate refused or faulted. `Refused` is a typed launch
/// repair — recorded evidence, never retried under a weaker profile;
/// `Fault` is infrastructure and leaves the request pending.
#[derive(Debug)]
pub enum GateError {
    Refused(String),
    Fault(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(f, "{reason}"),
            Self::Fault(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for GateError {}

fn refused(reason: impl Into<String>) -> GateError {
    GateError::Refused(reason.into())
}

fn fault(detail: impl Into<String>) -> GateError {
    GateError::Fault(detail.into())
}

/// Resolve a bare executable name on the host: well-known binary
/// directories first, then PATH order — the same lookup the
/// preflight's `which` performs. `None` is a typed refusal upstream,
/// never a degrade.
fn resolve_on_path(name: &str) -> Option<PathBuf> {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin", "/usr/sbin", "/sbin"] {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// The worker executable inside `argv[0]`: an absolute path must name
/// an existing file; a bare name resolves on PATH; a relative path
/// with a separator has no deterministic resolution and refuses.
fn resolve_worker(argv0: &str) -> Option<PathBuf> {
    let path = Path::new(argv0);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    if argv0.contains('/') {
        return None;
    }
    resolve_on_path(argv0)
}

/// The host kernel release the evidence identity binds — read from
/// procfs, `unknown` where procfs is absent (the primitive itself is
/// Linux-only, so the fallback only ever appears beside a refusal).
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Absolute-path argv elements that exist on the host become
/// read-only binds at their own path. Flags, sandbox paths (which do
/// not exist on the host), bare names, and missing paths are skipped
/// — a path the worker needs but that is absent fails honestly inside
/// the sandbox, never silently on the host.
pub fn extra_binds(inner_argv: &[String], worker: &Path) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::from([worker.to_path_buf()]);
    let mut binds = Vec::new();
    for arg in inner_argv.iter().skip(1) {
        let path = Path::new(arg);
        if path.is_absolute() && path.exists() && seen.insert(path.to_path_buf()) {
            binds.push(path.to_path_buf());
        }
    }
    binds
}

/// Build the full outer argv: `<bwrap> <namespace flags> <fixed
/// binds> --chdir <inner cwd> -- <inner argv>`. Pure — every decision
/// is data, so the suite asserts the exact boundary without a host.
/// `inner_cwd` is a sandbox path (`/workspace`, `/tmp`), never a host
/// path.
pub fn bwrap_args(
    primitive: &Path,
    plan: &MountPlan,
    inner_argv: &[String],
    inner_cwd: &str,
) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut args = vec![s(primitive)];
    // Fresh user/PID/IPC/network namespaces: the sandbox sees only
    // loopback, its own process table (escaped descendants die with
    // the namespace init), and its own SysV/POSIX IPC objects.
    // `--die-with-parent` ties the sandbox leader's life to the
    // supervisor's spawn; `--new-session` detaches it into its own
    // session like every other launch.
    args.extend(
        [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-net",
            "--die-with-parent",
            "--new-session",
        ]
        .map(str::to_string),
    );
    for dir in SYSTEM_BINDS {
        if Path::new(dir).exists() {
            args.extend([
                "--ro-bind".to_string(),
                (*dir).to_string(),
                (*dir).to_string(),
            ]);
        }
    }
    args.extend(
        [
            "--proc", "/proc", "--dev", "/dev", // kernel-provided views
            "--dir", "/tmp", "--tmpfs", "/tmp", // writable scratch
            "--dir", "/run", "--tmpfs", "/run", "--dir", "/home", "--tmpfs",
            "/home", // fresh HOME — no host home
            "--dir", "/taisan", // mount-point parent for the /taisan binds
        ]
        .map(str::to_string),
    );
    if let Some((host, read_only)) = &plan.workspace {
        args.extend([
            if *read_only { "--ro-bind" } else { "--bind" }.to_string(),
            s(host),
            SANDBOX_WORKSPACE.to_string(),
        ]);
    }
    if let Some(request) = &plan.request {
        args.extend([
            "--ro-bind".to_string(),
            s(request),
            SANDBOX_REQUEST.to_string(),
        ]);
    }
    if let Some(input) = &plan.input {
        args.extend(["--ro-bind".to_string(), s(input), SANDBOX_INPUT.to_string()]);
    }
    if let Some(out) = &plan.out {
        args.extend(["--bind".to_string(), s(out), SANDBOX_OUT.to_string()]);
    }
    if let Some(gitconfig) = &plan.gitconfig {
        args.extend([
            "--ro-bind".to_string(),
            s(gitconfig),
            SANDBOX_GITCONFIG.to_string(),
        ]);
    }
    for extra in &plan.extra_binds {
        args.extend(["--ro-bind".to_string(), s(extra), s(extra)]);
    }
    args.extend(["--ro-bind".to_string(), s(&plan.worker), s(&plan.worker)]);
    args.extend([
        "--chdir".to_string(),
        inner_cwd.to_string(),
        "--".to_string(),
    ]);
    args.extend(inner_argv.iter().cloned());
    args
}

/// Rewrite the isolation environment to the in-sandbox paths: `HOME`
/// and `TMPDIR` land on the tmpfs mounts and `GIT_CONFIG_GLOBAL` on
/// the read-only gitconfig bind. Every other allowlisted variable —
/// `PATH`, the git transport/credential pins — passes verbatim: the
/// sandbox adds no credential the profile did not already allow, and
/// removes none the launch needs.
pub fn contained_env(base: &WorkerEnv) -> WorkerEnv {
    let mut vars = Vec::with_capacity(base.vars.len() + 3);
    let mut seen = HashSet::new();
    for (name, value) in &base.vars {
        let value = match name.as_str() {
            "HOME" => "/home".to_string(),
            "TMPDIR" => "/tmp".to_string(),
            "GIT_CONFIG_GLOBAL" => SANDBOX_GITCONFIG.to_string(),
            _ => value.clone(),
        };
        seen.insert(name.clone());
        vars.push((name.clone(), value));
    }
    for (name, value) in [
        ("HOME", "/home"),
        ("TMPDIR", "/tmp"),
        ("GIT_CONFIG_GLOBAL", SANDBOX_GITCONFIG),
    ] {
        if seen.insert(name.to_string()) {
            vars.push((name.to_string(), value.to_string()));
        }
    }
    WorkerEnv { vars }
}

/// The persisted proof one identity earned. `passed = false` evidence
/// still records which probes failed so the refusal names them; it
/// suppresses re-probing only until `retry_after_unix_seconds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Evidence {
    format: String,
    profile_id: String,
    kernel_release: String,
    primitive: String,
    primitive_version: String,
    worker_argv: Vec<String>,
    probes: Vec<ProbeRecord>,
    passed: bool,
    failure: Option<String>,
    recorded_unix_seconds: i64,
    retry_after_unix_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeRecord {
    name: String,
    passed: bool,
    detail: String,
}

/// The evidence file for one `(profile, worker argv)` identity —
/// argv is part of the filename so alternating bindings never
/// thrash one file's identity.
fn evidence_path(
    workspace_root: &Path,
    profile: &IsolationProfile,
    worker_argv: &[String],
) -> PathBuf {
    let digest = sha256_hex("containment-argv-v1", worker_argv.join("\u{0}").as_bytes());
    let argv_digest = &digest[..12];
    workspace_root.join(".containment").join(format!(
        "{}-{}.json",
        isolation::sanitize_key(profile.id),
        argv_digest
    ))
}

fn identity_matches(
    evidence: &Evidence,
    profile: &IsolationProfile,
    kernel_release: &str,
    primitive: &Path,
    primitive_version: &str,
    worker_argv: &[String],
) -> bool {
    evidence.format == EVIDENCE_FORMAT
        && evidence.profile_id == profile.id
        && evidence.kernel_release == kernel_release
        && evidence.primitive.as_str() == primitive.to_string_lossy()
        && evidence.primitive_version == primitive_version
        && evidence.worker_argv == worker_argv
}

/// `<bwrap> --version`, bounded through the same process port every
/// probe uses. The version is part of the evidence identity: a bwrap
/// upgrade re-proves the boundary rather than inheriting the old
/// proof.
fn primitive_version(
    port: &impl ProcessControlPort,
    primitive: &Path,
    cwd: &Path,
) -> Result<String, GateError> {
    let argv = vec![primitive.to_string_lossy().into_owned(), "--version".into()];
    match port.run_probe(&argv, cwd, &WorkerEnv::default(), PROBE_TIMEOUT) {
        Ok(ProbeOutcome::Exited {
            stdout,
            success: true,
        }) => {
            let version = String::from_utf8_lossy(&stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if version.is_empty() {
                Err(refused(
                    "isolation.primitive_unusable: the sandbox primitive answered --version with no output",
                ))
            } else {
                Ok(version)
            }
        }
        Ok(ProbeOutcome::Exited { success: false, .. }) => Err(refused(
            "isolation.primitive_unusable: the sandbox primitive exited non-zero on --version",
        )),
        Ok(ProbeOutcome::Bounded) => Err(refused(
            "isolation.primitive_unusable: the sandbox primitive did not answer --version within the bound",
        )),
        Err(SpawnError::Refused { reason }) => Err(refused(format!(
            "isolation.primitive_unusable: the sandbox primitive refused to start: {reason}"
        ))),
        Err(SpawnError::Unknown { reason }) => Err(fault(format!(
            "isolation primitive probe spawn unknown: {reason}"
        ))),
    }
}

/// One negative probe: run `inner_argv` inside the containment plan
/// and record pass/fail. `Exited{success:true}` is the only pass —
/// a bounded wait, a refused spawn, or a non-zero exit all fail the
/// battery closed.
#[allow(clippy::too_many_arguments)]
fn run_named_probe(
    port: &impl ProcessControlPort,
    primitive: &Path,
    plan: &MountPlan,
    inner_argv: &[String],
    inner_cwd: &str,
    env: &WorkerEnv,
    host_cwd: &Path,
    name: &str,
    records: &mut Vec<ProbeRecord>,
) {
    let argv = bwrap_args(primitive, plan, inner_argv, inner_cwd);
    let (passed, detail) = match port.run_probe(&argv, host_cwd, env, PROBE_TIMEOUT) {
        Ok(ProbeOutcome::Exited { success: true, .. }) => (true, "passed".to_string()),
        Ok(ProbeOutcome::Exited {
            success: false,
            stdout,
        }) => (
            false,
            format!("exited non-zero{}", {
                let text = String::from_utf8_lossy(&stdout);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!(": {}", &trimmed[..trimmed.len().min(200)])
                }
            }),
        ),
        Ok(ProbeOutcome::Bounded) => (false, "did not answer within the bound".to_string()),
        Err(SpawnError::Refused { reason }) => (false, format!("spawn refused: {reason}")),
        Err(SpawnError::Unknown { reason }) => (false, format!("spawn unknown: {reason}")),
    };
    records.push(ProbeRecord {
        name: name.to_string(),
        passed,
        detail,
    });
}

/// The negative-probe battery (`docs/v0.3.md` §14 acceptance matrix,
/// OPS-01…OPS-09 evidence class). Each probe is a tiny `/bin/sh`
/// program run through the real `bwrap` argv the launches use, so a
/// pass proves the exact boundary a worker gets — not a lookalike.
/// The battery runs against a scratch mount plan: a read-only
/// workspace stand-in, a writable out dir, and the generated
/// gitconfig — the same shapes every role launch binds.
fn run_battery(
    port: &impl ProcessControlPort,
    workspace_root: &Path,
    profile: &IsolationProfile,
    primitive: &Path,
    worker: &Path,
) -> Result<Vec<ProbeRecord>, GateError> {
    // The probe scratch lives under `.isolation/probe-containment` —
    // inside the sweep's `probe-*` prefix so a crashed battery never
    // leaks, and outside every mount the battery itself creates.
    let scratch = isolation::scratch_root(workspace_root).join("probe-containment");
    let _ = std::fs::remove_dir_all(&scratch);
    let prepare_result = isolation::prepare(profile, &scratch);
    let env = match prepare_result {
        Ok(env) => contained_env(&env),
        Err(why) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(fault(format!(
                "isolation.environment_preparation_failed: {why}"
            )));
        }
    };
    let probe_ws = scratch.join("ws");
    let probe_out = scratch.join("out");
    for dir in [&probe_ws, &probe_out] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(fault(format!("create probe dir {}: {e}", dir.display())));
        }
    }
    let gitconfig = scratch.join("gitconfig");
    let plan = MountPlan {
        workspace: Some((probe_ws.clone(), true)),
        request: None,
        input: None,
        out: Some(probe_out.clone()),
        gitconfig: Some(gitconfig),
        extra_binds: vec![],
        worker: worker.to_path_buf(),
    };
    let mut records = Vec::new();
    let sh = |script: &str| vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];

    // ro-bind denies writes — and only counts when the mount exists.
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh(
            "test -d /workspace || exit 1; if touch /workspace/probe-write 2>/dev/null; then exit 1; fi",
        ),
        SANDBOX_WORKSPACE,
        &env,
        &scratch,
        "ro-bind-denies-write",
        &mut records,
    );
    // The writable out bind accepts writes (positive control: the
    // sandbox is not simply read-only everywhere).
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh(
            "test -d /taisan/out || exit 1; echo ok > /taisan/out/probe-rw && test -f /taisan/out/probe-rw",
        ),
        SANDBOX_OUT,
        &env,
        &scratch,
        "rw-bind-allows-write",
        &mut records,
    );
    // A symlink planted in the workspace cannot reach a host path:
    // the target was never mounted, so resolving through the link
    // lands on nothing inside the sandbox.
    let canary = workspace_root.join(".containment");
    let _ = std::fs::create_dir_all(&canary);
    let escape_link = probe_ws.join("escape-link");
    let _ = std::os::unix::fs::symlink(&canary, &escape_link);
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh("test ! -e /workspace/escape-link"),
        SANDBOX_WORKSPACE,
        &env,
        &scratch,
        "symlink-escape-denied",
        &mut records,
    );
    // A host path outside every bind is invisible inside — the proof
    // no ambient filesystem view survives (this is also the
    // "hygiene-only substitute" detector: without real mounts the
    // canary path resolves).
    run_named_probe(
        port,
        primitive,
        &plan,
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test ! -e \"$1\"".to_string(),
            "sh".to_string(),
            canary.to_string_lossy().into_owned(),
        ],
        "/tmp",
        &env,
        &scratch,
        "unmounted-path-invisible",
        &mut records,
    );
    // tmpfs scratch is writable at /tmp and the fresh /home.
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh(
            "echo x > /tmp/probe-p && test -f /tmp/probe-p && echo x > /home/probe-p && test -f /home/probe-p",
        ),
        "/tmp",
        &env,
        &scratch,
        "tmpfs-scratch-writable",
        &mut records,
    );
    // /home is an empty tmpfs — the host home is not merely unmounted,
    // it is shadowed. POSIX glob, no external tool required.
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh("set -- /home/*; [ \"$1\" = \"/home/*\" ]"),
        "/tmp",
        &env,
        &scratch,
        "sandbox-home-is-fresh",
        &mut records,
    );
    // The PID namespace reaps escaped descendants: a `setsid` child —
    // outside the process group — must die with the namespace init
    // before its delayed marker write lands in the bound out dir.
    // Two bounded probes: launch the escapee, then check the marker
    // on the host after the child's write window has passed.
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh("setsid sh -c 'sleep 1; touch /taisan/out/escaped' >/dev/null 2>&1 & exit 0"),
        "/tmp",
        &env,
        &scratch,
        "pid-namespace-launch",
        &mut records,
    );
    std::thread::sleep(Duration::from_millis(1600));
    let escaped = probe_out.join("escaped").exists();
    records.push(ProbeRecord {
        name: "pid-namespace-contains-escapes".to_string(),
        passed: !escaped,
        detail: if escaped {
            "an escaped descendant outlived the namespace init".to_string()
        } else {
            "escaped descendant died with the namespace".to_string()
        },
    });
    // The network namespace exposes loopback only: /proc/net/dev lists
    // exactly `lo`. Any other interface means the unshare did not
    // happen — the probe fails closed.
    run_named_probe(
        port,
        primitive,
        &plan,
        &sh(
            "if grep \"^[[:space:]]*[A-Za-z0-9_.-]*:\" /proc/net/dev | grep -v \"^[[:space:]]*lo:\" | grep -q .; then exit 1; fi",
        ),
        "/tmp",
        &env,
        &scratch,
        "network-namespace-loopback-only",
        &mut records,
    );
    // The worker executable resolves inside the sandbox at its bound
    // path — the mount plan's worker bind is proven, not assumed.
    run_named_probe(
        port,
        primitive,
        &plan,
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test -x \"$1\"".to_string(),
            "sh".to_string(),
            worker.to_string_lossy().into_owned(),
        ],
        "/tmp",
        &env,
        &scratch,
        "worker-binary-visible",
        &mut records,
    );
    // `describe`-era argv elements that are absolute host paths stay
    // reachable read-only — the same surface `extra_binds` grants.
    // (The battery plan binds none; the launch plans add them per
    // argv, covered by the argv-builder unit tests.)
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(records)
}

/// Evaluate the profile's containment requirement for one launch
/// argv. `Ok(Gate::Uncontained)` means the profile declares no
/// primitive; `Ok(Gate::Ready)` means the negative-probe evidence for
/// this exact identity is recorded (or was just proven); `Err` is a
/// typed refusal or an infrastructure fault — callers must never
/// substitute the unwrapped launch.
pub fn gate(
    port: &impl ProcessControlPort,
    workspace_root: &Path,
    profile: &IsolationProfile,
    worker_argv: &[String],
    now_unix_seconds: i64,
) -> Result<Gate, GateError> {
    gate_with_resolvers(
        port,
        workspace_root,
        profile,
        worker_argv,
        now_unix_seconds,
        &resolve_on_path,
        &resolve_worker,
    )
}

/// `gate` with the host lookups injected — the deterministic seam the
/// suite uses to prove evidence reuse, refusal, and re-proof without
/// a real `bwrap` on the host.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gate_with_resolvers(
    port: &impl ProcessControlPort,
    workspace_root: &Path,
    profile: &IsolationProfile,
    worker_argv: &[String],
    now_unix_seconds: i64,
    resolve_primitive: &dyn Fn(&str) -> Option<PathBuf>,
    resolve_worker: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<Gate, GateError> {
    let Some(primitive_name) = profile.sandbox_primitive else {
        return Ok(Gate::Uncontained);
    };
    if worker_argv.is_empty() {
        return Err(refused(
            "isolation.worker_argv_empty: the profile requires containment but no worker argv is configured",
        ));
    }
    let primitive = resolve_primitive(primitive_name).ok_or_else(|| {
        refused(format!(
            "isolation.primitive_missing: '{primitive_name}' required by profile '{}' is not present",
            profile.id
        ))
    })?;
    let worker = resolve_worker(&worker_argv[0]).ok_or_else(|| {
        refused(format!(
            "isolation.worker_unresolvable: worker argv[0] '{}' does not resolve to an executable file",
            worker_argv[0]
        ))
    })?;
    let kernel = kernel_release();
    let version = primitive_version(port, &primitive, workspace_root)?;

    let path = evidence_path(workspace_root, profile, worker_argv);
    if let Ok(raw) = std::fs::read(&path)
        && let Ok(evidence) = serde_json::from_slice::<Evidence>(&raw)
        && identity_matches(
            &evidence,
            profile,
            &kernel,
            &primitive,
            &version,
            worker_argv,
        )
    {
        if evidence.passed {
            return Ok(Gate::Ready { primitive, worker });
        }
        if evidence
            .retry_after_unix_seconds
            .is_some_and(|retry| now_unix_seconds < retry)
        {
            return Err(refused(format!(
                "isolation.containment_unproven: {}",
                evidence
                    .failure
                    .unwrap_or_else(|| "the negative probes failed".into())
            )));
        }
    }

    let records = run_battery(port, workspace_root, profile, &primitive, &worker)?;
    let failures: Vec<String> = records
        .iter()
        .filter(|record| !record.passed)
        .map(|record| format!("{}: {}", record.name, record.detail))
        .collect();
    let passed = failures.is_empty();
    let evidence = Evidence {
        format: EVIDENCE_FORMAT.into(),
        profile_id: profile.id.to_string(),
        kernel_release: kernel,
        primitive: primitive.to_string_lossy().into_owned(),
        primitive_version: version,
        worker_argv: worker_argv.to_vec(),
        probes: records,
        passed,
        failure: (!passed).then(|| failures.join("; ")),
        recorded_unix_seconds: now_unix_seconds,
        retry_after_unix_seconds: (!passed)
            .then(|| now_unix_seconds.saturating_add(EVIDENCE_RETRY_SECONDS)),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            fault(format!(
                "create containment evidence dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&evidence)
        .map_err(|e| fault(format!("serialize containment evidence: {e}")))?;
    std::fs::write(&tmp, &bytes)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            fault(format!(
                "write containment evidence {}: {e}",
                path.display()
            ))
        })?;

    if passed {
        Ok(Gate::Ready { primitive, worker })
    } else {
        Err(refused(format!(
            "isolation.containment_unproven: {}",
            evidence.failure.unwrap_or_default()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::ScriptedProcessControl;
    use workloop_core::worker::{FIXTURE_ISOLATION_PROFILE_ID, default_isolation_profile};

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-containment-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn plan_fixture() -> MountPlan {
        MountPlan {
            workspace: Some((PathBuf::from("/host/ws"), false)),
            request: Some(PathBuf::from("/host/inv/request.json")),
            input: Some(PathBuf::from("/host/inv/input")),
            out: Some(PathBuf::from("/host/inv/out")),
            gitconfig: Some(PathBuf::from("/host/scratch/gitconfig")),
            extra_binds: vec![PathBuf::from("/opt/worker.py")],
            worker: PathBuf::from("/opt/worker"),
        }
    }

    #[test]
    fn bwrap_args_carry_the_namespace_flags_and_fixed_binds() {
        let args = bwrap_args(
            Path::new("/usr/bin/bwrap"),
            &plan_fixture(),
            &[
                "worker".into(),
                "run".into(),
                "--request".into(),
                "/taisan/request.json".into(),
            ],
            "/workspace",
        );
        let text = args.join(" ");
        for flag in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-net",
            "--die-with-parent",
            "--new-session",
            "--proc /proc",
            "--dev /dev",
            "--tmpfs /tmp",
            "--tmpfs /home",
        ] {
            assert!(text.contains(flag), "missing {flag} in {text}");
        }
        // The role's writable workspace, the read-only request/input/
        // gitconfig binds, the writable out, the worker at its own
        // path, and the inner argv after `--`.
        assert!(text.contains("--bind /host/ws /workspace"), "{text}");
        assert!(
            text.contains("--ro-bind /host/inv/request.json /taisan/request.json"),
            "{text}"
        );
        assert!(
            text.contains("--ro-bind /host/inv/input /taisan/input"),
            "{text}"
        );
        assert!(text.contains("--bind /host/inv/out /taisan/out"), "{text}");
        assert!(
            text.contains("--ro-bind /host/scratch/gitconfig /taisan/gitconfig"),
            "{text}"
        );
        assert!(
            text.contains("--ro-bind /opt/worker.py /opt/worker.py"),
            "{text}"
        );
        assert!(text.contains("--ro-bind /opt/worker /opt/worker"), "{text}");
        assert!(text.contains("--chdir /workspace -- worker run"), "{text}");
        // No host home, store, or credential path is ever bound: the
        // only binds are the fixed system set plus the plan's own.
        assert!(!text.contains("/Users/"), "{text}");
    }

    #[test]
    fn bwrap_args_mark_assessment_workspace_read_only() {
        let mut plan = plan_fixture();
        plan.workspace = Some((PathBuf::from("/host/ws"), true));
        plan.request = None;
        plan.input = None;
        plan.out = None;
        let args = bwrap_args(
            Path::new("/usr/bin/bwrap"),
            &plan,
            &["worker".into(), "describe".into()],
            "/tmp",
        );
        let text = args.join(" ");
        assert!(text.contains("--ro-bind /host/ws /workspace"), "{text}");
        assert!(!text.contains("--bind /host/ws"), "{text}");
        assert!(!text.contains("/taisan/out"), "{text}");
        assert!(!text.contains("/taisan/request.json"), "{text}");
    }

    #[test]
    fn contained_env_rewrites_to_sandbox_paths() {
        let base = WorkerEnv {
            vars: vec![
                ("PATH".into(), "/usr/bin".into()),
                ("HOME".into(), "/host/home".into()),
                ("TMPDIR".into(), "/host/tmp".into()),
                ("GIT_CONFIG_GLOBAL".into(), "/host/gitconfig".into()),
                ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
            ],
        };
        let env = contained_env(&base);
        let map: std::collections::BTreeMap<_, _> = env.vars.iter().cloned().collect();
        assert_eq!(map["HOME"], "/home");
        assert_eq!(map["TMPDIR"], "/tmp");
        assert_eq!(map["GIT_CONFIG_GLOBAL"], "/taisan/gitconfig");
        assert_eq!(map["PATH"], "/usr/bin");
        assert_eq!(map["GIT_ALLOW_PROTOCOL"], "file");
        // Missing members are still set — the sandbox contract does
        // not depend on the base env carrying them.
        let sparse = contained_env(&WorkerEnv::default());
        let map: std::collections::BTreeMap<_, _> = sparse.vars.iter().cloned().collect();
        assert_eq!(map["HOME"], "/home");
        assert_eq!(map["GIT_CONFIG_GLOBAL"], "/taisan/gitconfig");
    }

    #[test]
    fn extra_binds_collect_only_existing_absolute_argv_paths() {
        let worker = Path::new("/opt/worker");
        // `/opt/worker.py` may not exist on the test host; use a path
        // guaranteed to exist instead.
        let real = std::env::temp_dir().join("workloop-extra-bind-target");
        std::fs::write(&real, "x").unwrap();
        let argv = vec![
            "worker".into(),
            real.to_string_lossy().into_owned(),
            worker.to_string_lossy().into_owned(),
            "/taisan/request.json".into(), // sandbox path, absent on host → skipped
            "relative/script.py".into(),   // relative → skipped
            "/nonexistent/definitely".into(), // missing → skipped
        ];
        let binds = extra_binds(&argv, worker);
        assert_eq!(binds, vec![real.clone()]);
        let _ = std::fs::remove_file(&real);
    }

    /// A profile that declares a primitive which cannot resolve —
    /// deterministic on every host.
    fn primitive_profile() -> IsolationProfile {
        IsolationProfile {
            id: "test-contained-v0.1",
            env_passthrough: &[],
            sandbox_primitive: Some("definitely-not-a-real-primitive-xyz"),
        }
    }

    #[test]
    fn a_missing_primitive_refuses_the_gate() {
        let root = tmp("missing");
        let port = ScriptedProcessControl::new();
        let err = gate(&port, &root, &primitive_profile(), &["worker".into()], 1000)
            .expect_err("missing primitive refuses");
        assert!(
            matches!(err, GateError::Refused(ref r) if r.contains("isolation.primitive_missing")),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_fixture_profile_is_never_contained() {
        let root = tmp("fixture");
        let port = ScriptedProcessControl::new();
        let gate = gate(
            &port,
            &root,
            &IsolationProfile {
                id: FIXTURE_ISOLATION_PROFILE_ID,
                env_passthrough: &[],
                sandbox_primitive: None,
            },
            &["worker".into()],
            1000,
        )
        .expect("uncontained gate");
        assert!(matches!(gate, Gate::Uncontained));
        // No probe ever ran — an uncontained profile spawns nothing
        // for the gate itself.
        assert!(port.probe_envs().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn success_probe() -> Result<ProbeOutcome, SpawnError> {
        Ok(ProbeOutcome::Exited {
            stdout: b"bubblewrap 0.9.0\n".to_vec(),
            success: true,
        })
    }

    #[test]
    fn proven_evidence_is_reused_and_failures_suppress_reprobing() {
        let root = tmp("evidence");
        std::fs::create_dir_all(&root).unwrap();
        let port = ScriptedProcessControl::new();
        // Every probe answers success — the version probe plus the
        // battery. Queue enough outcomes; the default also succeeds.
        for _ in 0..16 {
            port.push_probe(success_probe());
        }
        let profile = IsolationProfile {
            id: "test-contained-v0.1",
            env_passthrough: &[],
            sandbox_primitive: Some("bwrap"),
        };
        let argv = vec!["/bin/sh".to_string()];
        let first = gate_with_resolvers(
            &port,
            &root,
            &profile,
            &argv,
            1_000,
            &|_| Some(PathBuf::from("/usr/bin/bwrap")),
            &|_| Some(PathBuf::from("/bin/sh")),
        )
        .expect("first gate proves");
        assert!(matches!(first, Gate::Ready { .. }));
        let probes_after_first = port.probe_envs().len();
        assert!(probes_after_first > 1, "the battery ran probes");

        // Second call: identical identity → the battery replays from
        // evidence; only the `--version` identity probe re-runs.
        let second = gate_with_resolvers(
            &port,
            &root,
            &profile,
            &argv,
            1_001,
            &|_| Some(PathBuf::from("/usr/bin/bwrap")),
            &|_| Some(PathBuf::from("/bin/sh")),
        )
        .expect("evidence replays");
        assert!(matches!(second, Gate::Ready { .. }));
        assert_eq!(
            port.probe_envs().len(),
            probes_after_first + 1,
            "only the version identity probe re-runs"
        );

        // A different worker argv is a different identity → re-proves
        // the whole battery.
        let before_third = port.probe_envs().len();
        let third = gate_with_resolvers(
            &port,
            &root,
            &profile,
            &["/bin/sh".to_string(), "--other".to_string()],
            1_002,
            &|_| Some(PathBuf::from("/usr/bin/bwrap")),
            &|_| Some(PathBuf::from("/bin/sh")),
        )
        .expect("new identity re-proves");
        assert!(matches!(third, Gate::Ready { .. }));
        assert_eq!(
            port.probe_envs().len(),
            before_third + 10,
            "version probe plus the nine-probe battery"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_probe_records_evidence_and_refuses_until_the_window() {
        let root = tmp("failed");
        std::fs::create_dir_all(&root).unwrap();
        let port = ScriptedProcessControl::new();
        // Version probe succeeds; the first battery probe fails.
        port.push_probe(success_probe());
        port.push_probe(Ok(ProbeOutcome::Exited {
            stdout: b"denied".to_vec(),
            success: false,
        }));
        port.set_default_probe(success_probe());
        let profile = IsolationProfile {
            id: "test-contained-v0.1",
            env_passthrough: &[],
            sandbox_primitive: Some("bwrap"),
        };
        let argv = vec!["/bin/sh".to_string()];
        let err = gate_with_resolvers(
            &port,
            &root,
            &profile,
            &argv,
            2_000,
            &|_| Some(PathBuf::from("/usr/bin/bwrap")),
            &|_| Some(PathBuf::from("/bin/sh")),
        )
        .expect_err("failed battery refuses");
        assert!(
            matches!(err, GateError::Refused(ref r) if r.contains("isolation.containment_unproven")),
            "{err:?}"
        );
        let probes = port.probe_envs().len();
        // Inside the retry window the recorded failure refuses without
        // re-running the battery — only the version identity probe.
        let err = gate_with_resolvers(
            &port,
            &root,
            &profile,
            &argv,
            2_001,
            &|_| Some(PathBuf::from("/usr/bin/bwrap")),
            &|_| Some(PathBuf::from("/bin/sh")),
        )
        .expect_err("recorded failure refuses");
        assert!(matches!(err, GateError::Refused(_)));
        assert_eq!(
            port.probe_envs().len(),
            probes + 1,
            "no battery re-probe inside the window"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_default_profile_requires_bwrap() {
        let profile = default_isolation_profile();
        assert_eq!(profile.sandbox_primitive, Some("bwrap"));
    }
}
