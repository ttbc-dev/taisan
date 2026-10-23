//! `workloop` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::projection::{self, CommandResultLookup, QueryResult};
use workloop_core::store::Store;
use workloop_core::work::{WorkloopConfig, dispatch_once, handle_command, sweep};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

#[derive(Parser)]
#[command(name = "workloop", about = "GitHub development workloop (slice 0)")]
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
        /// `repo:number`; repeatable.
        #[arg(long)]
        issue: Vec<String>,
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
        #[arg(long)]
        pr_head: String,
        /// `finding_id:fix|dismiss|accept|wait`; repeatable.
        #[arg(long)]
        decision: Vec<String>,
    },
    ReleaseJudgment {
        #[command(flatten)]
        target: WorkTarget,
        #[arg(long)]
        pr_head: String,
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
    /// Drive one pass: dispatch, execution consumer, outbox, sweep.
    RunOnce,
    /// Drive the loop until interrupted (or --ticks passes).
    Run {
        #[arg(long, default_value_t = 0)]
        ticks: u32,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
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

/// Installation config: the actor identity persists here so it is stable
/// across restarts; worker argv and capacity configure the run loop.
#[derive(serde::Serialize, serde::Deserialize)]
struct Installation {
    actor_id: String,
    actor_kind: String,
    #[serde(default = "default_repository")]
    repository: String,
    #[serde(default = "default_capacity")]
    capacity: u32,
    #[serde(default)]
    worker_argv: Vec<String>,
    #[serde(default = "default_workspace_root")]
    workspace_root: String,
    #[serde(default)]
    allowed_effects: Option<AllowedEffects>,
    #[serde(default)]
    budget: Option<ExecutionBudget>,
}

fn default_repository() -> String {
    "local/repo".into()
}
fn default_capacity() -> u32 {
    1
}
fn default_workspace_root() -> String {
    "workspaces".into()
}

fn load_or_create_installation(path: &Path) -> Result<Installation, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("config: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let installation = Installation {
                actor_id: format!("actor-{}", uuidish()),
                actor_kind: "human".into(),
                repository: default_repository(),
                capacity: default_capacity(),
                worker_argv: vec![],
                workspace_root: default_workspace_root(),
                allowed_effects: None,
                budget: None,
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
            }
            let text =
                serde_json::to_string_pretty(&installation).map_err(|e| format!("config: {e}"))?;
            std::fs::write(path, text).map_err(|e| format!("config write: {e}"))?;
            Ok(installation)
        }
        Err(e) => Err(format!("config read: {e}")),
    }
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

fn parse_issue(s: &str) -> Result<GitHubIssueRef, String> {
    let (repo, number) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("issue '{s}': expected repo:number"))?;
    Ok(GitHubIssueRef {
        repository: repo.into(),
        number: number
            .parse()
            .map_err(|_| format!("issue '{s}': bad number"))?,
    })
}

fn parse_resource(s: &str) -> Result<WriteResource, String> {
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
            repository: repo.into(),
            branch: branch.into(),
        });
    }
    Err(format!(
        "resource '{s}': expected workspace:<id> or branch:<repo>:<name>"
    ))
}

fn parse_decision(s: &str) -> Result<FindingDecision, String> {
    let (id, choice) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("decision '{s}': expected finding_id:choice"))?;
    let choice = match choice {
        "fix" => FindingChoice::Fix,
        "dismiss" => FindingChoice::Dismiss,
        "accept" => FindingChoice::Accept,
        "wait" => FindingChoice::Wait,
        other => return Err(format!("decision '{s}': unknown choice '{other}'")),
    };
    Ok(FindingDecision {
        finding_id: id.into(),
        choice,
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

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("workloop: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("config.json"));
    let installation = load_or_create_installation(&config_path)?;
    let actor = Actor {
        kind: match installation.actor_kind.as_str() {
            "automation" => ActorKind::Automation,
            _ => ActorKind::Human,
        },
        external_id: installation.actor_id.clone(),
    };
    let config = WorkloopConfig {
        default_repository: installation.repository.clone(),
        allowed_effects: installation
            .allowed_effects
            .clone()
            .unwrap_or(AllowedEffects {
                workspace_write: true,
                github_write: false,
                network: false,
            }),
        budget: installation.budget.clone().unwrap_or_default(),
    };
    let store =
        SqliteStore::open(&cli.db, installation.capacity).map_err(|e| format!("store: {e}"))?;
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
            constraint,
            verification,
        } => {
            let body = CommandBody::Submit {
                intent,
                issue_refs: issue
                    .iter()
                    .map(|s| parse_issue(s))
                    .collect::<Result<_, _>>()?,
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
            pr_head,
            decision,
        } => {
            let body = CommandBody::JudgeFindings {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_pr_head: pr_head,
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
        Commands::ReleaseJudgment { target, pr_head } => {
            let body = CommandBody::ReleaseJudgment {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_pr_head: pr_head,
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
                    .map(|s| parse_resource(s))
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
                    .map(|s| parse_resource(s))
                    .collect::<Result<_, _>>()?,
            };
            print(
                &handle_command(&store, &config, &request(body), now())
                    .map_err(|e| e.to_string())?,
            );
        }
        Commands::RunOnce => {
            run_tick(&store, &config, &installation)?;
        }
        Commands::Run { ticks, interval_ms } => {
            let mut remaining = ticks;
            loop {
                run_tick(&store, &config, &installation)?;
                if remaining == 1 {
                    break;
                }
                remaining = remaining.saturating_sub(1);
                std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            }
        }
        Commands::WorkList => {
            let r = store
                .transact(|tx| projection::work_list(tx, now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::WorkDetail { work } => {
            let r = store
                .transact(|tx| projection::work_detail(tx, &WorkId(work), now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::Attention { work } => {
            let r = store
                .transact(|tx| projection::attention(tx, work.map(WorkId).as_ref(), now()))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
        Commands::Capacity => {
            let r = store
                .transact(|tx| projection::capacity(tx))
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
                .transact(|tx| projection::command_result(tx, &lookup))
                .map_err(|e| e.to_string())?;
            print(&r);
        }
    }
    Ok(())
}

/// One pass of the run loop: dispatch scan, execution consumer, GitHub
/// outbox, timed sweep — every decision committed through the same ports.
fn run_tick(
    store: &SqliteStore,
    config: &WorkloopConfig,
    installation: &Installation,
) -> Result<(), String> {
    let now = now();
    store
        .transact(|tx| dispatch_once(tx, config, now).map(|_| ()))
        .map_err(|e| e.to_string())?;
    if !installation.worker_argv.is_empty() {
        let supervisor = Supervisor::new(
            PosixProcessControl,
            PathBuf::from(&installation.workspace_root),
            installation.worker_argv.clone(),
            config.clone(),
        );
        supervisor.run_once(store, now).map_err(|e| e.to_string())?;
    }
    let mut github = FixtureGithub::new();
    github
        .apply_outbox(store, config, now)
        .map_err(|e| e.to_string())?;
    sweep(store, config, now).map_err(|e| e.to_string())?;
    let outstanding = store
        .transact(|tx| {
            Ok(QueryResult::Found {
                value: tx
                    .nonterminal_executions()?
                    .into_iter()
                    .map(|e| (e.id.0, format!("{:?}", e.state)))
                    .collect::<Vec<_>>(),
                as_of: tx.last_seq()?,
                sources: vec![],
            })
        })
        .map_err(|e| e.to_string())?;
    print(&outstanding);
    Ok(())
}
