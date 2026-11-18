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
use workloop_core::store::{Effect, IntakeSettingsRecord, IntakeTrigger, Store};
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

/// Persist the tick's resolved settings into the durable
/// installation record (#216): the revision-guarded upsert keeps the
/// single active `pending_queue_limit` every submit transaction
/// reads, so a profile reload reaches manual and selector admission
/// at the same tick boundary. An unchanged revision is a no-op; a
/// `ConstraintConflict` means a concurrent writer landed first — the
/// next tick re-reads and converges, so the error is reported here,
/// never retried in place.
fn persist_settings(store: &SqliteStore, settings: &IntakeSettings) -> Result<(), String> {
    let revision = settings.revision();
    let current = store
        .transact_deferred(|tx| tx.intake_settings())
        .map_err(|e| format!("settings read: {e}"))?;
    if current
        .as_ref()
        .is_some_and(|s| s.settings_revision == revision)
    {
        return Ok(());
    }
    let expected_revision = current.map(|s| s.settings_revision);
    let pending_queue_limit = settings.pending_queue_limit;
    store
        .transact(|tx| {
            let recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::SetIntakeSettings {
                settings: IntakeSettingsRecord {
                    settings_revision: revision,
                    pending_queue_limit,
                    recorded_seq,
                },
                expected_revision,
            })
        })
        .map_err(|e| format!("settings write: {e}"))
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

        // A run orphaned mid-session — its recorded owner proven dead
        // after this process started — would suppress every later Due
        // forever: the active-run check below never distinguishes a
        // live owner from a corpse. The sweep is idempotent and
        // owner-proof-gated, so live or uninspectable owners are left
        // untouched; a fault is reported and retried next tick.
        if let Err(e) = workloop_intake::recover_intake(store, &self.actor, now_unix_seconds) {
            eprintln!("taisan: intake recovery: {e}");
        }

        let resolved = match resolve_intake(self.config_flag.as_deref(), self.state_flag.as_deref())
        {
            Ok(config) => {
                self.report_diagnostic(None);
                // Persist before the due decision so an invocation
                // launched this tick admits under the fresh limit.
                // A write fault is reported and retried next tick —
                // it never suppresses the schedule itself.
                if let Some(cfg) = &config
                    && let Err(e) = persist_settings(store, &cfg.settings)
                {
                    eprintln!("taisan: intake settings persist: {e}");
                }
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
    /// `run --once`, `--work`, shutdown, tick-limit, and abort exits
    /// all leave through here so no hidden selector child can outlive
    /// the process. `join` itself has no timeout; what bounds the
    /// wait is the invocation's own work — the selector exchange is
    /// capped by the configured adapter timeout (≤ 300 s by profile
    /// validation) and admission is a bounded loop of store writes.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::process::Command;

    use workloop_core::domain::{ActorKind, AllowedEffects, ExecutionBudget, WorkPolicy};
    use workloop_core::identity::{
        ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability,
        HostInstanceId, RepositoryId, RoleBinding,
    };
    use workloop_core::store::{Effect, IntakeRun, IntakeRunState};
    use workloop_core::work::HostTemplate;
    use workloop_exec::supervisor::PosixProcessControl;
    use workloop_store::sqlite::SqliteStore;

    use super::*;

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        }
    }

    fn workloop_config() -> WorkloopConfig {
        WorkloopConfig {
            host: HostTemplate {
                adapter: AdapterId::trusted("fixture"),
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("test-host"),
                    RepositoryId::trusted("local-repo"),
                ),
                contract_version: 1,
                actors: vec![RoleBinding {
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: HostInstanceId::trusted("test-host"),
                        actor: HostActorId::trusted("pub-agent"),
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
            lanes: Default::default(),
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

    /// A run left `open` by an owner that died *after* this process
    /// started would suppress every later Due forever — the driver
    /// must reconcile it on a settings tick, not only at startup.
    #[test]
    fn maybe_tick_recovers_a_run_orphaned_after_process_start() {
        let store = SqliteStore::in_memory(4).unwrap();
        // A spawned-then-reaped child: provably absent from the
        // process table, so owner inspection reports it stopped.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let identity = PosixProcessControl::identity_of(pid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        let run = IntakeRun {
            run_id: "intake-orphan".into(),
            trigger: IntakeTrigger::ScheduledInterval,
            selector_id: AdapterId::trusted("test-sel"),
            selector_version: 1,
            policy_revision: 7,
            settings_revision: None,
            state: IntakeRunState::Open,
            detail_code: None,
            request_payload: b"{}".to_vec(),
            request_digest: "d".repeat(64),
            owner_pid: identity.pid,
            owner_started_at: identity.started_at,
            owner_nonce: "n".into(),
            created_unix_seconds: 1,
            finished_unix_seconds: None,
            recorded_seq: 1,
        };
        store
            .transact(|tx| tx.apply(&Effect::InsertIntakeRun { run }))
            .unwrap();

        // The profile path cannot exist, so resolution fails closed
        // and no selector invocation can spawn — only the recovery
        // sweep is under test.
        let missing = PathBuf::from("/nonexistent/taisan-intake-test/config.toml");
        let mut driver = IntakeDriver::new(
            Some(missing),
            None,
            actor(),
            workloop_config(),
            PathBuf::from("/nonexistent/workloop.db"),
            4,
            PathBuf::from("/tmp"),
        );
        driver.maybe_tick(&store, 1_700_000_000);

        let state = store
            .transact_deferred(|tx| Ok(tx.intake_run("intake-orphan")?.map(|r| r.state)))
            .unwrap()
            .unwrap();
        assert_eq!(state, IntakeRunState::Interrupted);
    }

    /// A minimal valid profile whose `[intake]` section is disabled —
    /// ticks resolve and persist settings but never spawn an
    /// invocation thread.
    fn profile_toml(limit: u32) -> String {
        format!(
            r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "https://api.github.com"
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"
use = "publication"

[roles.publication]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_TOKEN"

[intake]
enabled = false
interval_minutes = 30
timeout_seconds = 60
pending_queue_limit = {limit}
"#
        )
    }

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "workloop-intake-driver-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_profile(dir: &Path, limit: u32) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, profile_toml(limit)).unwrap();
        path
    }

    fn driver_for(profile: &Path, dir: &Path) -> IntakeDriver {
        IntakeDriver::new(
            Some(profile.to_path_buf()),
            None,
            actor(),
            workloop_config(),
            dir.join("workloop.db"),
            4,
            dir.to_path_buf(),
        )
    }

    fn settings_record(store: &SqliteStore) -> Option<IntakeSettingsRecord> {
        store.transact_deferred(|tx| tx.intake_settings()).unwrap()
    }

    const TICK_NOW: i64 = 1_700_000_000;

    /// The first valid tick persists the resolved revision and queue
    /// limit into the durable settings record every submit reads.
    #[test]
    fn settings_tick_persists_revision_and_limit() {
        let dir = root("persist");
        let profile = write_profile(&dir, 3);
        let store = SqliteStore::in_memory(4).unwrap();
        let mut driver = driver_for(&profile, &dir);

        driver.maybe_tick(&store, TICK_NOW);

        let record = settings_record(&store).expect("settings record persisted");
        let expected = load_profile(&profile)
            .unwrap()
            .intake_settings()
            .unwrap()
            .revision();
        assert_eq!(record.settings_revision, expected);
        assert_eq!(record.pending_queue_limit, 3);
        assert!(
            record.recorded_seq > 0,
            "recorded_seq allocated: {record:?}"
        );
    }

    /// Identical content keeps the revision — the record is not
    /// rewritten, so `recorded_seq` stays put.
    #[test]
    fn unchanged_tick_does_not_rewrite_the_record() {
        let dir = root("stable");
        let profile = write_profile(&dir, 3);
        let store = SqliteStore::in_memory(4).unwrap();
        let mut driver = driver_for(&profile, &dir);

        driver.maybe_tick(&store, TICK_NOW);
        let first = settings_record(&store).unwrap();
        driver.maybe_tick(&store, TICK_NOW + SETTINGS_TICK_SECONDS);
        let second = settings_record(&store).unwrap();

        assert_eq!(first, second);
    }

    /// An atomic file replacement takes effect at the next tick: the
    /// record carries the new limit and a new revision.
    #[test]
    fn edited_limit_applies_at_the_next_tick() {
        let dir = root("edited");
        let profile = write_profile(&dir, 3);
        let store = SqliteStore::in_memory(4).unwrap();
        let mut driver = driver_for(&profile, &dir);

        driver.maybe_tick(&store, TICK_NOW);
        let before = settings_record(&store).unwrap();

        write_profile(&dir, 7);
        driver.maybe_tick(&store, TICK_NOW + SETTINGS_TICK_SECONDS);

        let after = settings_record(&store).unwrap();
        assert_eq!(after.pending_queue_limit, 7);
        assert_ne!(after.settings_revision, before.settings_revision);
        assert_ne!(after.recorded_seq, before.recorded_seq);
    }

    /// An invalid reload suppresses the write: the last valid record
    /// stays active for admission and the next valid tick recovers.
    #[test]
    fn invalid_profile_keeps_the_last_valid_record() {
        let dir = root("invalid");
        let profile = write_profile(&dir, 3);
        let store = SqliteStore::in_memory(4).unwrap();
        let mut driver = driver_for(&profile, &dir);

        driver.maybe_tick(&store, TICK_NOW);
        let before = settings_record(&store).unwrap();

        std::fs::write(&profile, "this is not toml = [").unwrap();
        driver.maybe_tick(&store, TICK_NOW + SETTINGS_TICK_SECONDS);
        assert_eq!(settings_record(&store).unwrap(), before);

        write_profile(&dir, 5);
        driver.maybe_tick(&store, TICK_NOW + 2 * SETTINGS_TICK_SECONDS);
        assert_eq!(settings_record(&store).unwrap().pending_queue_limit, 5);
    }

    /// The persisted limit is what the authoritative submit
    /// transaction enforces: lowering it refuses the next manual
    /// submit, and raising it at a later tick admits again — a stale
    /// caller cannot supply or overwrite the active revision.
    #[test]
    fn manual_submit_observes_the_reloaded_queue_limit() {
        use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
        use workloop_core::work::handle_command;

        let dir = root("submit-limit");
        let profile = write_profile(&dir, 1);
        let store = SqliteStore::in_memory(8).unwrap();
        let cfg = workloop_config();
        let act = actor();
        let mut driver = driver_for(&profile, &dir);
        driver.maybe_tick(&store, TICK_NOW);

        let submit = |key: &str| {
            handle_command(
                &store,
                &cfg,
                &CommandRequest {
                    actor: act.clone(),
                    origin: workloop_core::domain::Origin("test".into()),
                    idempotency_key: key.into(),
                    body: CommandBody::Submit {
                        intent: format!("work-{key}"),
                        sources: vec![],
                        constraints: vec![],
                        verification_argv: vec![],
                    },
                },
                TICK_NOW,
            )
        };

        // Limit 1: the first submit takes the only position.
        let first = submit("first");
        assert!(
            matches!(first, Ok(CommandDecision::Accepted { .. })),
            "first submit: {first:?}"
        );
        // The queue is full — the next submit is refused capacity.
        assert!(matches!(
            submit("second"),
            Ok(CommandDecision::Rejected { .. })
        ));

        // Raising the bound at the next tick admits again without a
        // restart — the durable record, not the caller's profile,
        // decided.
        write_profile(&dir, 3);
        driver.maybe_tick(&store, TICK_NOW + SETTINGS_TICK_SECONDS);
        assert!(matches!(
            submit("third"),
            Ok(CommandDecision::Accepted { .. })
        ));
    }

    /// No profile file means intake is unconfigured — nothing is
    /// persisted and the documented default keeps applying.
    #[test]
    fn absent_profile_writes_nothing() {
        let dir = root("absent");
        let missing = dir.join("config.toml");
        let store = SqliteStore::in_memory(4).unwrap();
        let mut driver = driver_for(&missing, &dir);

        driver.maybe_tick(&store, TICK_NOW);

        assert!(settings_record(&store).is_none());
    }
}
