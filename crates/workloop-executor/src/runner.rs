//! The executor-side Invocation lifecycle: stage inputs, launch the
//! configured Worker through the unchanged `describe` / `run`
//! contract under the endpoint's approved isolation profile, inspect
//! and stop by recorded identities, and collect bounded output.
//!
//! Process custody reuses the supervisor's components —
//! `ProcessControlPort` for spawn/inspect/signal/reap, the
//! `containment` gate and mount plan for the v0.3 profile, and
//! `isolation` for the scrubbed environment. The executor never picks
//! an adapter: `worker_argv` is endpoint configuration, and the
//! observed `describe` declaration must match the digest the
//! coordinator's launch configuration bound.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use workloop_core::domain::{ProcessIdentity, sha256_hex};
use workloop_core::executor::{CollectionEntry, RemoteHandoffV1, collection_digest};
use workloop_core::ports::{ProcessControlPort, ProcessEvidence, SpawnError};
use workloop_core::worker::IsolationProfile;
use workloop_exec::{adapter, bounded, containment, isolation};

use crate::state::{
    CollectionItemRecord, CollectionRecord, EndpointState, ProcessRecord, RequestRecord,
    StateError, StopProof, write_atomic,
};

/// Signals used by the bounded escalation (SIGTERM → grace → SIGKILL).
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// The remote out-directory whitelist (contract §6): everything else
/// in `out/` stays executor-local.
const OUT_WHITELIST: &[&str] = &[
    "report.json",
    "runtime.log",
    "checkpoint.json",
    "handoff.json",
];

/// The produced-object closure item: one git bundle over
/// `base..produced` for producing roles.
const OBJECTS_ITEM: &str = "objects.bundle";

/// The inline delivery ceiling for one collect result: keeps the
/// receipt under the 1 MiB class bound with framing headroom. A
/// collection whose bounded items exceed it is `incomplete` —
/// chunked resumable transfer is #376's, not this slice's.
const INLINE_COLLECT_BUDGET: u64 = 768 * 1024;

/// Poll cadence while a stop escalation waits out the grace period
/// and the post-KILL settle window.
const STOP_POLL: Duration = Duration::from_millis(50);
const KILL_SETTLE: Duration = Duration::from_secs(3);
/// Bounded git invocation timeouts for workspace construction and
/// object bundling.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// One staged input item from a `prepare` payload.
#[derive(Debug, Clone)]
pub struct StageItem {
    pub item_id: String,
    /// `request` | `input` | `workspace`.
    pub dest: String,
    /// Relative path within the destination (ignored for `request`,
    /// which always lands as `request.json`).
    pub path: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}

/// Why `prepare` refused — mapped to a receipt status + §9 code by
/// the service.
#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub detail: String,
}

impl Refusal {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl From<StateError> for Refusal {
    fn from(e: StateError) -> Self {
        Self::new("unavailable", e.to_string())
    }
}

/// A safe relative path: UTF-8 slash-separated, non-empty, no
/// absolute form, backslash, NUL, or `.`/`..` component — the same
/// grammar the handoff manifest pins.
fn path_is_safe(path: &str) -> bool {
    RemoteHandoffV1::path_is_valid(path)
}

/// sha256 over item bytes under the executor's item tag.
pub fn item_digest(bytes: &[u8]) -> String {
    sha256_hex("taisan.executor.item.v1", bytes)
}

pub enum LaunchOutcome {
    /// `started` — durable process identity exists.
    Started {
        evidence_ref: String,
        identity_digest: String,
    },
    /// `known_not_started` — durable state proves no process exists.
    KnownNotStarted { code: &'static str, detail: String },
    /// `unknown` — a process may exist; nothing was proven.
    Unknown { code: &'static str, detail: String },
}

pub enum ObserveOutcome {
    Running {
        evidence_ref: String,
        identity_digest: String,
    },
    Exited {
        evidence_ref: String,
        identity_digest: String,
    },
    Stopped {
        proof: StopProof,
    },
    Unknown {
        code: &'static str,
        detail: String,
    },
}

pub enum StopOutcome {
    /// Intent is durable and signals were issued; final proof comes
    /// through a later `observe`.
    StopRequested,
    Stopped {
        proof: StopProof,
    },
    Unknown {
        code: &'static str,
        detail: String,
    },
}

pub enum CollectOutcome {
    Collected {
        collection_id: String,
        collection_digest: String,
        /// Items with bodies attached for inline delivery.
        items: Vec<(CollectionItemRecord, Vec<u8>)>,
    },
    Incomplete {
        code: &'static str,
        detail: String,
    },
    Invalid {
        code: &'static str,
        detail: String,
    },
    Unknown {
        code: &'static str,
        detail: String,
    },
}

/// A `&dyn` handle wrapped so the generic `&impl ProcessControlPort`
/// probe helpers accept it — every call delegates verbatim.
struct PortRef<'a>(&'a (dyn ProcessControlPort + Send + Sync));

impl ProcessControlPort for PortRef<'_> {
    fn spawn(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &workloop_core::ports::WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError> {
        self.0.spawn(argv, cwd, env)
    }
    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {
        self.0.snapshot_descendants(leader_pid)
    }
    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String> {
        self.0.group_members(pgid)
    }
    fn matching_processes(
        &self,
        command: &str,
    ) -> Result<Vec<workloop_core::ports::ProcessMatch>, String> {
        self.0.matching_processes(command)
    }
    fn processes_in_directory(
        &self,
        dir: &Path,
    ) -> Result<Vec<workloop_core::ports::ProcessMatch>, String> {
        self.0.processes_in_directory(dir)
    }
    fn processes_in_namespace_dir(
        &self,
        dir: &Path,
    ) -> Result<Vec<workloop_core::ports::ProcessMatch>, String> {
        self.0.processes_in_namespace_dir(dir)
    }
    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        self.0.inspect(identity)
    }
    fn signal_verified(
        &self,
        identity: &ProcessIdentity,
        signal: i32,
    ) -> workloop_core::ports::SignalOutcome {
        self.0.signal_verified(identity, signal)
    }
    fn signal_group(&self, pgid: i32, signal: i32) -> workloop_core::ports::SignalOutcome {
        self.0.signal_group(pgid, signal)
    }
    fn reap(&self, pid: i32) -> Option<i32> {
        self.0.reap(pid)
    }
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &workloop_core::ports::WorkerEnv,
        timeout: Duration,
    ) -> Result<workloop_core::ports::ProbeOutcome, SpawnError> {
        self.0.run_probe(argv, cwd, env, timeout)
    }
}

/// The executor-side runner. Holds no mutable in-memory Invocation
/// state — everything consults the durable records so a transport
/// session's lifetime is irrelevant.
pub struct Runner {
    port: Arc<dyn ProcessControlPort + Send + Sync>,
    state: EndpointState,
    profile: IsolationProfile,
    worker_argv: Vec<String>,
    max_state_bytes: u64,
    max_one_way_delay_ms: u64,
    clock_offset_ms: std::sync::atomic::AtomicI64,
    clock_uncertainty_ms: u64,
}

impl Runner {
    pub fn new(
        port: Arc<dyn ProcessControlPort + Send + Sync>,
        state: EndpointState,
        profile: IsolationProfile,
        worker_argv: Vec<String>,
        max_state_bytes: u64,
        max_one_way_delay_ms: u64,
        clock_uncertainty_ms: u64,
    ) -> Self {
        Self {
            port,
            state,
            profile,
            worker_argv,
            max_state_bytes,
            max_one_way_delay_ms,
            clock_offset_ms: std::sync::atomic::AtomicI64::new(0),
            clock_uncertainty_ms,
        }
    }

    /// Record the measured coordinator↔endpoint offset (coordinator
    /// minus endpoint wall clock) from the authenticated session's
    /// challenge timestamp. Applied at the conservative edge when a
    /// deadline is converted to local time (contract §8).
    pub fn record_clock_offset(&self, coordinator_time_unix_ms: i64, received_at_unix_ms: i64) {
        self.clock_offset_ms.store(
            coordinator_time_unix_ms.saturating_sub(received_at_unix_ms),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn measured_clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn now_seconds() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn request_dir(&self, record: &RequestRecord) -> PathBuf {
        self.state.request_path(&record.request_id)
    }

    // -----------------------------------------------------------------
    // prepare: validated, digest-checked staging
    // -----------------------------------------------------------------

    /// Stage the declared items: every body is digest-verified before
    /// it lands, staging is atomic (a `staging/` scratch renamed into
    /// `prepared/` only after every check), and no Worker process can
    /// exist yet. Returns the input-manifest digest recorded in the
    /// request record.
    pub fn stage(&self, record: &RequestRecord, items: &[StageItem]) -> Result<String, Refusal> {
        let dir = self.request_dir(record);
        let staging = dir.join("staging");
        let prepared = dir.join("prepared");
        if prepared.is_dir() {
            return Err(Refusal::new(
                "request_identity_mismatch",
                "request already has a prepared input set",
            ));
        }
        if items.len() as u64 > record.limits.max_item_count {
            return Err(Refusal::new(
                "limit_exceeded",
                format!("{} items exceed max_item_count", items.len()),
            ));
        }
        let total: u64 = items.iter().map(|i| i.bytes.len() as u64).sum();
        if total > record.limits.max_input_bytes {
            return Err(Refusal::new(
                "limit_exceeded",
                format!("input {total} bytes exceed max_input_bytes"),
            ));
        }
        if total > self.max_state_bytes {
            return Err(Refusal::new(
                "limit_exceeded",
                format!("input {total} bytes exceed the endpoint storage bound"),
            ));
        }
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .map_err(|e| Refusal::new("unavailable", e.to_string()))?;

        let mut request_seen = false;
        let mut manifest_entries: Vec<serde_json::Value> = Vec::new();
        for item in items {
            if !matches!(item.dest.as_str(), "request" | "input" | "workspace") {
                return Err(Refusal::new(
                    "input_invalid",
                    format!(
                        "item {:?} names unknown destination {:?}",
                        item.item_id, item.dest
                    ),
                ));
            }
            if item.bytes.len() as u64 > record.limits.max_input_bytes {
                return Err(Refusal::new(
                    "limit_exceeded",
                    "one item exceeds max_input_bytes",
                ));
            }
            if item_digest(&item.bytes) != item.digest {
                return Err(Refusal::new(
                    "input_invalid",
                    format!(
                        "item {:?} digest does not match its declared value",
                        item.item_id
                    ),
                ));
            }
            let target = match item.dest.as_str() {
                "request" => {
                    if request_seen {
                        return Err(Refusal::new("input_invalid", "duplicate request body"));
                    }
                    // The request body must be well-formed JSON — the
                    // Worker parses it verbatim, so an unparseable
                    // body is refused here, before any process exists.
                    if serde_json::from_slice::<serde_json::Value>(&item.bytes).is_err() {
                        return Err(Refusal::new(
                            "input_invalid",
                            "request body is not valid JSON",
                        ));
                    }
                    request_seen = true;
                    staging.join("request.json")
                }
                "input" => {
                    if !path_is_safe(&item.path) {
                        return Err(Refusal::new(
                            "input_invalid",
                            format!("item {:?} path {:?} is unsafe", item.item_id, item.path),
                        ));
                    }
                    staging.join("input").join(&item.path)
                }
                _ => {
                    if !path_is_safe(&item.path) {
                        return Err(Refusal::new(
                            "input_invalid",
                            format!("item {:?} path {:?} is unsafe", item.item_id, item.path),
                        ));
                    }
                    staging.join("workspace-items").join(&item.path)
                }
            };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Refusal::new("unavailable", e.to_string()))?;
            }
            write_atomic(&target, &item.bytes)?;
            manifest_entries.push(serde_json::json!({
                "item_id": item.item_id,
                "dest": item.dest,
                "path": if item.dest == "request" { "request.json" } else { item.path.as_str() },
                "bytes": item.bytes.len(),
                "digest": item.digest,
            }));
        }
        if !request_seen {
            return Err(Refusal::new("input_invalid", "no request body item staged"));
        }
        manifest_entries.sort_by(|a, b| {
            a["item_id"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["item_id"].as_str().unwrap_or_default())
        });
        let manifest_digest = sha256_hex(
            "taisan.executor.input-manifest.v1",
            &workloop_core::domain::canonical_json(&serde_json::Value::Array(manifest_entries)),
        );
        std::fs::rename(&staging, &prepared)
            .map_err(|e| Refusal::new("unavailable", e.to_string()))?;
        Ok(manifest_digest)
    }

    /// The canonical request body bytes staged at prepare — needed to
    /// serve them to the Worker at `/taisan/request.json`.
    fn prepared_request(&self, record: &RequestRecord) -> PathBuf {
        self.request_dir(record).join("prepared/request.json")
    }

    // -----------------------------------------------------------------
    // runtime credential staging (contract §7)
    // -----------------------------------------------------------------

    /// Stage the one-Invocation runtime credential into a mode-0600
    /// file bound to the launch configuration digest. It never
    /// appears in argv, env, receipts, or logs.
    pub fn stage_credential(
        &self,
        record: &RequestRecord,
        launch_config_digest: &str,
        secret: &[u8],
    ) -> Result<(), &'static str> {
        if launch_config_digest != record.launch_config_digest {
            return Err("request_unknown");
        }
        if record.state != "prepared" {
            return Err("not_prepared");
        }
        if secret.is_empty() || secret.len() as u64 > 4096 {
            return Err(if secret.is_empty() {
                "secret_invalid"
            } else {
                "secret_oversize"
            });
        }
        let path = self.request_dir(record).join("runtime-auth");
        write_atomic(&path, secret).map_err(|_| "secret_invalid")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Whether a staged runtime credential exists for this request.
    pub fn has_credential(&self, record: &RequestRecord) -> bool {
        self.request_dir(record).join("runtime-auth").is_file()
    }

    /// Remove the staged credential — after stop proof the file's
    /// retention ends (contract §7).
    fn release_credential(&self, record: &RequestRecord) {
        let _ = std::fs::remove_file(self.request_dir(record).join("runtime-auth"));
    }

    // -----------------------------------------------------------------
    // start
    // -----------------------------------------------------------------

    /// The conservative local cutoff for the bound deadline: the
    /// coordinator's absolute deadline converted to endpoint time at
    /// the pessimistic edge — measured offset plus uncertainty plus
    /// the configured one-way delay — so skew can only shorten the
    /// authorized interval (contract §8). `None` means no positive
    /// interval remains.
    fn local_deadline_ms(&self, record: &RequestRecord) -> Option<i64> {
        let offset = self
            .clock_offset_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let coordinator_now_ms = Self::now_ms().saturating_add(offset);
        let remaining = record
            .deadline_unix_seconds
            .saturating_mul(1000)
            .saturating_sub(coordinator_now_ms);
        let conservative = remaining
            .saturating_sub(self.clock_uncertainty_ms as i64)
            .saturating_sub(self.max_one_way_delay_ms as i64);
        if conservative <= 0 {
            return None;
        }
        Some(Self::now_ms().saturating_add(conservative))
    }

    /// Construct the managed workspace and launch the Worker. The
    /// caller holds the durable record; this runs the whole gate →
    /// declaration → spawn sequence and returns the outcome the
    /// start receipt carries.
    pub fn launch(&self, record: &RequestRecord) -> LaunchOutcome {
        let dir = self.request_dir(record);
        let prepared = dir.join("prepared");
        let workspace = dir.join("workspace");
        let out_dir = dir.join("out");
        let scratch = dir.join("scratch");

        // Deadline first: no positive remaining interval refuses the
        // start — skew and delay may consume budget, never extend it.
        let Some(local_deadline_ms) = self.local_deadline_ms(record) else {
            return LaunchOutcome::KnownNotStarted {
                code: "limit_exceeded",
                detail: "no positive runtime interval remains under the bound deadline".into(),
            };
        };

        // The declared runtime credential must be staged before a
        // launch that mounts it.
        if record.runtime_auth_mount && !self.has_credential(record) {
            return LaunchOutcome::KnownNotStarted {
                code: "input_invalid",
                detail: "runtime_auth_mount declared but no credential is staged".into(),
            };
        }

        // --- construct the managed workspace -------------------------
        let git_wanted = record
            .launch_config_body
            .get("workspace_git")
            .and_then(|v| v.as_bool())
            .unwrap_or(record.workspace_grant == "writable");
        if let Err(detail) = self.build_workspace(record, &prepared, &workspace, git_wanted) {
            return LaunchOutcome::KnownNotStarted {
                code: "input_invalid",
                detail,
            };
        }
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return LaunchOutcome::KnownNotStarted {
                code: "unavailable",
                detail: format!("out dir: {e}"),
            };
        }

        // --- scrubbed environment ------------------------------------
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            return LaunchOutcome::KnownNotStarted {
                code: "unavailable",
                detail: format!("scratch dir: {e}"),
            };
        }
        let env = match isolation::prepare(&self.profile, &scratch) {
            Ok(env) => env,
            Err(why) => {
                return LaunchOutcome::KnownNotStarted {
                    code: "input_invalid",
                    detail: format!("isolation.environment_preparation_failed: {why}"),
                };
            }
        };

        // --- containment gate: the negative preflight -----------------
        let now = Self::now_seconds();
        let port: &dyn ProcessControlPort = self.port.as_ref();
        let gate = match containment::gate(
            port,
            self.state.root(),
            &self.profile,
            &self.worker_argv,
            None,
            now,
        ) {
            Ok(gate) => gate,
            Err(containment::GateError::Refused(reason)) => {
                return LaunchOutcome::KnownNotStarted {
                    code: "capability_missing",
                    detail: format!("isolation.containment_refused: {reason}"),
                };
            }
            Err(containment::GateError::Fault(detail)) => {
                return LaunchOutcome::Unknown {
                    code: "unavailable",
                    detail: format!("isolation.containment_fault: {detail}"),
                };
            }
        };
        // A declared read-only workspace grant is only meaningful when
        // the gate can enforce it.
        if record.workspace_grant == "read_only" {
            let enforceable = matches!(gate, containment::Gate::Ready { .. });
            if !enforceable {
                return LaunchOutcome::KnownNotStarted {
                    code: "capability_missing",
                    detail: "workspace_grant read_only requires enforceable containment".into(),
                };
            }
        }

        // --- Worker declaration: unchanged `describe` contract --------
        let declaration = match &gate {
            containment::Gate::Uncontained => {
                match adapter::observe_worker_declaration_v2(
                    &PortRef(self.port.as_ref()),
                    &self.worker_argv,
                    &workspace,
                    &env,
                ) {
                    Ok((_, surface)) => surface,
                    Err(why) => {
                        return LaunchOutcome::KnownNotStarted {
                            code: "input_invalid",
                            detail: format!("worker declaration unavailable: {why}"),
                        };
                    }
                }
            }
            containment::Gate::Ready { primitive, worker } => {
                let gitconfig = scratch.join("gitconfig");
                let describe_argv = adapter::describe_argv(&self.worker_argv);
                let plan = containment::MountPlan {
                    workspace: Some((workspace.clone(), true)),
                    request: None,
                    input: None,
                    out: None,
                    gitconfig: Some(gitconfig),
                    runtime_auth: None,
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    ca_bundles: containment::ca_trust_binds(&self.profile),
                    extra_binds: containment::extra_binds(&describe_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: vec![self.state.root().to_path_buf()],
                    masked_files: vec![],
                };
                let outer = containment::bwrap_args(primitive, &plan, &describe_argv, "/workspace");
                let contained = containment::contained_env(&env, None);
                match adapter::observe_worker_declaration_v2_with_argv(
                    &PortRef(self.port.as_ref()),
                    &outer,
                    &scratch,
                    &contained,
                ) {
                    Ok((_, surface)) => surface,
                    Err(why) => {
                        return LaunchOutcome::KnownNotStarted {
                            code: "input_invalid",
                            detail: format!("worker declaration unavailable: {why}"),
                        };
                    }
                }
            }
        };

        // The observed declaration must match the binding the launch
        // configuration pinned — a drifted adapter is a configuration
        // refusal, never a quiet substitution.
        if let Err(detail) = self.check_worker_binding(record, &declaration) {
            return LaunchOutcome::KnownNotStarted {
                code: "configuration_mismatch",
                detail,
            };
        }

        // --- spawn ----------------------------------------------------
        let mut inner = self.worker_argv.clone();
        let (argv, launch_env, cwd): (Vec<String>, _, PathBuf) = match &gate {
            containment::Gate::Uncontained => {
                inner.extend([
                    "run".into(),
                    "--request".into(),
                    self.prepared_request(record).to_string_lossy().into_owned(),
                    "--out".into(),
                    out_dir.to_string_lossy().into_owned(),
                ]);
                (inner, env, workspace.clone())
            }
            containment::Gate::Ready { primitive, worker } => {
                inner.extend([
                    "run".into(),
                    "--request".into(),
                    containment::SANDBOX_REQUEST.into(),
                    "--out".into(),
                    containment::SANDBOX_OUT.into(),
                ]);
                let input_dir = dir.join("prepared/input");
                let plan = containment::MountPlan {
                    workspace: Some((workspace.clone(), record.workspace_grant == "read_only")),
                    request: Some(self.prepared_request(record)),
                    input: input_dir.is_dir().then_some(input_dir),
                    out: Some(out_dir.clone()),
                    gitconfig: Some(scratch.join("gitconfig")),
                    runtime_auth: if record.runtime_auth_mount {
                        Some(dir.join("runtime-auth"))
                    } else {
                        None
                    },
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    ca_bundles: containment::ca_trust_binds(&self.profile),
                    extra_binds: containment::extra_binds(&inner, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: vec![self.state.root().to_path_buf()],
                    masked_files: vec![],
                };
                if let Some(reason) = containment::plan_denial(&plan) {
                    return LaunchOutcome::KnownNotStarted {
                        code: "capability_missing",
                        detail: format!("mount plan denied: {reason}"),
                    };
                }
                (
                    containment::bwrap_args(primitive, &plan, &inner, "/workspace"),
                    containment::contained_env(&env, None),
                    workspace.clone(),
                )
            }
        };

        match self.port.spawn(&argv, &cwd, &launch_env) {
            Ok(identity) => {
                // Descendant coverage starts at launch — a writer that
                // escapes before the first poll is still recorded.
                let mut descendants = self
                    .port
                    .snapshot_descendants(identity.pid)
                    .unwrap_or_default();
                descendants.retain(|d| d.pid != identity.pid);
                let evidence_ref = format!(
                    "proc-{}-{}",
                    sanitize_branch_component(&record.request_id),
                    sanitize_branch_component(&record.invocation_id)
                );
                let snapshot = std::iter::once(identity.clone())
                    .chain(descendants.iter().cloned())
                    .collect::<Vec<_>>();
                let identity_digest = Self::process_identity_digest(&snapshot);
                let snapshot_digest = Self::snapshot_digest(&snapshot);
                let process = ProcessRecord {
                    evidence_ref: evidence_ref.clone(),
                    identity_digest: identity_digest.clone(),
                    leader: identity,
                    descendants,
                    snapshot_digest,
                    leader_exit_code: None,
                    local_deadline_unix_ms: local_deadline_ms,
                };
                if let Err(e) = self.state.put_process(&record.request_id, &process) {
                    return LaunchOutcome::Unknown {
                        code: "process_identity_unavailable",
                        detail: format!("process identity could not be recorded: {e}"),
                    };
                }
                LaunchOutcome::Started {
                    evidence_ref,
                    identity_digest,
                }
            }
            Err(SpawnError::Refused { reason }) => LaunchOutcome::KnownNotStarted {
                code: "known_not_started",
                detail: format!("spawn refused: {reason}"),
            },
            Err(SpawnError::Unknown { reason, .. }) => LaunchOutcome::Unknown {
                code: "process_identity_unavailable",
                detail: format!("spawn outcome unknown: {reason}"),
            },
        }
    }

    /// Verify the observed `describe` surface against the launch
    /// configuration's `worker_binding` block.
    fn check_worker_binding(
        &self,
        record: &RequestRecord,
        observed: &adapter::WorkerDeclaration,
    ) -> Result<(), String> {
        let binding = record
            .launch_config_body
            .get("worker_binding")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected = |key: &str| binding.get(key).and_then(|v| v.as_str());
        let mut mismatches = Vec::new();
        if let Some(adapter_id) = expected("adapter")
            && adapter_id != observed.adapter.as_str()
        {
            mismatches.push(format!(
                "adapter identity {adapter_id:?} bound, {:?} declared",
                observed.adapter.as_str()
            ));
        }
        if let Some(version) = expected("adapter_version")
            && version != observed.adapter_version
        {
            mismatches.push(format!(
                "adapter version {version:?} bound, {:?} declared",
                observed.adapter_version
            ));
        }
        if let Some(version) = expected("runtime_version")
            && version != observed.runtime_version
        {
            mismatches.push(format!(
                "runtime version {version:?} bound, {:?} declared",
                observed.runtime_version
            ));
        }
        if let Some(digest) = expected("capability_digest") {
            let observed_digest = observed.capability_digest();
            if digest != observed_digest {
                mismatches.push(format!(
                    "capability digest {digest} bound, {observed_digest} declared"
                ));
            }
        }
        if mismatches.is_empty() {
            Ok(())
        } else {
            Err(mismatches.join("; "))
        }
    }

    /// Build the managed workspace: materialized `workspace/` items
    /// plus `git init` on `work/<work_id>` when the launch declares a
    /// git workspace. Rebuilt workspaces are refused — the workspace
    /// is retained state, never silently reset under a bound request.
    fn build_workspace(
        &self,
        record: &RequestRecord,
        prepared: &Path,
        workspace: &Path,
        git_wanted: bool,
    ) -> Result<(), String> {
        if workspace.exists() {
            return Err("workspace already exists".into());
        }
        std::fs::create_dir_all(workspace).map_err(|e| format!("workspace: {e}"))?;
        let items = prepared.join("workspace-items");
        if items.is_dir() {
            copy_tree(&items, workspace).map_err(|e| format!("workspace items: {e}"))?;
        }
        if git_wanted {
            let branch = format!("work/{}", sanitize_branch_component(&record.work_id));
            run_git(&["init", "--initial-branch", &branch], workspace)
                .map_err(|e| format!("git init: {e}"))?;
        }
        Ok(())
    }

    /// Tagged digest over the recorded process identity set — the
    /// `identity_digest` member of `process` evidence in receipts.
    fn process_identity_digest(set: &[ProcessIdentity]) -> String {
        let entries: Vec<serde_json::Value> = set
            .iter()
            .map(|p| {
                serde_json::json!({
                    "pid": p.pid,
                    "started_at": p.started_at,
                    "pgid": p.pgid,
                })
            })
            .collect();
        sha256_hex(
            "taisan.executor.process-identity.v1",
            &workloop_core::domain::canonical_json(&serde_json::Value::Array(entries)),
        )
    }

    fn snapshot_digest(set: &[ProcessIdentity]) -> String {
        let mut sorted = set.to_vec();
        sorted.sort_by_key(|p| p.pid);
        let entries: Vec<serde_json::Value> = sorted
            .iter()
            .map(|p| {
                serde_json::json!({
                    "pid": p.pid,
                    "started_at": p.started_at,
                    "pgid": p.pgid,
                })
            })
            .collect();
        sha256_hex(
            "taisan.executor.descendant-snapshot.v1",
            &workloop_core::domain::canonical_json(&serde_json::Value::Array(entries)),
        )
    }

    // -----------------------------------------------------------------
    // observe
    // -----------------------------------------------------------------

    /// Inspect the recorded identities and return the observation the
    /// receipt carries. A durable stop proof or a stale incarnation
    /// dominates anything the process table could say.
    pub fn observe(&self, record: &RequestRecord, current_incarnation: &str) -> ObserveOutcome {
        // Durable terminal evidence replays across incarnations.
        if let Ok(Some(proof)) = self.state.stop_proof(&record.request_id) {
            return ObserveOutcome::Stopped { proof };
        }
        if record.bound_incarnation_id != current_incarnation {
            return ObserveOutcome::Unknown {
                code: "incarnation_mismatch",
                detail: format!(
                    "request bound to incarnation {} cannot be inspected by {current_incarnation}",
                    record.bound_incarnation_id
                ),
            };
        }
        let process = match self.state.process(&record.request_id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return ObserveOutcome::Unknown {
                    code: "unknown",
                    detail: "no process record exists".into(),
                };
            }
            Err(e) => {
                return ObserveOutcome::Unknown {
                    code: "unknown",
                    detail: format!("process record unreadable: {e}"),
                };
            }
        };
        let leader_state = self.port.inspect(&process.leader);
        match leader_state {
            ProcessEvidence::Alive { .. } => {
                // The deadline is an authorized-runtime bound, not a
                // liveness hint — enforce it as a stop, not a report.
                if let Some(outcome) = self.enforce_deadline(record, &process) {
                    return outcome;
                }
                ObserveOutcome::Running {
                    evidence_ref: process.evidence_ref,
                    identity_digest: process.identity_digest,
                }
            }
            ProcessEvidence::Stopped { exit_code, .. } => {
                // Record the proven exit once; the descendant set is
                // inspected before `stopped` may be claimed.
                let mut updated = process.clone();
                if updated.leader_exit_code.is_none() {
                    updated.leader_exit_code = Some(exit_code);
                    let _ = self.state.put_process(&record.request_id, &updated);
                }
                match self.remaining_identities(&updated) {
                    Ok(alive) if alive.is_empty() => {
                        let proof = self.prove_stopped(record, &updated, "exit", "");
                        ObserveOutcome::Stopped { proof }
                    }
                    Ok(_) => ObserveOutcome::Exited {
                        evidence_ref: updated.evidence_ref,
                        identity_digest: updated.identity_digest,
                    },
                    Err(detail) => ObserveOutcome::Unknown {
                        code: "unknown",
                        detail,
                    },
                }
            }
            ProcessEvidence::Unknown { reason } => ObserveOutcome::Unknown {
                code: "unknown",
                detail: format!("leader inspection inconclusive: {reason}"),
            },
        }
    }

    /// Deadline enforcement: if the conservative local cutoff passed
    /// while the leader is alive, the executor itself escalates a
    /// stop — an authorized-runtime bound enforced without waiting on
    /// the coordinator. Returns the resulting outcome when it fired.
    pub fn enforce_deadline(
        &self,
        record: &RequestRecord,
        process: &ProcessRecord,
    ) -> Option<ObserveOutcome> {
        let deadline = process.local_deadline_unix_ms;
        if deadline <= 0 || Self::now_ms() <= deadline {
            return None;
        }
        match self.stop_escalation(record, process) {
            StopOutcome::Stopped { proof } => Some(ObserveOutcome::Stopped { proof }),
            StopOutcome::StopRequested => Some(ObserveOutcome::Running {
                evidence_ref: process.evidence_ref.clone(),
                identity_digest: process.identity_digest.clone(),
            }),
            StopOutcome::Unknown { code, detail } => Some(ObserveOutcome::Unknown { code, detail }),
        }
    }

    /// Sweep every live request for an expired deadline — called on
    /// each incoming operation and by the serve loop's ticker so
    /// enforcement does not depend on coordinator liveness.
    pub fn enforce_deadlines(&self, current_incarnation: &str) {
        let Ok(requests) = self.state.requests() else {
            return;
        };
        for record in requests {
            if !record.is_live() || record.bound_incarnation_id != current_incarnation {
                continue;
            }
            let Ok(Some(process)) = self.state.process(&record.request_id) else {
                continue;
            };
            if let ProcessEvidence::Alive { .. } = self.port.inspect(&process.leader) {
                let _ = self.enforce_deadline(&record, &process);
            }
        }
    }

    /// Every identity the stop path must account for: the recorded
    /// descendant set plus every live member of the leader's group —
    /// orphans that kept the group stay covered. A failed group read
    /// is an error, never an empty proof set.
    fn remaining_identities(
        &self,
        process: &ProcessRecord,
    ) -> Result<Vec<ProcessIdentity>, String> {
        let mut alive: Vec<ProcessIdentity> = Vec::new();
        for identity in &process.descendants {
            if let ProcessEvidence::Alive { identity } = self.port.inspect(identity) {
                alive.push(identity);
            }
        }
        if let Some(pgid) = process.leader.pgid {
            match self.port.group_members(pgid) {
                Ok(members) => {
                    for member in members {
                        if member.pid == process.leader.pid {
                            continue;
                        }
                        if let ProcessEvidence::Alive { identity } = self.port.inspect(&member)
                            && !alive.iter().any(|a| a.pid == identity.pid)
                        {
                            alive.push(identity);
                        }
                    }
                }
                Err(detail) => {
                    return Err(format!("process group enumeration failed: {detail}"));
                }
            }
        }
        Ok(alive)
    }

    // -----------------------------------------------------------------
    // request_stop
    // -----------------------------------------------------------------

    /// Persist stop intent, then run the bounded escalation. The
    /// caller writes the durable intent first (identity before
    /// effect); a replayed intent returns its durable state.
    pub fn stop(&self, record: &RequestRecord, current_incarnation: &str) -> StopOutcome {
        if let Ok(Some(proof)) = self.state.stop_proof(&record.request_id) {
            return StopOutcome::Stopped { proof };
        }
        if record.bound_incarnation_id != current_incarnation {
            return StopOutcome::Unknown {
                code: "incarnation_mismatch",
                detail: format!(
                    "request bound to incarnation {} cannot be inspected by {current_incarnation}",
                    record.bound_incarnation_id
                ),
            };
        }
        let process = match self.state.process(&record.request_id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return StopOutcome::Unknown {
                    code: "unknown",
                    detail: "no process record to stop".into(),
                };
            }
            Err(e) => {
                return StopOutcome::Unknown {
                    code: "unknown",
                    detail: format!("process record unreadable: {e}"),
                };
            }
        };
        // Refresh the snapshot while the leader may still exist —
        // stop intent is the last point full descendant coverage can
        // be captured.
        let mut process = process;
        if let Ok(snapshot) = self.port.snapshot_descendants(process.leader.pid) {
            let mut merged = process.descendants.clone();
            for d in snapshot {
                if d.pid != process.leader.pid
                    && !merged
                        .iter()
                        .any(|m| m.pid == d.pid && m.started_at == d.started_at)
                {
                    merged.push(d);
                }
            }
            if merged != process.descendants {
                process.snapshot_digest = Self::snapshot_digest(
                    &std::iter::once(process.leader.clone())
                        .chain(merged.iter().cloned())
                        .collect::<Vec<_>>(),
                );
                process.descendants = merged;
                let _ = self.state.put_process(&record.request_id, &process);
            }
        }
        self.stop_escalation(record, &process)
    }

    /// The intent→snapshot→signal→inspect order: TERM the group and
    /// every re-verified identity, wait out the bound grace, KILL
    /// what remains, then inspect. Only an all-gone inspection earns
    /// `stopped`.
    fn stop_escalation(&self, record: &RequestRecord, process: &ProcessRecord) -> StopOutcome {
        // TERM: group first so members that kept the pgid die
        // together, then each recorded identity that re-verifies —
        // escaped descendants (setsid) are covered individually.
        if let Some(pgid) = process.leader.pgid {
            let _ = self.port.signal_group(pgid, SIGTERM);
        }
        for identity in &process.descendants {
            let _ = self.port.signal_verified(identity, SIGTERM);
        }
        let _ = self.port.signal_verified(&process.leader, SIGTERM);

        let grace = Duration::from_millis(record.stop_grace_ms.min(60_000));
        let deadline_wait = std::time::Instant::now() + grace;
        loop {
            match self.all_stopped(process) {
                Ok(true) => {
                    let proof = self.prove_stopped(record, process, "signaled", "");
                    return StopOutcome::Stopped { proof };
                }
                Ok(false) => {
                    if std::time::Instant::now() >= deadline_wait {
                        break;
                    }
                    std::thread::sleep(STOP_POLL);
                }
                Err(detail) => {
                    return StopOutcome::Unknown {
                        code: "unknown",
                        detail,
                    };
                }
            }
        }
        // Escalate.
        if let Some(pgid) = process.leader.pgid {
            let _ = self.port.signal_group(pgid, SIGKILL);
        }
        for identity in &process.descendants {
            let _ = self.port.signal_verified(identity, SIGKILL);
        }
        let _ = self.port.signal_verified(&process.leader, SIGKILL);

        let kill_deadline = std::time::Instant::now() + KILL_SETTLE;
        loop {
            match self.all_stopped(process) {
                Ok(true) => {
                    let proof = self.prove_stopped(record, process, "signaled", "");
                    return StopOutcome::Stopped { proof };
                }
                Ok(false) => {
                    if std::time::Instant::now() >= kill_deadline {
                        return StopOutcome::StopRequested;
                    }
                    std::thread::sleep(STOP_POLL);
                }
                Err(detail) => {
                    return StopOutcome::Unknown {
                        code: "unknown",
                        detail,
                    };
                }
            }
        }
    }

    /// `true` only when every recorded identity — leader, snapshot,
    /// and live group members — is inspected gone.
    fn all_stopped(&self, process: &ProcessRecord) -> Result<bool, String> {
        let leader_gone = match self.port.inspect(&process.leader) {
            ProcessEvidence::Alive { .. } => false,
            ProcessEvidence::Stopped { .. } => true,
            ProcessEvidence::Unknown { reason } => {
                return Err(format!("leader inspection inconclusive: {reason}"));
            }
        };
        if !leader_gone {
            return Ok(false);
        }
        match self.remaining_identities(process) {
            Ok(alive) => Ok(alive.is_empty()),
            Err(detail) => Err(detail),
        }
    }

    /// Reap the leader if it is ours, then build and persist the stop
    /// proof. The credential file's retention ends with proof.
    fn prove_stopped(
        &self,
        record: &RequestRecord,
        process: &ProcessRecord,
        manner: &str,
        operation_id: &str,
    ) -> StopProof {
        let _ = self.port.reap(process.leader.pid);
        let mut proof = StopProof {
            manner: manner.into(),
            leader_identity_digest: process.identity_digest.clone(),
            snapshot_digest: process.snapshot_digest.clone(),
            incarnation_id: record.bound_incarnation_id.clone(),
            proven_at_unix_seconds: Self::now_seconds(),
            proving_operation_id: operation_id.into(),
            proof_digest: String::new(),
        };
        proof.proof_digest = sha256_hex(
            "taisan.executor.stop-proof.v1",
            &workloop_core::domain::canonical_json(&serde_json::json!({
                "manner": proof.manner,
                "leader_identity_digest": proof.leader_identity_digest,
                "snapshot_digest": proof.snapshot_digest,
                "incarnation_id": proof.incarnation_id,
            })),
        );
        let _ = self.state.put_stop_proof(&record.request_id, &proof);
        self.release_credential(record);
        proof
    }

    /// Re-point an already-computed proof at the operation the
    /// receipt belongs to.
    pub fn finalize_proof(
        &self,
        record: &RequestRecord,
        mut proof: StopProof,
        operation_id: &str,
    ) -> StopProof {
        if proof.proving_operation_id.is_empty() {
            proof.proving_operation_id = operation_id.into();
            let _ = self.state.put_stop_proof(&record.request_id, &proof);
        }
        proof
    }

    /// Remove a torn prepare: the atomic staging commit survived but
    /// the binding record did not. The staged set is this request's
    /// own in-flight input — never a foreign one — so re-staging is
    /// the repair, not a reset.
    pub fn clear_torn_prepare(&self, request_id: &str) {
        let prepared = self.state.request_path(request_id).join("prepared");
        if prepared.is_dir() {
            let _ = std::fs::remove_dir_all(&prepared);
        }
    }

    // -----------------------------------------------------------------
    // collect
    // -----------------------------------------------------------------

    /// Gather the bounded output set: the whitelisted out-dir members,
    /// a validated `handoff.json` for producing roles, and the
    /// `base..produced` object bundle it names. Everything is read,
    /// validated, and digested before the durable collection record
    /// commits.
    pub fn collect(&self, record: &RequestRecord) -> CollectOutcome {
        if let Ok(Some(existing)) = self.state.collection(&record.request_id) {
            return self.attach_bodies(record, &existing);
        }
        if self
            .state
            .stop_proof(&record.request_id)
            .ok()
            .flatten()
            .is_none()
        {
            return CollectOutcome::Invalid {
                code: "collection_incomplete",
                detail: "no durable stop proof authorizes collection".into(),
            };
        }
        let dir = self.request_dir(record);
        let out_dir = dir.join("out");
        let mut items: Vec<(CollectionItemRecord, Vec<u8>)> = Vec::new();

        let mut item = |name: &str, required: bool, source: &str| -> Result<bool, CollectOutcome> {
            let path = out_dir.join(name);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    items.push((
                        CollectionItemRecord {
                            item_id: name.to_string(),
                            bytes: bytes.len() as u64,
                            content_digest: item_digest(&bytes),
                            source: source.to_string(),
                        },
                        bytes,
                    ));
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if required {
                        Err(CollectOutcome::Incomplete {
                            code: "collection_incomplete",
                            detail: format!("required member {name} is absent"),
                        })
                    } else {
                        Ok(false)
                    }
                }
                Err(e) => Err(CollectOutcome::Unknown {
                    code: "unavailable",
                    detail: format!("{name}: {e}"),
                }),
            }
        };

        // `report.json` is the one unconditionally required member;
        // the remaining whitelist members are collected when present.
        match item("report.json", true, "out/report.json") {
            Ok(_) => {}
            Err(outcome) => return outcome,
        }
        for name in &OUT_WHITELIST[1..] {
            match item(name, false, &format!("out/{name}")) {
                Ok(_) => {}
                Err(outcome) => return outcome,
            }
        }

        // A producing Invocation's manifest: strict parse plus the
        // request/Invocation identity check — a foreign or stale
        // manifest invalidates the collection.
        let handoff_bytes = std::fs::read(out_dir.join("handoff.json")).ok();
        if let Some(bytes) = &handoff_bytes {
            let manifest = match RemoteHandoffV1::parse(bytes, record.limits.max_item_count) {
                Ok(m) => m,
                Err(e) => {
                    return CollectOutcome::Invalid {
                        code: "collection_invalid",
                        detail: format!("handoff manifest refused: {e}"),
                    };
                }
            };
            let violations =
                manifest.identity_violations(&record.request_id, &record.invocation_id);
            if !violations.is_empty() {
                return CollectOutcome::Invalid {
                    code: "collection_invalid",
                    detail: violations.join("; "),
                };
            }
            // The produced object closure travels as one git bundle —
            // real object transport, never a ref claim.
            match self.bundle_objects(record, &manifest) {
                Ok(bundle) => {
                    items.push((
                        CollectionItemRecord {
                            item_id: OBJECTS_ITEM.into(),
                            bytes: bundle.len() as u64,
                            content_digest: item_digest(&bundle),
                            source: format!(
                                "git:{}..{}",
                                manifest.base_commit, manifest.produced_commit
                            ),
                        },
                        bundle,
                    ));
                }
                Err(detail) => {
                    return CollectOutcome::Invalid {
                        code: "collection_invalid",
                        detail,
                    };
                }
            }
        }

        let total: u64 = items.iter().map(|(i, _)| i.bytes).sum();
        if total > record.limits.max_output_bytes {
            return CollectOutcome::Incomplete {
                code: "limit_exceeded",
                detail: format!("output {total} bytes exceed max_output_bytes"),
            };
        }
        let mut entries: Vec<CollectionEntry> = items
            .iter()
            .map(|(i, _)| CollectionEntry {
                item_id: i.item_id.clone(),
                bytes: i.bytes,
                content_digest: i.content_digest.clone(),
                source: i.source.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.item_id.cmp(&b.item_id));
        let digest = match collection_digest(&entries) {
            Ok(d) => d,
            Err(e) => {
                return CollectOutcome::Unknown {
                    code: "unavailable",
                    detail: format!("collection digest: {e}"),
                };
            }
        };
        let collection = CollectionRecord {
            collection_id: format!(
                "col-{}",
                &sha256_hex(
                    "taisan.executor.collection-id.v1",
                    record.request_id.as_bytes(),
                )[..16]
            ),
            collection_digest: digest,
            items: items.iter().map(|(i, _)| i.clone()).collect(),
            collected_at_unix_seconds: Self::now_seconds(),
        };
        // Persist the bundle body under the request dir so the durable
        // record alone retains it until acknowledgement.
        if let Some((_, bundle)) = items.iter().find(|(i, _)| i.item_id == OBJECTS_ITEM)
            && let Err(e) = write_atomic(&dir.join(OBJECTS_ITEM), bundle)
        {
            return CollectOutcome::Unknown {
                code: "unavailable",
                detail: format!("object bundle: {e}"),
            };
        }
        if let Err(e) = self.state.put_collection(&record.request_id, &collection) {
            return CollectOutcome::Unknown {
                code: "unavailable",
                detail: format!("collection record: {e}"),
            };
        }
        self.attach_bodies(record, &collection)
    }

    /// Attach retained bodies to the durable collection for inline
    /// delivery — bounded by the inline budget so the receipt stays
    /// inside its class bound.
    fn attach_bodies(
        &self,
        record: &RequestRecord,
        collection: &CollectionRecord,
    ) -> CollectOutcome {
        let dir = self.request_dir(record);
        let mut with_bodies = Vec::new();
        let mut total = 0u64;
        for item in &collection.items {
            let path = if item.item_id == OBJECTS_ITEM {
                dir.join(OBJECTS_ITEM)
            } else {
                dir.join(&item.source)
            };
            match std::fs::File::open(&path).and_then(|mut f| {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map(|_| buf)
            }) {
                Ok(body) => {
                    total += body.len() as u64;
                    with_bodies.push((item.clone(), body));
                }
                Err(e) => {
                    return CollectOutcome::Unknown {
                        code: "unavailable",
                        detail: format!("retained body {} unreadable: {e}", item.item_id),
                    };
                }
            }
        }
        if total > INLINE_COLLECT_BUDGET {
            return CollectOutcome::Incomplete {
                code: "limit_exceeded",
                detail: format!(
                    "collection {total} bytes exceed the inline delivery bound; \
                     chunked collection is outside this slice"
                ),
            };
        }
        CollectOutcome::Collected {
            collection_id: collection.collection_id.clone(),
            collection_digest: collection.collection_digest.clone(),
            items: with_bodies,
        }
    }

    /// `git bundle create` of `base..produced` — the produced object
    /// closure as real git transport.
    fn bundle_objects(
        &self,
        record: &RequestRecord,
        manifest: &RemoteHandoffV1,
    ) -> Result<Vec<u8>, String> {
        let dir = self.request_dir(record);
        let workspace = dir.join("workspace");
        let bundle = dir.join("objects.bundle.tmp");
        let spec = format!("{}..{}", manifest.base_commit, manifest.produced_commit);
        let argv = vec![
            "git".to_string(),
            "-C".to_string(),
            workspace.to_string_lossy().into_owned(),
            "bundle".to_string(),
            "create".to_string(),
            bundle.to_string_lossy().into_owned(),
            spec,
        ];
        let bounds = bounded::ExchangeBounds {
            max_stdin: 0,
            max_stdout: 8192,
            max_stderr: 8192,
        };
        match bounded::exchange(&argv, &dir, &git_env(), &[], &bounds, GIT_TIMEOUT) {
            Ok(out) if out.exit_code == Some(0) => std::fs::read(&bundle)
                .map_err(|e| format!("object bundle read: {e}"))
                .inspect(|_| {
                    let _ = std::fs::remove_file(&bundle);
                }),
            Ok(out) => Err(format!(
                "object closure bundle failed (exit {:?}); the produced range is not provable",
                out.exit_code
            )),
            Err(e) => Err(format!("object closure bundle: {e:?}")),
        }
    }
}

/// A state-key rendering safe for use inside identifiers.
pub(crate) fn sanitize_branch_component(s: &str) -> String {
    isolation::sanitize_key(s)
}

/// Environment allowlist for the executor's own git invocations —
/// the same sealed rules the supervisor's workspace path uses.
fn git_env() -> workloop_core::ports::WorkerEnv {
    let mut env = bounded::selector_env();
    env.vars.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env.vars.push(("GIT_ALLOW_PROTOCOL".into(), "file".into()));
    env.vars.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    env
}

fn run_git(args: &[&str], cwd: &Path) -> Result<(), String> {
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    let bounds = bounded::ExchangeBounds {
        max_stdin: 0,
        max_stdout: 8192,
        max_stderr: 8192,
    };
    match bounded::exchange(&argv, cwd, &git_env(), &[], &bounds, GIT_TIMEOUT) {
        Ok(out) if out.exit_code == Some(0) => Ok(()),
        Ok(out) => Err(format!("git {:?} exited {:?}", args, out.exit_code)),
        Err(e) => Err(format!("git {:?}: {e:?}", args)),
    }
}

/// Copy a prepared tree into the workspace, verifying every path
/// stays inside the destination (no traversal, no symlinks).
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in walk(src) {
        let rel = entry
            .strip_prefix(src)
            .map_err(|_| "path outside staging".to_string())?;
        let rel_str = rel.to_string_lossy();
        if !path_is_safe(&rel_str) {
            return Err(format!("unsafe workspace path {rel_str:?}"));
        }
        let meta = entry.symlink_metadata().map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            return Err(format!("workspace item {rel_str:?} is a symlink"));
        }
        if meta.is_dir() {
            std::fs::create_dir_all(dst.join(rel)).map_err(|e| e.to_string())?;
        } else {
            std::fs::copy(&entry, dst.join(rel)).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn walk(root: &Path) -> Vec<PathBuf> {
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                out.push(path.clone());
                if path.is_dir() && !path.is_symlink() {
                    rec(&path, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    rec(root, &mut out);
    out
}
