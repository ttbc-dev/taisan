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
/// The runtime credential file — a read-only bind of exactly one
/// supervisor-written file (`docs/v0.3.md` §14.3). Its content is the
/// resolved credential; the path itself is the whole contract.
pub const SANDBOX_RUNTIME_AUTH: &str = "/taisan/runtime-auth";
/// The non-secret provider configuration — a read-only bind the
/// supervisor validates before mounting; `/workspace` can never
/// shadow it.
pub const SANDBOX_PROVIDER_CONFIG: &str = "/taisan/provider-config";
/// The supervisor-owned CONNECT proxy's Unix socket, bind-mounted so
/// the in-sandbox relay can reach it (`docs/v0.3.md` §14.4).
pub const SANDBOX_PROXY_SOCK: &str = "/taisan/proxy.sock";
/// The pinned verification toolchain — verification sandboxes only
/// (`docs/v0.3.md` §14.5).
pub const SANDBOX_TOOLCHAIN: &str = "/taisan/toolchain";

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
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// The resolved runtime credential file → `/taisan/runtime-auth`,
    /// read-only. Never a repository-host credential, token helper,
    /// SSH socket, or keyring.
    pub runtime_auth: Option<PathBuf>,
    /// The validated non-secret provider configuration →
    /// `/taisan/provider-config`, read-only. A file or directory.
    pub provider_config: Option<PathBuf>,
    /// The CONNECT proxy's Unix socket → `/taisan/proxy.sock`,
    /// read-only. Present only when the launch's egress allowlist is
    /// non-empty.
    pub proxy_sock: Option<PathBuf>,
    /// The pinned verification toolchain → `/taisan/toolchain`,
    /// read-only, plus a second bind at its own host path so declared
    /// argv that names toolchain binaries absolutely still resolves
    /// (verification sandboxes only).
    pub toolchain: Option<PathBuf>,
    /// Read-only dependency caches, each bound at its own absolute
    /// host path so tool configuration that names them keeps working
    /// (verification sandboxes only).
    pub caches: Vec<PathBuf>,
    /// Absolute-path argv elements that exist on the host, bound
    /// read-only at their own path — a `python3 /opt/worker.py`
    /// launch keeps its script reachable. This can only ever expose
    /// what the uncontained launch already could read; it never adds
    /// a path the worker argv did not name.
    pub extra_binds: Vec<PathBuf>,
    /// Absolute paths bound read-write at their own path — the
    /// verification worktree's private gitdir is the only user: `git
    /// status` must refresh its index, and the bind can never reach
    /// another artifact's refs because it names exactly one
    /// worktree's metadata directory.
    pub extra_rw_binds: Vec<PathBuf>,
    /// The resolved host path of the worker executable, bound
    /// read-only at its own absolute path. `None` for sandboxes whose
    /// inner argv is not a single resolved executable (verification
    /// commands resolve through PATH or `extra_binds`).
    pub worker: Option<PathBuf>,
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

/// In-sandbox destinations a bind spelled by its own host path must
/// never cover or enter. The sealed trees hold supervisor-mounted
/// content whose sandbox spelling differs from the host path — a bind
/// landing inside them serves host content where the boundary
/// contract promised the supervisor's mount (a `/taisan/request.json`
/// argv element on a host that has one would shadow the
/// digest-verified request; `/taisan/out` likewise). The covered
/// mounts are the kernel/tmpfs views and fixed bind destinations: an
/// equal or ancestor spelling covers the mount outright — a `/` argv
/// element or configured path would re-mount the host root over the
/// whole sandbox.
const SEALED_TREES: &[&str] = &[SANDBOX_WORKSPACE, "/taisan"];
const COVERED_MOUNTS: &[&str] = &["/tmp", "/run", "/home", "/proc", "/dev"];

/// Whether `candidate` — a bind destination spelled at its own host
/// path — covers or enters the supervisor-owned sandbox surface.
/// Component-wise: `/taisanevil` is outside `/taisan`, while `/` is
/// an ancestor of every fixed destination.
fn shadows_sandbox_mount(candidate: &Path) -> bool {
    SEALED_TREES
        .iter()
        .chain(COVERED_MOUNTS)
        .any(|mount| Path::new(mount).starts_with(candidate))
        || SEALED_TREES
            .iter()
            .any(|sealed| candidate.starts_with(sealed))
}

/// Absolute-path argv elements that exist on the host become
/// read-only binds at their own path. Flags, sandbox paths (which do
/// not exist on the host), bare names, and missing paths are skipped
/// — a path the worker needs but that is absent fails honestly inside
/// the sandbox, never silently on the host. Spellings that cover or
/// enter the supervisor-owned surface are skipped too: these binds
/// render after the `/taisan` and `/workspace` mounts, so an argv
/// element could otherwise shadow them (`/taisan/request.json` and
/// `/taisan/out` are always present in a contained v2 argv; `/`
/// re-mounts the host root). `worker` names the one executable the
/// plan binds separately; it is excluded here so the same path is
/// never bound twice.
pub fn extra_binds(inner_argv: &[String], worker: Option<&Path>) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = worker.map(Path::to_path_buf).into_iter().collect();
    let mut binds = Vec::new();
    for arg in inner_argv.iter().skip(1) {
        let path = Path::new(arg);
        if path.is_absolute()
            && !shadows_sandbox_mount(path)
            && path.exists()
            && seen.insert(path.to_path_buf())
        {
            binds.push(path.to_path_buf());
        }
    }
    binds
}

/// Refuse a mount plan whose binds would cover or enter the
/// supervisor-owned sandbox surface. Every bind the plan mounts at
/// its own spelling is checked — caches, argv-derived binds, the
/// writable metadata binds, the toolchain's own-path bind, and the
/// worker — and the toolchain source must not resolve to the host
/// root, which would mount the whole host filesystem read-only under
/// `/taisan/toolchain`. `Some(reason)` is a typed boundary refusal;
/// `None` admits the plan. Callers check before rendering argv: a
/// refused plan never reaches `bwrap`.
pub fn plan_denial(plan: &MountPlan) -> Option<String> {
    if let Some(toolchain) = &plan.toolchain {
        let resolved = toolchain
            .canonicalize()
            .unwrap_or_else(|_| toolchain.clone());
        if resolved.parent().is_none() {
            return Some(format!(
                "verification toolchain '{}' resolves to the host root — \
                 the toolchain mount must name a directory below it",
                toolchain.display()
            ));
        }
    }
    let own_path_binds = plan
        .toolchain
        .iter()
        .chain(plan.caches.iter())
        .chain(plan.extra_binds.iter())
        .chain(plan.extra_rw_binds.iter())
        .chain(plan.worker.iter());
    for bind in own_path_binds {
        let resolved = bind.canonicalize().unwrap_or_else(|_| bind.clone());
        if shadows_sandbox_mount(bind) || shadows_sandbox_mount(&resolved) {
            return Some(format!(
                "mount plan bind '{}' covers a supervisor-owned sandbox path",
                bind.display()
            ));
        }
    }
    None
}

/// Wrap `inner_argv` in the in-sandbox CONNECT relay
/// (`docs/v0.3.md` §14.4): the relay binds `127.0.0.1` inside the
/// network namespace, splices each connection to the bind-mounted
/// proxy socket, and stays alive as the sandbox init for the worker's
/// whole life. The contained environment's `HTTPS_PROXY` points at
/// the relay; the worker argv itself never sees the socket path.
pub fn relay_wrapped(inner_argv: &[String], relay_exe: &Path) -> Vec<String> {
    let mut argv = Vec::with_capacity(inner_argv.len() + 6);
    argv.push(relay_exe.to_string_lossy().into_owned());
    argv.push("__sandbox-relay".to_string());
    argv.push(SANDBOX_PROXY_SOCK.to_string());
    argv.push(crate::egress::RELAY_LISTEN.to_string());
    argv.push("--".to_string());
    argv.extend(inner_argv.iter().cloned());
    argv
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
    if let Some(runtime_auth) = &plan.runtime_auth {
        args.extend([
            "--ro-bind".to_string(),
            s(runtime_auth),
            SANDBOX_RUNTIME_AUTH.to_string(),
        ]);
    }
    if let Some(provider_config) = &plan.provider_config {
        args.extend([
            "--ro-bind".to_string(),
            s(provider_config),
            SANDBOX_PROVIDER_CONFIG.to_string(),
        ]);
    }
    if let Some(proxy_sock) = &plan.proxy_sock {
        args.extend([
            "--ro-bind".to_string(),
            s(proxy_sock),
            SANDBOX_PROXY_SOCK.to_string(),
        ]);
    }
    if let Some(toolchain) = &plan.toolchain {
        args.extend([
            "--ro-bind".to_string(),
            s(toolchain),
            SANDBOX_TOOLCHAIN.to_string(),
        ]);
        // Declared argv may name toolchain binaries by their host
        // absolute path; the own-path bind keeps that spelling valid
        // inside the sandbox. The canonical `/taisan/toolchain` mount
        // above is the profile-owned location either way. A root
        // toolchain path is never self-bound — that would mount the
        // whole host filesystem.
        if toolchain.file_name().is_some() {
            args.extend(["--ro-bind".to_string(), s(toolchain), s(toolchain)]);
        }
    }
    for cache in &plan.caches {
        args.extend(["--ro-bind".to_string(), s(cache), s(cache)]);
    }
    for extra in &plan.extra_binds {
        args.extend(["--ro-bind".to_string(), s(extra), s(extra)]);
    }
    for extra in &plan.extra_rw_binds {
        args.extend(["--bind".to_string(), s(extra), s(extra)]);
    }
    if let Some(worker) = &plan.worker {
        args.extend(["--ro-bind".to_string(), s(worker), s(worker)]);
    }
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
/// removes none the launch needs. `egress_proxy` is the in-sandbox
/// relay URL (`egress::RELAY_PROXY_URL`) when the launch's mount plan
/// binds a CONNECT proxy socket — `None` leaves egress fully denied
/// with no proxy variables at all.
pub fn contained_env(base: &WorkerEnv, egress_proxy: Option<&str>) -> WorkerEnv {
    let mut vars = Vec::with_capacity(base.vars.len() + 5);
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
    let mut fixed: Vec<(&str, &str)> = vec![
        ("HOME", "/home"),
        ("TMPDIR", "/tmp"),
        ("GIT_CONFIG_GLOBAL", SANDBOX_GITCONFIG),
    ];
    if let Some(url) = egress_proxy {
        // Both spellings: runtimes disagree on which proxy variable
        // they honor, and the relay is the only egress either way.
        fixed.push(("HTTPS_PROXY", url));
        fixed.push(("https_proxy", url));
    }
    for (name, value) in fixed {
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
    /// Digest of the runtime boundary the launch binds — credential
    /// reference, provider-config content, and model-API hosts
    /// (`docs/v0.3.md` §14.3). `None` on evidence written before the
    /// boundary existed; a boundary change is an identity change and
    /// re-proves the battery. The digest never contains credential
    /// material — only the reference text and non-secret config.
    #[serde(default)]
    boundary_digest: Option<String>,
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
    boundary_digest: Option<&str>,
) -> bool {
    evidence.format == EVIDENCE_FORMAT
        && evidence.profile_id == profile.id
        && evidence.kernel_release == kernel_release
        && evidence.primitive.as_str() == primitive.to_string_lossy()
        && evidence.primitive_version == primitive_version
        && evidence.worker_argv == worker_argv
        && evidence.boundary_digest.as_deref() == boundary_digest
}

/// `<bwrap> --version`, bounded through the same process port every
/// probe uses. The version is part of the evidence identity: a bwrap
/// upgrade re-proves the boundary rather than inheriting the old
/// proof.
fn primitive_version(
    port: &dyn ProcessControlPort,
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
    port: &dyn ProcessControlPort,
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
                    // The 200-byte detail bound lands on a char
                    // boundary — a lossy-rendered probe output can
                    // carry multi-byte characters there.
                    let mut end = trimmed.len().min(200);
                    while !trimmed.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!(": {}", &trimmed[..end])
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
    port: &dyn ProcessControlPort,
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
        Ok(env) => contained_env(&env, None),
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
        runtime_auth: None,
        provider_config: None,
        proxy_sock: None,
        toolchain: None,
        caches: vec![],
        extra_binds: vec![],
        extra_rw_binds: vec![],
        worker: Some(worker.to_path_buf()),
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
/// argv. `boundary_digest` is the runtime boundary's content identity
/// (`RuntimeBoundary::identity_digest`): credential reference,
/// provider-config content, and model-API hosts — `None` when no
/// boundary is configured. `Ok(Gate::Uncontained)` means the profile
/// declares no primitive; `Ok(Gate::Ready)` means the
/// negative-probe evidence for this exact identity is recorded (or
/// was just proven); `Err` is a typed refusal or an infrastructure
/// fault — callers must never substitute the unwrapped launch.
pub fn gate(
    port: &dyn ProcessControlPort,
    workspace_root: &Path,
    profile: &IsolationProfile,
    worker_argv: &[String],
    boundary_digest: Option<&str>,
    now_unix_seconds: i64,
) -> Result<Gate, GateError> {
    gate_with_resolvers(
        port,
        workspace_root,
        profile,
        worker_argv,
        boundary_digest,
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
    port: &dyn ProcessControlPort,
    workspace_root: &Path,
    profile: &IsolationProfile,
    worker_argv: &[String],
    boundary_digest: Option<&str>,
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
            boundary_digest,
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
        boundary_digest: boundary_digest.map(str::to_string),
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
            runtime_auth: None,
            provider_config: None,
            proxy_sock: None,
            toolchain: None,
            caches: vec![],
            extra_binds: vec![PathBuf::from("/opt/worker.py")],
            extra_rw_binds: vec![],
            worker: Some(PathBuf::from("/opt/worker")),
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
        let env = contained_env(&base, None);
        let map: std::collections::BTreeMap<_, _> = env.vars.iter().cloned().collect();
        assert_eq!(map["HOME"], "/home");
        assert_eq!(map["TMPDIR"], "/tmp");
        assert_eq!(map["GIT_CONFIG_GLOBAL"], "/taisan/gitconfig");
        assert_eq!(map["PATH"], "/usr/bin");
        assert_eq!(map["GIT_ALLOW_PROTOCOL"], "file");
        // No egress proxy was granted — no proxy variables exist.
        assert!(!map.contains_key("HTTPS_PROXY"));
        assert!(!map.contains_key("https_proxy"));
        // Missing members are still set — the sandbox contract does
        // not depend on the base env carrying them.
        let sparse = contained_env(&WorkerEnv::default(), None);
        let map: std::collections::BTreeMap<_, _> = sparse.vars.iter().cloned().collect();
        assert_eq!(map["HOME"], "/home");
        assert_eq!(map["GIT_CONFIG_GLOBAL"], "/taisan/gitconfig");
    }

    #[test]
    fn contained_env_adds_proxy_variables_only_when_egress_is_granted() {
        let env = contained_env(&WorkerEnv::default(), Some(crate::egress::RELAY_PROXY_URL));
        let map: std::collections::BTreeMap<_, _> = env.vars.iter().cloned().collect();
        assert_eq!(map["HTTPS_PROXY"], crate::egress::RELAY_PROXY_URL);
        assert_eq!(map["https_proxy"], crate::egress::RELAY_PROXY_URL);
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
        let binds = extra_binds(&argv, Some(worker));
        assert_eq!(binds, vec![real.clone()]);
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn sandbox_mount_shadowing_covers_both_directions() {
        for denied in [
            "/",
            "/taisan",
            "/taisan/request.json",
            "/taisan/out/nested",
            "/workspace",
            "/workspace/managed/file",
            "/tmp",
            "/run",
            "/home",
            "/proc",
            "/dev",
        ] {
            assert!(shadows_sandbox_mount(Path::new(denied)), "{denied}");
        }
        for allowed in [
            "/opt/worker.py",
            "/usr/bin/env",
            "/etc/ssl/cert.pem",
            "/var/lib/cache",
            "/taisanevil/x",
            "/tmp/host-file",
            "/home/user/.cache/toolchain",
        ] {
            assert!(!shadows_sandbox_mount(Path::new(allowed)), "{allowed}");
        }
    }

    #[test]
    fn extra_binds_skip_spellings_that_cover_the_sandbox_surface() {
        let worker = Path::new("/opt/worker");
        // `/` and `/tmp` exist on every host, so the skip is the
        // guard's doing — not the missing-path skip. `/etc` is a
        // same-spelling system bind and still collects.
        let real = std::env::temp_dir().join("workloop-extra-bind-shadow");
        std::fs::write(&real, "x").unwrap();
        let argv = vec![
            "worker".into(),
            "/".into(),
            "/tmp".into(),
            "/etc".into(),
            real.to_string_lossy().into_owned(),
        ];
        let binds = extra_binds(&argv, Some(worker));
        assert_eq!(binds, vec![PathBuf::from("/etc"), real.clone()]);
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn plan_denial_refuses_a_root_toolchain() {
        let mut plan = plan_fixture();
        plan.toolchain = Some(PathBuf::from("/"));
        let reason = plan_denial(&plan).expect("a root toolchain refuses");
        assert!(reason.contains("host root"), "{reason}");
        // Spellings that resolve to the root refuse identically.
        plan.toolchain = Some(PathBuf::from("/."));
        assert!(plan_denial(&plan).is_some());
        // A named directory admits.
        plan.toolchain = Some(PathBuf::from("/opt/toolchain"));
        assert!(plan_denial(&plan).is_none());
    }

    #[test]
    fn plan_denial_refuses_binds_over_the_sandbox_surface() {
        let mut plan = plan_fixture();
        plan.caches = vec![PathBuf::from("/")];
        assert!(plan_denial(&plan).is_some());
        plan.caches = vec![PathBuf::from("/taisan")];
        assert!(plan_denial(&plan).is_some());
        plan.caches = vec![];
        plan.extra_binds = vec![PathBuf::from("/taisan/request.json")];
        assert!(plan_denial(&plan).is_some());
        plan.extra_binds = vec![];
        plan.extra_rw_binds = vec![PathBuf::from("/workspace")];
        assert!(plan_denial(&plan).is_some());
        // The fixture plan itself admits.
        plan.extra_rw_binds = vec![];
        assert!(plan_denial(&plan).is_none());
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
        let err = gate(
            &port,
            &root,
            &primitive_profile(),
            &["worker".into()],
            None,
            1000,
        )
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
