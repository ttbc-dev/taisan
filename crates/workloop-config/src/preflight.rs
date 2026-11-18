//! Setup and permission preflight (U2.4): `init` scaffolds the
//! installation profile (`config.toml`) only from observed evidence,
//! and `check` runs the ordered, offline, fail-closed preflight.
//!
//! Design sources: `docs/installation-config.md` (setup contract, the
//! 8-stage `check` sequence, credential-reference existence rules,
//! failure/repair codes) and `docs/retention-policy.md` (the state
//! layout the paths/store stages read against).
//!
//! Ordering and fail-closure are the contract: stages run in the
//! documented order, the first failing stage stops the sequence, every
//! remaining stage is reported `not_run`, and each failure carries a
//! stage-scoped [`Diagnostic`] naming the key or path. `check` is
//! read-only by default — it writes nothing locally and touches no
//! remote state; the opt-in [`ProbeMode::Enabled`] probe is the only
//! remote path, and the probe port ([`workloop_core::ports::ProbePort`])
//! exposes identity and capability *reads* only, so a probe run can
//! never write remotely. Observed-identity evidence is persisted at
//! `init` (from explicit observed pointers — never fabricated) and
//! refreshed in the config only by a *passing* `check --probe`.
//!
//! Success is never a grant: `check` produces a per-run report, re-runs
//! every stage against live local state each time (including credential
//! existence), and dispatch-time revalidation remains mandatory — a
//! clean setup that is later revoked fails the next `check` at
//! credential resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use workloop_core::credential::CredentialProvider;
use workloop_core::identity::ActorRole;
use workloop_core::ports::{CapabilityRead, IdentityRead, ObservedIdentity, ProbePort};
use workloop_core::worker::{DEFAULT_ISOLATION_PROFILE_ID, isolation_profile};

use crate::profile::{
    Profile, SCHEMA_VERSION, default_workspace_root, parse_profile, validate_root_separation,
};

/// Mirrors `workloop_store::sqlite::FORMAT_VERSION`: the highest store
/// format header this offline preflight recognizes as supported. The
/// store itself stays authoritative at open; this constant only lets
/// `check` name a newer header without linking the store — `check`
/// never opens, migrates, or writes the store.
const KNOWN_STORE_FORMAT_MAX: i64 = 9;

/// The SQLite header magic ("SQLite format 3\0") — a file without it
/// is not a store at all.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Bound for every child process `check` starts (credential `command:`
/// existence, observed runtime version). A wedged binary can never
/// hang the preflight: the child is killed when the bound elapses and
/// the stage reports the bounded failure.
const PREFLIGHT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The ordered preflight stages (`docs/installation-config.md` setup
/// contract). The issue's shorthand — config parse → credential-ref
/// presence → capability declarations — names the spine of this
/// documented sequence; stage order is [`STAGE_ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStage {
    /// (1) Parse `config.toml`: schema version, unknown keys, structural validity.
    ConfigParse,
    /// (2) Identity tuples: role completeness, host/repository/actor
    /// scoping, allowlist coherence, and the persisted observed-identity
    /// evidence every bound role actor must carry.
    IdentityTuples,
    /// (3) Credential-reference existence: slots resolvable in existence
    /// only — values are never echoed or read into output.
    CredentialRefs,
    /// (4) Paths: state/workspace roots exist or are creatable, writable,
    /// correctly nested.
    Paths,
    /// (5) Worker runtime: `worker.argv` resolves deterministically and
    /// the pinned `runtime_version` matches the observed binary.
    WorkerRuntime,
    /// (6) Isolation primitives: the declared sandbox mechanism is present.
    IsolationPrimitives,
    /// (7) Store: schema version recognized, no unknown/corrupt state.
    Store,
    /// (8) `--probe` only: read-only identity and capability reads per
    /// bound actor on the pinned repository context.
    ProbeCapabilities,
}

/// The 8 documented stages in order. A plain `check` runs (1)–(7) and
/// reports (8) `not_run`; `check --probe` runs it last.
pub const STAGE_ORDER: [CheckStage; 8] = [
    CheckStage::ConfigParse,
    CheckStage::IdentityTuples,
    CheckStage::CredentialRefs,
    CheckStage::Paths,
    CheckStage::WorkerRuntime,
    CheckStage::IsolationPrimitives,
    CheckStage::Store,
    CheckStage::ProbeCapabilities,
];

impl CheckStage {
    /// The stable stage id used in reports and diagnostics.
    pub fn id(self) -> &'static str {
        match self {
            Self::ConfigParse => "config_parse",
            Self::IdentityTuples => "identity_tuples",
            Self::CredentialRefs => "credential_refs",
            Self::Paths => "paths",
            Self::WorkerRuntime => "worker_runtime",
            Self::IsolationPrimitives => "isolation_primitives",
            Self::Store => "store",
            Self::ProbeCapabilities => "probe_capabilities",
        }
    }
}

/// One stage's outcome. `not_run` stages were gated by an earlier
/// failure (or by `--probe` not being requested) — they never ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pass,
    Fail,
    NotRun,
}

/// A stage-scoped failure: a bounded failure class, the exact key or
/// path it concerns, and the operator-facing message. Messages name
/// identifiers (roles, keys, variable names, paths, mode bits) and
/// never carry a credential value — nothing the preflight reads as a
/// secret can appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// The documented failure/repair code (`config.absent`,
    /// `credential.missing`, `capability.denied`, …).
    pub code: String,
    /// The config key or filesystem path the failure is scoped to.
    pub key: String,
    /// Bounded operator-facing explanation.
    pub message: String,
}

impl Diagnostic {
    fn new(code: &str, key: impl AsRef<str>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            key: key.as_ref().into(),
            message: message.into(),
        }
    }
}

/// One executed (or gated) stage in the per-run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageResult {
    pub stage: CheckStage,
    pub status: StageStatus,
    /// Present and non-empty exactly when `status` is `fail`.
    pub diagnostics: Vec<Diagnostic>,
}

/// The kind of remote operation a probe performed. The enum has one
/// variant by construction: the probe port exposes reads only, so an
/// operation log entry can never be anything but a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOperationKind {
    Read,
}

/// One remote probe call, in execution order. Every attempted read is
/// logged — including one that failed — so the log is the complete
/// account of what the run touched remotely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeOperation {
    pub kind: ProbeOperationKind,
    /// `identity` or `capabilities`.
    pub call: String,
    pub role: String,
    pub actor: String,
}

/// The per-run structured preflight report. `check` persists no grant:
/// this structure is produced fresh on every invocation and is the only
/// artifact a passing run leaves behind (plus, for a passing
/// `check --probe`, the refreshed observed-identity evidence in the
/// config — observations, never permissions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    /// All 8 stages in order: executed stages carry `pass`/`fail`,
    /// gated stages carry `not_run`.
    pub stages: Vec<StageResult>,
    /// Every remote operation attempted, in order — empty for a plain
    /// `check` and for any run that stops before the probe stage.
    pub probe_operations: Vec<ProbeOperation>,
    /// Remote writes performed by this run. Always `0`: the probe port
    /// has no write method, so nothing in this module can produce one.
    pub remote_writes: usize,
    /// Success only when every executed stage passed *and* the identity
    /// evidence stage proved an observed identity read for every bound
    /// role actor — never from config declarations alone.
    pub success: bool,
}

impl CheckReport {
    /// The first failing stage, if the run stopped on one.
    pub fn failed_stage(&self) -> Option<&StageResult> {
        self.stages
            .iter()
            .find(|stage| stage.status == StageStatus::Fail)
    }
}

/// How this run treats stage (8), the `--probe`-only stage.
pub enum ProbeMode<'a> {
    /// Plain `check`: stage 8 is reported `not_run`; nothing remote runs.
    Disabled,
    /// `check --probe`: the read-only probe port drives stage 8.
    Enabled(&'a mut dyn ProbePort),
    /// `--probe` was requested but no adapter probe backend is wired —
    /// the stage fails closed with `capability.unknown` rather than
    /// fabricating a probe result or silently skipping the request.
    Unavailable,
}

/// Everything the stage functions read besides the config itself.
/// Environment, executable lookup, clock, and probe backend are all
/// injectable so the contract suite runs offline and deterministically.
pub struct CheckOptions<'a> {
    /// The installation state root (store, bodies, workspaces live under it).
    pub state_root: PathBuf,
    /// The store file to inspect at stage 7; `None` means
    /// `<state_root>/store.db` (the documented layout).
    pub store_path: Option<PathBuf>,
    /// The managed workspace root; `None` means the documented
    /// `<state_root>/workspaces`.
    pub workspace_root: Option<PathBuf>,
    pub probe: ProbeMode<'a>,
    /// Presence check for `env:` credential slots — never returns the
    /// value into report text; the library only tests `is_some()`.
    pub env_lookup: &'a dyn Fn(&str) -> Option<OsString>,
    /// Deterministic PATH-style name resolution for `worker.argv[0]`
    /// (non-absolute) and isolation primitives.
    pub which: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Observation clock for stage-8 evidence refresh.
    pub now_unix_seconds: i64,
}

/// The current unix time in seconds.
pub fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The process-environment credential-slot lookup `check` uses when a
/// caller injects nothing else.
fn default_env_lookup(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

impl<'a> CheckOptions<'a> {
    /// Options against the real process environment, PATH, and clock.
    /// The probe starts disabled — `--probe` is strictly opt-in.
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            store_path: None,
            workspace_root: None,
            probe: ProbeMode::Disabled,
            env_lookup: &default_env_lookup,
            which: &real_which,
            now_unix_seconds: now_unix_seconds(),
        }
    }
}

/// Resolve a bare command name on the process `PATH`. The caller
/// resolves absolute paths itself — an absolute `worker.argv[0]` never
/// consults `PATH`, so ambient surprises cannot redirect a pinned
/// runtime (installation-config CF-22).
fn real_which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|md| md.is_file())
}

/// The outcome of one bounded child-process run.
enum BoundedRun {
    Exited { success: bool, stdout: String },
    TimedOut,
    SpawnFailed(String),
}

/// How much of a child's stdout is retained for the caller. The pipe
/// is always drained to EOF — a verbose child must never block on a
/// full pipe — but only this prefix is kept, so a runaway writer
/// cannot grow the report without bound.
const BOUNDED_STDOUT_CAP: usize = 1024 * 1024;

/// Ambient variables a scrubbed preflight child may inherit: enough
/// for the binary to exec and answer `--version`, nothing that can
/// carry a host credential. `HOME`, token variables, and agent
/// sockets all stay out — the same boundary the supervisor's
/// `env_clear` launch applies. Windows process creation needs its
/// system variables even under `env_clear`.
const SCRUBBED_ENV_PASSTHROUGH: &[&str] =
    &["PATH", "LANG", "LC_ALL", "TMPDIR", "SYSTEMROOT", "WINDIR"];

/// The environment policy for one bounded run. A `command:` credential
/// provider resolves under the ambient environment — the same
/// environment dispatch gives it — while the worker `--version` probe
/// runs scrubbed so a compromised runtime cannot read host credentials
/// through a plain `check`.
enum ChildEnv {
    Inherited,
    Scrubbed,
}

/// Run `argv` with no shell, bounded by `timeout` for the whole run —
/// process completion and stdout collection share the one deadline.
/// stdin and stderr are discarded; stdout is drained concurrently so
/// output beyond pipe capacity cannot deadlock a healthy child, and
/// only [`BOUNDED_STDOUT_CAP`] bytes are retained. The child leads its
/// own process group, so the deadline's kill reaches every descendant
/// still inside it — a helper inheriting stdout cannot stretch the
/// run past the bound.
fn run_bounded(argv: &[String], timeout: Duration, env: ChildEnv) -> BoundedRun {
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    match env {
        ChildEnv::Inherited => {}
        ChildEnv::Scrubbed => {
            command.env_clear();
            for name in SCRUBBED_ENV_PASSTHROUGH {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return BoundedRun::SpawnFailed(e.to_string()),
    };
    // Drain stdout on a helper thread: the parent only polls the
    // process, so the pipe never fills while the child is running.
    let mut out = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = BOUNDED_STDOUT_CAP.saturating_sub(kept.len());
                    if room > 0 {
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        let _ = tx.send(kept);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_bounded_child(&mut child);
                    return BoundedRun::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                kill_bounded_child(&mut child);
                return BoundedRun::SpawnFailed(e.to_string());
            }
        }
    };
    // The leader exited inside the bound; EOF on its stdout is part of
    // the same bound. A descendant still holding the pipe open must
    // not stretch the run — at the deadline its group is killed and
    // the run reports the timeout.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(stdout) => BoundedRun::Exited {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
        },
        Err(_) => {
            kill_bounded_child(&mut child);
            BoundedRun::TimedOut
        }
    }
}

/// Kill a bounded run's whole process tree. The child was spawned with
/// `process_group(0)`, so its pid is also its group id and the group
/// signal reaches every descendant still inside it — the same
/// containment the handoff runner applies. Non-unix platforms can only
/// signal the immediate child.
#[cfg(unix)]
fn kill_bounded_child(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_bounded_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Classify a `parse_profile` refusal into a stage-1 failure code.
/// The message itself names the offending key; the code is the class.
fn parse_failure_code(message: &str) -> &'static str {
    if message.contains("invalid TOML") {
        "config.invalid_toml"
    } else if message.contains("unknown field") {
        "config.invalid_key"
    } else if message.contains("schema_version") {
        "config.schema_version"
    } else {
        "config.invalid"
    }
}

/// Stage (1): read and validate `config.toml`. Read-only — a refused
/// load leaves the file byte-identical; an absent file is
/// `config.absent`, never an empty pass.
fn stage_config_parse(config_path: &Path) -> (Option<Profile>, Vec<Diagnostic>) {
    let display = config_path.display().to_string();
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                None,
                vec![Diagnostic::new(
                    "config.absent",
                    &display,
                    format!(
                        "config.absent: {display} does not exist — run init or supply --config"
                    ),
                )],
            );
        }
        Err(e) => {
            return (
                None,
                vec![Diagnostic::new(
                    "config.unreadable",
                    &display,
                    format!("config read: {e}"),
                )],
            );
        }
    };
    if text.trim().is_empty() {
        return (
            None,
            vec![Diagnostic::new(
                "config.invalid",
                &display,
                format!("config: {display} exists but is empty — remove it to reinitialize"),
            )],
        );
    }
    match parse_profile(&text) {
        Ok(profile) => (Some(profile), vec![]),
        Err(message) => (
            None,
            vec![Diagnostic::new(
                parse_failure_code(&message),
                &display,
                message,
            )],
        ),
    }
}

/// The bound `(host, actor)` tuple of a role. Load refuses a role
/// missing either field; this stays total for hand-built profiles.
fn bound_tuple<'a>(
    role_name: &str,
    role: &'a crate::profile::RoleSection,
) -> Result<(&'a str, &'a str), Diagnostic> {
    match (role.host.as_deref(), role.actor.as_deref()) {
        (Some(host), Some(actor)) => Ok((host, actor)),
        _ => Err(Diagnostic::new(
            "identity.tuple_unresolved",
            format!("roles.{role_name}"),
            format!("role '{role_name}' is missing its host or actor binding"),
        )),
    }
}

/// Stage (2): identity tuples — role completeness, every bound role's
/// host carrying repositories, allowlist coherence (every publication
/// target has a bound publication role on its host and vice versa),
/// and the persisted observed-identity evidence for every bound role
/// actor (present, matching the bound actor, the host endpoint, and a
/// declared repository). Success here is what proves an *observed*
/// identity read happened — config declarations alone never satisfy it.
fn stage_identity_tuples(profile: &Profile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if profile.roles.is_empty() {
        diagnostics.push(Diagnostic::new(
            "identity.roles_missing",
            "roles",
            "no role is bound — bind at least one of publication/review/discussion before check can pass",
        ));
        return diagnostics;
    }
    for (role_name, role) in &profile.roles {
        let Ok((host_name, actor)) = bound_tuple(role_name, role) else {
            diagnostics.push(bound_tuple(role_name, role).unwrap_err());
            continue;
        };
        let Some(host) = profile.hosts.get(host_name) else {
            // Load already refuses an undeclared host; this defensive
            // arm keeps the stage total rather than panicking.
            diagnostics.push(Diagnostic::new(
                "identity.tuple_unresolved",
                format!("roles.{role_name}.host"),
                format!("role '{role_name}' references an undeclared host"),
            ));
            continue;
        };
        if host.repositories.is_empty() {
            diagnostics.push(Diagnostic::new(
                "identity.host_no_repositories",
                format!("hosts.{host_name}.repositories"),
                format!("host '{host_name}' declares no repository — a role bound to it can never operate"),
            ));
        }
        // Allowlist coherence for the bound publication role: its host
        // must declare at least one publication target, or the role
        // can never exercise publication authority anywhere.
        if role_name == "publication"
            && !host.repositories.iter().any(|repository| {
                repository.use_.as_deref() == Some("publication") && repository.id.is_some()
            })
        {
            diagnostics.push(Diagnostic::new(
                "identity.allowlist_incoherent",
                format!("hosts.{host_name}.repositories"),
                format!(
                    "bound publication role '{role_name}' has no use = \"publication\" repository on host '{host_name}'"
                ),
            ));
        }
        let key = format!("identity_evidence.{role_name}");
        let Some(evidence) = profile.identity_evidence.get(role_name) else {
            diagnostics.push(Diagnostic::new(
                "identity.evidence_absent",
                &key,
                format!(
                    "no observed identity evidence for bound role actor '{actor}' — init observed it \
                     once or a passing check --probe refreshes it; check never claims success from \
                     config declarations alone"
                ),
            ));
            continue;
        };
        if evidence.login != actor {
            diagnostics.push(Diagnostic::new(
                "identity.evidence_mismatch",
                format!("{key}.login"),
                format!(
                    "observed identity '{}' does not match the bound actor '{actor}'",
                    evidence.login
                ),
            ));
        }
        let endpoint = host.endpoint.as_deref().unwrap_or_default();
        if evidence.host_instance != endpoint {
            diagnostics.push(Diagnostic::new(
                "identity.evidence_mismatch",
                format!("{key}.host_instance"),
                format!(
                    "observed host instance '{}' does not match the declared endpoint '{endpoint}'",
                    evidence.host_instance
                ),
            ));
        }
        let declared: BTreeSet<&str> = host
            .repositories
            .iter()
            .filter_map(|repository| repository.id.as_deref())
            .collect();
        if !declared.contains(evidence.repository.as_str()) {
            diagnostics.push(Diagnostic::new(
                "identity.evidence_mismatch",
                format!("{key}.repository"),
                format!(
                    "observed repository '{}' is not declared under host '{host_name}'",
                    evidence.repository
                ),
            ));
        }
    }
    // Allowlist coherence the other way: a declared publication target
    // whose host binds no publication role is a dead allowlist entry —
    // nothing could ever publish there under this configuration.
    for (host_name, host) in &profile.hosts {
        let has_target = host
            .repositories
            .iter()
            .any(|repository| repository.use_.as_deref() == Some("publication"));
        if !has_target {
            continue;
        }
        let publisher_bound = profile.roles.iter().any(|(role_name, role)| {
            role_name == "publication" && role.host.as_deref() == Some(host_name)
        });
        if !publisher_bound {
            diagnostics.push(Diagnostic::new(
                "identity.allowlist_incoherent",
                format!("hosts.{host_name}.repositories"),
                format!(
                    "host '{host_name}' declares a use = \"publication\" repository but no publication role is bound to it"
                ),
            ));
        }
    }
    diagnostics
}

/// Stage (3): credential-reference *existence*, never value. `env:` —
/// the variable exists (its value is never read into output). `file:` —
/// the slot exists and is a regular owner-readable, owner-only file
/// (0600/0400), checked from metadata before any byte would be read.
/// `command:` — the argv exits zero inside the bound. A missing slot is
/// `credential.missing`; a provider that exists but fails is
/// `credential.unresolvable`. Reference shapes (which may embed pasted
/// secrets) are never echoed.
fn stage_credential_refs(profile: &Profile, options: &CheckOptions) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (role_name, role) in &profile.roles {
        let key = format!("roles.{role_name}.credential_ref");
        let Some(reference) = role.credential_ref.as_deref() else {
            // Load refuses a role without `credential_ref`; defensive.
            diagnostics.push(Diagnostic::new(
                "credential.missing",
                &key,
                format!("role '{role_name}' declares no credential_ref"),
            ));
            continue;
        };
        let provider = match CredentialProvider::parse_str(reference) {
            Ok(provider) => provider,
            Err(rule) => {
                diagnostics.push(Diagnostic::new(
                    "credential.missing",
                    &key,
                    format!("roles.{role_name}.credential_ref: {rule}"),
                ));
                continue;
            }
        };
        match provider {
            CredentialProvider::Env { name } => {
                if (options.env_lookup)(&name).is_none() {
                    diagnostics.push(Diagnostic::new(
                        "credential.missing",
                        &key,
                        format!("credential slot env:{name} for role '{role_name}' is not set — define it before check can pass"),
                    ));
                }
            }
            CredentialProvider::File { path } => {
                if let Err(message) = file_slot_exists(&path) {
                    diagnostics.push(Diagnostic::new(&message.0, &key, message.1));
                }
            }
            CredentialProvider::Command { argv } => {
                match run_bounded(&argv, PREFLIGHT_COMMAND_TIMEOUT, ChildEnv::Inherited) {
                    BoundedRun::Exited { success: true, .. } => {}
                    BoundedRun::Exited { success: false, .. } => diagnostics.push(Diagnostic::new(
                        "credential.unresolvable",
                        &key,
                        format!("command: credential provider for role '{role_name}' exited with a non-zero status"),
                    )),
                    BoundedRun::TimedOut => diagnostics.push(Diagnostic::new(
                        "credential.unresolvable",
                        &key,
                        format!(
                            "command: credential provider for role '{role_name}' did not exit within the {}s preflight bound",
                            PREFLIGHT_COMMAND_TIMEOUT.as_secs()
                        ),
                    )),
                    BoundedRun::SpawnFailed(detail) => diagnostics.push(Diagnostic::new(
                        "credential.unresolvable",
                        &key,
                        format!("command: credential provider for role '{role_name}' could not be started ({detail})"),
                    )),
                }
            }
        }
    }
    diagnostics
}

/// Metadata-only existence for a `file:` slot: `(code, message)`.
/// The path itself is not echoed (it may embed operator-chosen names
/// the same hygiene rule protects for the reference), but the role and
/// the exact rule are always named via the caller's key.
#[cfg(unix)]
fn file_slot_exists(path: &Path) -> Result<(), (String, String)> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                "credential.missing".into(),
                "file: credential slot is not present — no file exists at the configured path"
                    .into(),
            ));
        }
        Err(e) => {
            return Err((
                "credential.unresolvable".into(),
                format!("file: credential slot could not be inspected ({e})"),
            ));
        }
    };
    if !metadata.is_file() {
        return Err((
            "credential.unresolvable".into(),
            "file: credential slot is not a regular file".into(),
        ));
    }
    // The same ownership contract `read_credential_file` enforces at
    // dispatch: a foreign-owned file can never resolve, so preflight
    // must not pass it on permission bits alone.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err((
            "credential.unresolvable".into(),
            "file: credential slot is not owned by the current user — dispatch refuses foreign-owned files".into(),
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err((
            "credential.unresolvable".into(),
            format!(
                "file: credential slot has mode {mode:o} — owner-only permissions (0600 or 0400) \
                 are required and the value is never read under weak permissions"
            ),
        ));
    }
    if mode & 0o400 == 0 {
        return Err((
            "credential.unresolvable".into(),
            format!("file: credential slot has mode {mode:o} — the owner cannot read it"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn file_slot_exists(path: &Path) -> Result<(), (String, String)> {
    if !path.exists() {
        return Err((
            "credential.missing".into(),
            "file: credential slot is not present — no file exists at the configured path".into(),
        ));
    }
    Err((
        "credential.unresolvable".into(),
        "file: credential slots fail closed on non-POSIX platforms (permissions cannot be verified)".into(),
    ))
}

/// Stage (4): installation paths — all absolute (never the ambient
/// working directory), correctly nested per the documented layout, and
/// each state/workspace root present-and-writable or creatable. The
/// stage only inspects: it creates nothing and writes nothing.
fn stage_paths(config_path: &Path, state_root: &Path, workspace_root: &Path) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (name, path) in [
        ("config path", config_path),
        ("state root", state_root),
        ("workspace root", workspace_root),
    ] {
        if !path.is_absolute() {
            diagnostics.push(Diagnostic::new(
                "paths.relative",
                name,
                format!(
                    "{name} must be absolute, never the ambient working directory (got '{}')",
                    path.display()
                ),
            ));
        }
    }
    if !diagnostics.is_empty() {
        return diagnostics;
    }
    if let Err(message) = validate_root_separation(config_path, state_root, workspace_root) {
        // The shared validator's message names the offender; the key is
        // derived from the same message so the diagnostic stays scoped.
        let key = if message.contains("config path") {
            config_path
        } else if message.contains("workspace root") && message.contains("nested") {
            state_root
        } else {
            workspace_root
        };
        diagnostics.push(Diagnostic::new(
            "paths.nested",
            key.display().to_string(),
            message,
        ));
    }
    for (name, path) in [
        ("state root", state_root),
        ("workspace root", workspace_root),
    ] {
        if let Err(message) = directory_usable(path) {
            diagnostics.push(Diagnostic::new(
                &message.0,
                path.display().to_string(),
                format!("{name}: {}", message.1),
            ));
        }
    }
    diagnostics
}

/// Whether a root exists-and-is-writable or is creatable — judged from
/// metadata only, so the inspection itself never writes.
#[cfg(unix)]
fn directory_usable(path: &Path) -> Result<(), (String, String)> {
    use std::os::unix::ffi::OsStrExt;
    let usable = |dir: &Path, code_on_failure: &'static str| -> Result<(), (String, String)> {
        let metadata = match std::fs::metadata(dir) {
            Ok(metadata) => metadata,
            Err(e) => return Err((code_on_failure.into(), format!("{}: {e}", dir.display()))),
        };
        if !metadata.is_dir() {
            return Err((
                code_on_failure.into(),
                format!("{} exists but is not a directory", dir.display()),
            ));
        }
        // access(2) asks the kernel whether THIS process can write and
        // search the directory — real uid/gid, supplementary groups,
        // and ACLs included. Owner mode bits cannot answer that: a
        // foreign-owned 0755 directory looks writable to its owner and
        // a 0666 directory lacks the search permission creation needs.
        let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return Err((
                code_on_failure.into(),
                format!("{}: path cannot be checked", dir.display()),
            ));
        };
        if unsafe { libc::access(c_path.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
            return Err((
                code_on_failure.into(),
                format!(
                    "{} is not writable and searchable by the current process",
                    dir.display()
                ),
            ));
        }
        Ok(())
    };
    if path.exists() {
        return usable(path, "paths.unwritable");
    }
    // Absent: creatable when the nearest existing ancestor is a usable
    // directory. The check itself creates nothing.
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if dir.exists() {
            return usable(dir, "paths.not_creatable").map_err(|(code, message)| {
                (
                    code,
                    format!("{message} — the missing root below it is not creatable"),
                )
            });
        }
        ancestor = dir.parent();
    }
    Err((
        "paths.not_creatable".into(),
        format!("no existing parent directory for {}", path.display()),
    ))
}

#[cfg(not(unix))]
fn directory_usable(path: &Path) -> Result<(), (String, String)> {
    if path.exists() {
        if path.is_dir() {
            return Ok(());
        }
        return Err((
            "paths.unwritable".into(),
            format!("{} exists but is not a directory", path.display()),
        ));
    }
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if dir.exists() {
            if dir.is_dir() {
                return Ok(());
            }
            return Err((
                "paths.not_creatable".into(),
                format!("{} exists but is not a directory", dir.display()),
            ));
        }
        ancestor = dir.parent();
    }
    Err((
        "paths.not_creatable".into(),
        format!("no existing parent directory for {}", path.display()),
    ))
}

/// The pin form itself is unsupported — neither `>=X.Y.Z` nor
/// `X.Y.Z`. Distinct from a *mismatch*: an unsupported pin never
/// silently passes and never silently mismatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedVersionPin;

/// Parse a `>=X.Y.Z` or exact `X.Y.Z` version pin against an observed
/// triple. `Err` means the pin form itself is unsupported — never a
/// silent pass and never a silent mismatch.
pub fn version_satisfies(
    pin: &str,
    observed: (u64, u64, u64),
) -> Result<bool, UnsupportedVersionPin> {
    fn triple(text: &str) -> Result<(u64, u64, u64), UnsupportedVersionPin> {
        let parts: Vec<&str> = text.trim().split('.').collect();
        match parts.as_slice() {
            [major, minor, patch] => Ok((
                major.parse().map_err(|_| UnsupportedVersionPin)?,
                minor.parse().map_err(|_| UnsupportedVersionPin)?,
                patch.parse().map_err(|_| UnsupportedVersionPin)?,
            )),
            _ => Err(UnsupportedVersionPin),
        }
    }
    if let Some(rest) = pin.strip_prefix(">=") {
        return Ok(observed >= triple(rest)?);
    }
    Ok(observed == triple(pin)?)
}

/// Extract the first `X.Y.Z` triple from observed `--version` output.
fn observed_version(stdout: &str) -> Option<(u64, u64, u64)> {
    let digits: Vec<(usize, char)> = stdout
        .char_indices()
        .filter(|(_, c)| c.is_ascii_digit())
        .collect();
    let bytes = stdout.as_bytes();
    for (start, _) in &digits {
        let mut end = *start;
        // major
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end >= bytes.len() || bytes[end] != b'.' {
            continue;
        }
        let minor_start = end + 1;
        let mut minor_end = minor_start;
        while minor_end < bytes.len() && bytes[minor_end].is_ascii_digit() {
            minor_end += 1;
        }
        if minor_end == minor_start || minor_end >= bytes.len() || bytes[minor_end] != b'.' {
            continue;
        }
        let patch_start = minor_end + 1;
        let mut patch_end = patch_start;
        while patch_end < bytes.len() && bytes[patch_end].is_ascii_digit() {
            patch_end += 1;
        }
        if patch_end == patch_start {
            continue;
        }
        let major = stdout[*start..end].parse().ok()?;
        let minor = stdout[minor_start..minor_end].parse().ok()?;
        let patch = stdout[patch_start..patch_end].parse().ok()?;
        return Some((major, minor, patch));
    }
    None
}

/// Stage (5): the worker runtime resolves deterministically and the
/// pinned version matches the observed binary. No `worker.argv` means
/// no runtime is declared — nothing to verify, never a silent claim
/// that a runtime exists.
fn stage_worker_runtime(profile: &Profile, options: &CheckOptions) -> Vec<Diagnostic> {
    if profile.worker.argv.is_empty() {
        return Vec::new();
    }
    let argv0 = &profile.worker.argv[0];
    let key = "worker.argv";
    let resolved = if Path::new(argv0).is_absolute() {
        if !Path::new(argv0).exists() {
            return vec![Diagnostic::new(
                "runtime.missing",
                key,
                format!("worker argv[0] '{argv0}' does not exist"),
            )];
        }
        if !is_executable_file(Path::new(argv0)) {
            return vec![Diagnostic::new(
                "runtime.not_executable",
                key,
                format!("worker argv[0] '{argv0}' is not an executable file"),
            )];
        }
        Some(PathBuf::from(argv0))
    } else {
        (options.which)(argv0)
    };
    let Some(resolved) = resolved else {
        return vec![Diagnostic::new(
            "runtime.missing",
            key,
            format!(
                "worker argv[0] '{argv0}' did not resolve on the configured lookup — install the pinned runtime or use an absolute path"
            ),
        )];
    };
    let Some(pin) = profile.worker.runtime_version.as_deref() else {
        return Vec::new();
    };
    let mut version_argv = vec![resolved.to_string_lossy().into_owned()];
    version_argv.extend(profile.worker.argv.iter().skip(1).cloned());
    version_argv.push("--version".into());
    let stdout = match run_bounded(&version_argv, PREFLIGHT_COMMAND_TIMEOUT, ChildEnv::Scrubbed) {
        BoundedRun::Exited {
            success: true,
            stdout,
        } => stdout,
        BoundedRun::Exited { success: false, .. } => {
            return vec![Diagnostic::new(
                "runtime.version_unknown",
                "worker.runtime_version",
                "the runtime's --version command exited with a non-zero status",
            )];
        }
        BoundedRun::TimedOut => {
            return vec![Diagnostic::new(
                "runtime.version_unknown",
                "worker.runtime_version",
                format!(
                    "the runtime did not answer --version within the {}s preflight bound",
                    PREFLIGHT_COMMAND_TIMEOUT.as_secs()
                ),
            )];
        }
        BoundedRun::SpawnFailed(detail) => {
            return vec![Diagnostic::new(
                "runtime.missing",
                key,
                format!("worker runtime could not be started to read its version ({detail})"),
            )];
        }
    };
    let Some(observed) = observed_version(&stdout) else {
        return vec![Diagnostic::new(
            "runtime.version_unknown",
            "worker.runtime_version",
            "the runtime's --version output did not yield an X.Y.Z triple — the pin cannot be verified",
        )];
    };
    match version_satisfies(pin, observed) {
        Ok(true) => Vec::new(),
        Ok(false) => vec![Diagnostic::new(
            "runtime.version_mismatch",
            "worker.runtime_version",
            format!(
                "observed runtime version {}.{}.{} does not satisfy the pinned constraint '{pin}'",
                observed.0, observed.1, observed.2
            ),
        )],
        Err(UnsupportedVersionPin) => vec![Diagnostic::new(
            "runtime.version_pin",
            "worker.runtime_version",
            format!("unsupported version pin '{pin}' — expected '>=X.Y.Z' or 'X.Y.Z'"),
        )],
    }
}

/// The sandbox primitive an approved profile requires at preflight.
/// The reference profile needs `bwrap`; the contract-harness profile
/// launches without an external primitive. The registry itself stays
/// closed in `workloop-core` — an unapproved id never reaches here.
fn required_isolation_primitive(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        DEFAULT_ISOLATION_PROFILE_ID => Some("bwrap"),
        _ => None,
    }
}

/// Stage (6): the declared sandbox mechanism is present. Only a
/// declared worker makes a launch possible — an installation with no
/// `worker.argv` declares no launch, so no primitive is demanded.
/// Failures name the primitive (the documented `runtime.missing` case:
/// a host with no `bwrap` fails here, earlier passes still reported).
fn stage_isolation_primitives(profile: &Profile, options: &CheckOptions) -> Vec<Diagnostic> {
    let profile_id = profile
        .worker
        .isolation_profile
        .as_deref()
        .unwrap_or(DEFAULT_ISOLATION_PROFILE_ID);
    if isolation_profile(profile_id).is_none() {
        // Load refuses unknown names; defensive fail-closed arm.
        return vec![Diagnostic::new(
            "isolation.unapproved",
            "worker.isolation_profile",
            format!("'{profile_id}' is not an approved isolation profile"),
        )];
    }
    if profile.worker.argv.is_empty() {
        return Vec::new();
    }
    let Some(primitive) = required_isolation_primitive(profile_id) else {
        return Vec::new();
    };
    if (options.which)(primitive).is_none() {
        return vec![Diagnostic::new(
            "runtime.missing",
            "worker.isolation_profile",
            format!(
                "isolation primitive '{primitive}' required by profile '{profile_id}' is not present"
            ),
        )];
    }
    Vec::new()
}

/// How many times stage (7) re-reads the main header and the WAL
/// together before declaring the store uninspectable. A live store
/// under write load can keep moving; the bound keeps `check` finite
/// and fails closed rather than trusting a stale view.
const STORE_INSPECT_ATTEMPTS: u32 = 3;

/// Stage (7): the store is recognized or absent. `check` never opens
/// the store (opening can journal, flip WAL, or migrate) — the header
/// is read directly: magic, page count, and `user_version`. Absent or
/// fresh means the first open will bootstrap; an unversioned file with
/// user pages, a foreign file, or a newer format each fail closed with
/// the store path named.
fn stage_store(store_path: &Path) -> Vec<Diagnostic> {
    let display = store_path.display().to_string();
    // Each attempt pairs a fresh main-header read with a WAL scan. A
    // live store can checkpoint or restart its WAL mid-inspection, so
    // only a quiet attempt classifies; a store that keeps moving fails
    // closed as unreadable rather than trusting a stale view.
    let mut attempt = 0;
    let (page_count, version) = loop {
        attempt += 1;
        let mut header = [0u8; 100];
        let read = match read_store_header(store_path, &mut header) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                return vec![Diagnostic::new(
                    "store.unreadable",
                    &display,
                    format!("store read: {e}"),
                )];
            }
        };
        if read == 0 {
            // A zero-length file is a fresh database; the first open
            // bootstraps it under the store's own format gate.
            return Vec::new();
        }
        if read < 100 || header[..16] != SQLITE_MAGIC[..] {
            return vec![Diagnostic::new(
                "store.corrupt",
                &display,
                format!("{display} is not a SQLite store file"),
            )];
        }
        let page_count = u32::from_be_bytes([header[28], header[29], header[30], header[31]]);
        let version = i64::from(u32::from_be_bytes([
            header[60], header[61], header[62], header[63],
        ]));
        // A WAL-mode store keeps committed writes in `<store>-wal` until
        // a checkpoint lands them in the main file, so the header's
        // version and page count are stale exactly while committed
        // frames are pending — a live initialized store would read as
        // unversioned. Inspect the WAL's committed prefix instead:
        // reads only, so the no-local-writes rule is unchanged.
        match wal_store_facts(store_path, &header) {
            Ok(WalScan::Facts(facts)) => {
                break (
                    facts.committed_pages.unwrap_or(page_count),
                    facts.page1_user_version.unwrap_or(version),
                );
            }
            Ok(WalScan::Quiet) => break (page_count, version),
            Ok(WalScan::Moved) if attempt < STORE_INSPECT_ATTEMPTS => continue,
            Ok(WalScan::Moved) => {
                return vec![Diagnostic::new(
                    "store.unreadable",
                    &display,
                    "the store changed while it was being inspected",
                )];
            }
            Err(message) => {
                return vec![Diagnostic::new("store.unreadable", &display, message)];
            }
        }
    };
    if version == 0 {
        if page_count == 0 {
            return Vec::new(); // fresh header-only database
        }
        return vec![Diagnostic::new(
            "store.unversioned",
            &display,
            "unversioned store file (pre-v1 format); archive it and start a new store",
        )];
    }
    if version > KNOWN_STORE_FORMAT_MAX {
        return vec![Diagnostic::new(
            "store.unsupported",
            &display,
            format!("store format {version} is newer than supported {KNOWN_STORE_FORMAT_MAX}"),
        )];
    }
    Vec::new()
}

fn read_store_header(path: &Path, buffer: &mut [u8; 100]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    file.read(buffer)
}

/// WAL on-disk constants (SQLite file format): the magic's low bit
/// selects the checksum byte order, and every multi-byte field is
/// stored big-endian.
const WAL_MAGIC_LITTLE_ENDIAN: u32 = 0x377f_0682;
const WAL_MAGIC_BIG_ENDIAN: u32 = 0x377f_0683;
const WAL_FORMAT_VERSION: u32 = 3_007_000;
const WAL_HEADER_LEN: usize = 32;
const WAL_FRAME_HEADER_LEN: usize = 24;

/// What the committed prefix of `<store>-wal` proves about the store.
/// `None` fields leave the main-file header authoritative.
struct WalFacts {
    /// Database size in pages after the newest commit frame.
    committed_pages: Option<u32>,
    /// `user_version` recovered from the newest committed page-1
    /// image — the stamp lives in page 1 alone, so only a committed
    /// page-1 frame can make the main header's copy stale.
    page1_user_version: Option<i64>,
}

/// The outcome of one WAL inspection attempt.
enum WalScan {
    /// No committed WAL facts — the main-file header is authoritative.
    Quiet,
    /// Committed facts from a WAL generation that stayed stable across
    /// the whole scan.
    Facts(WalFacts),
    /// The WAL restarted or grew mid-scan — the main header and the
    /// WAL must be inspected again together before classifying.
    Moved,
}

fn wal_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("4-byte field"))
}

/// The WAL cumulative checksum: each u32 pair folds as
/// `s1 += x0 + s2; s2 += x1 + s1` (the second sum sees the updated
/// `s1`), with words read big- or little-endian as the magic selects.
fn wal_checksum(s1: &mut u32, s2: &mut u32, data: &[u8], big_endian: bool) {
    for pair in data.as_chunks::<8>().0 {
        let read = |bytes: &[u8]| {
            if big_endian {
                u32::from_be_bytes(bytes[..4].try_into().expect("4-byte word"))
            } else {
                u32::from_le_bytes(bytes[..4].try_into().expect("4-byte word"))
            }
        };
        let (x0, x1) = (read(&pair[..4]), read(&pair[4..]));
        *s1 = s1.wrapping_add(x0).wrapping_add(*s2);
        *s2 = s2.wrapping_add(x1).wrapping_add(*s1);
    }
}

/// Read the committed prefix of `<store>-wal` when `header` marks the
/// database WAL-mode (format bytes 18/19). `Quiet` means there is
/// nothing committed in the WAL — the main header is then
/// authoritative — `Moved` means the store changed under the scan and
/// the caller must inspect header and WAL together again, and `Err`
/// means the WAL cannot be inspected: the stage reports the check as
/// unavailable rather than trusting a possibly stale header.
fn wal_store_facts(store_path: &Path, header: &[u8; 100]) -> Result<WalScan, String> {
    use std::io::{Read, Seek, SeekFrom};
    if header[18] != 2 || header[19] != 2 {
        return Ok(WalScan::Quiet);
    }
    let mut wal_name = store_path.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let fail = |reason: &str| format!("write-ahead log {}: {reason}", wal_path.display());
    let mut file = match std::fs::File::open(&wal_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WalScan::Quiet),
        Err(e) => return Err(fail(&e.to_string())),
    };
    let len = file.metadata().map_err(|e| fail(&e.to_string()))?.len();
    if len == 0 {
        return Ok(WalScan::Quiet);
    }
    if len < WAL_HEADER_LEN as u64 {
        return Err(fail("the header is truncated"));
    }
    let mut wal_header = [0u8; WAL_HEADER_LEN];
    file.read_exact(&mut wal_header)
        .map_err(|e| fail(&e.to_string()))?;
    let big_endian = match wal_u32(&wal_header[0..4]) {
        WAL_MAGIC_LITTLE_ENDIAN => false,
        WAL_MAGIC_BIG_ENDIAN => true,
        _ => return Err(fail("unknown format")),
    };
    if wal_u32(&wal_header[4..8]) != WAL_FORMAT_VERSION {
        return Err(fail("unknown format version"));
    }
    let page_size = wal_u32(&wal_header[8..12]);
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(fail("invalid page size"));
    }
    let (salt1, salt2) = (wal_u32(&wal_header[16..20]), wal_u32(&wal_header[20..24]));
    let (mut s1, mut s2) = (0u32, 0u32);
    wal_checksum(&mut s1, &mut s2, &wal_header[..24], big_endian);
    if s1 != wal_u32(&wal_header[24..28]) || s2 != wal_u32(&wal_header[28..32]) {
        return Err(fail("header checksum mismatch"));
    }
    let generation_header = wal_header;

    // Walk the frames: salts identify the current WAL generation and
    // the cumulative checksum marks the committed prefix — a torn
    // tail or foreign generation simply ends the scan, exactly as a
    // SQLite reader treats it.
    let page_len = page_size as usize;
    let frame_len = WAL_FRAME_HEADER_LEN + page_len;
    let mut frame_header = [0u8; WAL_FRAME_HEADER_LEN];
    let mut page = vec![0u8; page_len];
    let mut offset = WAL_HEADER_LEN as u64;
    // The open transaction's page-1 image: `None` = no page-1 frame
    // yet, `Some(None)` = a page-1 frame that fails the database
    // magic, `Some(Some(v))` = its `user_version`. Only a commit
    // promotes it — an uncommitted tail frame stays invisible, exactly
    // as a SQLite reader treats it.
    let mut pending_page1: Option<Option<i64>> = None;
    let mut committed_page1_version = None;
    let mut committed_pages = None;
    while offset + frame_len as u64 <= len {
        if file.read_exact(&mut frame_header).is_err() {
            break;
        }
        if wal_u32(&frame_header[8..12]) != salt1 || wal_u32(&frame_header[12..16]) != salt2 {
            break;
        }
        if file.read_exact(&mut page).is_err() {
            break;
        }
        wal_checksum(&mut s1, &mut s2, &frame_header[..8], big_endian);
        wal_checksum(&mut s1, &mut s2, &page, big_endian);
        if s1 != wal_u32(&frame_header[16..20]) || s2 != wal_u32(&frame_header[20..24]) {
            break;
        }
        if wal_u32(&frame_header[0..4]) == 1 {
            // `user_version` sits at bytes 60-63 of the page-1 image —
            // read it now, while the frame is checksum-validated, so
            // no later reread can observe recycled WAL content. The
            // database magic must open the image: bytes 60-63 of a
            // non-database page are not a version.
            pending_page1 =
                Some((page[..16] == SQLITE_MAGIC[..]).then(|| i64::from(wal_u32(&page[60..64]))));
        }
        let n_truncate = wal_u32(&frame_header[4..8]);
        if n_truncate != 0 {
            match pending_page1.take() {
                Some(Some(version)) => committed_page1_version = Some(version),
                Some(None) => {
                    return Err(fail("a committed page-1 frame is not a database page"));
                }
                None => {}
            }
            committed_pages = Some(n_truncate);
        }
        offset += frame_len as u64;
    }

    // The facts are only meaningful for the generation the scan
    // started in: a checkpoint restarting the WAL mid-scan rewrote the
    // header, and appended frames mean the committed prefix moved on —
    // either way the caller re-inspects header and WAL together.
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut wal_header))
        .map_err(|e| fail(&e.to_string()))?;
    if wal_header != generation_header {
        return Ok(WalScan::Moved);
    }
    if file.metadata().map_err(|e| fail(&e.to_string()))?.len() != len {
        return Ok(WalScan::Moved);
    }
    Ok(WalScan::Facts(WalFacts {
        committed_pages,
        page1_user_version: committed_page1_version,
    }))
}

/// Stage (8), `--probe` only: read-only identity and capability reads
/// for every bound actor on the pinned repository context. Order:
/// identity reads for all bound roles (an unreadable probe is
/// `capability.unknown`, a wrong identity is `credential.actor_mismatch`
/// — never a rebinding), then capability reads evaluated per required
/// capability (`unsupported` / `denied` / `unknown` never pass), then,
/// only after every read passed, the observed-identity evidence is
/// refreshed in the config. The first failure stops the stage; nothing
/// remote runs after it.
fn stage_probe(
    profile: &Profile,
    config_path: &Path,
    port: &mut dyn ProbePort,
    operations: &mut Vec<ProbeOperation>,
    now: i64,
) -> Vec<Diagnostic> {
    let mut observed_by_role: BTreeMap<String, ObservedIdentity> = BTreeMap::new();
    for (role_name, role) in &profile.roles {
        let (host_name, actor) = match bound_tuple(role_name, role) {
            Ok(tuple) => tuple,
            Err(diagnostic) => return vec![diagnostic],
        };
        let Some(host) = profile.hosts.get(host_name) else {
            return vec![Diagnostic::new(
                "identity.tuple_unresolved",
                format!("roles.{role_name}.host"),
                format!("role '{role_name}' references an undeclared host"),
            )];
        };
        let endpoint = host.endpoint.clone().unwrap_or_default();
        let repository = profile
            .identity_evidence
            .get(role_name)
            .map(|evidence| evidence.repository.clone())
            .unwrap_or_default();
        operations.push(ProbeOperation {
            kind: ProbeOperationKind::Read,
            call: "identity".into(),
            role: role_name.clone(),
            actor: actor.into(),
        });
        let observed = port.read_actor_identity(&IdentityRead {
            role: role_name.clone(),
            actor: actor.into(),
            host_instance: endpoint.clone(),
            repository: repository.clone(),
        });
        let observed = match observed {
            Ok(observed) => observed,
            Err(detail) => {
                return vec![Diagnostic::new(
                    "capability.unknown",
                    format!("identity_evidence.{role_name}"),
                    format!(
                        "probe identity read for role '{role_name}' is unknown (not denied): {detail}"
                    ),
                )];
            }
        };
        if observed.login != actor {
            return vec![Diagnostic::new(
                "credential.actor_mismatch",
                format!("identity_evidence.{role_name}.login"),
                format!(
                    "probe observed identity '{}' but the bound actor is '{actor}' — the binding is never retargeted here",
                    observed.login
                ),
            )];
        }
        if observed.host_instance != endpoint || observed.repository != repository {
            return vec![Diagnostic::new(
                "identity.context_mismatch",
                format!("identity_evidence.{role_name}"),
                format!(
                    "probe answered outside the pinned context (host '{}', repository '{}')",
                    observed.host_instance, observed.repository
                ),
            )];
        }
        observed_by_role.insert(role_name.clone(), observed);
    }
    for (role_name, role) in &profile.roles {
        let (host_name, actor) = match bound_tuple(role_name, role) {
            Ok(tuple) => tuple,
            Err(diagnostic) => return vec![diagnostic],
        };
        let Some(host) = profile.hosts.get(host_name) else {
            continue; // already refused above
        };
        let required = &profile.capabilities.required;
        let capability_names: Vec<String> = match role_name.as_str() {
            "publication" => required.publication.clone(),
            "review" => required.review.clone(),
            _ => required.discussion.clone(),
        };
        operations.push(ProbeOperation {
            kind: ProbeOperationKind::Read,
            call: "capabilities".into(),
            role: role_name.clone(),
            actor: actor.into(),
        });
        let observation = match port.read_capabilities(&CapabilityRead {
            role: role_name.clone(),
            actor: actor.into(),
            host_instance: host.endpoint.clone().unwrap_or_default(),
            repository: profile
                .identity_evidence
                .get(role_name)
                .map(|evidence| evidence.repository.clone())
                .unwrap_or_default(),
            capabilities: capability_names.clone(),
        }) {
            Ok(observation) => observation,
            Err(detail) => {
                return vec![Diagnostic::new(
                    "capability.unknown",
                    format!("capabilities.required.{role_name}"),
                    format!(
                        "probe capability read for role '{role_name}' is unknown (not denied): {detail}"
                    ),
                )];
            }
        };
        for wanted in &capability_names {
            // A requested capability the probe did not answer is
            // `unknown`, never an empty pass.
            let Some(status) = observation
                .statuses
                .iter()
                .find(|status| status.capability == *wanted)
            else {
                return vec![Diagnostic::new(
                    "capability.unknown",
                    format!("capabilities.required.{role_name}"),
                    format!("probe did not report capability '{wanted}' for role '{role_name}'"),
                )];
            };
            let failure = match (status.support, status.grant) {
                (workloop_core::ports::CapabilitySupport::Unsupported, _) => Some((
                    "capability.unsupported",
                    format!(
                        "required capability '{wanted}' for role '{role_name}' is unsupported by the host"
                    ),
                )),
                (_, workloop_core::ports::CapabilityGrant::Denied) => Some((
                    "capability.denied",
                    format!(
                        "required capability '{wanted}' is denied for actor '{actor}' — re-grant or bind an authorized actor"
                    ),
                )),
                (workloop_core::ports::CapabilitySupport::Unknown, _)
                | (_, workloop_core::ports::CapabilityGrant::Unknown) => Some((
                    "capability.unknown",
                    format!(
                        "required capability '{wanted}' for role '{role_name}' is unknown — hold, never read as denied or supported"
                    ),
                )),
                _ => None,
            };
            if let Some((code, message)) = failure {
                return vec![Diagnostic::new(
                    code,
                    format!("capabilities.required.{role_name}"),
                    message,
                )];
            }
        }
    }
    // Every remote read passed: persist the refreshed observed-identity
    // evidence (observations only — never a permission grant).
    if let Err(message) = refresh_identity_evidence(config_path, &observed_by_role, now) {
        return vec![Diagnostic::new(
            "identity.evidence_refresh",
            config_path.display().to_string(),
            message,
        )];
    }
    Vec::new()
}

/// Rewrite the observed-identity evidence for every bound role from a
/// fully passing probe. Values are round-tripped through the same
/// validator the loader uses; publication is an atomic replace under
/// the config publish lock. Nothing changes when every entry already
/// carries this observation — a no-op run leaves the file untouched.
fn refresh_identity_evidence(
    config_path: &Path,
    observed_by_role: &BTreeMap<String, ObservedIdentity>,
    now: i64,
) -> Result<(), String> {
    let text = std::fs::read_to_string(config_path).map_err(|e| format!("config read: {e}"))?;
    let mut document: toml::Value =
        toml::from_str(&text).map_err(|e| format!("config: {}", e.message().trim_end()))?;
    let table = document
        .as_table_mut()
        .ok_or_else(|| "config: top level is not a table".to_string())?;
    let evidence = table
        .entry("identity_evidence")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let evidence_table = evidence
        .as_table_mut()
        .ok_or_else(|| "identity_evidence: table expected".to_string())?;
    let mut changed = false;
    for (role_name, observed) in observed_by_role {
        let entry = evidence_table
            .entry(role_name.clone())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let entry_table = entry
            .as_table_mut()
            .ok_or_else(|| format!("identity_evidence.{role_name}: table expected"))?;
        let fields = [
            ("login", observed.login.clone()),
            ("host_instance", observed.host_instance.clone()),
            ("repository", observed.repository.clone()),
        ];
        for (field, value) in fields {
            let current = entry_table
                .get(field)
                .and_then(toml::Value::as_str)
                .unwrap_or_default();
            if current != value {
                entry_table.insert(field.into(), toml::Value::String(value));
                changed = true;
            }
        }
        let current_time = entry_table
            .get("observed_unix_seconds")
            .and_then(toml::Value::as_integer)
            .unwrap_or(-1);
        if current_time != now {
            entry_table.insert("observed_unix_seconds".into(), toml::Value::Integer(now));
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let rendered = toml::to_string(&document).map_err(|e| format!("config refresh render: {e}"))?;
    // Never publish a refresh the loader would refuse.
    parse_profile(&rendered).map_err(|e| format!("config refresh validation: {e}"))?;
    replace_atomically(config_path, &rendered)
}

/// Atomically replace `path` with `content`: stage a synced scratch
/// file in the same directory, rename it over the target under the
/// shared config publish lock, then sync the parent. A crash leaves
/// either the old file or the new one — never a partial — and the
/// uniquely named scratch never loads as a config.
fn replace_atomically(path: &Path, content: &str) -> Result<(), String> {
    let _lock = crate::lock_publish(path)?;
    let scratch = crate::scratch_path(path);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
            .map_err(|e| format!("config scratch: {e}"))?;
        file.write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("config scratch write: {e}"))?;
    }
    let result = std::fs::rename(&scratch, path)
        .map_err(|e| format!("config refresh publish: {e}"))
        .and_then(|()| crate::sync_parent(path).map_err(|e| format!("config dir sync: {e}")));
    if result.is_err() {
        let _ = std::fs::remove_file(&scratch);
    }
    result
}

/// Run the ordered offline preflight over `config_path`. Plain
/// (`ProbeMode::Disabled`): stages (1)–(7), stage (8) `not_run`, no
/// probe invocation, no writes anywhere. With `--probe`: stage (8)
/// runs last through the read-only port; on full success the observed
/// identity evidence is refreshed in the config. The first failing
/// stage stops the sequence; later stages are reported `not_run`.
pub fn check(config_path: &Path, options: &mut CheckOptions) -> CheckReport {
    let mut report = CheckReport {
        stages: Vec::new(),
        probe_operations: Vec::new(),
        remote_writes: 0,
        success: false,
    };
    let workspace_root = options
        .workspace_root
        .clone()
        .unwrap_or_else(|| default_workspace_root(&options.state_root));
    let store_path = options
        .store_path
        .clone()
        .unwrap_or_else(|| options.state_root.join("store.db"));

    let advance =
        |report: &mut CheckReport, stage: CheckStage, diagnostics: Vec<Diagnostic>| -> bool {
            let status = if diagnostics.is_empty() {
                StageStatus::Pass
            } else {
                StageStatus::Fail
            };
            report.stages.push(StageResult {
                stage,
                status,
                diagnostics,
            });
            status == StageStatus::Pass
        };
    let finish = |mut report: CheckReport, success: bool| -> CheckReport {
        for stage in STAGE_ORDER {
            if !report.stages.iter().any(|entry| entry.stage == stage) {
                report.stages.push(StageResult {
                    stage,
                    status: StageStatus::NotRun,
                    diagnostics: Vec::new(),
                });
            }
        }
        report.success = success;
        report
    };

    let (profile, diagnostics) = stage_config_parse(config_path);
    advance(&mut report, CheckStage::ConfigParse, diagnostics);
    // `profile` is `None` exactly when stage (1) failed.
    let Some(profile) = profile else {
        return finish(report, false);
    };

    if !advance(
        &mut report,
        CheckStage::IdentityTuples,
        stage_identity_tuples(&profile),
    ) {
        return finish(report, false);
    }
    if !advance(
        &mut report,
        CheckStage::CredentialRefs,
        stage_credential_refs(&profile, options),
    ) {
        return finish(report, false);
    }
    if !advance(
        &mut report,
        CheckStage::Paths,
        stage_paths(config_path, &options.state_root, &workspace_root),
    ) {
        return finish(report, false);
    }
    if !advance(
        &mut report,
        CheckStage::WorkerRuntime,
        stage_worker_runtime(&profile, options),
    ) {
        return finish(report, false);
    }
    if !advance(
        &mut report,
        CheckStage::IsolationPrimitives,
        stage_isolation_primitives(&profile, options),
    ) {
        return finish(report, false);
    }
    if !advance(&mut report, CheckStage::Store, stage_store(&store_path)) {
        return finish(report, false);
    }

    let now = options.now_unix_seconds;
    let diagnostics = match &mut options.probe {
        // Plain check: the probe stage never runs and nothing remote happens.
        ProbeMode::Disabled => {
            report.stages.push(StageResult {
                stage: CheckStage::ProbeCapabilities,
                status: StageStatus::NotRun,
                diagnostics: Vec::new(),
            });
            return finish(report, true);
        }
        ProbeMode::Unavailable => vec![Diagnostic::new(
            "capability.unknown",
            "probe",
            "--probe was requested but no adapter probe backend is wired for this installation — \
             probe reads require the host adapter; nothing was probed and nothing was written",
        )],
        ProbeMode::Enabled(port) => stage_probe(
            &profile,
            config_path,
            &mut **port,
            &mut report.probe_operations,
            now,
        ),
    };
    let success = diagnostics.is_empty();
    advance(&mut report, CheckStage::ProbeCapabilities, diagnostics);
    finish(report, success)
}

/// Everything `init` scaffolds the profile from. All identity values
/// are explicit observed pointers — `init` validates them against each
/// other and never invents one.
pub struct InitRepository {
    pub id: String,
    pub display: Option<String>,
    /// `publication` or `source`.
    pub use_: String,
}

pub struct InitHost {
    /// The `[hosts.<name>]` table alias (display only — host instance
    /// identity is the endpoint).
    pub name: String,
    pub adapter: String,
    /// The stable endpoint identity (the observed host instance).
    pub endpoint: String,
    pub contract_version: u32,
    pub repositories: Vec<InitRepository>,
}

pub struct InitRole {
    /// `publication` / `review` / `discussion`.
    pub role: String,
    pub host: String,
    pub actor: String,
    /// Shape-validated reference (`env:` / `file:` / `command:`) — never
    /// a secret value.
    pub credential_ref: String,
}

/// One observed identity read, supplied explicitly to `init`.
pub struct InitObservation {
    pub role: String,
    /// Observed actor login or application slug — must equal the bound
    /// role's actor.
    pub login: String,
    /// Observed host instance — must equal the role's host endpoint.
    pub host_instance: String,
    /// Observed repository stable id — must be declared under that host.
    pub repository: String,
}

pub struct InitSpec {
    pub hosts: Vec<InitHost>,
    pub roles: Vec<InitRole>,
    /// The egress allowlist (`verification.egress.allowlist`).
    pub egress_allowlist: Vec<String>,
    /// One observation per bound role — `init` refuses to publish an
    /// incomplete one.
    pub observations: Vec<InitObservation>,
    /// When these observations were made (unix seconds).
    pub observed_unix_seconds: i64,
}

/// What a successful `init` wrote (derived from the file that actually
/// landed, never from the staged request).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitOutcome {
    pub path: String,
    pub roles: Vec<String>,
    pub egress_allowlist: Vec<String>,
    pub identity_evidence: Vec<String>,
}

/// The required-capability floor `init` scaffolds — the documented
/// baseline from `docs/installation-config.md`. It declares what Work
/// will require, never what the host supports (that is stage 8's read).
fn documented_required_capabilities() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut required = BTreeMap::new();
    required.insert(
        "publication",
        vec![
            "repository_read",
            "branch_read",
            "push_ref",
            "change_request_read",
            "change_request_create",
            "change_request_update",
            "comment_write",
            "ready",
        ],
    );
    required.insert(
        "review",
        vec![
            "repository_read",
            "branch_read",
            "review_feedback",
            "inline_comment",
            "thread_reply",
            "thread_resolve",
        ],
    );
    required.insert("discussion", vec!["comment_read", "comment_write"]);
    required
}

/// Validate the spec against itself before anything is rendered —
/// every failure names the offending role/host and `init` publishes
/// nothing (a partial init leaves either no config or a complete one).
fn validate_spec(spec: &InitSpec) -> Result<(), String> {
    if spec.hosts.is_empty() {
        return Err("init: at least one host is required".into());
    }
    if spec.roles.is_empty() {
        return Err("init: at least one bound role is required".into());
    }
    if spec.observed_unix_seconds < 0 {
        return Err(format!(
            "init: observed_unix_seconds: negative observation time {} is refused",
            spec.observed_unix_seconds
        ));
    }
    let mut host_names = BTreeSet::new();
    for host in &spec.hosts {
        if !host_names.insert(host.name.as_str()) {
            return Err(format!("init: duplicate host '{}'", host.name));
        }
        if host.repositories.is_empty() {
            return Err(format!(
                "init: host '{}' declares no repository — supply at least one observed repository id",
                host.name
            ));
        }
        for repository in &host.repositories {
            if !matches!(repository.use_.as_str(), "publication" | "source") {
                return Err(format!(
                    "init: hosts.{}.repositories (id {}): unknown use '{}' — expected publication or source",
                    host.name, repository.id, repository.use_
                ));
            }
        }
    }
    let mut role_names = BTreeSet::new();
    let mut bound_roles = BTreeSet::new();
    for role in &spec.roles {
        if ActorRole::from_name(&role.role).is_none() {
            return Err(format!(
                "init: unknown role '{}' — expected publication, review, or discussion",
                role.role
            ));
        }
        if !role_names.insert(role.role.as_str()) {
            return Err(format!("init: duplicate role '{}'", role.role));
        }
        let host = spec
            .hosts
            .iter()
            .find(|host| host.name == role.host)
            .ok_or_else(|| {
                format!(
                    "init: role '{}' references host '{}' which init was not given",
                    role.role, role.host
                )
            })?;
        CredentialProvider::parse_str(&role.credential_ref)
            .map_err(|rule| format!("init: roles.{}.credential_ref: {rule}", role.role))?;
        if let Some(observation) = spec
            .observations
            .iter()
            .find(|observation| observation.role == role.role)
        {
            if observation.login != role.actor {
                return Err(format!(
                    "init: observed identity '{}' for role '{}' does not match the bound actor '{}' — \
                     init never fabricates or guesses identity",
                    observation.login, role.role, role.actor
                ));
            }
            if observation.host_instance != host.endpoint {
                return Err(format!(
                    "init: observed host instance '{}' for role '{}' does not match its declared endpoint '{}'",
                    observation.host_instance, role.role, host.endpoint
                ));
            }
            if !host
                .repositories
                .iter()
                .any(|repository| repository.id == observation.repository)
            {
                return Err(format!(
                    "init: observed repository '{}' for role '{}' is not declared under host '{}'",
                    observation.repository, role.role, host.name
                ));
            }
            bound_roles.insert(role.role.as_str());
        }
    }
    for role in &spec.roles {
        if !bound_roles.contains(role.role.as_str()) {
            return Err(format!(
                "init: no observed identity for bound role '{}' — supply an observed login, host \
                 instance, and repository stable id; init never fabricates identity",
                role.role
            ));
        }
    }
    for observation in &spec.observations {
        if !role_names.contains(observation.role.as_str()) {
            return Err(format!(
                "init: observation for '{}' names a role that is not bound",
                observation.role
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for observation in &spec.observations {
        if !seen.insert(observation.role.as_str()) {
            return Err(format!(
                "init: duplicate observation for role '{}'",
                observation.role
            ));
        }
    }
    Ok(())
}

/// Render the spec as the `config.toml` document. Values only — the
/// same validator (`parse_profile`) accepts the result before anything
/// is published.
fn render(spec: &InitSpec) -> Result<String, String> {
    use toml::Value;

    let mut root = toml::map::Map::new();
    root.insert(
        "schema_version".into(),
        Value::Integer(i64::from(SCHEMA_VERSION)),
    );

    let mut installation = toml::map::Map::new();
    installation.insert("capacity_slots".into(), Value::Integer(1));
    root.insert("installation".into(), Value::Table(installation));

    let mut hosts = toml::map::Map::new();
    for host in &spec.hosts {
        let mut table = toml::map::Map::new();
        table.insert("adapter".into(), Value::String(host.adapter.clone()));
        table.insert("endpoint".into(), Value::String(host.endpoint.clone()));
        table.insert(
            "contract_version".into(),
            Value::Integer(i64::from(host.contract_version)),
        );
        let repositories: Vec<Value> = host
            .repositories
            .iter()
            .map(|repository| {
                let mut entry = toml::map::Map::new();
                entry.insert("id".into(), Value::String(repository.id.clone()));
                if let Some(display) = &repository.display {
                    entry.insert("display".into(), Value::String(display.clone()));
                }
                entry.insert("use".into(), Value::String(repository.use_.clone()));
                Value::Table(entry)
            })
            .collect();
        table.insert("repositories".into(), Value::Array(repositories));
        hosts.insert(host.name.clone(), Value::Table(table));
    }
    root.insert("hosts".into(), Value::Table(hosts));

    let mut roles = toml::map::Map::new();
    for role in &spec.roles {
        let mut table = toml::map::Map::new();
        table.insert("host".into(), Value::String(role.host.clone()));
        table.insert("actor".into(), Value::String(role.actor.clone()));
        table.insert(
            "credential_ref".into(),
            Value::String(role.credential_ref.clone()),
        );
        roles.insert(role.role.clone(), Value::Table(table));
    }
    root.insert("roles".into(), Value::Table(roles));

    let mut evidence = toml::map::Map::new();
    for observation in &spec.observations {
        let mut table = toml::map::Map::new();
        table.insert("login".into(), Value::String(observation.login.clone()));
        table.insert(
            "host_instance".into(),
            Value::String(observation.host_instance.clone()),
        );
        table.insert(
            "repository".into(),
            Value::String(observation.repository.clone()),
        );
        table.insert(
            "observed_unix_seconds".into(),
            Value::Integer(spec.observed_unix_seconds),
        );
        evidence.insert(observation.role.clone(), Value::Table(table));
    }
    root.insert("identity_evidence".into(), Value::Table(evidence));

    let mut verification = toml::map::Map::new();
    let mut egress = toml::map::Map::new();
    egress.insert(
        "allowlist".into(),
        Value::Array(
            spec.egress_allowlist
                .iter()
                .map(|entry| Value::String(entry.clone()))
                .collect(),
        ),
    );
    verification.insert("egress".into(), Value::Table(egress));
    root.insert("verification".into(), Value::Table(verification));

    let mut required = toml::map::Map::new();
    for (role, capabilities) in documented_required_capabilities() {
        required.insert(
            role.into(),
            Value::Array(
                capabilities
                    .into_iter()
                    .map(|name| Value::String(name.to_string()))
                    .collect(),
            ),
        );
    }
    let mut capabilities = toml::map::Map::new();
    capabilities.insert("required".into(), Value::Table(required));
    root.insert("capabilities".into(), Value::Table(capabilities));

    toml::to_string(&Value::Table(root)).map_err(|e| format!("init render: {e}"))
}

/// Scaffold `config.toml` from observed evidence.
///
/// Non-destructive: an existing config is never rewritten (`init`
/// refuses with `config.exists`, so operator-set fields can never be
/// silently re-enabled or changed). The document is validated by round
/// trip through the same loader before publication, and publication is
/// the shared atomic publish — a crash leaves either no config or a
/// complete one, never a partial. Only installation-owned files are
/// touched: the config and, if absent, its parent directory.
pub fn init(config_path: &Path, spec: &InitSpec) -> Result<InitOutcome, String> {
    if config_path.exists() {
        return Err(format!(
            "config.exists: {} already exists — init never rewrites an existing configuration",
            config_path.display()
        ));
    }
    validate_spec(spec)?;
    let text = render(spec)?;
    parse_profile(&text).map_err(|e| format!("init: {e}"))?;
    if let Some(parent) = config_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    let scratch = crate::scratch_path(config_path);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
            .map_err(|e| format!("config scratch: {e}"))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("config scratch write: {e}"))?;
    }
    crate::publish_with(config_path, &scratch, |from, to| {
        std::fs::hard_link(from, to)
    })?;
    // Report the file that actually landed (a concurrent first writer
    // may have won the publish race), never the staged request.
    let landed = std::fs::read_to_string(config_path).map_err(|e| format!("config read: {e}"))?;
    let profile = parse_profile(&landed).map_err(|e| format!("config: {e}"))?;
    Ok(InitOutcome {
        path: config_path.display().to_string(),
        roles: profile.roles.keys().cloned().collect(),
        egress_allowlist: profile.verification.egress.allowlist.clone(),
        identity_evidence: profile.identity_evidence.keys().cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version pins compare against the observed triple; unsupported
    /// forms are `Err`, never a silent pass or silent mismatch.
    #[test]
    fn runtime_version_pins_are_exact_and_bounded() {
        assert!(version_satisfies(">=2.0.0", (2, 0, 0)).unwrap());
        assert!(version_satisfies(">=2.0.0", (2, 1, 0)).unwrap());
        assert!(!version_satisfies(">=2.0.0", (1, 9, 9)).unwrap());
        assert!(version_satisfies("2.0.0", (2, 0, 0)).unwrap());
        assert!(!version_satisfies("2.0.0", (2, 0, 1)).unwrap());
        assert!(version_satisfies(">=2.0.0", (2, 0, 0)).is_ok());
        assert!(version_satisfies(">=2.0", (2, 0, 0)).is_err());
        assert!(version_satisfies("^2.0.0", (2, 0, 0)).is_err());
        assert!(version_satisfies("latest", (2, 0, 0)).is_err());
    }

    /// The first `X.Y.Z` triple in `--version` output is observed;
    /// output without one yields `None` (the stage reports
    /// `runtime.version_unknown`, never a guess).
    #[test]
    fn observed_versions_parse_the_first_triple() {
        assert_eq!(observed_version("tool 3.14.15 (build)"), Some((3, 14, 15)));
        assert_eq!(observed_version("3.14.15"), Some((3, 14, 15)));
        assert_eq!(observed_version("v10.2.3rc1"), Some((10, 2, 3)));
        assert_eq!(observed_version("no version here"), None);
        assert_eq!(observed_version("3.14"), None);
    }

    /// A child writing far beyond pipe capacity still exits inside the
    /// bound: stdout is drained while the process runs, never only
    /// after it exits.
    #[test]
    fn bounded_runs_drain_output_beyond_pipe_capacity() {
        let run = run_bounded(
            &[
                "sh".into(),
                "-c".into(),
                "head -c 262144 /dev/zero | tr '\\0' 'a'".into(),
            ],
            Duration::from_secs(10),
            ChildEnv::Inherited,
        );
        match run {
            BoundedRun::Exited { success, stdout } => {
                assert!(success);
                assert_eq!(stdout.len(), 262_144);
            }
            other => panic!("expected a clean exit, got {}", run_name(&other)),
        }
    }

    /// A leader that exits while a descendant keeps stdout open cannot
    /// stretch the run past the deadline — the whole process group is
    /// killed when the bound elapses.
    #[test]
    fn bounded_runs_reap_descendants_holding_stdout_at_the_deadline() {
        let start = Instant::now();
        let run = run_bounded(
            &["sh".into(), "-c".into(), "sleep 30 & exit 0".into()],
            Duration::from_millis(500),
            ChildEnv::Inherited,
        );
        assert!(matches!(run, BoundedRun::TimedOut));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the run ended at its bound, not the descendant's"
        );
    }

    /// The scrubbed environment carries only the passthrough names —
    /// `HOME` and anything credential-bearing stays out — while an
    /// inherited run sees the ambient environment, matching what
    /// dispatch gives a `command:` provider.
    #[test]
    fn bounded_runs_scrub_the_probe_environment() {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
        let Some(home) = home else {
            return; // no ambient HOME to distinguish the policies
        };
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "printf %s \"${HOME:-unset}\"".into(),
        ];
        match run_bounded(&argv, Duration::from_secs(5), ChildEnv::Scrubbed) {
            BoundedRun::Exited { stdout, .. } => assert_eq!(stdout, "unset"),
            other => panic!("expected a clean exit, got {}", run_name(&other)),
        }
        match run_bounded(&argv, Duration::from_secs(5), ChildEnv::Inherited) {
            BoundedRun::Exited { stdout, .. } => {
                assert_eq!(stdout, home.to_string_lossy())
            }
            other => panic!("expected a clean exit, got {}", run_name(&other)),
        }
    }

    fn run_name(run: &BoundedRun) -> &'static str {
        match run {
            BoundedRun::Exited { .. } => "exited",
            BoundedRun::TimedOut => "timed out",
            BoundedRun::SpawnFailed(_) => "spawn failed",
        }
    }

    /// Build a synthetic WAL: valid header salts and cumulative
    /// checksums over `frames` of `(pgno, n_truncate, page bytes)`.
    /// Page-1 images carry `user_version` at bytes 60-63 exactly where
    /// SQLite keeps it.
    fn synthetic_wal(page_size: u32, frames: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let mut wal = Vec::new();
        let mut header = vec![0u8; WAL_HEADER_LEN];
        header[0..4].copy_from_slice(&WAL_MAGIC_LITTLE_ENDIAN.to_be_bytes());
        header[4..8].copy_from_slice(&WAL_FORMAT_VERSION.to_be_bytes());
        header[8..12].copy_from_slice(&page_size.to_be_bytes());
        let (salt1, salt2) = (0xA5A5_A5A5u32, 0x5A5A_5A5Au32);
        header[16..20].copy_from_slice(&salt1.to_be_bytes());
        header[20..24].copy_from_slice(&salt2.to_be_bytes());
        let (mut s1, mut s2) = (0u32, 0u32);
        wal_checksum(&mut s1, &mut s2, &header[..24], false);
        header[24..28].copy_from_slice(&s1.to_be_bytes());
        header[28..32].copy_from_slice(&s2.to_be_bytes());
        wal.extend_from_slice(&header);
        for (pgno, n_truncate, page) in frames {
            let mut frame = vec![0u8; WAL_FRAME_HEADER_LEN];
            frame[0..4].copy_from_slice(&pgno.to_be_bytes());
            frame[4..8].copy_from_slice(&n_truncate.to_be_bytes());
            frame[8..12].copy_from_slice(&salt1.to_be_bytes());
            frame[12..16].copy_from_slice(&salt2.to_be_bytes());
            wal_checksum(&mut s1, &mut s2, &frame[..8], false);
            wal_checksum(&mut s1, &mut s2, page, false);
            frame[16..20].copy_from_slice(&s1.to_be_bytes());
            frame[20..24].copy_from_slice(&s2.to_be_bytes());
            wal.extend_from_slice(&frame);
            wal.extend_from_slice(page);
        }
        wal
    }

    /// A WAL-mode main header: format bytes 18/19 mark WAL, the rest is
    /// irrelevant to the scan.
    fn wal_mode_header() -> [u8; 100] {
        let mut header = [0u8; 100];
        header[..16].copy_from_slice(SQLITE_MAGIC);
        header[18] = 2;
        header[19] = 2;
        header
    }

    /// `wal_store_facts` promotes `user_version` only from committed
    /// page-1 frames — extracted inline from the checksum-validated
    /// buffer — and the newest commit wins.
    #[test]
    fn wal_facts_come_from_committed_checksum_validated_frames() {
        let dir = std::env::temp_dir().join(format!(
            "preflight-wal-unit-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("store.db");
        let mut page1 = vec![0u8; 4096];
        page1[..16].copy_from_slice(SQLITE_MAGIC);
        page1[60..64].copy_from_slice(&6u32.to_be_bytes());
        let mut page1_newer = vec![0u8; 4096];
        page1_newer[..16].copy_from_slice(SQLITE_MAGIC);
        page1_newer[60..64].copy_from_slice(&7u32.to_be_bytes());
        let data_page = vec![0u8; 4096];
        let wal = synthetic_wal(
            4096,
            &[
                (1, 0, page1.clone()),
                (2, 5, data_page.clone()), // commits txn 1: 5 pages, page1 v6
                (1, 0, page1_newer.clone()),
                (3, 6, data_page.clone()), // commits txn 2: page1 v7
            ],
        );
        std::fs::write(dir.join("store.db-wal"), &wal).unwrap();
        let _ = std::fs::write(&store, b"");

        match wal_store_facts(&store, &wal_mode_header()) {
            Ok(WalScan::Facts(facts)) => {
                assert_eq!(facts.committed_pages, Some(6));
                assert_eq!(facts.page1_user_version, Some(7));
            }
            other => panic!("expected committed facts, got {}", wal_scan_name(&other)),
        }

        // A frame whose bytes no longer match its checksum ends the
        // committed prefix exactly where a SQLite reader would stop.
        let mut tampered = wal.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        std::fs::write(dir.join("store.db-wal"), &tampered).unwrap();
        match wal_store_facts(&store, &wal_mode_header()) {
            Ok(WalScan::Facts(facts)) => {
                // Only the first transaction survives the bad tail.
                assert_eq!(facts.committed_pages, Some(5));
                assert_eq!(facts.page1_user_version, Some(6));
            }
            other => panic!("expected committed facts, got {}", wal_scan_name(&other)),
        }

        // A checksum-valid page-1 frame that does not open with the
        // database magic is not a version source: committing it makes
        // the inspection unavailable, never a pass.
        let mut bad_page1 = vec![0u8; 4096];
        bad_page1[60..64].copy_from_slice(&6u32.to_be_bytes());
        let bad_wal = synthetic_wal(4096, &[(1, 3, bad_page1)]);
        std::fs::write(dir.join("store.db-wal"), &bad_wal).unwrap();
        assert!(wal_store_facts(&store, &wal_mode_header()).is_err());
        // The same image uncommitted — no commit frame after it —
        // stays invisible, exactly as a SQLite reader treats a tail.
        let uncommitted_wal =
            synthetic_wal(4096, &[(1, 0, vec![0u8; 4096]), (2, 0, vec![0u8; 4096])]);
        std::fs::write(dir.join("store.db-wal"), &uncommitted_wal).unwrap();
        match wal_store_facts(&store, &wal_mode_header()) {
            Ok(WalScan::Facts(facts)) => {
                assert_eq!(facts.committed_pages, None);
                assert_eq!(facts.page1_user_version, None);
            }
            other => panic!("expected committed facts, got {}", wal_scan_name(&other)),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-WAL headers and absent WALs report `Quiet` — the main
    /// header stays authoritative.
    #[test]
    fn wal_facts_stay_quiet_without_a_wal_generation() {
        let dir = std::env::temp_dir().join(format!(
            "preflight-wal-quiet-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("store.db");
        let _ = std::fs::write(&store, b"");

        let mut journal_header = wal_mode_header();
        journal_header[18] = 1;
        journal_header[19] = 1;
        assert!(matches!(
            wal_store_facts(&store, &journal_header),
            Ok(WalScan::Quiet)
        ));
        assert!(matches!(
            wal_store_facts(&store, &wal_mode_header()),
            Ok(WalScan::Quiet)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wal_scan_name(scan: &Result<WalScan, String>) -> &'static str {
        match scan {
            Ok(WalScan::Quiet) => "quiet",
            Ok(WalScan::Facts(_)) => "facts",
            Ok(WalScan::Moved) => "moved",
            Err(_) => "error",
        }
    }
}
