//! The single-flight eight-phase tick driver (`taisan run`).
//!
//! The loop owns timing, I/O and ordering only: it supplies the clock,
//! the tick deadline and observed facts, and every durable transition
//! still goes through Core (`dispatch_scan`, `consume_due_recovery_intents`,
//! `sweep_scoped`, `handle_observation`) or the store's own mechanisms.
//! One process holds the installation's run lease for the whole run, so
//! lock contention fails closed with `lock.held` and a crashed driver's
//! lease is reclaimed only after its recorded process identity is
//! provably gone — never two concurrent drivers.
//!
//! Phases run in the approved order. A gated phase that sees the tick
//! deadline already passed defers (reporting `tick.deadline`); item
//! loops check the deadline before starting new work; the timed sweep
//! and Attention derivation always run because they decide protective,
//! read-only outcomes rather than starting work. A phase-level
//! infrastructure failure aborts the pass: remaining phases report
//! `pass_aborted` and the failure surfaces once. A SIGINT/SIGTERM
//! shutdown signal finishes the current phase boundary and exits —
//! the remaining phases report `shutdown` and the next run re-enters
//! through phase order; workers are never touched by the signal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use workloop_config::{Installation, LoopConfig, RetentionConfig};
use workloop_core::domain::{ExecutionId, ExecutionState, WorkId};
use workloop_core::observation::HostActionStatus;
use workloop_core::ports::ProcessControlPort;
use workloop_core::projection::{self, QueryResult};
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::work::{
    WorkloopConfig, consume_due_recovery_intents, dispatch_scan, sweep_scoped,
};
use workloop_core::worker_v2::{IssueSnapshotBody, WireIssue};
use workloop_exec::adapter;
use workloop_exec::bodies::{BodyStore, CAS_CATEGORY};
use workloop_exec::isolation;
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::adapter::GitHubAdapter;
use workloop_github::api::IssueSnapshotRead;
use workloop_github::fixture::{FixtureGithub, OutboxReport, ReconcileReport, SourceRefreshReport};
use workloop_github::gh_cli::GhCliApi;
use workloop_store::retention::{CategoryLimits, SweepParams, SweepReport};
use workloop_store::sqlite::SqliteStore;

/// The approved phase order; a pass always reports all eight entries.
pub const PHASES: [&str; 8] = [
    "source_refresh",
    "dispatch_admission",
    "in_flight_supervision",
    "host_outbox",
    "timed_sweep",
    "recovery_scheduling",
    "retention_sweep",
    "attention_derivation",
];

/// Owner-approved per-tick bounds the loop enforces around Core.
pub const OUTBOX_ITEMS_PER_TICK: usize = 8;
pub const RECOVERY_ITEMS_PER_TICK: usize = 16;

/// Set by the SIGINT/SIGTERM handler, read at every phase boundary.
/// The handler itself only stores this flag — a pass in progress runs
/// to its current phase boundary and the driver exits between phases,
/// never mid-phase.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Whether a shutdown signal has been received.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Install the SIGINT/SIGTERM handler the `run` loop honors. With the
/// handler in place a signal never takes the default disposition (an
/// immediate mid-phase kill): it is recorded here and the loop exits
/// at the next phase boundary, leaving every durable effect already
/// committed by its store transaction.
pub fn install_shutdown_handler() -> Result<(), String> {
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::signal(
                signal,
                handle_shutdown_signal as *const () as libc::sighandler_t,
            ) == libc::SIG_ERR as libc::sighandler_t
            {
                return Err(format!("cannot install a handler for signal {signal}"));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseReport {
    pub phase: String,
    /// `completed` | `deferred` | `skipped` | `failed`.
    pub status: String,
    pub processed: usize,
    /// Items left for a later pass (tick deadline or pass bound).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<String>,
    /// Items deliberately not acted on, with their recorded reason.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PhaseReport {
    fn new(phase: &str, status: &str) -> Self {
        Self {
            phase: phase.to_string(),
            status: status.to_string(),
            processed: 0,
            deferred: Vec::new(),
            held: Vec::new(),
            reason: None,
        }
    }

    fn completed(phase: &str) -> Self {
        Self::new(phase, "completed")
    }

    fn deferred_phase(phase: &str) -> Self {
        let mut p = Self::new(phase, "deferred");
        p.reason = Some("tick.deadline".into());
        p
    }

    fn skipped(phase: &str, reason: &str) -> Self {
        let mut p = Self::new(phase, "skipped");
        p.reason = Some(reason.into());
        p
    }

    fn failed(phase: &str, error: impl std::fmt::Display) -> Self {
        let mut p = Self::new(phase, "failed");
        p.reason = Some(error.to_string());
        p
    }
}

/// One bounded pass's report — the durable, always-JSON result the loop
/// leaves behind for every tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TickReport {
    pub schema_version: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub started_unix_seconds: i64,
    pub deadline_unix_seconds: i64,
    pub deadline_exceeded: bool,
    pub aborted: bool,
    pub phases: Vec<PhaseReport>,
}

impl TickReport {
    fn failed_phase(&self) -> Option<&str> {
        self.phases
            .iter()
            .find(|p| p.status == "failed")
            .map(|p| p.phase.as_str())
    }
}

/// Loop-driver settings for one process.
#[derive(Debug, Clone)]
pub struct PassSettings {
    pub loop_config: LoopConfig,
    pub retention: RetentionConfig,
    /// The installation's own files — the live store, its SQLite
    /// sidecars and the config — that the retention sweep must never
    /// reclaim even though they live under the state root. Supplied by
    /// the composition root, which is the only place that knows the
    /// store and config paths.
    pub protected_paths: Vec<PathBuf>,
}

impl PassSettings {
    pub fn from_installation(
        installation: &Installation,
        protected_paths: Vec<PathBuf>,
    ) -> Result<Self, String> {
        let loop_config = installation.loop_settings();
        if loop_config.interval_seconds == 0 {
            return Err("loop.interval_seconds must be > 0".into());
        }
        if loop_config.tick_deadline_seconds == 0 {
            return Err("loop.tick_deadline_seconds must be > 0".into());
        }
        if loop_config.source_refresh_timeout_seconds == 0 {
            return Err("loop.source_refresh_timeout_seconds must be > 0".into());
        }
        let retention = installation.retention_settings();
        if retention.interval_seconds <= 0
            || retention.max_bodies == 0
            || retention.max_bytes == 0
            || retention.delete_error_limit == 0
        {
            return Err(
                "retention sweep bounds (interval_seconds, max_bodies, max_bytes, \
                 delete_error_limit) must all be > 0"
                    .into(),
            );
        }
        Ok(Self {
            loop_config,
            retention,
            protected_paths,
        })
    }
}

pub trait HostDriver {
    fn refresh_sources(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
    ) -> Result<SourceRefreshReport, StoreError>;

    fn reconcile_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<ReconcileReport, StoreError>;

    fn apply_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError>;
}

impl HostDriver for FixtureGithub {
    fn refresh_sources(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
    ) -> Result<SourceRefreshReport, StoreError> {
        FixtureGithub::refresh_sources(self, store, config, clock, deadline, scope)
    }

    fn reconcile_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<ReconcileReport, StoreError> {
        FixtureGithub::reconcile_outbox_bounded(
            self, store, config, clock, deadline, scope, max_items,
        )
    }

    fn apply_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError> {
        FixtureGithub::apply_outbox_bounded(self, store, config, clock, deadline, scope, max_items)
    }
}

impl<C: workloop_github::api::GitHubApi> HostDriver for GitHubAdapter<C> {
    fn refresh_sources(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
    ) -> Result<SourceRefreshReport, StoreError> {
        GitHubAdapter::refresh_sources(self, store, config, clock, deadline, scope)
    }

    fn reconcile_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<ReconcileReport, StoreError> {
        GitHubAdapter::reconcile_outbox_bounded(
            self, store, config, clock, deadline, scope, max_items,
        )
    }

    fn apply_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError> {
        GitHubAdapter::apply_outbox_bounded(self, store, config, clock, deadline, scope, max_items)
    }
}

pub enum RunHost {
    Fixture(Box<FixtureGithub>),
    Github(Box<GitHubAdapter<GhCliApi>>),
}

impl RunHost {
    /// The configured host adapter; `object_source` is the local
    /// repository the GitHub transport pushes produced objects from —
    /// the installation-owned handoff repository. `None` leaves
    /// `write_ref` on the refs API (test composition and object-less
    /// hosts), where a head the host has never seen fails the write
    /// honestly.
    pub fn from_config_with_transport(
        config: &WorkloopConfig,
        object_source: Option<PathBuf>,
    ) -> Result<Self, String> {
        match config.host.adapter.as_str() {
            "fixture" => Ok(Self::Fixture(Box::new(FixtureGithub::new()))),
            "github" => {
                if config.host.repository.host.as_str() != "github.com"
                    || workloop_core::identity::canonical_host_instance(
                        config.host.repository.host.as_str(),
                    ) != "github.com"
                {
                    return Err("driver.host_unsupported".into());
                }
                let mut api = GhCliApi::new().with_hostname("github.com");
                if let Some(source) = object_source {
                    api = api.with_git_transport(source);
                }
                Ok(Self::Github(Box::new(
                    GitHubAdapter::for_host(
                        workloop_core::identity::HostInstanceId::trusted("github.com"),
                        api,
                    )
                    .with_credentials(Box::new(workloop_github::credential::OsProviders)),
                )))
            }
            other => Err(format!("unsupported host adapter: {other}")),
        }
    }
}

impl HostDriver for RunHost {
    fn refresh_sources(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
    ) -> Result<SourceRefreshReport, StoreError> {
        match self {
            Self::Fixture(host) => host.refresh_sources(store, config, clock, deadline, scope),
            Self::Github(host) => host.refresh_sources(store, config, clock, deadline, scope),
        }
    }

    fn reconcile_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<ReconcileReport, StoreError> {
        match self {
            Self::Fixture(host) => {
                host.reconcile_outbox_bounded(store, config, clock, deadline, scope, max_items)
            }
            Self::Github(host) => {
                host.reconcile_outbox_bounded(store, config, clock, deadline, scope, max_items)
            }
        }
    }

    fn apply_outbox_bounded(
        &mut self,
        store: &SqliteStore,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError> {
        match self {
            Self::Fixture(host) => {
                host.apply_outbox_bounded(store, config, clock, deadline, scope, max_items)
            }
            Self::Github(host) => {
                host.apply_outbox_bounded(store, config, clock, deadline, scope, max_items)
            }
        }
    }
}

/// Everything one pass drives. The supervisor and host adapter are
/// owned by the caller so their state persists across ticks of one run.
pub struct Pass<'a, H: HostDriver = FixtureGithub> {
    pub store: &'a SqliteStore,
    pub config: &'a WorkloopConfig,
    pub settings: &'a PassSettings,
    pub supervisor: Option<&'a Supervisor<PosixProcessControl>>,
    pub host: &'a mut H,
    /// The installation state root: the one boundary every reclaimable
    /// body (CAS, workspace, out directory, handoff object) lives under.
    pub state_root: &'a Path,
}

/// Retain every issue snapshot a source refresh read as an immutable
/// CAS body (`docs/v0.3.md` §4): the envelope's canonical bytes land
/// under the spec-bound lookup path, so the dispatch builder resolves
/// the exact retained body without enumerating the catalogue and a
/// Worker never re-fetches remote context. The body's eligibility
/// horizon is the CAS category's documented age limit from now —
/// re-observation on later passes extends it, so a body lives exactly
/// as long as its source keeps being read plus the retention window.
/// A retention fault fails the phase: an unretained read is not
/// dispatchable material, and the next pass re-reads and retries.
fn retain_issue_snapshots(
    store: &SqliteStore,
    state_root: &Path,
    snapshots: &[IssueSnapshotRead],
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    if snapshots.is_empty() {
        return Ok(());
    }
    let bodies = BodyStore::new(state_root.to_path_buf());
    let horizon = CategoryLimits::documented_default(CAS_CATEGORY)
        .and_then(|limits| limits.max_age)
        .map(|max_age| now_unix_seconds.saturating_add(max_age));
    for read in snapshots {
        let issue = &read.issue;
        let snapshot = IssueSnapshotBody::new(
            WireIssue {
                host: issue.repository.host.as_str().to_string(),
                repository: issue.repository.repository.as_str().to_string(),
                issue: issue.issue.as_str().to_string(),
            },
            read.source_revision.clone(),
            read.title.clone(),
            read.body.clone(),
            read.content_digest.clone(),
        );
        let bytes = snapshot.canonical_bytes();
        bodies
            .retain_at(
                store,
                CAS_CATEGORY,
                &snapshot.retained_path(),
                &bytes,
                now_unix_seconds,
                horizon,
            )
            .map_err(|e| StoreError::Internal {
                detail: format!("retain issue snapshot {}: {e}", issue.issue.as_str()),
            })?;
    }
    Ok(())
}

/// Run exactly one bounded pass over the eight ordered phases.
/// `clock` is supplied by the caller so tests can script deadlines; in
/// production it reads the wall clock. `shutdown` is checked at every
/// phase boundary: when it reports true the current phase finishes and
/// the remaining phases are reported `shutdown`, never started.
pub fn run_pass<H: HostDriver>(
    pass: &mut Pass<'_, H>,
    scope: Option<&WorkId>,
    clock: &mut dyn FnMut() -> i64,
    shutdown: &dyn Fn() -> bool,
) -> TickReport {
    let started = clock();
    let deadline = started + pass.settings.loop_config.tick_deadline_seconds as i64;
    let source_deadline = started + pass.settings.loop_config.source_refresh_timeout_seconds as i64;
    let mut phases: Vec<PhaseReport> = Vec::with_capacity(PHASES.len());
    let mut aborted = false;

    // Gated phases check the deadline before starting new work; once a
    // phase fails, every later phase reports `pass_aborted` instead of
    // silently running on a broken pass. A shutdown signal is checked
    // at the same boundary: the phase that was running finishes, the
    // rest report `shutdown` and wait for the next run.
    macro_rules! gated {
        ($index:expr) => {{
            let name = PHASES[$index];
            if aborted {
                phases.push(PhaseReport::skipped(name, "pass_aborted"));
                false
            } else if shutdown() {
                phases.push(PhaseReport::skipped(name, "shutdown"));
                false
            } else {
                let now = clock();
                if now >= deadline {
                    phases.push(PhaseReport::deferred_phase(name));
                    false
                } else {
                    true
                }
            }
        }};
    }
    macro_rules! push {
        ($report:expr) => {{
            let report = $report;
            if report.status == "failed" {
                aborted = true;
            }
            phases.push(report);
        }};
    }

    // Phase 1 — source refresh (host adapter, read only). The
    // configured `RunHost` owns the host legs: the in-process fixture
    // or the production GitHub adapter. Any other adapter identity is
    // refused when the run configures its host, so a pass never
    // fabricates reads or receipts for an adapter that does not own
    // them.
    if gated!(0) {
        let refresh_deadline = source_deadline.min(deadline);
        let result = pass.host.refresh_sources(
            pass.store,
            pass.config,
            clock,
            Some(refresh_deadline),
            scope,
        );
        push!(match result {
            Ok(r) => {
                let mut p = PhaseReport::completed(PHASES[0]);
                p.processed = r.unchanged + r.ingested;
                p.deferred = r.deferred;
                // Retain the immutable snapshot bodies the dispatch
                // builder resolves — before the phase reports done, so
                // a retained read is never lost between observation
                // and request materialization.
                match retain_issue_snapshots(
                    pass.store,
                    pass.state_root,
                    &r.snapshots,
                    r.now_unix_seconds,
                ) {
                    Ok(()) => p,
                    Err(e) => PhaseReport::failed(PHASES[0], e),
                }
            }
            Err(e) => PhaseReport::failed(PHASES[0], e),
        });
    }

    // Phase 2 — dispatch admission (Core judges; the whole scan shares
    // one store transaction and isolates each admission in its own
    // attempt savepoint, with deadline and scope supplied as facts).
    // When the scan leaves unbound pending requests, the worker
    // declaration is probed per distinct request workspace — the same
    // cwd the consume-time re-verification uses, so a recorded binding
    // always re-verifies at launch — and each in-scope unbound request
    // records its binding in a following transaction. The same gate the
    // consume path applies holds here: a request whose Execution can no
    // longer launch is never probed at all. An idle pass probes
    // nothing: no unbound request means no spawned worker. A probe that
    // does not answer leaves the requests unbound; the supervisor's
    // fail-closed selection handles them at consume.
    if gated!(1) {
        let store = pass.store;
        let config = pass.config;
        let result = store.transact(|tx| {
            let scan = dispatch_scan(tx, config, &mut *clock, Some(deadline), scope)?;
            // The binding pass stays inside the pass scope: a scoped
            // `--work` run never writes another Work's requests, and a
            // request whose Execution is not Ready — paused, stopping,
            // cancelled — is never probed since it can never launch.
            let mut unbound = Vec::new();
            for request in tx.pending_requests()? {
                if request.adapter_binding.is_some()
                    || !scope.is_none_or(|w| request.work_id == *w)
                    || tx.execution(&request.execution_id)?.map(|e| e.state)
                        != Some(ExecutionState::Ready)
                {
                    continue;
                }
                unbound.push(request);
            }
            Ok((scan, unbound))
        });
        let result = result.and_then(|(scan, unbound)| {
            let mut binding_cut = false;
            if let Some(supervisor) = pass.supervisor
                && !unbound.is_empty()
            {
                let mut by_workspace: std::collections::BTreeMap<
                    PathBuf,
                    Vec<(String, ExecutionId)>,
                > = std::collections::BTreeMap::new();
                for request in &unbound {
                    by_workspace
                        .entry(supervisor.workspace_of(&request.work_id))
                        .or_default()
                        .push((request.request_id.clone(), request.execution_id.clone()));
                }
                for (workspace, request_ids) in by_workspace {
                    // Each probe spawns a bounded `describe`, so like
                    // every other item loop the tick deadline is
                    // checked before starting new work — a cut leaves
                    // the remaining requests unbound and durable; the
                    // consume path's fail-closed selection binds them.
                    if clock() >= deadline {
                        binding_cut = true;
                        break;
                    }
                    // The dispatch-transaction Ready read is stale by
                    // now: a lease-free command (e.g. cancel) can move
                    // an Execution between it and this probe. Re-read
                    // before spawning — a request whose Execution can
                    // no longer launch is never probed at all.
                    let still_ready = store.transact(|tx| {
                        let mut ready = Vec::new();
                        for (request_id, execution_id) in &request_ids {
                            if tx
                                .execution(execution_id)?
                                .is_some_and(|e| e.state == ExecutionState::Ready)
                            {
                                ready.push((request_id.clone(), execution_id.clone()));
                            }
                        }
                        Ok(ready)
                    })?;
                    if still_ready.is_empty() {
                        continue;
                    }
                    // The probe spawns inside the profile's scratch
                    // tree, never the managed `ws-<work>` directory:
                    // `describe` is effect-free and a pre-created
                    // workspace would land `workspace::prepare` on an
                    // unreceipted conflict when the v2 consume path
                    // materializes the request. The consume path's own
                    // probe uses the same scratch-cwd rule.
                    // The shared prebind scratch is reclaimed as soon
                    // as the probe process is gone, so a fixed key never
                    // accumulates storage between passes.
                    let prebind_scratch =
                        isolation::scratch_root(&supervisor.workspace_root).join("probe-prebind");
                    let Ok(env) =
                        isolation::prepare(&supervisor.isolation_profile, &prebind_scratch)
                    else {
                        continue;
                    };
                    let probe_cwd = prebind_scratch.join("cwd");
                    if std::fs::create_dir_all(&probe_cwd).is_err() {
                        isolation::release(&prebind_scratch);
                        continue;
                    }
                    // The prebind probe proves the same boundary the
                    // consume path launches under: a profile that
                    // requires containment gets the bwrap argv and the
                    // workspace bound read-only; a refused or faulted
                    // gate simply leaves the requests unbound — the
                    // consume path records the typed refusal.
                    let gate = match supervisor.containment_gate(&supervisor.worker_argv, clock()) {
                        Ok(Ok(gate)) => gate,
                        _ => {
                            isolation::release(&prebind_scratch);
                            continue;
                        }
                    };
                    let probed = supervisor.contained_probe(
                        &gate,
                        &supervisor.worker_argv,
                        Some(workspace.as_path()),
                        &prebind_scratch.join("gitconfig"),
                        &env,
                    );
                    let Ok((probe_exec_argv, probe_exec_env)) = probed else {
                        // A boundary refusal is recorded by the consume
                        // path as a typed repair — the prebind probe
                        // simply leaves the requests unbound.
                        isolation::release(&prebind_scratch);
                        continue;
                    };
                    let declared = adapter::observe_worker_declaration_with_argv(
                        &supervisor.port,
                        &probe_exec_argv,
                        &probe_cwd,
                        &probe_exec_env,
                    );
                    isolation::release(&prebind_scratch);
                    let Ok(declaration) = declared else {
                        continue;
                    };
                    let binding = declaration.binding(&supervisor.worker_argv);
                    store.transact(|tx| {
                        for (request_id, execution_id) in &still_ready {
                            // The same gate once more inside the commit:
                            // an Execution stopped between the pre-probe
                            // read and this transaction is not bound.
                            if tx
                                .execution(execution_id)?
                                .is_some_and(|e| e.state == ExecutionState::Ready)
                            {
                                tx.apply(&Effect::BindRequestAdapter {
                                    request_id: request_id.clone(),
                                    binding: binding.clone(),
                                })?;
                            }
                        }
                        Ok(())
                    })?;
                }
            }
            Ok((scan, binding_cut))
        });
        push!(match result {
            Ok((scan, binding_cut)) => {
                let mut p = PhaseReport::completed(PHASES[1]);
                p.processed = scan.admitted.len();
                p.deferred = scan
                    .deferred_deadline
                    .into_iter()
                    .map(|e| e.0)
                    .collect::<Vec<_>>();
                if binding_cut {
                    p.reason = Some("tick.deadline".into());
                }
                p
            }
            Err(e) => PhaseReport::failed(PHASES[1], e),
        });
    }

    // Phase 3 — in-flight supervision (supervisor launches, polls and
    // drives stops; scoped passes never touch another Work's writers).
    // Every supervision item loop checks the tick deadline before it
    // starts new work; a pass cut short reports `tick.deadline` and the
    // untouched items wait, durable, for the next pass.
    if gated!(2) {
        let result = (|| -> Result<(usize, bool), String> {
            let before = pass
                .store
                .transact(|tx| tx.pending_requests())
                .map_err(|e| e.to_string())?
                .len();
            let mut cut = false;
            if let Some(supervisor) = pass.supervisor {
                let now = clock();
                cut = supervisor
                    .run_once_scoped(pass.store, now, scope, clock, Some(deadline))
                    .map_err(|e| e.to_string())?;
            }
            let after = pass
                .store
                .transact(|tx| tx.pending_requests())
                .map_err(|e| e.to_string())?
                .len();
            Ok((before.saturating_sub(after), cut))
        })();
        push!(match result {
            Ok((consumed, cut)) => {
                let mut p = PhaseReport::completed(PHASES[2]);
                p.processed = consumed;
                if cut {
                    p.reason = Some("tick.deadline".into());
                }
                p
            }
            Err(e) => PhaseReport::failed(PHASES[2], e),
        });
    }

    // Phase 4 — host outbox (≤ 8 attempts per tick, then report the
    // untouched remainder; the tick deadline is checked before every
    // new item, so a late pass defers the drain instead of writing
    // past the deadline). A read-only reconcile pass runs first so a
    // receipt lost mid-flight settles from the remote's observed
    // state before any new attempt — never by replaying the write.
    if gated!(3) {
        {
            let result = pass
                .host
                .reconcile_outbox_bounded(
                    pass.store,
                    pass.config,
                    clock,
                    Some(deadline),
                    scope,
                    OUTBOX_ITEMS_PER_TICK,
                )
                .and_then(|reconciled| {
                    pass.host
                        .apply_outbox_bounded(
                            pass.store,
                            pass.config,
                            clock,
                            Some(deadline),
                            scope,
                            OUTBOX_ITEMS_PER_TICK,
                        )
                        .map(|applied| (reconciled, applied))
                })
                .and_then(|(reconciled, r)| {
                    // The phase reports every item left untouched for
                    // the next pass: the reconcile pass's unread
                    // remainder joins the write drain's unattempted
                    // remainder (the drain only reports `pending`
                    // intents, so an `unknown` the read never reached
                    // would otherwise vanish), deduplicated, and an
                    // item the drain settled in between is dropped —
                    // it is no longer untouched.
                    let mut deferred = reconciled.deferred.clone();
                    for id in &r.deferred {
                        if !deferred.contains(id) {
                            deferred.push(id.clone());
                        }
                    }
                    if !deferred.is_empty() {
                        let outstanding: std::collections::HashSet<String> =
                            pass.store.transact(|tx| {
                                Ok(tx
                                    .action_intents()?
                                    .into_iter()
                                    .filter(|i| {
                                        matches!(
                                            i.status,
                                            HostActionStatus::Pending
                                                | HostActionStatus::Unknown { .. }
                                        )
                                    })
                                    .map(|i| i.action_id)
                                    .collect())
                            })?;
                        deferred.retain(|id| outstanding.contains(id));
                    }
                    Ok((reconciled, r, deferred))
                });
            push!(match result {
                Ok((reconciled, r, deferred)) => {
                    let mut p = PhaseReport::completed(PHASES[3]);
                    p.processed = reconciled.examined + r.attempted;
                    p.deferred = deferred;
                    // Intents the pass examined but cannot settle —
                    // credential-blocked ones stay `pending` awaiting
                    // repair, skipped ones reappear every pass — are
                    // held with their recorded reason so a silent stall
                    // is visible in the tick report.
                    let (held, credential_blocked) = outbox_held(&reconciled, &r);
                    p.held = held;
                    p.reason = Some(format!(
                        "reconciled(applied={} failed={} unknown={}) applied={} failed={} unknown={} unsupported={} credential_blocked={} skipped={}",
                        reconciled.applied,
                        reconciled.failed,
                        reconciled.still_unknown,
                        r.applied,
                        r.failed,
                        r.unknown,
                        r.unsupported,
                        credential_blocked,
                        r.skipped.len()
                    ));
                    p
                }
                Err(e) => PhaseReport::failed(PHASES[3], e),
            });
        }
    }

    // Phase 5 — timed sweep (clock-derived Core decisions; always runs —
    // it stops work rather than starting it — unless the pass is
    // aborted or the driver is shutting down at this boundary).
    {
        let name = PHASES[4];
        if aborted {
            phases.push(PhaseReport::skipped(name, "pass_aborted"));
        } else if shutdown() {
            phases.push(PhaseReport::skipped(name, "shutdown"));
        } else {
            let now = clock();
            let result = sweep_scoped(pass.store, pass.config, now, scope);
            push!(match result {
                Ok(r) => {
                    let mut p = PhaseReport::completed(name);
                    p.processed = r.examined;
                    p.held = r
                        .conflicts
                        .iter()
                        .map(|id| format!("{}:sweep_conflict", id.0))
                        .collect();
                    p.reason = Some(format!(
                        "stops_recorded={} terminal_failures={} completed={}",
                        r.stops_recorded, r.terminal_failures, r.completed
                    ));
                    p
                }
                Err(e) => PhaseReport::failed(name, e),
            });
        }
    }

    // Phase 6 — recovery scheduling (due intents, revalidated, bounded;
    // every intent fires under its own transaction — a per-Work
    // conflict is recorded against that Work, never pass-fatal).
    if gated!(5) {
        let store = pass.store;
        let result = consume_due_recovery_intents(
            store,
            pass.config,
            clock,
            Some(deadline),
            scope,
            RECOVERY_ITEMS_PER_TICK,
        );
        push!(match result {
            Ok(scan) => {
                let mut p = PhaseReport::completed(PHASES[5]);
                p.processed = scan.consumed + scan.retired;
                p.deferred = scan.deferred;
                p.held = scan
                    .held
                    .into_iter()
                    .map(|(id, reason)| format!("{id}:{reason}"))
                    .collect();
                p.reason = Some(format!(
                    "requeued={} failed_terminal={} retired={}",
                    scan.requeued, scan.failed_terminal, scan.retired
                ));
                p
            }
            Err(e) => PhaseReport::failed(PHASES[5], e),
        });
    }

    // Phase 7 — retention sweep (U2 tooling: bounded, cadence-gated,
    // protective pins honoured; a diagnostic --work pass never runs it).
    {
        let name = PHASES[6];
        if aborted {
            phases.push(PhaseReport::skipped(name, "pass_aborted"));
        } else if shutdown() {
            phases.push(PhaseReport::skipped(name, "shutdown"));
        } else if scope.is_some() {
            phases.push(PhaseReport::skipped(name, "work_scope"));
        } else {
            let now = clock();
            if now >= deadline {
                phases.push(PhaseReport::deferred_phase(name));
            } else if !pass.settings.retention.enabled {
                phases.push(PhaseReport::skipped(name, "retention_disabled"));
            } else {
                // Single-owner maintenance: the sweep holds the same
                // loop-independent installation maintenance lease as an
                // explicit cleanup. A maintenance pass that loses the
                // lease reports already_running instead of racing.
                match RunLeaseGuard::acquire_maintenance(pass.store, now) {
                    Ok(_maintenance) => {
                        let r = pass.settings.retention.clone();
                        let params = SweepParams {
                            interval_seconds: r.interval_seconds,
                            max_bodies: r.max_bodies,
                            max_bytes: r.max_bytes,
                            delete_error_limit: r.delete_error_limit,
                            protected: pass.settings.protected_paths.clone(),
                        };
                        let result = pass.store.retention_sweep(&params, pass.state_root, now);
                        push!(match result {
                            Ok(report) => retention_phase(report),
                            Err(e) => PhaseReport::failed(name, e),
                        });
                    }
                    Err(e) => {
                        let reason = match &e {
                            workloop_store::sqlite::RunLeaseError::Held { .. } => {
                                format!("already_running: {e}")
                            }
                            workloop_store::sqlite::RunLeaseError::Unavailable { .. } => {
                                format!("lock.unavailable: {e}")
                            }
                            workloop_store::sqlite::RunLeaseError::Store(store_error) => {
                                format!("store_error: {store_error}")
                            }
                        };
                        phases.push(PhaseReport::skipped(name, &reason))
                    }
                }
            }
        }
    }

    // Phase 8 — Attention derivation (recomputed read-only; always runs —
    // it starts no work and must reflect this pass's durable state —
    // unless the pass is aborted or the driver is shutting down).
    {
        let name = PHASES[7];
        if aborted {
            phases.push(PhaseReport::skipped(name, "pass_aborted"));
        } else if shutdown() {
            phases.push(PhaseReport::skipped(name, "shutdown"));
        } else {
            let now = clock();
            let result = pass
                .store
                .transact_deferred(|tx| projection::attention(tx, scope, now));
            push!(match result {
                Ok(QueryResult::Found { value, .. }) => {
                    let mut p = PhaseReport::completed(name);
                    p.processed = value.len();
                    p
                }
                // A scoped pass over an unknown Work: no Attention, no
                // failure — distinct from an unreadable store.
                Ok(QueryResult::NotFound | QueryResult::Forbidden) => {
                    PhaseReport::completed(name)
                }
                Ok(QueryResult::StoreUnavailable) => {
                    PhaseReport::failed(name, "store unavailable")
                }
                Err(e) => PhaseReport::failed(name, e),
            });
        }
    }

    // A phase that started before the deadline can still cross it: the
    // retention sweep and Attention derivation always run and have no
    // per-item deadline gate. The deferred-phase markers cover the
    // earlier gated phases; the closing clock read makes an overrun
    // inside phases 7-8 visible instead of reporting a deadline that
    // the pass actually passed as unmet.
    let deadline_exceeded = phases
        .iter()
        .any(|p| p.status == "deferred" && p.reason.as_deref() == Some("tick.deadline"))
        || clock() >= deadline;
    TickReport {
        schema_version: "1".into(),
        kind: "tick_report".into(),
        scope: scope.map(|w| w.0.clone()),
        started_unix_seconds: started,
        deadline_unix_seconds: deadline,
        deadline_exceeded,
        aborted,
        phases,
    }
}

fn retention_phase(report: SweepReport) -> PhaseReport {
    let mut p = PhaseReport::completed(PHASES[6]);
    if !report.ran {
        let reason = report.defer_reason.as_deref().unwrap_or("cadence");
        return PhaseReport::skipped(PHASES[6], reason);
    }
    p.processed = report.reclaimed.len();
    p.held = report
        .held
        .iter()
        .chain(report.failed.iter())
        .map(|h| format!("{}:{}", h.body_id, h.reason))
        .collect();
    p.reason = Some(format!(
        "bytes_reclaimed={} eligible_remaining={}",
        report.bytes_reclaimed, report.eligible_remaining
    ));
    p
}

impl TickReport {
    /// The phase that aborted the pass, if any — used for the process
    /// exit path after the report itself has been persisted and printed.
    pub fn abort_reason(&self) -> Option<String> {
        self.failed_phase()
            .map(|phase| format!("tick aborted in phase {phase}"))
    }
}

/// Hold the run lease for the lifetime of a run. Released on drop so an
/// early return or panic cannot strand a lease that only this process
/// can clear; a crashed process leaves a lease the next driver reclaims
/// after inspecting the recorded identity as gone.
pub struct RunLeaseGuard<'a> {
    store: &'a SqliteStore,
    owner: workloop_store::sqlite::RunLeaseOwner,
    lease_key: &'static str,
}

impl<'a> RunLeaseGuard<'a> {
    pub fn acquire(
        store: &'a SqliteStore,
        now_unix_seconds: i64,
    ) -> Result<Self, workloop_store::sqlite::RunLeaseError> {
        Self::acquire_keyed(store, now_unix_seconds, "run", |store, owner, now| {
            store.acquire_run_lease(owner, |recorded| PosixProcessControl.inspect(recorded), now)
        })
    }

    /// The loop-independent maintenance lease for explicit tooling: a
    /// second maintenance writer is refused, the loop driver's lease is
    /// unaffected.
    pub fn acquire_maintenance(
        store: &'a SqliteStore,
        now_unix_seconds: i64,
    ) -> Result<Self, workloop_store::sqlite::RunLeaseError> {
        Self::acquire_keyed(
            store,
            now_unix_seconds,
            "maintenance",
            |store, owner, now| {
                store.acquire_maintenance_lease(
                    owner,
                    |recorded| PosixProcessControl.inspect(recorded),
                    now,
                )
            },
        )
    }

    fn acquire_keyed(
        store: &'a SqliteStore,
        now_unix_seconds: i64,
        lease_key: &'static str,
        acquire: impl FnOnce(
            &SqliteStore,
            &workloop_store::sqlite::RunLeaseOwner,
            i64,
        ) -> Result<
            workloop_store::sqlite::RunLeaseGrant,
            workloop_store::sqlite::RunLeaseError,
        >,
    ) -> Result<Self, workloop_store::sqlite::RunLeaseError> {
        let identity =
            PosixProcessControl::identity_of(std::process::id() as i32).map_err(|reason| {
                workloop_store::sqlite::RunLeaseError::Unavailable {
                    reason: format!("cannot record this process's identity: {reason}"),
                }
            })?;
        let owner = workloop_store::sqlite::RunLeaseOwner {
            pid: identity.pid,
            started_at: identity.started_at,
            nonce: format!("{lease_key}-{}", uuidish()),
        };
        acquire(store, &owner, now_unix_seconds)?;
        Ok(Self {
            store,
            owner,
            lease_key,
        })
    }
}

impl Drop for RunLeaseGuard<'_> {
    fn drop(&mut self) {
        let _ = match self.lease_key {
            "maintenance" => self.store.release_maintenance_lease(&self.owner),
            _ => self.store.release_run_lease(&self.owner),
        };
    }
}

fn uuidish() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", std::process::id())
}

/// Compose the outbox phase's held list and credential-blocked count
/// from the reconcile and apply-drain reports. A pending intent that
/// fails the credential gate is reported by both passes; each blocked
/// intent is listed and counted once. The apply drain returns its
/// routing verdicts — a missing Execution, a denied host_write, a
/// foreign host — before reaching the credential gate, while reconcile
/// credential-checks every outstanding intent first, so one intent can
/// surface under both a credential code and a skipped reason. The
/// drain's verdict is the proximate reason no write was attempted, so
/// it wins and each intent is held exactly once.
fn outbox_held(reconciled: &ReconcileReport, applied: &OutboxReport) -> (Vec<String>, usize) {
    let skipped_ids: std::collections::HashSet<&str> = applied
        .skipped
        .iter()
        .map(|s| s.action_id.as_str())
        .collect();
    let mut held_ids = std::collections::HashSet::new();
    let mut credential_blocked = 0;
    let mut held: Vec<String> = Vec::new();
    for b in reconciled
        .credential_blocked
        .iter()
        .chain(&applied.credential_blocked)
    {
        if skipped_ids.contains(b.action_id.as_str()) {
            continue;
        }
        if held_ids.insert(b.action_id.clone()) {
            credential_blocked += 1;
            held.push(format!("{}:{}", b.action_id, b.code));
        }
    }
    held.extend(
        applied
            .skipped
            .iter()
            .map(|s| format!("{}:{}", s.action_id, s.reason)),
    );
    (held, credential_blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use workloop_core::command::*;
    use workloop_core::domain::*;
    use workloop_core::identity::*;
    use workloop_core::observation::{
        ArtifactFact, BranchState, CheckConclusion, Completeness, HostActionStatus, IssueState,
        NormalizedEffectFact, Observation, ObservationFact, ProcessStateFact,
    };
    use workloop_core::store::{Effect, HostAction, HostActionIntent};
    use workloop_core::work::handle_command;
    use workloop_github::api::{
        ApiError, GitHubApi, PullState, RemoteCheckRun, RemoteComment, RemoteIssue, RemotePull,
        RemoteReview, RemoteReviewFinding, RepositoryPermissions,
    };

    const NOW: i64 = 1_700_000_000;
    const TICK: u64 = 60;

    fn config() -> WorkloopConfig {
        WorkloopConfig {
            placement: Ok(workloop_core::placement::PlacementPolicy::default()),
            host: workloop_core::work::HostTemplate {
                adapter: AdapterId::trusted("fixture"),
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("host-local"),
                    RepositoryId::trusted("local-repo"),
                ),
                contract_version: 1,
                actors: vec![RoleBinding {
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: HostInstanceId::trusted("host-local"),
                        actor: HostActorId::trusted("bot-pub"),
                    },
                    credential_ref: CredentialRef::trusted("default"),
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
            budget: ExecutionBudget::default(),
            policy: WorkPolicy::default(),
        }
    }

    fn settings() -> PassSettings {
        PassSettings {
            loop_config: LoopConfig {
                interval_seconds: 30,
                tick_deadline_seconds: TICK,
                source_refresh_timeout_seconds: 30,
            },
            retention: RetentionConfig::default(),
            protected_paths: Vec::new(),
        }
    }

    fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
        let d = handle_command(
            store,
            &config(),
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
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
        let CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } = d
        else {
            panic!("submit {key}: {d:?}")
        };
        (work_id.unwrap(), execution_id)
    }

    fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
        store.transact(|tx| tx.execution(id)).unwrap().unwrap()
    }

    fn issue(number: u64) -> IssueRef {
        IssueRef {
            repository: config().host.repository.clone(),
            issue: IssueId::trusted(format!("issue-{number}")),
            display_number: Some(number),
        }
    }

    fn source(number: u64, revision: &str, digest: &str) -> SourceSnapshot {
        SourceSnapshot {
            issue: issue(number),
            source_revision: revision.into(),
            content_digest: digest.into(),
        }
    }

    fn config_with_issue_read() -> WorkloopConfig {
        let mut c = config();
        c.declared_capabilities.insert(HostCapability::IssueRead);
        c
    }

    fn submit_with_sources(
        store: &SqliteStore,
        key: &str,
        sources: Vec<SourceSnapshot>,
    ) -> (WorkId, ExecutionId) {
        let d = handle_command(
            store,
            &config_with_issue_read(),
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("cli".into()),
                idempotency_key: key.into(),
                body: CommandBody::Submit {
                    intent: format!("intent-{key}"),
                    sources,
                    constraints: vec![],
                    verification_argv: vec![],
                },
            },
            NOW,
        )
        .unwrap();
        let CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } = d
        else {
            panic!("submit {key}: {d:?}")
        };
        (work_id.unwrap(), execution_id)
    }

    fn fixture_bin() -> String {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug/workloop-fixture-worker")
            .to_string_lossy()
            .into_owned()
    }

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-run-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A unique per-call state root for pass-level tests: phase 1 now
    /// retains issue snapshot bodies under `<state_root>/bodies`, so a
    /// pass must never aim that at the process working directory.
    fn pass_state_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "workloop-pass-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn supervisor(mode: &str, root: std::path::PathBuf) -> Supervisor<PosixProcessControl> {
        // Contract launches are fixture-profile evidence: the
        // reference profile's bwrap boundary is proven by the
        // containment suite, not by these pass-level tests.
        Supervisor::new(
            PosixProcessControl,
            root,
            vec![fixture_bin(), mode.to_string()],
            config(),
        )
        .with_isolation_profile(
            workloop_core::worker::isolation_profile(
                workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
            )
            .unwrap(),
        )
    }

    /// One pass whose host adapter and (optional) supervisor are the
    /// caller's, so a test can steer source reads and inject crash
    /// boundaries between passes.
    fn run_supervised(
        store: &SqliteStore,
        settings: &PassSettings,
        host: &mut FixtureGithub,
        supervisor: Option<&Supervisor<PosixProcessControl>>,
        clock: &mut dyn FnMut() -> i64,
    ) -> TickReport {
        let state_root = pass_state_root();
        run_pass(
            &mut Pass {
                store,
                config: &config(),
                settings,
                supervisor,
                host,
                state_root: state_root.as_path(),
            },
            None,
            clock,
            &|| false,
        )
    }

    fn run(
        store: &SqliteStore,
        settings: &PassSettings,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
    ) -> TickReport {
        let mut host = FixtureGithub::new();
        let state_root = pass_state_root();
        run_pass(
            &mut Pass {
                store,
                config: &config(),
                settings,
                supervisor: None,
                host: &mut host,
                state_root: state_root.as_path(),
            },
            scope,
            clock,
            &|| false,
        )
    }

    fn statuses(report: &TickReport) -> Vec<&str> {
        report.phases.iter().map(|p| p.status.as_str()).collect()
    }

    #[test]
    fn eight_phases_run_in_the_approved_order() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        let report = run(&store, &settings, None, &mut || NOW);
        let names: Vec<&str> = report.phases.iter().map(|p| p.phase.as_str()).collect();
        assert_eq!(names, PHASES.to_vec(), "all eight phases in order");
        assert!(
            report.phases.iter().all(|p| p.status == "completed"),
            "an empty installation completes every phase: {:?}",
            report.phases
        );
        assert!(!report.aborted);
        assert!(!report.deadline_exceeded);
        assert_eq!(report.schema_version, "1");
        assert_eq!(report.kind, "tick_report");
        assert_eq!(report.abort_reason(), None);
    }

    #[test]
    fn a_deadline_at_pass_start_defers_every_gated_phase() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (_w, execution_id) = submit(&store, "k-defer");
        let settings = settings();
        // Call 1 starts the pass; every later clock read is past the
        // deadline (NOW + TICK).
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls == 1 { NOW } else { NOW + TICK as i64 }
        });
        assert_eq!(
            statuses(&report),
            vec![
                "deferred",
                "deferred",
                "deferred",
                "deferred",
                "completed",
                "deferred",
                "deferred",
                "completed",
            ],
            "gated phases defer; sweep and Attention still run"
        );
        assert!(report.deadline_exceeded);
        assert!(!report.aborted);
        assert!(
            report
                .phases
                .iter()
                .filter(|p| p.status == "deferred")
                .all(|p| p.reason.as_deref() == Some("tick.deadline"))
        );
        assert_eq!(
            execution(&store, &execution_id).state,
            ExecutionState::Queued,
            "a deadline never admits new work; the item waits for the next pass"
        );
    }

    #[test]
    fn a_mid_phase_deadline_defers_the_remaining_dispatch_item() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (_w1, e1) = submit(&store, "k-1");
        let (_w2, e2) = submit(&store, "k-2");
        let settings = settings();
        // Calls 1-5 (pass start, phase-1 gate, refresh clock, phase-2
        // gate, first dispatch item) are inside the budget; the clock
        // jumps to the deadline before the second dispatch item.
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls <= 5 { NOW } else { NOW + TICK as i64 }
        });
        assert!(!report.aborted);
        let dispatch = &report.phases[1];
        assert_eq!(dispatch.phase, "dispatch_admission");
        assert_eq!(dispatch.status, "completed");
        assert_eq!(
            dispatch.processed, 1,
            "one item admitted before the deadline"
        );
        assert_eq!(
            dispatch.deferred,
            vec![e2.0.clone()],
            "the unfinished item is reported, never dropped"
        );
        assert_eq!(execution(&store, &e1).state, ExecutionState::Ready);
        assert_eq!(
            execution(&store, &e2).state,
            ExecutionState::Queued,
            "the deferred item is handled next pass"
        );
        assert!(report.deadline_exceeded);
        assert_eq!(
            statuses(&report)[2..],
            vec![
                "deferred",
                "deferred",
                "completed",
                "deferred",
                "deferred",
                "completed",
            ],
            "later gated phases defer; sweep and Attention still run: {:?}",
            report.phases
        );
    }

    /// Past the Work-wide deadline the next dispatch pass emits no new
    /// trial: admission decides the same terminal outcome the timed
    /// sweep records — at the request-creation seam, before a writer
    /// could ever launch inside the same tick.
    #[test]
    fn past_the_deadline_the_dispatch_pass_emits_no_new_execution() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-past-deadline");
        let cfg = config();
        // The deadline anchor is durable usage: the chain's first
        // dispatch was long enough ago that automatic-time accounting
        // (elapsed minus recorded human waiting) is over the ceiling.
        store
            .transact(|tx| {
                let mut usage = tx.work_usage(&w)?.unwrap_or_else(|| WorkUsage::new(&w));
                usage.first_dispatch_unix_seconds =
                    Some(NOW - cfg.policy.automatic_time_limit_seconds as i64 - 1);
                tx.apply(&Effect::UpdateWorkUsage { usage })
            })
            .unwrap();
        let settings = settings();
        let report = run(&store, &settings, None, &mut || NOW);
        assert!(!report.aborted);
        let dispatch = &report.phases[1];
        assert_eq!(dispatch.phase, "dispatch_admission");
        assert_eq!(dispatch.status, "completed");
        assert_eq!(
            dispatch.processed, 0,
            "past the deadline no new trial is dispatched"
        );
        assert!(
            dispatch.deferred.is_empty(),
            "the item is decided, not deferred past the deadline"
        );
        assert_eq!(execution(&store, &e).state, ExecutionState::Failed);
        assert_eq!(
            execution(&store, &e).terminal_code.as_deref(),
            Some("automatic_time_exhausted")
        );
        assert!(
            store
                .transact(|tx| tx.pending_requests())
                .unwrap()
                .is_empty(),
            "no Invocation request exists for a trial that never dispatched"
        );
        // The supervision record exposes the deadline that fired.
        let record = store
            .transact(|tx| projection::supervision(tx, &e))
            .unwrap()
            .unwrap();
        assert!(
            record.deadline_unix_seconds.is_some_and(|d| d < NOW),
            "the supervision record's deadline is what admission enforced"
        );
        assert!(record.capacity_slot.is_none());
        assert!(record.reservation_ids.is_empty());
    }

    #[test]
    fn the_outbox_item_loop_stops_at_the_tick_deadline() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-outbox");
        let cfg = config();
        let action_id = store
            .transact(|tx| {
                let action_id = "act-outbox".to_string();
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id: action_id.clone(),
                        work_id: w.clone(),
                        host: cfg.host.repository.host.clone(),
                        repository: cfg.host.repository.clone(),
                        role: ActorRole::Publication,
                        actor: cfg.host.actors[0].actor.clone(),
                        action: HostAction::OpenChangeRequest {
                            source_branch: format!("work/{}", w.0),
                            target_branch: "main".into(),
                            title: "t".into(),
                            body: String::new(),
                        },
                        payload_digest: "digest".into(),
                        status: HostActionStatus::Pending,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
                Ok(action_id)
            })
            .unwrap();
        let settings = settings();
        // Calls 1-7 (pass start, the phase gates, refresh, dispatch)
        // stay inside the budget; the clock reaches the tick deadline
        // exactly where the outbox item loop starts consulting it, so
        // the drain must stop before its first write instead of
        // running all 8 attempts past the deadline.
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls <= 7 { NOW } else { NOW + TICK as i64 }
        });
        let outbox = &report.phases[3];
        assert_eq!(outbox.phase, "host_outbox");
        assert_eq!(outbox.status, "completed");
        assert_eq!(outbox.processed, 0, "no write starts after the deadline");
        assert_eq!(
            outbox.deferred,
            vec![action_id.clone()],
            "the untouched intent is reported for the next pass"
        );
        assert!(report.deadline_exceeded);
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].status,
            HostActionStatus::Pending,
            "the intent waits durable for the next pass"
        );
        assert_eq!(
            statuses(&report),
            vec![
                "completed",
                "completed",
                "completed",
                "completed",
                "completed",
                "deferred",
                "deferred",
                "completed",
            ],
            "later gated phases defer; sweep and Attention still run: {:?}",
            report.phases
        );
    }

    fn insert_outbox_intent(
        store: &SqliteStore,
        work_id: &WorkId,
        action: HostAction,
        status: HostActionStatus,
    ) -> String {
        let cfg = config();
        store
            .transact(|tx| {
                let action_id = tx.alloc_id("act")?;
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id: action_id.clone(),
                        work_id: work_id.clone(),
                        host: cfg.host.repository.host.clone(),
                        repository: cfg.host.repository.clone(),
                        role: ActorRole::Publication,
                        actor: cfg.host.actors[0].actor.clone(),
                        action,
                        payload_digest: "digest".into(),
                        status,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
                Ok(action_id)
            })
            .unwrap()
    }

    fn push_intent(branch: &str, produced: &str) -> HostAction {
        HostAction::PushRef {
            branch: branch.into(),
            produced: CommitRevision::new(config().host.repository.clone(), produced).unwrap(),
            expected_prior: None,
        }
    }

    /// The reconcile pass's untouched remainder reaches the phase
    /// report: when the tick deadline stops the read-only pass before
    /// it examines an outstanding `unknown` intent, the write drain —
    /// which only ever reports `pending` intents — cannot surface it,
    /// so the phase's deferred list must carry it for the next pass.
    #[test]
    fn the_outbox_phase_reports_the_reconcile_remand_at_the_tick_deadline() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-reconcile-remand");
        let unknown_id = insert_outbox_intent(&store, &w, push_intent("work/unread", "head-1"), {
            HostActionStatus::Unknown {
                reason: "response lost".into(),
            }
        });
        let settings = settings();
        // Same budget as the write-drain deadline test: calls 1-7
        // (pass start, the phase gates, refresh, dispatch) stay inside
        // the budget, so the deadline trips where the reconcile item
        // loop first consults it — before any intent is examined.
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls <= 7 { NOW } else { NOW + TICK as i64 }
        });
        let outbox = &report.phases[3];
        assert_eq!(outbox.phase, "host_outbox");
        assert_eq!(outbox.status, "completed");
        assert_eq!(
            outbox.processed, 0,
            "the deadline stops the read before its first item"
        );
        assert_eq!(
            outbox.deferred,
            vec![unknown_id.clone()],
            "the reconcile pass's unread remainder is reported for the next pass"
        );
        assert!(report.deadline_exceeded);
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        assert_eq!(intents.len(), 1);
        assert!(
            matches!(intents[0].status, HostActionStatus::Unknown { .. }),
            "the unread intent stays outstanding: {:?}",
            intents[0].status
        );
    }

    /// The retention sweep starts inside the deadline but has no
    /// per-item deadline check. A pass that crosses the deadline while
    /// sweeping must still say so, even though no phase reports
    /// `tick.deadline` (the gated phases all started inside the budget).
    #[test]
    fn a_deadline_crossed_during_the_retention_sweep_is_reported() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        // Calls 1-14 (through the retention gate) are inside the
        // budget; the clock reaches the deadline before Attention
        // derivation, so the sweep itself overran the tick.
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls <= 14 { NOW } else { NOW + TICK as i64 }
        });
        assert!(
            report.phases.iter().all(|p| p.status != "deferred"),
            "no gated phase deferred: {:?}",
            report.phases
        );
        assert_eq!(report.phases[6].status, "completed");
        assert!(
            report.deadline_exceeded,
            "the retention sweep crossed the deadline: {:?}",
            report.phases
        );
    }

    /// Catalogue paths are installation-relative — a managed body
    /// records a `bodies/…` path under the state root — and the sweep
    /// resolves them under the same state root. An eligible body loses
    /// both its bytes and its `present` state; a mismatched resolution
    /// root would mark the row `reclaimed` as `missing` while the real
    /// file survives.
    #[test]
    fn the_retention_sweep_reclaims_an_eligible_body_under_the_bodies_root() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        let state_root = pass_state_root();
        let rel = "bodies/issue-snapshot/eligible";
        let file = state_root.join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"snapshot").unwrap();
        store
            .register_body(&workloop_store::retention::CatalogBody {
                body_id: "snap-1".into(),
                category: "cas".into(),
                path: rel.into(),
                bytes: 8,
                registered_unix_seconds: NOW - 100,
                eligible_after_unix_seconds: Some(NOW - 10),
                pinned: false,
            })
            .unwrap();

        let mut host = FixtureGithub::new();
        let report = run_pass(
            &mut Pass {
                store: &store,
                config: &config(),
                settings: &settings,
                supervisor: None,
                host: &mut host,
                state_root: state_root.as_path(),
            },
            None,
            &mut || NOW,
            &|| false,
        );

        let retention = &report.phases[6];
        assert_eq!(retention.phase, "retention_sweep");
        assert_eq!(retention.status, "completed");
        assert!(!file.exists(), "the sweep deletes the body file: {file:?}");
        let (state, reason, _) = store.body_state("snap-1").unwrap().unwrap();
        assert_eq!(state, "reclaimed");
        assert_eq!(reason.as_deref(), Some("age_expired"));
        let _ = std::fs::remove_dir_all(&state_root);
    }

    /// Attention derivation always runs and never consults the deadline
    /// itself. The closing clock read must surface a pass that crossed
    /// the deadline while deriving Attention.
    #[test]
    fn a_deadline_crossed_during_attention_derivation_is_reported() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        // Calls 1-15 (through the Attention gate) are inside the
        // budget; the deadline is first observed after derivation.
        let mut calls = 0u32;
        let report = run(&store, &settings, None, &mut || {
            calls += 1;
            if calls <= 15 { NOW } else { NOW + TICK as i64 }
        });
        assert!(
            report.phases.iter().all(|p| p.status != "deferred"),
            "no gated phase deferred: {:?}",
            report.phases
        );
        assert_eq!(report.phases[7].status, "completed");
        assert!(
            report.deadline_exceeded,
            "Attention derivation crossed the deadline: {:?}",
            report.phases
        );
    }

    /// The reconcile pass's per-tick item bound stops the read before
    /// every outstanding intent: the `unknown` remainder the write
    /// drain cannot report (it drains `pending` only) must still reach
    /// the phase's deferred list, while the examined intents settle
    /// from the read alone.
    #[test]
    fn the_outbox_phase_reports_unknown_intents_past_the_reconcile_bound() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-reconcile-bound");
        let mut ids = Vec::new();
        for n in 0..=OUTBOX_ITEMS_PER_TICK {
            ids.push(insert_outbox_intent(
                &store,
                &w,
                push_intent(&format!("work/bound-{n}"), "head-1"),
                HostActionStatus::Unknown {
                    reason: "response lost".into(),
                },
            ));
        }
        let settings = settings();
        let report = run(&store, &settings, None, &mut || NOW);
        let outbox = &report.phases[3];
        assert_eq!(outbox.phase, "host_outbox");
        assert_eq!(outbox.status, "completed");
        assert_eq!(
            outbox.processed, OUTBOX_ITEMS_PER_TICK,
            "the bound stops the read at eight items"
        );
        assert_eq!(
            outbox.deferred,
            vec![ids[OUTBOX_ITEMS_PER_TICK].clone()],
            "the unexamined unknown intent is reported for the next pass"
        );
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        assert_eq!(intents.len(), OUTBOX_ITEMS_PER_TICK + 1);
        let failed = intents
            .iter()
            .filter(|i| matches!(i.status, HostActionStatus::Failed { .. }))
            .count();
        assert_eq!(
            failed, OUTBOX_ITEMS_PER_TICK,
            "the examined intents settled failed on definitive absence"
        );
        let outstanding: Vec<&str> = intents
            .iter()
            .filter(|i| matches!(i.status, HostActionStatus::Unknown { .. }))
            .map(|i| i.action_id.as_str())
            .collect();
        assert_eq!(
            outstanding,
            vec![ids[OUTBOX_ITEMS_PER_TICK].as_str()],
            "only the unexamined intent stays unknown"
        );
    }

    /// The merged remainder never reports an item the same pass
    /// already settled: eight unread `unknown` intents exhaust the
    /// reconcile bound and defer two `pending` pushes the write drain
    /// then lands — nothing is untouched, so the phase reports nothing
    /// deferred.
    #[test]
    fn the_outbox_phase_drops_from_the_remand_what_the_drain_settled() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-reconcile-merged");
        for n in 0..OUTBOX_ITEMS_PER_TICK {
            insert_outbox_intent(
                &store,
                &w,
                push_intent(&format!("work/unread-{n}"), "head-1"),
                HostActionStatus::Unknown {
                    reason: "response lost".into(),
                },
            );
        }
        let mut pending_ids = Vec::new();
        for n in 0..2 {
            pending_ids.push(insert_outbox_intent(
                &store,
                &w,
                push_intent(&format!("work/pending-{n}"), "head-2"),
                HostActionStatus::Pending,
            ));
        }
        let settings = settings();
        let report = run(&store, &settings, None, &mut || NOW);
        let outbox = &report.phases[3];
        assert_eq!(outbox.status, "completed");
        assert_eq!(
            outbox.processed,
            OUTBOX_ITEMS_PER_TICK + 2,
            "eight reads plus two writes"
        );
        assert!(
            outbox.deferred.is_empty(),
            "every item was handled this pass: {:?}",
            outbox.deferred
        );
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        let applied: Vec<&str> = intents
            .iter()
            .filter(|i| i.status == HostActionStatus::Applied)
            .map(|i| i.action_id.as_str())
            .collect();
        assert_eq!(
            applied,
            pending_ids.iter().map(String::as_str).collect::<Vec<_>>(),
            "the deferred-then-reached pending pushes were settled by the drain"
        );
        let failed = intents
            .iter()
            .filter(|i| matches!(i.status, HostActionStatus::Failed { .. }))
            .count();
        assert_eq!(failed, OUTBOX_ITEMS_PER_TICK);
    }

    #[test]
    fn a_scoped_pass_admits_only_the_named_work() {
        let store = SqliteStore::in_memory(4).unwrap();
        let (w1, e1) = submit(&store, "k-1");
        let (_w2, e2) = submit(&store, "k-2");
        let settings = settings();
        let report = run(&store, &settings, Some(&w1), &mut || NOW);
        assert_eq!(report.scope, Some(w1.0.clone()));
        assert_eq!(execution(&store, &e1).state, ExecutionState::Ready);
        assert_eq!(
            execution(&store, &e2).state,
            ExecutionState::Queued,
            "the other Work is untouched by the scoped pass"
        );
        let retention = &report.phases[6];
        assert_eq!(retention.phase, "retention_sweep");
        assert_eq!(retention.status, "skipped");
        assert_eq!(retention.reason.as_deref(), Some("work_scope"));
        assert!(!report.aborted);
    }

    #[test]
    fn a_phase_failure_aborts_the_pass_and_skips_the_rest() {
        let store = SqliteStore::in_memory(2).unwrap();
        let mut settings = settings();
        // Invalid retention bounds: the sweep refuses before any work,
        // the phase reports the failure, the pass aborts.
        settings.retention.max_bodies = 0;
        let report = run(&store, &settings, None, &mut || NOW);
        assert!(report.aborted);
        let retention = &report.phases[6];
        assert_eq!(retention.status, "failed");
        assert!(
            retention
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("positive")),
            "{:?}",
            retention.reason
        );
        assert_eq!(report.phases[7].status, "skipped");
        assert_eq!(report.phases[7].reason.as_deref(), Some("pass_aborted"));
        // Earlier phases still reported their durable progress.
        assert_eq!(report.phases[0].status, "completed");
        assert_eq!(
            report.abort_reason().as_deref(),
            Some("tick aborted in phase retention_sweep")
        );
    }

    #[test]
    fn the_tick_report_is_a_single_json_object() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        let report = run(&store, &settings, None, &mut || NOW);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema_version"], "1");
        assert_eq!(value["kind"], "tick_report");
        assert!(value["phases"].as_array().unwrap().len() == 8);
        assert_eq!(value["aborted"], false);
    }

    #[test]
    fn a_shutdown_signal_stops_the_pass_at_the_phase_boundary() {
        let store = SqliteStore::in_memory(2).unwrap();
        let settings = settings();
        // The signal lands at the boundary before phase 4: the first
        // three phases finish the work they already started, the
        // remaining five report `shutdown` and are never started —
        // the unconditional sweep and Attention phases included.
        let checks = std::cell::Cell::new(0u32);
        let shutdown = || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        };
        let mut host = FixtureGithub::new();
        let state_root = pass_state_root();
        let report = run_pass(
            &mut Pass {
                store: &store,
                config: &config(),
                settings: &settings,
                supervisor: None,
                host: &mut host,
                state_root: state_root.as_path(),
            },
            None,
            &mut || NOW,
            &shutdown,
        );
        assert_eq!(
            statuses(&report),
            vec![
                "completed",
                "completed",
                "completed",
                "skipped",
                "skipped",
                "skipped",
                "skipped",
                "skipped",
            ],
            "the pass ends at the phase boundary: {:?}",
            report.phases
        );
        for phase in &report.phases[3..] {
            assert_eq!(phase.reason.as_deref(), Some("shutdown"), "{:?}", phase);
        }
        assert!(!report.aborted, "shutdown is not a phase failure");
        assert!(!report.deadline_exceeded);
        assert_eq!(report.abort_reason(), None);
        assert_eq!(report.schema_version, "1");
    }

    /// A source read that moved between the ready commit and the launch
    /// holds the durable request: refresh reports the change, the
    /// Execution holds for reapproval, and the recorded request's
    /// Invocation is abandoned without ever spawning a writer.
    #[test]
    fn a_source_change_between_ready_and_spawn_never_launches_a_writer() {
        let store = SqliteStore::in_memory(1).unwrap();
        let (w, e) = submit_with_sources(&store, "k-hold", vec![source(9, "rev-1", "d-1")]);
        let settings = settings();
        let root = tmp_root("hold");
        let mut host = FixtureGithub::new();
        host.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);

        // Pass 1 — matching read admits: queued→ready, capacity and the
        // durable request committed. No supervisor this pass: the driver
        // ends here, as a crash between the ready commit and the launch
        // would leave it.
        let sup = supervisor("hang", root.clone());
        run_supervised(&store, &settings, &mut host, None, &mut || NOW);
        assert_eq!(execution(&store, &e).state, ExecutionState::Ready);

        // The source moved under the approved spec; the next pass reads it.
        host.set_issue(&issue(9), "rev-2", "d-2", IssueState::Open);
        run_supervised(&store, &settings, &mut host, Some(&sup), &mut || NOW);

        let execution = execution(&store, &e);
        assert_eq!(
            execution.state,
            ExecutionState::Paused,
            "the hold settles after the unlaunched request is abandoned"
        );
        assert_eq!(
            execution.repair_prerequisites,
            vec![RepairPrerequisite::SpecReapproval]
        );
        let work = store.transact(|tx| tx.work(&w)).unwrap().unwrap();
        assert_eq!(work.source_gate, SourceGate::Changed);
        let invocations = store
            .transact(|tx| tx.invocations_for_execution(&e))
            .unwrap();
        assert_eq!(invocations.len(), 1, "one recorded Invocation, never two");
        assert_eq!(invocations[0].state, InvocationState::Abandoned);
        assert!(
            invocations[0].process_identity.is_none(),
            "no writer ever existed for the held request"
        );
        assert_eq!(
            execution.invocations_used, 1,
            "the charged admission survives — no budget reset"
        );
        assert!(
            store.transact(|tx| tx.slot_holders()).unwrap().is_empty(),
            "the slot is released once no writer can exist"
        );
    }

    /// Phase 1 retains every successfully-read issue source as an
    /// immutable snapshot body under the CAS root: the catalogue row
    /// resolves by the spec-bound lookup path, the file on disk
    /// verifies against the catalogue identity, and a later pass over
    /// unchanged state re-registers the same body as a replay.
    #[test]
    fn a_source_refresh_retains_the_issue_snapshot_body_for_dispatch() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (_w, _e) = submit_with_sources(&store, "k-snap", vec![source(9, "rev-1", "d-1")]);
        let settings = settings();
        let mut host = FixtureGithub::new();
        host.set_issue_content(
            &issue(9),
            "rev-1",
            "the title",
            "the body",
            IssueState::Open,
        );
        let state_root = pass_state_root();
        let mut clock = || NOW;
        run_pass(
            &mut Pass {
                store: &store,
                config: &config(),
                settings: &settings,
                supervisor: None,
                host: &mut host,
                state_root: state_root.as_path(),
            },
            None,
            &mut clock,
            &|| false,
        );

        let snapshot = IssueSnapshotBody::new(
            WireIssue {
                host: "host-local".into(),
                repository: "local-repo".into(),
                issue: "issue-9".into(),
            },
            "rev-1",
            "the title",
            "the body",
            issue_content_digest("the title", "the body"),
        );
        let path = snapshot.retained_path();
        let entry = store
            .catalogue_entry_at(CAS_CATEGORY, &path)
            .unwrap()
            .expect("the read issue's snapshot is retained");
        assert_eq!(entry.state, "present");
        assert_eq!(entry.category, CAS_CATEGORY);
        // The file under the CAS root resolves to verified bytes.
        let bodies = BodyStore::new(state_root.clone());
        let resolved = bodies
            .resolve_at(&store, CAS_CATEGORY, &path)
            .unwrap()
            .expect("present");
        assert_eq!(resolved.bytes, snapshot.canonical_bytes());
        assert_eq!(resolved.body_id, entry.body_id);

        // A second pass over unchanged state replays the same body —
        // no duplicate row, no rewritten file.
        run_pass(
            &mut Pass {
                store: &store,
                config: &config(),
                settings: &settings,
                supervisor: None,
                host: &mut host,
                state_root: state_root.as_path(),
            },
            None,
            &mut clock,
            &|| false,
        );
        let entry2 = store
            .catalogue_entry_at(CAS_CATEGORY, &path)
            .unwrap()
            .expect("still retained");
        assert_eq!(entry2.body_id, entry.body_id);
        assert_eq!(entry2.bytes, entry.bytes);
    }

    /// A crash after the queued→ready commit replays the durable request
    /// exactly once on the next pass: the supervisor reconciles the
    /// recorded request and launches a single writer.
    #[test]
    fn a_request_persisted_before_a_crash_launches_once_on_restart() {
        let store = SqliteStore::in_memory(1).unwrap();
        let (w, e) = submit_with_sources(&store, "k-replay", vec![source(9, "rev-1", "d-1")]);
        let settings = settings();
        let root = tmp_root("replay");
        let mut host = FixtureGithub::new();
        host.set_issue(&issue(9), "rev-1", "d-1", IssueState::Open);

        // Pass 1 without supervision commits ready + request.
        run_supervised(&store, &settings, &mut host, None, &mut || NOW);
        assert_eq!(execution(&store, &e).state, ExecutionState::Ready);
        assert_eq!(
            store.transact(|tx| tx.pending_requests()).unwrap().len(),
            1,
            "the durable request survives the crash"
        );

        // Restart: a new supervisor replays the request log and launches
        // exactly one writer for it.
        let sup = supervisor("clean-exit", root.clone());
        run_supervised(&store, &settings, &mut host, Some(&sup), &mut || NOW + 1);
        let invocations = store
            .transact(|tx| tx.invocations_for_execution(&e))
            .unwrap();
        let coding: Vec<_> = invocations
            .iter()
            .filter(|i| i.role == InvocationRole::Coding)
            .collect();
        assert_eq!(coding.len(), 1, "one Invocation per request");
        assert!(
            coding[0].process_identity.is_some(),
            "the replayed request produced exactly one writer"
        );
        let execution = execution(&store, &e);
        // The replayed coding admission is the first charge; when the
        // exit lands inside this pass the publish gate may also admit
        // the pre-publication invariant check — a distinct pending
        // request, never a second writer for the replayed one.
        let pending = store.transact(|tx| tx.pending_requests()).unwrap();
        assert!(
            pending
                .iter()
                .all(|r| r.role == InvocationRole::InvariantCheck),
            "the only follow-up request is the invariant check"
        );
        assert_eq!(
            execution.invocations_used,
            1 + pending.len() as u32,
            "admissions equal the consumed coding request plus any pending check"
        );
        assert!(execution.started_seq.is_some());
        let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
        assert_eq!(
            usage.first_dispatch_unix_seconds,
            Some(NOW),
            "the budget anchor is the first dispatch, never re-armed by restart"
        );
    }

    /// A crash between Invocation persist and spawn leaves the start
    /// unprovable: the next pass reports the process state unknown, the
    /// Execution stays owned and uncertain, and no second writer is
    /// launched — restart after restart.
    #[test]
    fn a_spawn_with_no_recorded_identity_recovers_uncertain_without_a_second_writer() {
        let store = SqliteStore::in_memory(1).unwrap();
        let (_w, e) = submit(&store, "k-nearspawn");
        let settings = settings();
        let root = tmp_root("nearspawn");
        let mut host = FixtureGithub::new();

        // Pass 1 commits ready + request; the "crash" then consumes the
        // request and persists a prepared Invocation without ever
        // reaching the spawn.
        run_supervised(&store, &settings, &mut host, None, &mut || NOW);
        let request = store
            .transact(|tx| {
                Ok(tx
                    .pending_requests()?
                    .into_iter()
                    .next()
                    .expect("one request"))
            })
            .unwrap();
        store
            .transact(|tx| {
                let inv_id = tx.alloc_id("inv")?;
                tx.apply(&Effect::InsertInvocation {
                    invocation: Invocation {
                        id: InvocationId(inv_id),
                        request_id: request.request_id.clone(),
                        execution_id: e.clone(),
                        role: InvocationRole::Coding,
                        state: InvocationState::Prepared,
                        process_identity: None,
                        descendants: vec![],
                        required_artifacts: vec![],
                        evidence_refs: vec![],
                    },
                })?;
                tx.apply(&Effect::MarkRequestConsumed {
                    request_id: request.request_id.clone(),
                })?;
                Ok(())
            })
            .unwrap();

        let sup = supervisor("hang", root.clone());
        run_supervised(&store, &settings, &mut host, Some(&sup), &mut || NOW + 1);
        let after = execution(&store, &e);
        assert_eq!(
            after.state,
            ExecutionState::Uncertain,
            "an unprovable start is uncertainty, never assumed death or success"
        );
        let invocations = store
            .transact(|tx| tx.invocations_for_execution(&e))
            .unwrap();
        assert_eq!(
            invocations.len(),
            1,
            "no second writer while the start is unproven"
        );
        assert!(invocations[0].process_identity.is_none());
        assert_eq!(after.invocations_used, 1);

        // Another restart cannot resurrect or duplicate the writer.
        run_supervised(&store, &settings, &mut host, Some(&sup), &mut || NOW + 2);
        assert_eq!(execution(&store, &e).state, ExecutionState::Uncertain);
        assert_eq!(
            store
                .transact(|tx| tx.invocations_for_execution(&e))
                .unwrap()
                .len(),
            1
        );
    }

    /// A pending intent that fails the credential gate is reported by
    /// both the reconcile pass and the apply drain; the phase lists and
    /// counts it once, alongside genuinely distinct blocks and skips.
    #[test]
    fn outbox_held_dedupes_credential_blocks_across_both_passes() {
        let block = |action_id: &str| workloop_github::fixture::CredentialBlock {
            action_id: action_id.into(),
            code: "credential.missing",
        };
        let reconciled = ReconcileReport {
            credential_blocked: vec![block("act-1"), block("act-2"), block("act-4")],
            ..Default::default()
        };
        let applied = OutboxReport {
            credential_blocked: vec![block("act-1")],
            skipped: vec![
                workloop_github::fixture::SkippedIntent {
                    action_id: "act-3".into(),
                    reason: "missing_execution",
                },
                workloop_github::fixture::SkippedIntent {
                    action_id: "act-4".into(),
                    reason: "host_write_denied",
                },
            ],
            ..Default::default()
        };
        let (held, credential_blocked) = outbox_held(&reconciled, &applied);
        assert_eq!(
            credential_blocked, 2,
            "one intent blocked once, and a credential block shadowed by              the drain's routing verdict is not counted"
        );
        assert_eq!(
            held,
            vec![
                "act-1:credential.missing".to_string(),
                "act-2:credential.missing".to_string(),
                "act-3:missing_execution".to_string(),
                "act-4:host_write_denied".to_string(),
            ],
            "act-4 is held once under the drain's verdict, not twice with              contradictory reasons"
        );
    }

    #[test]
    fn the_shutdown_handler_records_a_signal_without_killing_the_process() {
        install_shutdown_handler().expect("the handler installs");
        unsafe { libc::raise(libc::SIGINT) };
        assert!(
            shutdown_requested(),
            "SIGINT is recorded for the next phase boundary instead of \
             taking the default mid-phase kill disposition"
        );
    }

    /// A one-shot fault armed on one transport operation: `Refuse`
    /// answers before any effect lands, `Lose` performs the effect
    /// and then loses the response — the `unknown` receipt path.
    #[derive(Clone)]
    enum ArmedFault {
        Refuse(ApiError),
        Lose(ApiError),
    }

    #[derive(Default, Clone)]
    struct FakeGitHub {
        repo_ok: bool,
        issues: std::collections::HashMap<String, Result<RemoteIssue, ApiError>>,
        pulls: std::collections::HashMap<String, Result<RemotePull, ApiError>>,
        branches: std::collections::HashMap<String, Result<Option<String>, ApiError>>,
        checks: std::collections::HashMap<String, Result<Vec<RemoteCheckRun>, ApiError>>,
        reviews: std::collections::HashMap<String, Result<Vec<RemoteReview>, ApiError>>,
        findings: std::collections::HashMap<String, Result<Vec<RemoteReviewFinding>, ApiError>>,
        deadlines: Vec<Option<std::time::Instant>>,
        /// `(repository, change id)` → posted comments, in order.
        comments: std::collections::HashMap<String, Vec<RemoteComment>>,
        /// `child object` → `parent object` — the ancestry
        /// `contains_revision` walks.
        parents: std::collections::HashMap<String, String>,
        pull_seq: u64,
        comment_seq: u64,
        /// One-shot armed faults per operation name.
        armed: std::collections::HashMap<String, ArmedFault>,
        /// The permission bits `read_permissions` reports.
        permissions: RepositoryPermissions,
        /// The login `viewer()` attests — the ambient identity the
        /// double models; `bot-pub` (the suite's bound publisher)
        /// by default.
        viewer: Option<String>,
    }

    impl FakeGitHub {
        fn new() -> Self {
            Self {
                repo_ok: true,
                permissions: RepositoryPermissions {
                    push: true,
                    pull: true,
                },
                ..Default::default()
            }
        }

        fn pull(head_sha: &str) -> RemotePull {
            RemotePull {
                id: "pr-1".into(),
                display_number: 1,
                head_sha: head_sha.into(),
                source_branch: "work/w".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
                state: PullState::Open,
                merged: false,
                mergeable: None,
                mergeable_state: String::new(),
                base_sha: String::new(),
                conflicting_paths: vec![],
            }
        }

        /// Arm a one-shot fault on `op`: the next call refuses before
        /// any effect (`Refuse`) or lands the effect and loses the
        /// response (`Lose`).
        fn arm(&mut self, op: &str, fault: ArmedFault) {
            self.armed.insert(op.to_string(), fault);
        }

        /// Take the armed fault for `op`, if any — one-shot.
        fn take_fault(&mut self, op: &str) -> Option<ArmedFault> {
            self.armed.remove(op)
        }

        /// The branch head the remote records, `None` when absent.
        fn branch_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
            match self.branches.get(&format!("{repository}:{branch}")) {
                Some(Ok(head)) => head.clone(),
                _ => None,
            }
        }

        /// Seed `(repository, branch)` at `head` for write guards.
        fn seed_branch_head(&mut self, repository: &RepositoryRef, branch: &str, head: &str) {
            self.branches
                .insert(format!("{repository}:{branch}"), Ok(Some(head.to_string())));
        }
    }

    impl GitHubApi for FakeGitHub {
        fn read_repository(
            &mut self,
            _repository: &RepositoryRef,
        ) -> Result<Option<String>, ApiError> {
            if self.repo_ok {
                Ok(Some("default-head".into()))
            } else {
                Err(ApiError::Unknown {
                    reason: "outage".into(),
                })
            }
        }

        fn read_permissions(
            &mut self,
            _repository: &RepositoryRef,
        ) -> Result<RepositoryPermissions, ApiError> {
            if let Some(fault) = self.take_fault("read_permissions") {
                return Err(match fault {
                    ArmedFault::Refuse(e) | ArmedFault::Lose(e) => e,
                });
            }
            Ok(self.permissions)
        }

        fn read_branch(
            &mut self,
            repository: &RepositoryRef,
            branch: &str,
        ) -> Result<Option<String>, ApiError> {
            self.branches
                .get(&format!("{repository}:{branch}"))
                .cloned()
                .unwrap_or_else(|| {
                    Err(ApiError::Unknown {
                        reason: "branch read unsupported".into(),
                    })
                })
        }

        fn write_ref(
            &mut self,
            repository: &RepositoryRef,
            branch: &str,
            new_head: &str,
            expected_prior: Option<&str>,
        ) -> Result<String, ApiError> {
            let fault = self.take_fault("write_ref");
            let lost = match &fault {
                Some(ArmedFault::Refuse(e)) => return Err(e.clone()),
                Some(ArmedFault::Lose(e)) => Some(e.clone()),
                None => None,
            };
            let key = format!("{repository}:{branch}");
            let current = match self.branches.get(&key) {
                Some(Ok(head)) => head.clone(),
                Some(Err(e)) => return Err(e.clone()),
                None => None,
            };
            if current.as_deref() != expected_prior {
                return Err(ApiError::Refused {
                    reason: "stale_head".into(),
                });
            }
            self.branches.insert(key, Ok(Some(new_head.to_string())));
            if let Some(e) = lost {
                return Err(e);
            }
            Ok(new_head.to_string())
        }

        fn read_pull(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
        ) -> Result<RemotePull, ApiError> {
            self.pulls
                .get(&format!("{repository}:{}", change_id.as_str()))
                .cloned()
                .unwrap_or_else(|| {
                    Err(ApiError::Unknown {
                        reason: "outage".into(),
                    })
                })
        }

        fn open_pull(
            &mut self,
            repository: &RepositoryRef,
            source_branch: &str,
            target_branch: &str,
            title: &str,
            body: &str,
        ) -> Result<RemotePull, ApiError> {
            let fault = self.take_fault("open_pull");
            let lost = match &fault {
                Some(ArmedFault::Refuse(e)) => return Err(e.clone()),
                Some(ArmedFault::Lose(e)) => Some(e.clone()),
                None => None,
            };
            let prefix = format!("{repository}:");
            let duplicate = self.pulls.iter().any(|(key, p)| {
                key.starts_with(&prefix)
                    && matches!(p, Ok(pull)
                        if pull.source_branch == source_branch
                            && pull.target_branch == target_branch
                            && !pull.merged)
            });
            if duplicate {
                return Err(ApiError::Refused {
                    reason: "pull request already exists".into(),
                });
            }
            self.pull_seq += 1;
            let head_sha = self
                .branch_head(repository, source_branch)
                .unwrap_or_default();
            let pull = RemotePull {
                id: format!("pr-{}", self.pull_seq),
                display_number: self.pull_seq,
                head_sha,
                source_branch: source_branch.to_string(),
                target_branch: target_branch.to_string(),
                title: title.to_string(),
                body: body.to_string(),
                state: PullState::Open,
                merged: false,
                mergeable: None,
                mergeable_state: String::new(),
                base_sha: String::new(),
                conflicting_paths: vec![],
            };
            self.pulls
                .insert(format!("{repository}:{}", pull.id), Ok(pull.clone()));
            if let Some(e) = lost {
                return Err(e);
            }
            Ok(pull)
        }

        fn update_pull(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
            title: Option<&str>,
            body: Option<&str>,
        ) -> Result<RemotePull, ApiError> {
            let fault = self.take_fault("update_pull");
            let lost = match &fault {
                Some(ArmedFault::Refuse(e)) => return Err(e.clone()),
                Some(ArmedFault::Lose(e)) => Some(e.clone()),
                None => None,
            };
            let key = format!("{repository}:{}", change_id.as_str());
            let mut pull = match self.pulls.get(&key) {
                Some(Ok(pull)) => pull.clone(),
                Some(Err(e)) => return Err(e.clone()),
                None => {
                    return Err(ApiError::Refused {
                        reason: "pull not found".into(),
                    });
                }
            };
            if let Some(title) = title {
                pull.title = title.to_string();
            }
            if let Some(body) = body {
                pull.body = body.to_string();
            }
            self.pulls.insert(key, Ok(pull.clone()));
            if let Some(e) = lost {
                return Err(e);
            }
            Ok(pull)
        }

        fn comment_on_pull(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
            marker: &str,
            body: &str,
        ) -> Result<RemoteComment, ApiError> {
            let fault = self.take_fault("comment_on_pull");
            let lost = match &fault {
                Some(ArmedFault::Refuse(e)) => return Err(e.clone()),
                Some(ArmedFault::Lose(e)) => Some(e.clone()),
                None => None,
            };
            self.comment_seq += 1;
            let comment = RemoteComment {
                comment_id: format!("c-{}", self.comment_seq),
                marker: marker.to_string(),
                body: body.to_string(),
            };
            self.comments
                .entry(format!("{repository}:{}", change_id.as_str()))
                .or_default()
                .push(comment.clone());
            if let Some(e) = lost {
                return Err(e);
            }
            Ok(comment)
        }

        fn find_comment(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
            marker: &str,
        ) -> Result<Option<RemoteComment>, ApiError> {
            let fault = self.take_fault("find_comment");
            if let Some(ArmedFault::Refuse(e)) | Some(ArmedFault::Lose(e)) = &fault {
                return Err(e.clone());
            }
            let found = self
                .comments
                .get(&format!("{repository}:{}", change_id.as_str()))
                .and_then(|comments| comments.iter().find(|c| c.marker == marker).cloned());
            Ok(found)
        }

        fn list_pulls_on_branch(
            &mut self,
            repository: &RepositoryRef,
            source_branch: &str,
            head_sha: Option<&str>,
        ) -> Result<Vec<RemotePull>, ApiError> {
            let fault = self.take_fault("list_pulls_on_branch");
            if let Some(ArmedFault::Refuse(e)) | Some(ArmedFault::Lose(e)) = &fault {
                return Err(e.clone());
            }
            let prefix = format!("{repository}:");
            Ok(self
                .pulls
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .filter_map(|(_, p)| match p {
                    Ok(pull)
                        if pull.source_branch == source_branch
                            && head_sha.is_none_or(|sha| pull.head_sha == sha) =>
                    {
                        Some(pull.clone())
                    }
                    _ => None,
                })
                .collect())
        }

        fn check_runs(
            &mut self,
            repository: &RepositoryRef,
            head_sha: &str,
        ) -> Result<Vec<RemoteCheckRun>, ApiError> {
            self.checks
                .get(&format!("{repository}:{head_sha}"))
                .cloned()
                .unwrap_or(Ok(vec![]))
        }

        fn reviews(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
        ) -> Result<Vec<RemoteReview>, ApiError> {
            self.reviews
                .get(&format!("{repository}:{}", change_id.as_str()))
                .cloned()
                .unwrap_or(Ok(vec![]))
        }

        fn review_findings(
            &mut self,
            repository: &RepositoryRef,
            change_id: &ChangeRequestId,
        ) -> Result<Vec<RemoteReviewFinding>, ApiError> {
            self.findings
                .get(&format!("{repository}:{}", change_id.as_str()))
                .cloned()
                .unwrap_or(Ok(vec![]))
        }

        fn contains_revision(
            &mut self,
            _repository: &RepositoryRef,
            ancestor: &str,
            descendant: &str,
        ) -> Result<bool, ApiError> {
            let fault = self.take_fault("contains_revision");
            if let Some(ArmedFault::Refuse(e)) | Some(ArmedFault::Lose(e)) = &fault {
                return Err(e.clone());
            }
            let mut current = descendant.to_string();
            for _ in 0..=self.parents.len() {
                if current == ancestor {
                    return Ok(true);
                }
                match self.parents.get(&current) {
                    Some(parent) => current = parent.clone(),
                    None => return Ok(false),
                }
            }
            Ok(false)
        }

        fn read_issue(&mut self, issue: &IssueRef) -> Result<RemoteIssue, ApiError> {
            self.issues
                .get(issue.issue.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    Err(ApiError::Unknown {
                        reason: "outage".into(),
                    })
                })
        }

        fn viewer(&mut self) -> Result<Option<String>, ApiError> {
            Ok(Some(
                self.viewer.clone().unwrap_or_else(|| "bot-pub".into()),
            ))
        }

        fn set_read_deadline(&mut self, deadline: Option<std::time::Instant>) {
            self.deadlines.push(deadline);
        }
    }

    fn remote_key(repository: &RepositoryRef, id: &str) -> String {
        format!("{repository}:{id}")
    }

    fn cfg_key(id: &str) -> String {
        remote_key(&config().host.repository, id)
    }

    fn change_ref(id: &str) -> ChangeRequestRef {
        ChangeRequestRef {
            repository: config().host.repository.clone(),
            change_request: ChangeRequestId::trusted(id),
            display_number: None,
        }
    }

    fn production_adapter(client: FakeGitHub) -> GitHubAdapter<FakeGitHub> {
        GitHubAdapter::for_host(config().host.repository.host.clone(), client)
    }

    fn run_production(
        store: &SqliteStore,
        settings: &PassSettings,
        adapter: &mut GitHubAdapter<FakeGitHub>,
        clock: &mut dyn FnMut() -> i64,
    ) -> TickReport {
        let cfg = config();
        let state_root = pass_state_root();
        run_pass(
            &mut Pass {
                store,
                config: &cfg,
                settings,
                supervisor: None,
                host: adapter,
                state_root: state_root.as_path(),
            },
            None,
            clock,
            &|| false,
        )
    }

    fn issue_facts(store: &SqliteStore, work_id: &WorkId) -> Vec<(String, String, IssueState)> {
        store
            .transact(|tx| tx.observations_for_work(work_id))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::Issue {
                    source_revision,
                    content_digest,
                    state,
                    ..
                } => Some((
                    source_revision.clone(),
                    content_digest.clone(),
                    state.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    fn head_facts(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
        store
            .transact(|tx| tx.observations_for_work(work_id))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::ChangeHead { head, .. } => Some(head.object.clone()),
                _ => None,
            })
            .collect()
    }

    fn review_facts(store: &SqliteStore, work_id: &WorkId) -> Vec<(String, Completeness)> {
        store
            .transact(|tx| tx.observations_for_work(work_id))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::Review {
                    target,
                    completeness,
                    ..
                } => Some((target.head.object.clone(), completeness.clone())),
                _ => None,
            })
            .collect()
    }

    fn open_change_receipt(
        store: &SqliteStore,
        work_id: &WorkId,
        change: &ChangeRequestRef,
        target_branch: &str,
    ) {
        let action_id = insert_outbox_intent(
            store,
            work_id,
            HostAction::OpenChangeRequest {
                source_branch: "work/w".into(),
                target_branch: target_branch.into(),
                title: "t".into(),
                body: String::new(),
            },
            HostActionStatus::Applied,
        );
        workloop_core::work::handle_observation(
            store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id,
                    authenticated_actor: None,
                    status: HostActionStatus::Applied,
                    effects: vec![NormalizedEffectFact::ChangeOpened {
                        change: change.clone(),
                        head: CommitRevision::new(config().host.repository.clone(), "head-open")
                            .unwrap(),
                        source_branch: "work/w".into(),
                        target_branch: target_branch.into(),
                    }],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    }

    #[test]
    fn the_run_driver_selects_the_configured_adapter_visibly() {
        let mut cfg = config();
        assert!(
            matches!(
                RunHost::from_config_with_transport(&cfg, None),
                Ok(RunHost::Fixture(_))
            ),
            "fixture config must not pick production"
        );
        cfg.host.adapter = AdapterId::trusted("github");
        cfg.allowed_effects.host_write = false;
        cfg.host.repository = RepositoryRef::new(
            HostInstanceId::trusted("github.com"),
            RepositoryId::trusted("owner/repo"),
        );
        assert!(
            matches!(
                RunHost::from_config_with_transport(&cfg, None),
                Ok(RunHost::Github(_))
            ),
            "github config must not pick the fixture"
        );
        cfg.host.repository = RepositoryRef::new(
            HostInstanceId::trusted("host-local"),
            RepositoryId::trusted("local-repo"),
        );
        let err = RunHost::from_config_with_transport(&cfg, None)
            .err()
            .unwrap();
        assert!(
            err.contains("driver.host_unsupported"),
            "a non-github.com host refuses before any run: {err}"
        );
        cfg.host.repository = RepositoryRef::new(
            HostInstanceId::trusted("ghe.corp.example"),
            RepositoryId::trusted("owner/repo"),
        );
        let err = RunHost::from_config_with_transport(&cfg, None)
            .err()
            .unwrap();
        assert!(
            err.contains("driver.host_unsupported"),
            "an enterprise host refuses before any run: {err}"
        );
        for alias in ["api.github.com", "https://github.com", "github.com/owner"] {
            cfg.host.repository = RepositoryRef::new(
                HostInstanceId::trusted(alias),
                RepositoryId::trusted("owner/repo"),
            );
            let err = RunHost::from_config_with_transport(&cfg, None)
                .err()
                .unwrap();
            assert!(
                err.contains("driver.host_unsupported"),
                "raw host {alias} must refuse rather than be remapped: {err}"
            );
        }
        cfg.host.repository = RepositoryRef::new(
            HostInstanceId::trusted("github.com"),
            RepositoryId::trusted("owner/repo"),
        );
        cfg.allowed_effects.host_write = true;
        assert!(
            matches!(
                RunHost::from_config_with_transport(&cfg, None),
                Ok(RunHost::Github(_))
            ),
            "github + host_write composes the production driver: {cfg:?}"
        );
        cfg.host.adapter = AdapterId::trusted("s1_bridge");
        let err = RunHost::from_config_with_transport(&cfg, None)
            .err()
            .unwrap();
        assert!(
            err.contains("unsupported host adapter"),
            "an unknown adapter is a visible config error: {err}"
        );
    }

    #[test]
    fn the_production_driver_ingests_source_issue_facts_and_deduplicates() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) =
            submit_with_sources(&store, "k-prod-src", vec![source(7, "rev-1", "digest-1")]);
        let mut fake = FakeGitHub::new();
        fake.issues.insert(
            "issue-7".into(),
            Ok(RemoteIssue {
                source_revision: "rev-1".into(),
                content_digest: "digest-1".into(),
                state: IssueState::Open,
                title: String::new(),
                body: String::new(),
            }),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(statuses(&report), vec!["completed"; 8]);
        assert_eq!(
            issue_facts(&store, &w).last().unwrap(),
            &("rev-1".into(), "digest-1".into(), IssueState::Open)
        );
        let before = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .len();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(report.phases[0].status, "completed");
        assert_eq!(
            store
                .transact(|tx| tx.observations_for_work(&w))
                .unwrap()
                .len(),
            before
        );
    }

    #[test]
    fn the_production_driver_records_outage_then_recovers() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit_with_sources(&store, "k-prod-out", vec![source(7, "rev-1", "d1")]);
        let mut adapter = production_adapter(FakeGitHub::new());
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(statuses(&report), vec!["completed"; 8]);
        assert_eq!(
            issue_facts(&store, &w).last().unwrap(),
            &(String::new(), String::new(), IssueState::Unavailable),
            "an unreadable source is recorded unavailable, never clean"
        );
        adapter.client_mut().issues.insert(
            "issue-7".into(),
            Ok(RemoteIssue {
                source_revision: "rev-1".into(),
                content_digest: "d1".into(),
                state: IssueState::Open,
                title: String::new(),
                body: String::new(),
            }),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            issue_facts(&store, &w).last().unwrap(),
            &("rev-1".into(), "d1".into(), IssueState::Open),
            "the next pass records the recovered read"
        );
    }

    #[test]
    fn two_works_sharing_one_change_each_record_their_own_facts() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w1, _e1) = submit(&store, "k-share-1");
        let (w2, _e2) = submit(&store, "k-share-2");
        let change = change_ref("cr-shared");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        for w in [&w1, &w2] {
            insert_outbox_intent(
                &store,
                w,
                HostAction::MarkReady {
                    change: change.clone(),
                    expected_head: head.clone(),
                },
                HostActionStatus::Applied,
            );
        }
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-shared"), Ok(FakeGitHub::pull("head-1")));
        fake.checks.insert(
            cfg_key("head-1"),
            Ok(vec![RemoteCheckRun {
                check_run_id: "run-9".into(),
                name: "ci".into(),
                head_sha: "head-1".into(),
                conclusion: CheckConclusion::Success,
                producer: "app-ci".into(),
            }]),
        );
        fake.reviews.insert(cfg_key("cr-shared"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-shared"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(statuses(&report), vec!["completed"; 8]);
        for w in [&w1, &w2] {
            assert_eq!(head_facts(&store, w), vec!["head-1".to_string()]);
            let observations = store.transact(|tx| tx.observations_for_work(w)).unwrap();
            let check = observations.iter().find_map(|o| match &o.fact {
                ObservationFact::Check {
                    check_id,
                    producer,
                    conclusion,
                    head,
                    ..
                } => Some((
                    check_id.clone(),
                    producer.actor.clone(),
                    conclusion.clone(),
                    head.object.clone(),
                )),
                _ => None,
            });
            assert_eq!(
                check.map(|(id, actor, conclusion, head)| (
                    id,
                    actor.as_str().to_string(),
                    conclusion,
                    head
                )),
                Some((
                    "ci".into(),
                    "app-ci".into(),
                    CheckConclusion::Success,
                    "head-1".into()
                )),
                "work {} keeps the exact check producer and head",
                w.0
            );
        }
        let ids: Vec<String> = store
            .transact(|tx| tx.all_observations())
            .unwrap()
            .iter()
            .filter_map(|o| o.delivery.as_ref().map(|d| d.event_id.clone()))
            .filter(|id| id.starts_with("check:"))
            .collect();
        assert_eq!(ids.len(), 2, "each work records its own check fact");
        assert!(ids.iter().any(|id| id.contains(&w1.0)));
        assert!(ids.iter().any(|id| id.contains(&w2.0)));
        for w in [&w1, &w2] {
            let review = review_facts(&store, w)
                .into_iter()
                .find(|(_, completeness)| completeness.is_complete());
            assert_eq!(
                review.map(|(h, _)| h),
                Some("head-1".to_string()),
                "work {} keeps its own complete review at the exact head",
                w.0
            );
        }
    }

    #[test]
    fn a_fresh_adapter_instance_still_records_the_new_head() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-fresh");
        let change = change_ref("cr-fresh");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let settings = settings();
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-fresh"), Ok(FakeGitHub::pull("head-1")));
        let mut adapter = production_adapter(fake);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-fresh"), Ok(FakeGitHub::pull("head-2")));
        let mut adapter = production_adapter(fake);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            head_facts(&store, &w),
            vec!["head-1".to_string(), "head-2".to_string()],
            "the fresh instance records the moved head"
        );
    }

    #[test]
    fn a_failed_review_read_records_unavailable_not_empty() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-review-out");
        let change = change_ref("cr-review");
        let head = CommitRevision::new(config().host.repository.clone(), "head-9").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-review"), Ok(FakeGitHub::pull("head-9")));
        fake.reviews.insert(
            cfg_key("cr-review"),
            Err(ApiError::Unknown {
                reason: "outage".into(),
            }),
        );
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let completeness = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .iter()
            .find_map(|o| match &o.fact {
                ObservationFact::Review {
                    target,
                    completeness,
                    ..
                } => Some((target.head.object.clone(), completeness.clone())),
                _ => None,
            });
        assert_eq!(
            completeness,
            Some((
                "head-9".into(),
                Completeness::Unavailable {
                    reason: "reviews read failed".into()
                }
            )),
            "a review endpoint failure is unavailable evidence at the exact head"
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-review"), Ok(FakeGitHub::pull("head-9")));
        fake.reviews.insert(cfg_key("cr-review"), Ok(vec![]));
        fake.findings.insert(
            cfg_key("cr-review"),
            Err(ApiError::Unknown {
                reason: "outage".into(),
            }),
        );
        let mut adapter = production_adapter(fake);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let observations = store.transact(|tx| tx.observations_for_work(&w)).unwrap();
        let unavailable: Vec<_> = observations
            .iter()
            .filter(|o| {
                matches!(
                    &o.fact,
                    ObservationFact::Review {
                        completeness: Completeness::Unavailable { .. },
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(unavailable.len(), 2);
    }

    #[test]
    fn production_outbox_attempts_pending_intents_through_the_bounded_drain() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-prod-outbox");
        let push_id = insert_outbox_intent(
            &store,
            &w,
            HostAction::PushRef {
                branch: "work/w".into(),
                produced: CommitRevision::new(config().host.repository.clone(), "h-1").unwrap(),
                expected_prior: None,
            },
            HostActionStatus::Pending,
        );
        // The branch read cannot be answered: the pre-write check is
        // indeterminate, so the intent stays `pending` and the write
        // is never attempted blind — an unreadable read fabricates no
        // `unknown` verdict the next absent reconcile could settle
        // `failed` against a write that never ran.
        let mut adapter = production_adapter(FakeGitHub::new());
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(report.phases[3].phase, "host_outbox");
        assert_eq!(
            report.phases[3].status, "completed",
            "the bounded drain completes with the item held: {:?}",
            report.phases[3]
        );
        assert!(
            matches!(intent_status(&store, &push_id), HostActionStatus::Pending),
            "an indeterminate pre-write read stays pending, never a blind write"
        );
        // A second pass keeps the intent pending — it is never dropped
        // and never re-attempted blind while the read cannot answer.
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(matches!(
            intent_status(&store, &push_id),
            HostActionStatus::Pending
        ));
        let store = SqliteStore::in_memory(2).unwrap();
        submit(&store, "k-prod-empty");
        let mut adapter = production_adapter(FakeGitHub::new());
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            statuses(&report),
            vec!["completed"; 8],
            "no outstanding intents means an empty outbox report"
        );
    }

    /// #355: the production driver itself produces the CapabilityReport
    /// evidence the publish gate reads. A Work parked at `waiting` with
    /// complete producing evidence — the state the non-live acceptance
    /// capture stalled in — gets its stage intents recorded and applied
    /// by the ordinary pass, with no test-injected report.
    #[test]
    fn the_production_driver_reports_capabilities_and_publishes() {
        let store = SqliteStore::in_memory(2).unwrap();
        // Lanes off: this test isolates the capability gate; lane
        // evidence has its own coverage.
        let mut submit_cfg = config();
        submit_cfg.lanes = LanePolicy {
            invariant_check: false,
            code_review: false,
        };
        let (w, e) = {
            let d = handle_command(
                &store,
                &submit_cfg,
                &CommandRequest {
                    actor: Actor {
                        kind: ActorKind::Human,
                        external_id: "op".into(),
                    },
                    origin: Origin("cli".into()),
                    idempotency_key: "k-cap-publish".into(),
                    body: CommandBody::Submit {
                        intent: "intent-cap-publish".into(),
                        sources: vec![],
                        constraints: vec![],
                        verification_argv: vec![],
                    },
                },
                NOW,
            )
            .unwrap();
            let CommandDecision::Accepted {
                work_id,
                effect: CommandEffect::WorkCreated { execution_id },
                ..
            } = d
            else {
                panic!("submit: {d:?}")
            };
            (work_id.unwrap(), execution_id)
        };
        // The producing Invocation resolved `Done` with its required
        // artifact present — the durable shape a finished worker
        // leaves — and the Execution waits for reconciliation.
        let digest = "produced-commit-1".to_string();
        let invocation_id = store
            .transact(|tx| {
                let invocation = Invocation {
                    id: InvocationId(tx.alloc_id("inv")?),
                    request_id: format!("{}:inv:1", e.0),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    descendants: vec![],
                    required_artifacts: vec![RequiredArtifact {
                        kind: "artifact.json".into(),
                        path: "artifact.json".into(),
                        expected_digest: Some(digest.clone()),
                    }],
                    evidence_refs: vec![],
                };
                tx.apply(&Effect::InsertInvocation {
                    invocation: invocation.clone(),
                })?;
                let mut execution = tx.execution(&e)?.unwrap();
                let expected_state = execution.state.clone();
                execution.state = ExecutionState::Waiting;
                execution.waiting_reason = Some(WaitingReason::Reconciliation);
                tx.apply(&Effect::UpdateExecution {
                    execution,
                    expected_state,
                })?;
                Ok(invocation.id)
            })
            .unwrap();
        workloop_core::work::handle_observation(
            &store,
            &submit_cfg,
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: Some(e.clone()),
                fact: ObservationFact::Artifact {
                    invocation_id,
                    required_artifact: "artifact.json".into(),
                    state: ArtifactFact::Present {
                        digest: digest.clone(),
                    },
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
        // The #355 stall, reproduced: complete evidence, no capability
        // report, zero intents.
        assert!(
            store.transact(|tx| tx.action_intents()).unwrap().is_empty(),
            "no report means the publish gate records nothing"
        );
        let mut fake = FakeGitHub::new();
        // The managed branch does not exist yet: the first push
        // expects an absent prior ref.
        fake.branches.insert(
            format!("{}:work/{}", submit_cfg.host.repository, w.0),
            Ok(None),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            report.phases.iter().all(|p| p.status != "failed"),
            "the pass completes: {:?}",
            report.phases
        );
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        let push = intents
            .iter()
            .find(|i| matches!(i.action, HostAction::PushRef { .. }))
            .expect("the capability report unblocked the push intent");
        assert_eq!(
            push.status,
            HostActionStatus::Applied,
            "the bounded drain applied the recorded push: {intents:?}"
        );
        // The applied push's read-back lands as durable evidence, and
        // the open-change-request stage follows on the same pass.
        assert!(
            intents
                .iter()
                .any(|i| matches!(i.action, HostAction::OpenChangeRequest { .. })),
            "the publish chain advanced past push_ref: {intents:?}"
        );
        let reports: Vec<_> = store
            .transact(|tx| tx.all_observations())
            .unwrap()
            .into_iter()
            .filter(|o| {
                matches!(
                    o.fact,
                    ObservationFact::CapabilityReport { ref host, .. }
                        if canonical_host_instance(host.as_str())
                            == canonical_host_instance(config().host.repository.host.as_str())
                )
            })
            .collect();
        assert_eq!(reports.len(), 1, "one capability report per change");
    }

    #[test]
    fn the_refresh_deadline_defers_subjects_and_later_phases_report() {
        let store = SqliteStore::in_memory(2).unwrap();
        submit_with_sources(&store, "k-dl-1", vec![source(7, "r", "d")]);
        submit_with_sources(&store, "k-dl-2", vec![source(8, "r", "d")]);
        let mut adapter = production_adapter(FakeGitHub::new());
        let settings = settings();
        let mut calls = 0u32;
        let report = run_production(&store, &settings, &mut adapter, &mut || {
            calls += 1;
            if calls <= 4 { NOW } else { NOW + TICK as i64 }
        });
        assert_eq!(report.phases.len(), 8);
        assert_eq!(report.phases[0].status, "completed");
        assert_eq!(
            report.phases[0].deferred.len(),
            1,
            "one source is deferred to the next pass: {:?}",
            report.phases[0]
        );
        assert!(
            report.phases.iter().all(|p| p.status != "failed"),
            "deadline deferral is never a failed phase: {:?}",
            report.phases
        );
        assert_eq!(
            adapter.client_mut().deadlines.len(),
            1,
            "the adapter installs one absolute read deadline per refresh"
        );
    }

    #[test]
    fn out_of_order_head_history_never_rolls_the_current_head_back() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-order");
        let cfg = config();
        let change = change_ref("cr-order");
        open_change_receipt(&store, &w, &change, "main");
        let obs = |head: &str, at: i64| Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(e.clone()),
            fact: ObservationFact::ChangeHead {
                change: change.clone(),
                head: CommitRevision::new(cfg.host.repository.clone(), head).unwrap(),
            },
            observed_unix_seconds: at,
        };
        workloop_core::work::handle_observation(&store, &cfg, &obs("head-new", NOW + 10), NOW + 10)
            .unwrap();
        workloop_core::work::handle_observation(&store, &cfg, &obs("head-new", NOW + 10), NOW + 10)
            .unwrap();
        workloop_core::work::handle_observation(&store, &cfg, &obs("head-old", NOW), NOW + 20)
            .unwrap();
        let heads = head_facts(&store, &w);
        assert_eq!(
            heads,
            vec![
                "head-new".to_string(),
                "head-new".to_string(),
                "head-old".to_string()
            ],
            "history keeps every delivery"
        );
        let detail = store
            .transact(|tx| projection::work_detail(&*tx, &w, NOW + 30))
            .unwrap();
        let QueryResult::Found { value: detail, .. } = detail else {
            panic!("work detail missing: {detail:?}")
        };
        assert_eq!(
            detail
                .current_change
                .and_then(|(_, head)| head)
                .map(|h| h.object),
            Some("head-new".to_string()),
            "the stale delivery stays history — current never rolls back"
        );
    }

    #[test]
    fn a_change_outside_the_works_binding_is_never_rebound() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-rebind");
        let foreign = ChangeRequestRef {
            repository: RepositoryRef::new(
                config().host.repository.host.clone(),
                RepositoryId::trusted("other-repo"),
            ),
            change_request: ChangeRequestId::trusted("cr-9"),
            display_number: None,
        };
        let mut adapter = production_adapter(FakeGitHub::new());
        let cfg = config();
        let result = adapter.refresh_change_for_work(&store, &cfg, &foreign, &w, &Some(e), NOW);
        assert!(
            matches!(result, Err(StoreError::ConstraintConflict { .. })),
            "a foreign repository cannot be re-attributed: {result:?}"
        );
        assert!(head_facts(&store, &w).is_empty(), "no fact was ingested");
    }

    #[test]
    fn a_poll_availability_cycle_at_one_head_recovers_then_fails_again() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-cycle");
        let change = change_ref("cr-cycle");
        let head = CommitRevision::new(config().host.repository.clone(), "head-9").unwrap();
        open_change_receipt(&store, &w, &change, "main");
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-cycle"), Ok(FakeGitHub::pull("head-9")));
        fake.reviews.insert(cfg_key("cr-cycle"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-cycle"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let review_availability = |store: &SqliteStore| {
            let detail = store
                .transact(|tx| projection::work_detail(&*tx, &w, NOW))
                .unwrap();
            let QueryResult::Found { sources, value, .. } = detail else {
                panic!("work detail missing: {detail:?}")
            };
            (
                sources
                    .iter()
                    .find(|s| s.source.starts_with("review"))
                    .map(|s| s.availability.clone()),
                value.current_round_id.is_some(),
            )
        };
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            review_availability(&store),
            (Some(projection::Availability::Known), true),
            "the first clean poll seals the complete round"
        );
        adapter.client_mut().reviews.insert(
            cfg_key("cr-cycle"),
            Err(ApiError::Unknown {
                reason: "outage".into(),
            }),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            review_facts(&store, &w).last().unwrap().1,
            Completeness::Unavailable {
                reason: "reviews read failed".into()
            },
            "the outage is the latest recorded review evidence"
        );
        assert_eq!(
            review_availability(&store),
            (Some(projection::Availability::Known), true),
            "the projection retains the prior sealed round through the outage window"
        );
        adapter
            .client_mut()
            .reviews
            .insert(cfg_key("cr-cycle"), Ok(vec![]));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            review_facts(&store, &w).last().unwrap().1,
            Completeness::Complete,
            "the recovered poll is re-ingested at the same head, not deduplicated away"
        );
        assert_eq!(
            review_availability(&store),
            (Some(projection::Availability::Known), true),
            "the projected review availability is known again"
        );
        adapter.client_mut().reviews.insert(
            cfg_key("cr-cycle"),
            Err(ApiError::Unknown {
                reason: "outage".into(),
            }),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            review_facts(&store, &w).last().unwrap().1,
            Completeness::Unavailable {
                reason: "reviews read failed".into()
            },
            "the second identical outage is recorded too"
        );
        assert_eq!(
            review_availability(&store),
            (Some(projection::Availability::Known), true),
            "the prior complete round stays projected through the second outage"
        );
        let review_observations = review_facts(&store, &w);
        assert_eq!(
            review_observations
                .iter()
                .map(|(_, c)| c.is_complete())
                .collect::<Vec<_>>(),
            vec![true, false, true, false],
            "every poll episode lands its own review fact"
        );
    }

    #[test]
    fn findings_changes_a_b_a_resurface_the_first_set() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-aba");
        let change = change_ref("cr-aba");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let finding = |id: &str| RemoteReviewFinding {
            finding_id: id.into(),
            review_id: "rv-1".into(),
            commit_sha: "head-1".into(),
            body: format!("body-{id}"),
        };
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-aba"), Ok(FakeGitHub::pull("head-1")));
        fake.reviews.insert(cfg_key("cr-aba"), Ok(vec![]));
        fake.findings
            .insert(cfg_key("cr-aba"), Ok(vec![finding("f-a")]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let round_sets = |store: &SqliteStore| {
            store
                .transact(|tx| {
                    let rounds = tx.rounds_for_work(&w)?;
                    let mut sets = Vec::new();
                    for round in &rounds {
                        sets.push(
                            tx.occurrences(&round.id)?
                                .into_iter()
                                .map(|o| o.source_key)
                                .collect::<Vec<_>>(),
                        );
                    }
                    Ok(sets)
                })
                .unwrap()
        };
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(round_sets(&store), vec![vec!["f-a".to_string()]]);
        adapter
            .client_mut()
            .findings
            .insert(cfg_key("cr-aba"), Ok(vec![finding("f-b")]));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            round_sets(&store),
            vec![vec!["f-a".to_string()], vec!["f-b".to_string()]]
        );
        adapter
            .client_mut()
            .findings
            .insert(cfg_key("cr-aba"), Ok(vec![finding("f-a")]));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let sets = round_sets(&store);
        assert_eq!(
            sets.last().unwrap(),
            &vec!["f-a".to_string()],
            "the return to the earlier finding set is re-ingested, not suppressed: {sets:?}"
        );
    }

    #[test]
    fn a_work_bound_to_a_different_adapter_is_never_read() {
        let store = SqliteStore::in_memory(2).unwrap();
        let mut drifted_cfg = config_with_issue_read();
        drifted_cfg.host.adapter = AdapterId::trusted("legacy-github");
        let d = handle_command(
            &store,
            &drifted_cfg,
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("cli".into()),
                idempotency_key: "k-drifted".into(),
                body: CommandBody::Submit {
                    intent: "intent-drifted".into(),
                    sources: vec![source(7, "rev-1", "d1")],
                    constraints: vec![],
                    verification_argv: vec![],
                },
            },
            NOW,
        )
        .unwrap();
        let CommandDecision::Accepted {
            work_id: Some(w),
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } = d
        else {
            panic!("submit drifted: {d:?}")
        };
        let mut fake = FakeGitHub::new();
        fake.issues.insert(
            "issue-7".into(),
            Ok(RemoteIssue {
                source_revision: "rev-1".into(),
                content_digest: "d1".into(),
                state: IssueState::Open,
                title: String::new(),
                body: String::new(),
            }),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(statuses(&report), vec!["completed"; 8]);
        assert!(
            issue_facts(&store, &w).is_empty(),
            "a foreign-adapter work is skipped, never read: {:?}",
            issue_facts(&store, &w)
        );
        let cfg = config();
        let change = change_ref("cr-drifted");
        let result = adapter.refresh_change_for_work(
            &store,
            &cfg,
            &change,
            &w,
            &Some(execution_id.clone()),
            NOW,
        );
        assert!(
            matches!(result, Err(StoreError::ConstraintConflict { .. })),
            "an explicit read of a drifted-adapter work is rejected: {result:?}"
        );
        assert!(head_facts(&store, &w).is_empty());
    }

    #[test]
    fn refresh_change_for_work_rejects_a_foreign_execution() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w1, _e1) = submit(&store, "k-exec-1");
        let (_w2, e2) = submit(&store, "k-exec-2");
        let mut adapter = production_adapter(FakeGitHub::new());
        let cfg = config();
        let change = change_ref("cr-exec");
        let result = adapter.refresh_change_for_work(&store, &cfg, &change, &w1, &Some(e2), NOW);
        assert!(
            matches!(result, Err(StoreError::ConstraintConflict { .. })),
            "an execution of another work is rejected: {result:?}"
        );
        assert!(head_facts(&store, &w1).is_empty());
    }

    #[test]
    fn the_same_change_id_in_another_repository_never_crosses() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-cross");
        let change = change_ref("cr-x");
        let head = CommitRevision::new(config().host.repository.clone(), "head-a").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let other_repo = RepositoryRef::new(
            config().host.repository.host.clone(),
            RepositoryId::trusted("other-repo"),
        );
        let foreign_change = ChangeRequestRef {
            repository: other_repo.clone(),
            change_request: ChangeRequestId::trusted("cr-x"),
            display_number: None,
        };
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: None,
                fact: ObservationFact::ChangeHead {
                    change: foreign_change,
                    head: CommitRevision::new(other_repo.clone(), "head-b").unwrap(),
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-x"), Ok(FakeGitHub::pull("head-a")));
        fake.pulls.insert(
            remote_key(&other_repo, "cr-x"),
            Ok(FakeGitHub::pull("head-b")),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let heads = head_facts(&store, &w);
        assert_eq!(
            heads.iter().filter(|h| h.as_str() == "head-a").count(),
            1,
            "the bound repository's read lands: {heads:?}"
        );
        assert_eq!(
            heads.iter().filter(|h| h.as_str() == "head-b").count(),
            1,
            "the foreign repository's observation stays its own evidence: {heads:?}"
        );
    }

    #[test]
    fn a_target_branch_move_updates_the_current_target_without_a_head_move() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-target");
        let change = change_ref("cr-target");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        open_change_receipt(&store, &w, &change, "main");
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-target"), Ok(FakeGitHub::pull("head-1")));
        fake.reviews.insert(cfg_key("cr-target"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-target"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let detail = |store: &SqliteStore| {
            let detail = store
                .transact(|tx| projection::work_detail(&*tx, &w, NOW))
                .unwrap();
            let QueryResult::Found { value, .. } = detail else {
                panic!("work detail missing: {detail:?}")
            };
            value
        };
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let first = detail(&store);
        assert!(
            first.current_round_id.is_some(),
            "the review fact binds the observed target revision: {first:?}"
        );
        adapter
            .client_mut()
            .branches
            .insert(cfg_key("main"), Ok(Some("tb-2".into())));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let second = detail(&store);
        assert_eq!(
            head_facts(&store, &w)
                .iter()
                .filter(|h| h.as_str() == "head-1")
                .count(),
            1,
            "the head never moved and the identical re-read is deduplicated"
        );
        assert_ne!(
            first.current_round_id, second.current_round_id,
            "a target-only move is a new diff target with its own round"
        );
        let targets: Vec<Option<String>> = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::Review { target, .. } => {
                    Some(target.target_revision.as_ref().map(|r| r.object.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            targets,
            vec![Some("tb-1".to_string()), Some("tb-2".to_string())],
            "each review binds the target revision it was read against"
        );
    }

    #[test]
    fn a_historical_review_is_never_rebound_onto_a_moved_target() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-pin");
        let change = change_ref("cr-pin");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        open_change_receipt(&store, &w, &change, "main");
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let review = |id: &str| RemoteReview {
            review_id: id.into(),
            commit_sha: "head-1".into(),
            author: "reviewer".into(),
            body: String::new(),
            changes_requested: false,
        };
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-pin"), Ok(FakeGitHub::pull("head-1")));
        fake.reviews
            .insert(cfg_key("cr-pin"), Ok(vec![review("rv-1")]));
        fake.findings.insert(cfg_key("cr-pin"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let detail = |store: &SqliteStore| {
            let detail = store
                .transact(|tx| projection::work_detail(&*tx, &w, NOW))
                .unwrap();
            let QueryResult::Found { value, sources, .. } = detail else {
                panic!("work detail missing: {detail:?}")
            };
            (value, sources)
        };
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let (first, _) = detail(&store);
        assert!(
            first.current_round_id.is_some(),
            "the clean review seals a round for the observed target"
        );
        adapter
            .client_mut()
            .branches
            .insert(cfg_key("main"), Ok(Some("tb-2".into())));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let (moved, moved_sources) = detail(&store);
        assert_eq!(
            moved.current_round_id, None,
            "the old review is not re-sealed for the moved target"
        );
        assert!(
            moved_sources
                .iter()
                .any(|s| s.source == "review"
                    && s.availability == projection::Availability::Unavailable),
            "the review surface reports unavailable for the unmanned target: {moved_sources:?}"
        );
        let rv1_targets: Vec<Option<String>> = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::Review {
                    target,
                    source_revision,
                    ..
                } if source_revision.as_deref() == Some("rv-1") => {
                    Some(target.target_revision.as_ref().map(|r| r.object.clone()))
                }
                _ => None,
            })
            .collect();
        assert!(
            rv1_targets.iter().all(|t| t.as_deref() == Some("tb-1")),
            "the historical review stays bound to the target it examined: {rv1_targets:?}"
        );
        adapter
            .client_mut()
            .reviews
            .insert(cfg_key("cr-pin"), Ok(vec![review("rv-1"), review("rv-2")]));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let (fresh, _) = detail(&store);
        assert!(
            fresh.current_round_id.is_some(),
            "the genuinely new review seals the current target"
        );
        let rv2_targets: Vec<Option<String>> = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::Review {
                    target,
                    source_revision,
                    ..
                } if source_revision.as_deref() == Some("rv-2") => {
                    Some(target.target_revision.as_ref().map(|r| r.object.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            rv2_targets,
            vec![Some("tb-2".to_string())],
            "the new review binds the target it was observed under"
        );
    }

    #[test]
    fn identical_polls_record_each_fact_once_per_state() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-dedup");
        let change = change_ref("cr-dedup");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-dedup"), Ok(FakeGitHub::pull("head-1")));
        fake.checks.insert(
            cfg_key("head-1"),
            Ok(vec![RemoteCheckRun {
                check_run_id: "run-1".into(),
                name: "ci".into(),
                head_sha: "head-1".into(),
                conclusion: CheckConclusion::Success,
                producer: "app-ci".into(),
            }]),
        );
        fake.reviews.insert(cfg_key("cr-dedup"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-dedup"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        // A merge-detection check is service bookkeeping, not evidence:
        // its durable record is the pair's service cursor on the usage
        // row, overwritten in place — never an observation per pass.
        let count = |store: &SqliteStore| {
            store
                .transact(|tx| tx.observations_for_work(&w))
                .unwrap()
                .len()
        };
        let service_position = |store: &SqliteStore| {
            store
                .transact(|tx| tx.work_usage(&w))
                .unwrap()
                .and_then(|usage| usage.merge_check_position(&change))
        };
        let first = count(&store);
        let first_service = service_position(&store);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            count(&store),
            first,
            "identical polls record no duplicate facts"
        );
        let service = service_position(&store);
        assert!(
            service.is_some() && service > first_service,
            "each pass advances the pair's service cursor"
        );
        let rounds = store.transact(|tx| tx.rounds_for_work(&w)).unwrap().len();
        assert_eq!(rounds, 1, "identical polls seal no extra round");
        adapter.client_mut().checks.insert(
            cfg_key("head-1"),
            Ok(vec![RemoteCheckRun {
                check_run_id: "run-1".into(),
                name: "ci".into(),
                head_sha: "head-1".into(),
                conclusion: CheckConclusion::Failure,
                producer: "app-ci".into(),
            }]),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            count(&store) > first,
            "a changed conclusion is new evidence"
        );
    }
    #[test]
    fn the_source_refresh_report_counts_each_change_subject_exactly_once() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-report");
        let change = change_ref("cr-report");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-report"), Ok(FakeGitHub::pull("head-1")));
        fake.checks.insert(
            cfg_key("head-1"),
            Ok(vec![RemoteCheckRun {
                check_run_id: "run-1".into(),
                name: "ci".into(),
                head_sha: "head-1".into(),
                conclusion: CheckConclusion::Success,
                producer: "app-ci".into(),
            }]),
        );
        fake.reviews.insert(cfg_key("cr-report"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-report"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let cfg = config();
        let first = adapter
            .refresh_sources(&store, &cfg, &mut || NOW, None, None)
            .unwrap();
        assert_eq!(
            (first.ingested, first.unchanged, first.deferred.len()),
            (1, 0, 0),
            "the first poll of one change subject is one ingested subject"
        );
        let repeat = adapter
            .refresh_sources(&store, &cfg, &mut || NOW, None, None)
            .unwrap();
        assert_eq!(
            (repeat.ingested, repeat.unchanged),
            (0, 1),
            "an identical repeat is one unchanged subject"
        );
        adapter.client_mut().checks.insert(
            cfg_key("head-1"),
            Ok(vec![RemoteCheckRun {
                check_run_id: "run-1".into(),
                name: "ci".into(),
                head_sha: "head-1".into(),
                conclusion: CheckConclusion::Failure,
                producer: "app-ci".into(),
            }]),
        );
        let changed = adapter
            .refresh_sources(&store, &cfg, &mut || NOW, None, None)
            .unwrap();
        assert_eq!(
            (changed.ingested, changed.unchanged),
            (1, 0),
            "a check-only transition still counts as an ingested subject"
        );
        adapter.client_mut().reviews.insert(
            cfg_key("cr-report"),
            Err(ApiError::Unknown {
                reason: "outage".into(),
            }),
        );
        let outage = adapter
            .refresh_sources(&store, &cfg, &mut || NOW, None, None)
            .unwrap();
        assert_eq!(
            (outage.ingested, outage.unchanged),
            (1, 0),
            "unavailable evidence is still a new observation for the subject"
        );
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            report.phases[0].processed, 1,
            "phase 1 reports the processed change subject"
        );
    }

    #[test]
    fn a_webhook_partial_review_pins_the_first_concrete_target() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-whpin");
        let change = change_ref("cr-whpin");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        open_change_receipt(&store, &w, &change, "main");
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head.clone(),
            },
            HostActionStatus::Applied,
        );
        let review = |id: &str| RemoteReview {
            review_id: id.into(),
            commit_sha: "head-1".into(),
            author: "reviewer".into(),
            body: String::new(),
            changes_requested: false,
        };
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-whpin"), Ok(FakeGitHub::pull("head-1")));
        fake.reviews
            .insert(cfg_key("cr-whpin"), Ok(vec![review("rv-1")]));
        fake.findings.insert(cfg_key("cr-whpin"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let cfg = config();
        let delivery = workloop_github::adapter::WebhookDelivery {
            event_id: "guid-rv1".into(),
            emitted_at: NOW,
            fact: workloop_github::adapter::WebhookFact::Review {
                change: change.clone(),
                head: head.clone(),
                review_id: "rv-1".into(),
                source: "reviewer".into(),
            },
        };
        let receipt = adapter
            .deliver_webhook(&store, &cfg, &delivery, NOW)
            .unwrap();
        assert_eq!(
            receipt,
            workloop_core::work::IngestReceipt::Recorded,
            "the webhook records its partial, target-less review"
        );
        let detail = |store: &SqliteStore| {
            let detail = store
                .transact(|tx| projection::work_detail(&*tx, &w, NOW))
                .unwrap();
            let QueryResult::Found { value, .. } = detail else {
                panic!("work detail missing: {detail:?}")
            };
            value
        };
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            detail(&store).current_round_id.is_some(),
            "the first concrete target seals the review seen earlier as partial"
        );
        let dup = adapter
            .deliver_webhook(&store, &cfg, &delivery, NOW)
            .unwrap();
        assert_eq!(
            dup,
            workloop_core::work::IngestReceipt::Duplicate,
            "the same delivery GUID stays idempotent"
        );
        adapter
            .client_mut()
            .branches
            .insert(cfg_key("main"), Ok(Some("tb-2".into())));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            detail(&store).current_round_id,
            None,
            "the pinned review is not rebound to the moved target"
        );
        adapter.client_mut().reviews.insert(
            cfg_key("cr-whpin"),
            Ok(vec![review("rv-1"), review("rv-2")]),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            detail(&store).current_round_id.is_some(),
            "a genuinely new review under the moved target seals the current round"
        );
    }

    #[test]
    fn the_production_adapter_declares_issue_read_for_source_bound_work() {
        // A config derived from the adapter's own declared set — the
        // shape contract and live tests build — must admit a
        // source-bound Submit. The driver reads declared issues, so a
        // missing `issue_read` rejects that Work as
        // unsupported_capability even though the read exists.
        let store = SqliteStore::in_memory(2).unwrap();
        let mut cfg = config_with_issue_read();
        cfg.declared_capabilities = GitHubAdapter::<FakeGitHub>::declared_capabilities()
            .iter()
            .map(|name| HostCapability::from_name(name).expect("contract capability name"))
            .collect();
        let d = handle_command(
            &store,
            &cfg,
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("cli".into()),
                idempotency_key: "k-caps".into(),
                body: CommandBody::Submit {
                    intent: "intent-caps".into(),
                    sources: vec![source(7, "rev-1", "d1")],
                    constraints: vec![],
                    verification_argv: vec![],
                },
            },
            NOW,
        )
        .unwrap();
        assert!(
            matches!(d, CommandDecision::Accepted { .. }),
            "a source-bound submit under the adapter's declared set is admitted: {d:?}"
        );
    }

    #[test]
    fn a_source_on_an_unserved_host_records_unavailable_and_is_never_read() {
        let store = SqliteStore::in_memory(2).unwrap();
        let foreign_source = SourceSnapshot {
            issue: IssueRef {
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("ghe.corp.example"),
                    RepositoryId::trusted("other/repo"),
                ),
                issue: IssueId::trusted("issue-9"),
                display_number: Some(9),
            },
            source_revision: "rev-f".into(),
            content_digest: "digest-f".into(),
        };
        let (w, _e) = submit_with_sources(&store, "k-foreign-src", vec![foreign_source]);
        let mut fake = FakeGitHub::new();
        // The answer the served host would return if the read were
        // attempted: recording it would bind the foreign source to the
        // wrong remote's state.
        fake.issues.insert(
            "issue-9".into(),
            Ok(RemoteIssue {
                source_revision: "rev-wrong-host".into(),
                content_digest: "d-wrong".into(),
                state: IssueState::Open,
                title: String::new(),
                body: String::new(),
            }),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            issue_facts(&store, &w).last().unwrap(),
            &(String::new(), String::new(), IssueState::Unavailable),
            "an unserved host is unavailable evidence, never the served host's answer"
        );
    }

    #[test]
    fn a_finished_works_change_is_never_polled_again() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-finished");
        let change = change_ref("cr-done");
        let head = CommitRevision::new(config().host.repository.clone(), "head-1").unwrap();
        insert_outbox_intent(
            &store,
            &w,
            HostAction::MarkReady {
                change: change.clone(),
                expected_head: head,
            },
            HostActionStatus::Applied,
        );
        // The Execution reaches a terminal state: the Work's change is
        // history and phase 1 owes it no further reads — the same
        // active-Execution scope `declared_sources` applies to issues.
        store
            .transact(|tx| {
                let mut execution = tx.execution(&e)?.unwrap();
                let expected = execution.state.clone();
                execution.state = ExecutionState::Succeeded;
                tx.apply(&Effect::UpdateExecution {
                    execution,
                    expected_state: expected,
                })
            })
            .unwrap();
        let mut fake = FakeGitHub::new();
        fake.pulls
            .insert(cfg_key("cr-done"), Ok(FakeGitHub::pull("head-1")));
        fake.reviews.insert(cfg_key("cr-done"), Ok(vec![]));
        fake.findings.insert(cfg_key("cr-done"), Ok(vec![]));
        fake.branches
            .insert(cfg_key("main"), Ok(Some("tb-1".into())));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(statuses(&report), vec!["completed"; 8]);
        assert!(
            head_facts(&store, &w).is_empty(),
            "a finished Work's change is never re-read: {:?}",
            head_facts(&store, &w)
        );
    }

    fn intent_status(store: &SqliteStore, action_id: &str) -> HostActionStatus {
        store
            .transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| i.action_id == action_id)
                    .map(|i| i.status))
            })
            .unwrap()
            .unwrap()
    }

    /// Every `host_action_result` receipt recorded for the Work, in
    /// observation order.
    fn action_receipts(
        store: &SqliteStore,
        work_id: &WorkId,
    ) -> Vec<(String, HostActionStatus, Vec<NormalizedEffectFact>)> {
        store
            .transact(|tx| tx.observations_for_work(work_id))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::HostActionResult {
                    action_id,
                    status,
                    effects,
                    ..
                } => Some((action_id.clone(), status.clone(), effects.clone())),
                _ => None,
            })
            .collect()
    }

    /// Push lands and the follow-up create is refused: each stage keeps
    /// its own receipt — the push `applied`, the create `failed` — and
    /// the terminal failure is never retried into a duplicate effect.
    #[test]
    fn a_production_push_and_a_failed_create_keep_separate_receipts() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-prod-separate");
        let mut fake = FakeGitHub::new();
        fake.branches.insert(cfg_key("work/prod"), Ok(None));
        fake.arm(
            "open_pull",
            ArmedFault::Refuse(ApiError::Refused {
                reason: "validation failed".into(),
            }),
        );
        let push_id = insert_outbox_intent(
            &store,
            &w,
            push_intent("work/prod", "prod-head-1"),
            HostActionStatus::Pending,
        );
        let open_id = insert_outbox_intent(
            &store,
            &w,
            HostAction::OpenChangeRequest {
                source_branch: "work/prod".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
            HostActionStatus::Pending,
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);

        assert_eq!(
            intent_status(&store, &push_id),
            HostActionStatus::Applied,
            "the push receipt lands applied"
        );
        assert!(
            matches!(
                intent_status(&store, &open_id),
                HostActionStatus::Failed { .. }
            ),
            "the refused create is a definite failure, never unknown: {:?}",
            intent_status(&store, &open_id)
        );
        let receipts = action_receipts(&store, &w);
        assert_eq!(receipts.len(), 2, "one receipt per stage: {receipts:?}");
        assert!(receipts.iter().any(|(id, status, effects)| {
            id == &push_id
                && *status == HostActionStatus::Applied
                && effects.iter().any(|e| {
                    matches!(e, NormalizedEffectFact::RefPublished { branch, .. }
                        if branch == "work/prod")
                })
        }));
        assert!(receipts.iter().any(|(id, status, _)| {
            id == &open_id && matches!(status, HostActionStatus::Failed { .. })
        }));
        assert_eq!(
            adapter
                .client()
                .branch_head(&config().host.repository, "work/prod")
                .as_deref(),
            Some("prod-head-1"),
            "the remote branch carries the produced revision"
        );
        assert!(
            !adapter.client().pulls.values().any(|p| p.is_ok()),
            "the refused create left no change request"
        );

        // The failed stage is terminal: the next pass replays nothing.
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(action_receipts(&store, &w).len(), 2);
        assert!(
            !adapter.client().pulls.values().any(|p| p.is_ok()),
            "no blind retry opened a change request"
        );
    }

    /// A create whose response was lost lands remotely anyway: after a
    /// restart the read-only reconcile pass finds the recorded
    /// revision's matching change and settles `applied` — the original
    /// action id, payload, and actor are never re-issued.
    #[test]
    fn an_unknown_create_reconciles_from_the_remote_after_restart() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-prod-unknown");
        // The push already settled: it is the recorded source revision
        // the open's read-back matches against.
        insert_outbox_intent(
            &store,
            &w,
            push_intent("work/prod", "prod-head-1"),
            HostActionStatus::Applied,
        );
        let mut fake = FakeGitHub::new();
        fake.seed_branch_head(&config().host.repository, "work/prod", "prod-head-1");
        let open_id = insert_outbox_intent(
            &store,
            &w,
            HostAction::OpenChangeRequest {
                source_branch: "work/prod".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
            HostActionStatus::Pending,
        );
        // The create lands but its response is lost: `unknown`, and the
        // remote now holds exactly one matching change request.
        fake.arm(
            "open_pull",
            ArmedFault::Lose(ApiError::Unknown {
                reason: "response lost".into(),
            }),
        );
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            matches!(
                intent_status(&store, &open_id),
                HostActionStatus::Unknown { .. }
            ),
            "the lost response is unknown, never failed: {:?}",
            intent_status(&store, &open_id)
        );
        assert_eq!(adapter.client().pull_seq, 1, "one create attempt landed");

        // Restart: a fresh adapter over the same remote state. The
        // reconcile pass — not a re-issued write — settles the intent.
        let remote = adapter.client().clone();
        let mut adapter = production_adapter(remote);
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            intent_status(&store, &open_id),
            HostActionStatus::Applied,
            "the read-back adopts the landed change under the original action id"
        );
        assert_eq!(
            adapter.client().pull_seq,
            1,
            "reconciliation never issues a second create"
        );
        let opened = action_receipts(&store, &w)
            .into_iter()
            .flat_map(|(_, _, effects)| effects)
            .find_map(|e| match e {
                NormalizedEffectFact::ChangeOpened { change, .. } => Some(change),
                _ => None,
            })
            .expect("the settled receipt carries the opened change");
        assert_eq!(opened.change_request.as_str(), "pr-1");
    }

    /// The remote head moved past the recorded prior: the guarded
    /// write refuses `stale_head`, the receipt is `failed`, and the
    /// observed remote position is recorded before the refusal so a
    /// later re-issue derives its guard from fresh evidence.
    #[test]
    fn a_stale_expected_head_refuses_the_push_and_records_the_remote_position() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, _e) = submit(&store, "k-prod-stale");
        let prior = CommitRevision::new(config().host.repository.clone(), "old-head").unwrap();
        let push_id = insert_outbox_intent(
            &store,
            &w,
            HostAction::PushRef {
                branch: "work/stale".into(),
                produced: CommitRevision::new(config().host.repository.clone(), "prod-head-2")
                    .unwrap(),
                expected_prior: Some(prior),
            },
            HostActionStatus::Pending,
        );
        let mut fake = FakeGitHub::new();
        fake.seed_branch_head(&config().host.repository, "work/stale", "new-head");
        let mut adapter = production_adapter(fake);
        let settings = settings();
        run_production(&store, &settings, &mut adapter, &mut || NOW);

        assert!(
            matches!(
                intent_status(&store, &push_id),
                HostActionStatus::Failed { ref reason } if reason.contains("stale_head")
            ),
            "the stale guard is a definite refusal: {:?}",
            intent_status(&store, &push_id)
        );
        let branch_states: Vec<BranchState> = store
            .transact(|tx| tx.observations_for_work(&w))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::BranchState { state, .. } => Some(state.clone()),
                _ => None,
            })
            .collect();
        assert!(
            branch_states.iter().any(|s| matches!(
                s,
                BranchState::At { revision } if revision.object == "new-head"
            )),
            "the fresh remote position is recorded before the refusal: {branch_states:?}"
        );
        assert_eq!(
            adapter
                .client()
                .branch_head(&config().host.repository, "work/stale")
                .as_deref(),
            Some("new-head"),
            "the refused write never moved the remote"
        );
    }

    /// Permission drift has two honest shapes: a revoked execution
    /// grant skips the drain without a receipt or a spent attempt, and
    /// a remote refusal is a definite `failed` — never `unknown`,
    /// never retried blind.
    #[test]
    fn permission_drift_skips_or_fails_never_retries_blind() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-prod-drift");
        let push_id = insert_outbox_intent(
            &store,
            &w,
            push_intent("work/drift", "prod-head-3"),
            HostActionStatus::Pending,
        );
        // The execution's host_write grant is revoked after admission.
        store
            .transact(|tx| {
                let mut execution = tx.execution(&e)?.unwrap();
                let expected = execution.state.clone();
                execution.allowed_effects.host_write = false;
                tx.apply(&Effect::UpdateExecution {
                    execution,
                    expected_state: expected,
                })
            })
            .unwrap();
        let mut fake = FakeGitHub::new();
        fake.branches.insert(cfg_key("work/drift"), Ok(None));
        let mut adapter = production_adapter(fake);
        let settings = settings();
        let report = run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(
            intent_status(&store, &push_id),
            HostActionStatus::Pending,
            "a denied grant leaves the intent pending"
        );
        assert!(
            action_receipts(&store, &w).is_empty(),
            "no receipt is recorded for an attempt that never reached the host"
        );
        assert_eq!(
            adapter
                .client()
                .branch_head(&config().host.repository, "work/drift"),
            None,
            "the remote was never touched"
        );
        let _ = report;

        // Restore the grant; the remote now refuses the write — a
        // definite failure recorded once, terminal thereafter.
        store
            .transact(|tx| {
                let mut execution = tx.execution(&e)?.unwrap();
                let expected = execution.state.clone();
                execution.allowed_effects.host_write = true;
                tx.apply(&Effect::UpdateExecution {
                    execution,
                    expected_state: expected,
                })
            })
            .unwrap();
        adapter.client_mut().arm(
            "write_ref",
            ArmedFault::Refuse(ApiError::Refused {
                reason: "permission denied".into(),
            }),
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            matches!(
                intent_status(&store, &push_id),
                HostActionStatus::Failed { ref reason } if reason == "permission denied"
            ),
            "the remote refusal is a definite failure: {:?}",
            intent_status(&store, &push_id)
        );
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert_eq!(action_receipts(&store, &w).len(), 1);
    }

    /// A terminal settle waits for the outstanding receipt: with the
    /// writer proven gone and a cancel recorded, the Execution stays
    /// `stopping` — no Outcome — while the intent is `unknown`. The
    /// reconcile pass's `failed` receipt is what lets the settle
    /// record the cancelled Outcome.
    #[test]
    fn a_terminal_settle_waits_for_the_outstanding_receipt() {
        let store = SqliteStore::in_memory(2).unwrap();
        let (w, e) = submit(&store, "k-prod-settle");
        let settings = settings();
        // A writer exists so its proven exit is the stop proof the
        // settle requires.
        let mut adapter = production_adapter(FakeGitHub::new());
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        let request = store
            .transact(|tx| {
                Ok(tx
                    .pending_requests()?
                    .into_iter()
                    .next()
                    .expect("one request"))
            })
            .unwrap();
        let invocation_id = store
            .transact(|tx| {
                let inv_id = tx.alloc_id("inv")?;
                tx.apply(&Effect::InsertInvocation {
                    invocation: Invocation {
                        id: InvocationId(inv_id.clone()),
                        request_id: request.request_id.clone(),
                        execution_id: e.clone(),
                        role: InvocationRole::Coding,
                        state: InvocationState::Prepared,
                        process_identity: Some(ProcessIdentity {
                            pid: 4242,
                            started_at: "boot".into(),
                            pgid: None,
                        }),
                        descendants: vec![],
                        required_artifacts: vec![],
                        evidence_refs: vec![],
                    },
                })?;
                Ok(InvocationId(inv_id))
            })
            .unwrap();
        // The push's response was lost: outstanding until reconciled.
        let push_id = insert_outbox_intent(
            &store,
            &w,
            push_intent("work/settle", "prod-head-4"),
            HostActionStatus::Unknown {
                reason: "response lost".into(),
            },
        );
        // Cancel the Work, then prove the writer gone.
        let version = store
            .transact(|tx| Ok(tx.work(&w)?.unwrap().version))
            .unwrap();
        handle_command(
            &store,
            &config(),
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("cli".into()),
                idempotency_key: "k-prod-settle-cancel".into(),
                body: CommandBody::Cancel {
                    work_id: w.clone(),
                    expected_work_version: version,
                    execution_id: Some(e.clone()),
                },
            },
            NOW,
        )
        .unwrap();
        assert_eq!(execution(&store, &e).state, ExecutionState::Stopping);
        workloop_core::work::handle_observation(
            &store,
            &config(),
            &Observation {
                delivery: None,
                subject_work_id: Some(w.clone()),
                subject_execution_id: Some(e.clone()),
                fact: ObservationFact::ProcessState {
                    invocation_id,
                    process_identity: "4242@boot".into(),
                    state: ProcessStateFact::Exited,
                    exit_code: Some(0),
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
        let settled = execution(&store, &e);
        assert_eq!(
            settled.state,
            ExecutionState::Stopping,
            "the outstanding intent blocks the terminal settle"
        );
        assert!(settled.outcome.is_none(), "no Outcome before the receipt");

        // The remote branch is definitively absent: reconcile records
        // `failed`, and that receipt re-drives the deferred settle.
        adapter
            .client_mut()
            .branches
            .insert(cfg_key("work/settle"), Ok(None));
        run_production(&store, &settings, &mut adapter, &mut || NOW);
        assert!(
            matches!(
                intent_status(&store, &push_id),
                HostActionStatus::Failed { .. }
            ),
            "definitive absence settles the unknown intent failed"
        );
        let settled = execution(&store, &e);
        assert_eq!(settled.state, ExecutionState::Cancelled);
        assert_eq!(
            settled.outcome.map(|o| o.state),
            Some(ExecutionState::Cancelled),
            "the Outcome lands only after the receipt and the stop proof"
        );
    }
}
