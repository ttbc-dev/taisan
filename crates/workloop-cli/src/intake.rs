//! The CLI's intake surface: parameter resolution shared by the
//! one-shot `intake` command and the `run` driver's scheduler, plus
//! the nonblocking driver that ticks the 60-second settings check and
//! launches due invocations on their own thread.
//!
//! The profile is re-read on every settings-check tick — never cached
//! across ticks — so a configuration reload takes effect at the next
//! tick boundary and an invalid reload suppresses intake while the
//! last valid state keeps governing. A missing profile is "intake not
//! configured", not an error: installations without the TOML profile
//! simply never schedule intake.

use std::path::{Path, PathBuf};
use std::time::Duration;

use workloop_config::profile::{
    IntakeSelectorBinding, IntakeSettings, Profile, load_profile, resolve_paths_from_env,
};
use workloop_core::domain::Actor;
use workloop_core::identity::RepositoryRef;
use workloop_core::store::{IntakeTrigger, Store};
use workloop_core::work::WorkloopConfig;
use workloop_intake::scheduler::{IntakeScheduler, SETTINGS_TICK_SECONDS, TickDecision};
use workloop_intake::{self, IntakeError, IntakeParams, IntakeReport, SelectorBinding};
use workloop_store::sqlite::SqliteStore;

/// The resolved intake configuration for one settings-check tick:
/// the validated settings snapshot plus, when a complete selector
/// binding exists, everything an invocation needs. `selector` is
/// `None` only for a disabled intake — validation already guarantees
/// `enabled = true` implies a binding.
#[derive(Debug, Clone)]
pub struct IntakeConfig {
    pub settings: IntakeSettings,
    pub selector: Option<SelectorConfig>,
    /// The profile-derived request scope — every repository the
    /// installation can read source issues from.
    pub repositories: Vec<RepositoryRef>,
}

/// A complete `[intake.selector]` binding resolved for invocation.
#[derive(Debug, Clone)]
pub struct SelectorConfig {
    pub binding: SelectorBinding,
    pub task_constraints: Vec<String>,
    pub verification_argv: Vec<String>,
    pub required_permissions: Vec<String>,
}

/// What the profile resolution produced for one tick.
pub enum ProfileState {
    /// No profile file exists at the resolved path — intake is simply
    /// not configured on this installation.
    Absent,
    Loaded(Box<Profile>),
    /// The profile exists but failed to load or validate; the message
    /// is the bounded loader diagnostic (values already redacted).
    Failed(String),
}

/// Resolve the TOML profile path through the fixed precedence and
/// classify what is there. Never mutates anything.
pub fn profile_state(config_flag: Option<&Path>, state_flag: Option<&Path>) -> ProfileState {
    let paths = match resolve_paths_from_env(config_flag, state_flag) {
        Ok(p) => p,
        Err(e) => return ProfileState::Failed(e),
    };
    if !paths.config_path.exists() {
        return ProfileState::Absent;
    }
    match load_profile(&paths.config_path) {
        Ok(profile) => ProfileState::Loaded(Box::new(profile)),
        Err(e) => ProfileState::Failed(e),
    }
}

/// The intake configuration for this tick, or `None` when the profile
/// is absent or `[intake]`/`source_repositories` fails validation —
/// every one of those suppresses intake rather than guessing. The
/// `Err` carries the bounded loader diagnostic for status reporting.
pub fn resolve_intake(
    config_flag: Option<&Path>,
    state_flag: Option<&Path>,
) -> Result<Option<IntakeConfig>, String> {
    let profile = match profile_state(config_flag, state_flag) {
        ProfileState::Absent => return Ok(None),
        ProfileState::Failed(e) => return Err(e),
        ProfileState::Loaded(p) => *p,
    };
    let settings = profile.intake_settings()?;
    let selector = settings
        .selector
        .as_ref()
        .map(selector_config)
        .transpose()?;
    let repositories = profile.source_repositories()?;
    Ok(Some(IntakeConfig {
        settings,
        selector,
        repositories,
    }))
}

fn selector_config(binding: &IntakeSelectorBinding) -> Result<SelectorConfig, String> {
    Ok(SelectorConfig {
        binding: SelectorBinding {
            adapter: binding.adapter.clone(),
            selector_version: binding.selector_version,
            policy_revision: binding.policy_revision,
            argv: binding.argv.clone(),
        },
        task_constraints: binding.task_constraints.clone(),
        verification_argv: binding.verification_argv.clone(),
        required_permissions: binding.required_permissions.clone(),
    })
}

/// Everything one spawned invocation owns — moved into the thread so
/// the driver never shares borrows across the pass loop.
struct OwnedInvocation {
    db_path: PathBuf,
    capacity: u32,
    trigger: IntakeTrigger,
    binding: SelectorBinding,
    task_constraints: Vec<String>,
    verification_argv: Vec<String>,
    required_permissions: Vec<String>,
    actor: Actor,
    work_config: WorkloopConfig,
    repositories: Vec<RepositoryRef>,
    settings_revision: String,
    cwd: PathBuf,
    timeout: Duration,
}

/// The bounded outcome a finished invocation thread reports back —
/// plain owned data, safe to cross the join boundary.
#[derive(Debug)]
pub struct InvocationOutcome {
    pub run_id: Option<String>,
    pub state: Option<String>,
    pub detail_code: Option<String>,
    pub launched: bool,
    pub accepted: usize,
    pub replayed: usize,
    pub rejected: usize,
    pub capacity_deferred: usize,
    pub unknown: usize,
    pub interrupted: usize,
    pub error: Option<String>,
}

impl InvocationOutcome {
    /// The one-line stderr diagnostic a finished invocation reports.
    fn summary(&self) -> String {
        match (&self.run_id, &self.state, &self.detail_code, &self.error) {
            (Some(run_id), Some(state), detail, None) => format!(
                "run {run_id}: {state}{} (accepted {}, replayed {}, rejected {}, \
                 capacity_deferred {}, unknown {}, interrupted {}; launched {})",
                detail
                    .as_ref()
                    .map(|d| format!(", detail {d}"))
                    .unwrap_or_default(),
                self.accepted,
                self.replayed,
                self.rejected,
                self.capacity_deferred,
                self.unknown,
                self.interrupted,
                self.launched,
            ),
            (_, _, _, Some(error)) => format!("failed before a run record: {error}"),
            _ => "finished".to_string(),
        }
    }

    fn from_report(report: IntakeReport) -> Self {
        let mut out = Self {
            run_id: Some(report.run_id),
            state: Some(format!("{:?}", report.state).to_lowercase()),
            detail_code: report.detail_code,
            launched: report.launched,
            accepted: 0,
            replayed: 0,
            rejected: 0,
            capacity_deferred: 0,
            unknown: 0,
            interrupted: 0,
            error: None,
        };
        for c in &report.candidates {
            match c.status {
                workloop_core::store::IntakeCandidateStatus::Accepted => out.accepted += 1,
                workloop_core::store::IntakeCandidateStatus::Replayed => out.replayed += 1,
                workloop_core::store::IntakeCandidateStatus::Rejected => out.rejected += 1,
                workloop_core::store::IntakeCandidateStatus::CapacityDeferred => {
                    out.capacity_deferred += 1
                }
                workloop_core::store::IntakeCandidateStatus::Unknown => out.unknown += 1,
                workloop_core::store::IntakeCandidateStatus::Interrupted => out.interrupted += 1,
                _ => {}
            }
        }
        out
    }

    fn from_error(error: &IntakeError) -> Self {
        Self {
            run_id: None,
            state: None,
            detail_code: None,
            launched: false,
            accepted: 0,
            replayed: 0,
            rejected: 0,
            capacity_deferred: 0,
            unknown: 0,
            interrupted: 0,
            error: Some(format!("{error:?}")),
        }
    }
}

/// The `run` driver's intake half: a process-local scheduler ticked on
/// the 60-second settings check, spawning at most one invocation
/// thread at a time. The thread opens its own store connection — the
/// driver's connection is never shared across threads, and a faulted
/// invocation cannot poison the pass loop's store.
pub struct IntakeDriver {
    scheduler: IntakeScheduler,
    config_flag: Option<PathBuf>,
    state_flag: Option<PathBuf>,
    actor: Actor,
    work_config: WorkloopConfig,
    db_path: PathBuf,
    capacity: u32,
    /// The adapter's working directory: the installation state root —
    /// a stable, installation-owned directory, never the ambient cwd.
    selector_cwd: PathBuf,
    next_check_unix_seconds: i64,
    invocation: Option<std::thread::JoinHandle<InvocationOutcome>>,
    /// The last profile diagnostic already reported on stderr — the
    /// same failure is announced once, not every tick.
    last_diagnostic: Option<String>,
}

impl IntakeDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_flag: Option<PathBuf>,
        state_flag: Option<PathBuf>,
        actor: Actor,
        work_config: WorkloopConfig,
        db_path: PathBuf,
        capacity: u32,
        selector_cwd: PathBuf,
    ) -> Self {
        Self {
            scheduler: IntakeScheduler::new(),
            config_flag,
            state_flag,
            actor,
            work_config,
            db_path,
            capacity,
            selector_cwd,
            // First check fires on the first boundary — an enabled
            // profile is immediately due at startup by contract.
            next_check_unix_seconds: 0,
            invocation: None,
            last_diagnostic: None,
        }
    }

    /// Reap a finished invocation thread, reporting its bounded
    /// outcome on stderr. Returns whether an invocation is still live.
    fn invocation_alive(&mut self) -> bool {
        let finished = self.invocation.as_ref().is_some_and(|h| h.is_finished());
        if finished && let Some(handle) = self.invocation.take() {
            match handle.join() {
                Ok(outcome) => {
                    eprintln!("taisan: intake invocation finished: {}", outcome.summary())
                }
                Err(_) => eprintln!("taisan: intake invocation thread panicked"),
            }
        }
        self.invocation.is_some()
    }

    /// Whether a durable `open`/`admitting` run exists — the other
    /// half of "intake active". Read through the driver's own
    /// connection; a store fault suppresses the tick (fail closed).
    fn durable_active(store: &SqliteStore) -> bool {
        store
            .transact_deferred(|tx| tx.active_intake_run())
            .map(|r| r.is_some())
            .unwrap_or(true)
    }

    /// Evaluate the settings-check tick when due. Cheap no-op between
    /// ticks; call it at pass boundaries and inside the idle sleep so
    /// the 60-second cadence holds regardless of the pass interval.
    pub fn maybe_tick(&mut self, store: &SqliteStore, now_unix_seconds: i64) {
        if now_unix_seconds < self.next_check_unix_seconds {
            return;
        }
        self.next_check_unix_seconds = now_unix_seconds + SETTINGS_TICK_SECONDS;
        self.invocation_alive();

        let resolved = match resolve_intake(self.config_flag.as_deref(), self.state_flag.as_deref())
        {
            Ok(config) => {
                self.report_diagnostic(None);
                config
            }
            Err(e) => {
                // A profile that fails to load or validate suppresses
                // intake this tick; the scheduler's state is untouched
                // and the next tick retries.
                self.report_diagnostic(Some(e));
                None
            }
        };
        let settings = resolved.as_ref().map(|c| &c.settings);
        let active = self.invocation.is_some() || Self::durable_active(store);
        let decision = self.scheduler.tick(now_unix_seconds, settings, active);
        if let (TickDecision::Due { trigger }, Some(config)) = (decision, resolved)
            && let Some(selector) = config.selector.clone()
        {
            self.spawn(trigger, config, selector);
        }
    }

    fn report_diagnostic(&mut self, diagnostic: Option<String>) {
        if diagnostic == self.last_diagnostic {
            return;
        }
        self.last_diagnostic = diagnostic.clone();
        if let Some(d) = diagnostic {
            eprintln!("taisan: intake settings: {d}");
        }
    }

    fn spawn(&mut self, trigger: IntakeTrigger, config: IntakeConfig, selector: SelectorConfig) {
        let owned = OwnedInvocation {
            db_path: self.db_path.clone(),
            capacity: self.capacity,
            trigger,
            binding: selector.binding,
            task_constraints: selector.task_constraints,
            verification_argv: selector.verification_argv,
            required_permissions: selector.required_permissions,
            actor: self.actor.clone(),
            work_config: self.workloop_config_clone(),
            repositories: config.repositories,
            settings_revision: config.settings.revision(),
            cwd: self.selector_cwd.clone(),
            timeout: Duration::from_secs(config.settings.timeout_seconds),
        };
        self.invocation = Some(std::thread::spawn(move || run_owned(owned)));
    }

    fn workloop_config_clone(&self) -> WorkloopConfig {
        self.work_config.clone()
    }

    /// Join a still-running invocation before the driver exits —
    /// `run --once`, `--work`, shutdown, and tick-limit exits all leave
    /// through here so no hidden selector child can outlive the
    /// process. The wait is bounded by the configured adapter timeout
    /// (≤ 300 s by profile validation).
    pub fn finish(&mut self) {
        if let Some(handle) = self.invocation.take() {
            match handle.join() {
                Ok(outcome) => {
                    eprintln!("taisan: intake invocation finished: {}", outcome.summary())
                }
                Err(_) => eprintln!("taisan: intake invocation thread panicked"),
            }
        }
    }
}

fn run_owned(owned: OwnedInvocation) -> InvocationOutcome {
    let store = match SqliteStore::open(&owned.db_path, owned.capacity) {
        Ok(s) => s,
        Err(e) => {
            let mut outcome = InvocationOutcome::from_error(&IntakeError::Lease {
                reason: format!("intake store open: {e}"),
            });
            outcome.error = Some(format!("store open: {e}"));
            return outcome;
        }
    };
    let binding = &owned.binding;
    let params = IntakeParams {
        trigger: owned.trigger,
        binding,
        actor: &owned.actor,
        work_config: &owned.work_config,
        repositories: &owned.repositories,
        task_constraints: &owned.task_constraints,
        verification_argv: &owned.verification_argv,
        required_permissions: &owned.required_permissions,
        settings_revision: Some(&owned.settings_revision),
        cwd: &owned.cwd,
        timeout: owned.timeout,
    };
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let now_secs = epoch.as_secs() as i64;
    let now_ms = epoch.as_millis() as i64;
    match workloop_intake::run_intake(&store, &params, now_secs, now_ms) {
        Ok(report) => InvocationOutcome::from_report(report),
        Err(e) => InvocationOutcome::from_error(&e),
    }
}
