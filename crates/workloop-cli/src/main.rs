//! `taisan` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use run_loop::{
    Pass, PassSettings, RunLeaseGuard, install_shutdown_handler, run_pass, shutdown_requested,
};
use workloop_config::{Installation, load_or_create};
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::{
    CommitRevision, IssueId, IssueRef, RepositoryId, RepositoryRef, SourceSnapshot,
};
use workloop_core::projection::{self, CommandResultLookup};
use workloop_core::store::Store;
use workloop_core::work::{WorkloopConfig, handle_command};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

mod run_loop;

#[derive(Parser)]
#[command(
    name = "taisan",
    bin_name = "taisan",
    about = "taisan work orchestration loop (slice 0)"
)]
struct Cli {
    /// SQLite store path.
    #[arg(long, global = true, default_value = "workloop.db")]
    db: PathBuf,
    /// Installation config path (actor identity, worker argv, capacity).
    /// Created with a fresh stable actor id on first use.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Idempotency key for the command; generated when omitted.
    #[arg(long, global = true)]
    idempotency_key: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct WorkTarget {
    #[arg(long)]
    work: String,
    #[arg(long)]
    expected_version: u64,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit new Work.
    Submit {
        #[arg(long)]
        intent: String,
        /// `repo:number`; repeatable. Each declared issue is paired
        /// (by position) with the authoritative `--issue-revision` and
        /// `--issue-digest` the resolver observed — the CLI never
        /// fabricates them.
        #[arg(long)]
        issue: Vec<String>,
        /// Authoritative revision for the `--issue` at the same
        /// position; repeatable, one per declared issue.
        #[arg(long)]
        issue_revision: Vec<String>,
        /// Authoritative content digest for the `--issue` at the same
        /// position; repeatable, one per declared issue.
        #[arg(long)]
        issue_digest: Vec<String>,
        #[arg(long)]
        constraint: Vec<String>,
        /// Verification command line; repeatable, space-separated.
        #[arg(long)]
        verification: Vec<String>,
    },
    Retry {
        #[command(flatten)]
        target: WorkTarget,
        #[arg(long)]
        expected_execution: String,
    },
    Resume {
        #[command(flatten)]
        target: WorkTarget,
        #[arg(long)]
        execution: String,
    },
    Cancel {
        #[command(flatten)]
        target: WorkTarget,
        #[arg(long)]
        execution: Option<String>,
    },
    Acknowledge {
        #[command(flatten)]
        target: WorkTarget,
        #[arg(long)]
        attention: String,
    },
    JudgeFindings {
        #[command(flatten)]
        target: WorkTarget,
        /// Exact head object id the displayed findings are scoped to.
        #[arg(long)]
        head: String,
        /// `occurrence_id:fix|dismiss|accept|wait[:rationale]`;
        /// repeatable.
        #[arg(long)]
        decision: Vec<String>,
    },
    ReleaseJudgment {
        #[command(flatten)]
        target: WorkTarget,
        /// Exact head object id to release the judgment against.
        #[arg(long)]
        head: String,
    },
    ClaimResources {
        /// `workspace:<id>` or `branch:<repo>:<name>`; repeatable.
        #[arg(long)]
        resource: Vec<String>,
        #[arg(long, default_value_t = 600)]
        ttl_seconds: u64,
    },
    ReleaseResources {
        #[arg(long)]
        resource: Vec<String>,
    },
    /// Drive the eight-phase loop: one bounded pass with --once, one
    /// Work with --work, repeated until interrupted (or --ticks).
    Run {
        /// Run exactly one pass and exit.
        #[arg(long)]
        once: bool,
        /// Scope the pass to one Work id (diagnosis).
        #[arg(long)]
        work: Option<String>,
        /// Seconds between passes (overrides loop.interval_seconds).
        #[arg(long)]
        interval: Option<u64>,
        /// Stop after this many passes; 0 runs until interrupted.
        #[arg(long, default_value_t = 0)]
        ticks: u32,
    },
    WorkList,
    WorkDetail {
        #[arg(long)]
        work: String,
    },
    Attention {
        #[arg(long)]
        work: Option<String>,
    },
    Capacity,
    CommandResult {
        #[arg(long)]
        command_id: Option<String>,
        #[arg(long)]
        origin: Option<String>,
        #[arg(long)]
        replay_key: Option<String>,
    },
}

fn uuidish() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", std::process::id())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_issue(
    s: &str,
    host: &workloop_core::identity::HostInstanceId,
) -> Result<IssueRef, String> {
    let (repo, number) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("issue '{s}': expected repo:number"))?;
    let display_number: u64 = number
        .parse()
        .map_err(|_| format!("issue '{s}': bad number"))?;
    Ok(IssueRef {
        repository: RepositoryRef::new(
            host.clone(),
            RepositoryId::new(repo).map_err(|e| format!("issue '{s}': {e}"))?,
        ),
        issue: IssueId::trusted(number),
        display_number: Some(display_number),
    })
}

fn parse_head(s: &str, repository: &RepositoryRef) -> Result<CommitRevision, String> {
    CommitRevision::new(repository.clone(), s).map_err(|e| format!("head '{s}': {e}"))
}

fn parse_resource(
    s: &str,
    host: &workloop_core::identity::HostInstanceId,
) -> Result<WriteResource, String> {
    if let Some(id) = s.strip_prefix("workspace:") {
        return Ok(WriteResource::Workspace {
            workspace_id: id.into(),
        });
    }
    if let Some(rest) = s.strip_prefix("branch:") {
        let (repo, branch) = rest
            .split_once(':')
            .ok_or_else(|| format!("resource '{s}': expected branch:<repo>:<name>"))?;
        return Ok(WriteResource::Branch {
            repository: RepositoryRef::new(
                host.clone(),
                RepositoryId::new(repo).map_err(|e| format!("resource '{s}': {e}"))?,
            ),
            branch: branch.into(),
        });
    }
    Err(format!(
        "resource '{s}': expected workspace:<id> or branch:<repo>:<name>"
    ))
}

fn parse_decision(s: &str) -> Result<FindingDecision, String> {
    let (id, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("decision '{s}': expected occurrence_id:choice[:rationale]"))?;
    let (choice, rationale) = match rest.split_once(':') {
        Some((choice, rationale)) => (choice, Some(rationale.to_string())),
        None => (rest, None),
    };
    let choice = match choice {
        "fix" => FindingChoice::Fix,
        "dismiss" => FindingChoice::Dismiss,
        "accept" => FindingChoice::Accept,
        "wait" => FindingChoice::Wait,
        other => return Err(format!("decision '{s}': unknown choice '{other}'")),
    };
    Ok(FindingDecision {
        occurrence_id: id.into(),
        choice,
        rationale,
    })
}

fn shlex_split(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn print(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("output serializes")
    );
}

pub(crate) fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("taisan: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("config.json"));
    let installation = load_or_create(&config_path)?;
    let actor = installation.actor();
    let config = installation.workloop_config()?;
    // State lives beside the store file: CAS bodies, workspaces, out
    // directories and the handoff repository all resolve under this one
    // root. Resolve it once, at startup, so no pass depends on the
    // process working directory.
    let db_abs = std::path::absolute(&cli.db).map_err(|e| format!("db path: {e}"))?;
    let state_root = db_abs
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let store =
        SqliteStore::open(&db_abs, installation.capacity).map_err(|e| format!("store: {e}"))?;
    let key = || {
        cli.idempotency_key
            .clone()
            .unwrap_or_else(|| format!("cli-{}", uuidish()))
    };
    let request = |body: CommandBody| CommandRequest {
        actor: actor.clone(),
        origin: Origin("cli".into()),
        idempotency_key: key(),
        body,
    };

    match cli.command {
        Commands::Submit {
            intent,
            issue,
            issue_revision,
            issue_digest,
            constraint,
            verification,
        } => {
            if issue.len() != issue_revision.len() || issue.len() != issue_digest.len() {
                return Err(format!(
                    "each --issue needs a paired --issue-revision and --issue-digest \
                     ({} issues, {} revisions, {} digests)",
                    issue.len(),
                    issue_revision.len(),
                    issue_digest.len()
                ));
            }
            let host = &config.host.repository.host;
            let sources = issue
                .iter()
                .zip(&issue_revision)
                .zip(&issue_digest)
                .map(|((s, revision), digest)| {
                    Ok(SourceSnapshot {
                        issue: parse_issue(s, host)?,
                        source_revision: revision.clone(),
                        content_digest: digest.clone(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let body = CommandBody::Submit {
                intent,
                sources,
                constraints: constraint,
                verification_argv: verification.iter().map(|s| shlex_split(s)).collect(),
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::Retry {
            target,
            expected_execution,
        } => {
            let body = CommandBody::Retry {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_execution_id: ExecutionId(expected_execution),
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::Resume { target, execution } => {
            let body = CommandBody::Resume {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                execution_id: ExecutionId(execution),
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::Cancel { target, execution } => {
            let body = CommandBody::Cancel {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                execution_id: execution.map(ExecutionId),
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::Acknowledge { target, attention } => {
            let body = CommandBody::Acknowledge {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                attention_id: attention,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::JudgeFindings {
            target,
            head,
            decision,
        } => {
            let body = CommandBody::JudgeFindings {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_head: parse_head(&head, &config.host.repository)?,
                decisions: decision
                    .iter()
                    .map(|s| parse_decision(s))
                    .collect::<Result<_, _>>()?,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::ReleaseJudgment { target, head } => {
            let body = CommandBody::ReleaseJudgment {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_head: parse_head(&head, &config.host.repository)?,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::ClaimResources {
            resource,
            ttl_seconds,
        } => {
            let body = CommandBody::ClaimResources {
                resources: resource
                    .iter()
                    .map(|s| parse_resource(s, &config.host.repository.host))
                    .collect::<Result<_, _>>()?,
                ttl_seconds,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::ReleaseResources { resource } => {
            let body = CommandBody::ReleaseResources {
                resources: resource
                    .iter()
                    .map(|s| parse_resource(s, &config.host.repository.host))
                    .collect::<Result<_, _>>()?,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::Run {
            once,
            work,
            interval,
            ticks,
        } => {
            // SIGINT/SIGTERM finish the current phase boundary and
            // exit; without the handler the default disposition would
            // kill this process mid-phase.
            install_shutdown_handler()?;
            let settings = PassSettings::from_installation(
                &installation,
                installation_files(&db_abs, &config_path),
            )?;
            let interval = interval.unwrap_or(settings.loop_config.interval_seconds);
            if interval == 0 {
                return Err("run --interval must be > 0".into());
            }
            let scope = work.map(WorkId);
            // One Supervisor across ticks: the per-request start/stop
            // locks live on it, so recreating it would drop
            // serialization. The FixtureGithub lives for the whole run
            // for the same reason: its PR numbering and head state must
            // survive between ticks, or publishes in different ticks
            // restart at PR 1 and a head recheck can never match a PR
            // published in an earlier tick.
            let supervisor = build_supervisor(&config, &installation, &state_root);
            let mut github = FixtureGithub::new();
            // Single-flight: one lease covers the whole run, taken
            // before the first pass and released when this process ends.
            // A second `run` fails closed with `lock.held`; a crashed
            // run's lease is reclaimed only after its recorded process
            // identity is inspected as gone.
            let _lease = RunLeaseGuard::acquire(&store, now()).map_err(|e| e.to_string())?;
            let mut passes_done = 0u32;
            loop {
                if shutdown_requested() {
                    break;
                }
                let mut clock = now;
                let report = run_pass(
                    &mut Pass {
                        store: &store,
                        config: &config,
                        settings: &settings,
                        supervisor: supervisor.as_ref(),
                        host: &mut github,
                        state_root: &state_root,
                    },
                    scope.as_ref(),
                    &mut clock,
                    &shutdown_requested,
                );
                // The pass leaves a durable tick report first, then the
                // same report on stdout — one JSON object per pass.
                let payload =
                    serde_json::to_string(&report).map_err(|e| format!("tick report: {e}"))?;
                store
                    .append_event("tick_report", &payload)
                    .map_err(|e| format!("tick report: {e}"))?;
                print(&report);
                passes_done += 1;
                if let Some(reason) = report.abort_reason() {
                    return Err(reason);
                }
                if once || (ticks != 0 && passes_done >= ticks) {
                    break;
                }
                // Sleep to the next pass in short slices so a signal
                // during the idle gap exits promptly instead of after
                // the full interval. The interval is seconds — the
                // `--interval` help and `loop.interval_seconds` both
                // say so — so the countdown runs in milliseconds and
                // the gap lasts the documented cadence, not a
                // millisecond-scale fraction of it.
                let mut remaining_ms = interval.saturating_mul(1000);
                while remaining_ms > 0 && !shutdown_requested() {
                    let slice = remaining_ms.min(100);
                    std::thread::sleep(std::time::Duration::from_millis(slice));
                    remaining_ms -= slice;
                }
            }
        }
        Commands::WorkList => {
            let r = store
                .transact_deferred(|tx| projection::work_list(tx, now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::WorkDetail { work } => {
            let r = store
                .transact_deferred(|tx| projection::work_detail(tx, &WorkId(work), now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::Attention { work } => {
            let r = store
                .transact_deferred(|tx| projection::attention(tx, work.map(WorkId).as_ref(), now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::Capacity => {
            let r = store
                .transact_deferred(|tx| projection::capacity(tx))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::CommandResult {
            command_id,
            origin,
            replay_key,
        } => {
            let lookup = match (command_id, origin, replay_key) {
                (Some(command_id), _, _) => CommandResultLookup::Id {
                    command_id: CommandId(command_id),
                },
                (None, Some(origin), Some(replay_key)) => CommandResultLookup::ReplayKey {
                    origin: Origin(origin),
                    actor: actor.clone(),
                    idempotency_key: replay_key,
                },
                _ => {
                    return Err(
                        "command-result needs --command-id or --origin + --replay-key".into(),
                    );
                }
            };
            let r = store
                .transact_deferred(|tx| projection::command_result(tx, &lookup))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
    }
    Ok(())
}

fn build_supervisor(
    config: &WorkloopConfig,
    installation: &Installation,
    state_root: &std::path::Path,
) -> Option<Supervisor<PosixProcessControl>> {
    if installation.worker_argv.is_empty() {
        return None;
    }
    Some(Supervisor::new(
        PosixProcessControl,
        installation_path(state_root, &installation.workspace_root),
        installation.worker_argv.clone(),
        config.clone(),
    ))
}

/// Resolve an installation-relative path against the one state root.
/// An absolute configured path is used as-is; a relative one never
/// resolves against the ambient working directory.
fn installation_path(state_root: &std::path::Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        state_root.join(path)
    }
}

/// The installation's own files: the live store, its SQLite sidecars
/// and the config. All three sit under the state root, so containment
/// alone would let the retention sweep reclaim them if a catalog row
/// ever pointed at them; they are declared here, at the composition
/// root that knows these paths, and the sweep holds them instead.
fn installation_files(db_abs: &Path, config_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![db_abs.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = db_abs.as_os_str().to_os_string();
        name.push(suffix);
        paths.push(PathBuf::from(name));
    }
    paths.push(std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf()));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::identity::{
        ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, HostActorId, HostActorRef,
        HostCapability, HostInstanceId, RoleBinding,
    };
    use workloop_core::observation::HostActionStatus;
    use workloop_core::store::{Effect, HostAction, HostActionIntent};
    use workloop_core::work::{HostTemplate, admit_queued};

    fn host_instance() -> HostInstanceId {
        HostInstanceId::trusted("host-local")
    }

    fn repository() -> RepositoryRef {
        RepositoryRef::new(host_instance(), RepositoryId::trusted("local-repo"))
    }

    fn publisher() -> HostActorRef {
        HostActorRef {
            host: host_instance(),
            actor: HostActorId::trusted("bot-pub"),
        }
    }

    fn config() -> WorkloopConfig {
        WorkloopConfig {
            host: HostTemplate {
                adapter: AdapterId::trusted("fixture"),
                repository: repository(),
                contract_version: 1,
                actors: vec![RoleBinding {
                    role: ActorRole::Publication,
                    actor: publisher(),
                    credential_ref: "default".into(),
                }],
            },
            declared_capabilities: [
                HostCapability::RepositoryRead,
                HostCapability::PushRef,
                HostCapability::ChangeRequestCreate,
                HostCapability::ChangeRequestRead,
                HostCapability::ChangeRequestUpdate,
                HostCapability::CommentCreate,
                HostCapability::ChecksRead,
                HostCapability::ReviewFindingsRead,
                HostCapability::IssueRead,
            ]
            .into_iter()
            .collect(),
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
            now(),
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

    /// The realistic drain context for an outstanding publish intent:
    /// admitted, then waiting on the host write.
    fn force_waiting_on_host_write(store: &SqliteStore, eid: &ExecutionId) {
        store
            .transact(|tx| {
                let mut e = tx.execution(eid)?.unwrap();
                e.state = ExecutionState::Waiting;
                e.waiting_reason = Some(WaitingReason::HostWrite);
                tx.apply(&Effect::UpdateExecution {
                    execution: e,
                    expected_state: ExecutionState::Ready,
                })
            })
            .unwrap();
    }

    fn queue_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) {
        store
            .transact(|tx| {
                let action_id = tx.alloc_id("act")?;
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id,
                        work_id: work_id.clone(),
                        host: host_instance(),
                        repository: repository(),
                        role: ActorRole::Publication,
                        actor: publisher(),
                        action,
                        payload_digest: "test-digest".into(),
                        status: HostActionStatus::Pending,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
                Ok(())
            })
            .unwrap();
    }

    fn queue_open_change(store: &SqliteStore, work_id: &WorkId) {
        queue_intent(
            store,
            work_id,
            HostAction::OpenChangeRequest {
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
        );
    }

    fn change_for(number: u64) -> ChangeRequestRef {
        ChangeRequestRef {
            repository: repository(),
            change_request: ChangeRequestId::trusted(format!("cr-{number}")),
            display_number: Some(number),
        }
    }

    /// One driver pass with default settings — the same entry point the
    /// `run` command uses, minus the lease (the test process is the
    /// only driver here).
    fn tick(
        store: &SqliteStore,
        config: &WorkloopConfig,
        host: &mut FixtureGithub,
    ) -> run_loop::TickReport {
        let settings = PassSettings {
            loop_config: Default::default(),
            retention: Default::default(),
            protected_paths: Vec::new(),
        };
        run_pass(
            &mut Pass {
                store,
                config,
                settings: &settings,
                supervisor: None,
                host,
                state_root: std::path::Path::new("."),
            },
            None,
            &mut now,
            &|| false,
        )
    }

    #[test]
    fn host_state_persists_across_ticks_of_one_run() {
        let store = SqliteStore::in_memory(2).unwrap();
        let config = config();
        let (w1, e1) = submit(&store, "tick-1");
        let (w2, e2) = submit(&store, "tick-2");
        for eid in [&e1, &e2] {
            store
                .transact(|tx| admit_queued(tx, &config, eid, now()).map(|_| ()))
                .unwrap();
            force_waiting_on_host_write(&store, eid);
        }

        // One fixture for the whole run, as `run` keeps it across ticks.
        let mut github = FixtureGithub::new();

        // Tick 1: work 1 opens its change request.
        queue_open_change(&store, &w1);
        tick(&store, &config, &mut github);
        assert_eq!(
            github.applied(),
            &["open_change:host-local/local-repo:cr-1"]
        );

        // Tick 2: work 2 opens later — its change number must continue,
        // not restart at 1 and collide with work 1's.
        queue_open_change(&store, &w2);
        tick(&store, &config, &mut github);
        assert_eq!(
            github.applied(),
            &[
                "open_change:host-local/local-repo:cr-1",
                "open_change:host-local/local-repo:cr-2"
            ]
        );

        // Tick 3: an UpdateChangeRequest head recheck matches the head
        // recorded in tick 1 — impossible against a fixture that forgot
        // tick 1.
        let head = CommitRevision::new(repository(), format!("work/{}-head", w1.0)).unwrap();
        queue_intent(
            &store,
            &w1,
            HostAction::UpdateChangeRequest {
                change: change_for(1),
                expected_head: head,
                title: None,
                body: Some("edited".into()),
            },
        );
        tick(&store, &config, &mut github);
        assert_eq!(
            github.applied(),
            &[
                "open_change:host-local/local-repo:cr-1",
                "open_change:host-local/local-repo:cr-2",
                "update_change:host-local/local-repo:cr-1"
            ]
        );
    }
}
