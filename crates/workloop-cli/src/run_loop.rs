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
use workloop_core::identity::AdapterId;
use workloop_core::ports::ProcessControlPort;
use workloop_core::projection::{self, QueryResult};
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::work::{
    WorkloopConfig, consume_due_recovery_intents, dispatch_scan, sweep_scoped,
};
use workloop_exec::adapter;
use workloop_exec::isolation;
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::retention::{SweepParams, SweepReport};
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

/// Everything one pass drives. The supervisor and host adapter are
/// owned by the caller so their state persists across ticks of one run.
pub struct Pass<'a> {
    pub store: &'a SqliteStore,
    pub config: &'a WorkloopConfig,
    pub settings: &'a PassSettings,
    pub supervisor: Option<&'a Supervisor<PosixProcessControl>>,
    pub host: &'a mut FixtureGithub,
    /// The installation state root: the one boundary every reclaimable
    /// body (CAS, workspace, out directory, handoff object) lives under.
    pub state_root: &'a Path,
}

/// Run exactly one bounded pass over the eight ordered phases.
/// `clock` is supplied by the caller so tests can script deadlines; in
/// production it reads the wall clock. `shutdown` is checked at every
/// phase boundary: when it reports true the current phase finishes and
/// the remaining phases are reported `shutdown`, never started.
pub fn run_pass(
    pass: &mut Pass<'_>,
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

    // The in-process fixture owns the host legs only when the
    // installation binds it; any other adapter identity names an
    // out-of-process adapter (e.g. the S1 bridge) that performs host
    // reads and effects against the same store. Running the fixture
    // then would fabricate `unavailable` source reads and applied
    // receipts for real pending intents.
    let in_process_fixture = pass.config.host.adapter == AdapterId::trusted("fixture");

    // Phase 1 — source refresh (host adapter, read only).
    if gated!(0) {
        if !in_process_fixture {
            push!(PhaseReport::skipped(PHASES[0], "external_adapter"));
        } else {
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
                    p
                }
                Err(e) => PhaseReport::failed(PHASES[0], e),
            });
        }
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
                    // The probe spawns inside the workspace the consume
                    // path would create before launch; a fresh Work's
                    // `ws-<work>` does not exist yet, so it is created
                    // here first — a refused creation fails the phase
                    // instead of silently skipping the binding.
                    std::fs::create_dir_all(&workspace).map_err(|e| StoreError::Internal {
                        detail: format!("create workspace {}: {e}", workspace.display()),
                    })?;
                    // The probe runs under the profile's environment —
                    // a prep failure skips the binding exactly like an
                    // unanswered describe: the consume path's
                    // fail-closed selection refuses the launch. The
                    // shared prebind scratch is reclaimed as soon as
                    // the probe process is gone, so a fixed key never
                    // accumulates storage between passes.
                    let prebind_scratch =
                        isolation::scratch_root(&supervisor.workspace_root).join("probe-prebind");
                    let Ok(env) =
                        isolation::prepare(&supervisor.isolation_profile, &prebind_scratch)
                    else {
                        continue;
                    };
                    let declared = adapter::observe_worker_declaration(
                        &supervisor.port,
                        &supervisor.worker_argv,
                        &workspace,
                        &env,
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
    // past the deadline).
    if gated!(3) {
        if !in_process_fixture {
            push!(PhaseReport::skipped(PHASES[3], "external_adapter"));
        } else {
            let result = pass.host.apply_outbox_bounded(
                pass.store,
                pass.config,
                clock,
                Some(deadline),
                scope,
                OUTBOX_ITEMS_PER_TICK,
            );
            push!(match result {
                Ok(r) => {
                    let mut p = PhaseReport::completed(PHASES[3]);
                    p.processed = r.attempted;
                    p.deferred = r.deferred;
                    p.reason = Some(format!(
                        "applied={} failed={} unknown={} unsupported={}",
                        r.applied, r.failed, r.unknown, r.unsupported
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
                        "stops_recorded={} terminal_failures={}",
                        r.stops_recorded, r.terminal_failures
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

    let deadline_exceeded = phases
        .iter()
        .any(|p| p.status == "deferred" && p.reason.as_deref() == Some("tick.deadline"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use workloop_core::command::*;
    use workloop_core::domain::*;
    use workloop_core::identity::*;
    use workloop_core::observation::{HostActionStatus, IssueState};
    use workloop_core::store::{Effect, HostAction, HostActionIntent};
    use workloop_core::work::handle_command;

    const NOW: i64 = 1_700_000_000;
    const TICK: u64 = 60;

    fn config() -> WorkloopConfig {
        WorkloopConfig {
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

    fn supervisor(mode: &str, root: std::path::PathBuf) -> Supervisor<PosixProcessControl> {
        Supervisor::new(
            PosixProcessControl,
            root,
            vec![fixture_bin(), mode.to_string()],
            config(),
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
        run_pass(
            &mut Pass {
                store,
                config: &config(),
                settings,
                supervisor,
                host,
                state_root: std::path::Path::new("."),
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
        run_pass(
            &mut Pass {
                store,
                config: &config(),
                settings,
                supervisor: None,
                host: &mut host,
                state_root: std::path::Path::new("."),
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
        let report = run_pass(
            &mut Pass {
                store: &store,
                config: &config(),
                settings: &settings,
                supervisor: None,
                host: &mut host,
                state_root: std::path::Path::new("."),
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
        assert_eq!(invocations.len(), 1, "one Invocation per request");
        assert!(
            invocations[0].process_identity.is_some(),
            "the replayed request produced exactly one writer"
        );
        let execution = execution(&store, &e);
        assert_eq!(execution.invocations_used, 1);
        assert!(execution.started_seq.is_some());
        let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
        assert_eq!(
            usage.first_dispatch_unix_seconds,
            Some(NOW),
            "the budget anchor is the first dispatch, never re-armed by restart"
        );
        assert!(
            store
                .transact(|tx| tx.pending_requests())
                .unwrap()
                .is_empty(),
            "the consumed request never launches twice"
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
}
