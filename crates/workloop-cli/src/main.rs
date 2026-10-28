//! `workloop` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::projection::{self, CommandResultLookup};
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
    if let Ok(text) = std::fs::read_to_string(path) {
        return serde_json::from_str(&text).map_err(|e| format!("config: {e}"));
    }
    // Exclusive create: when CLI and TUI (or two CLI processes) start
    // concurrently before the file exists, exactly one write lands and
    // every loser adopts the file that did — one actor identity across
    // adapters.
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
    let text = serde_json::to_string_pretty(&installation).map_err(|e| format!("config: {e}"))?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("config write: {e}"))?;
            return Ok(installation);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("config create: {e}")),
    }
    // Lost the create race: adopt the winner's file, retrying past a
    // brief view of its in-flight write.
    for _ in 0..100 {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(installation) => return Ok(installation),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Err(e) => return Err(format!("config read: {e}")),
        }
    }
    Err("config: first writer did not finish".into())
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
            let supervisor = build_supervisor(&config, &installation);
            let mut github = FixtureGithub::new();
            run_tick(
                &store,
                &config,
                &installation,
                supervisor.as_ref(),
                &mut github,
            )?;
        }
        Commands::Run { ticks, interval_ms } => {
            // One Supervisor across ticks: the per-request start/stop locks
            // live on it, so recreating it would drop serialization. The
            // FixtureGithub lives for the whole loop for the same reason:
            // its PR numbering and head state must survive between ticks,
            // or publishes in different ticks restart at PR 1 and an
            // UpdatePr/PostPrComment head recheck can never match a PR
            // published in an earlier tick.
            let supervisor = build_supervisor(&config, &installation);
            let mut github = FixtureGithub::new();
            let mut remaining = ticks;
            loop {
                run_tick(
                    &store,
                    &config,
                    &installation,
                    supervisor.as_ref(),
                    &mut github,
                )?;
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

fn build_supervisor(
    config: &WorkloopConfig,
    installation: &Installation,
) -> Option<Supervisor<PosixProcessControl>> {
    if installation.worker_argv.is_empty() {
        return None;
    }
    Some(Supervisor::new(
        PosixProcessControl,
        PathBuf::from(&installation.workspace_root),
        installation.worker_argv.clone(),
        config.clone(),
    ))
}

/// One pass of the run loop: dispatch scan, execution consumer, GitHub
/// outbox, timed sweep — every decision committed through the same ports.
/// The GitHub adapter is owned by the caller so its remote state (PR
/// numbering, heads, applied writes) persists across ticks of one `run`.
fn run_tick(
    store: &SqliteStore,
    config: &WorkloopConfig,
    _installation: &Installation,
    supervisor: Option<&Supervisor<PosixProcessControl>>,
    github: &mut FixtureGithub,
) -> Result<(), String> {
    let now = now();
    store
        .transact(|tx| dispatch_once(tx, config, now).map(|_| ()))
        .map_err(|e| e.to_string())?;
    if let Some(supervisor) = supervisor {
        supervisor.run_once(store, now).map_err(|e| e.to_string())?;
    }
    github
        .apply_outbox(store, config, now)
        .map_err(|e| e.to_string())?;
    sweep(store, config, now).map_err(|e| e.to_string())?;
    // Status output goes through the same read-only projection as the
    // query subcommands — never a raw store read.
    let list = store
        .transact(|tx| projection::work_list(tx, now))
        .map_err(|e| e.to_string())?;
    print(&list);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::store::{Effect, PrAction, PrActionIntent, PrActionStatus};
    use workloop_core::work::admit_queued;

    fn config() -> WorkloopConfig {
        WorkloopConfig {
            default_repository: "local/repo".into(),
            allowed_effects: AllowedEffects {
                workspace_write: true,
                github_write: true,
                network: false,
            },
            budget: ExecutionBudget::default(),
        }
    }

    fn installation() -> Installation {
        Installation {
            actor_id: "actor-test".into(),
            actor_kind: "human".into(),
            repository: "local/repo".into(),
            capacity: 2,
            worker_argv: vec![],
            workspace_root: "workspaces".into(),
            allowed_effects: None,
            budget: None,
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
                    issue_refs: vec![],
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
    /// admitted, then waiting on the GitHub write.
    fn force_waiting_on_github_write(store: &SqliteStore, eid: &ExecutionId) {
        store
            .transact(|tx| {
                let mut e = tx.execution(eid)?.unwrap();
                e.state = ExecutionState::Waiting;
                e.waiting_reason = Some(WaitingReason::GithubWrite);
                tx.apply(&Effect::UpdateExecution {
                    execution: e,
                    expected_state: ExecutionState::Ready,
                })
            })
            .unwrap();
    }

    fn queue_publish(store: &SqliteStore, work_id: &WorkId) {
        store
            .transact(|tx| {
                let action_id = tx.alloc_id("act")?;
                tx.apply(&Effect::InsertPrActionIntent {
                    intent: PrActionIntent {
                        action_id,
                        work_id: work_id.clone(),
                        repository: "local/repo".into(),
                        action: PrAction::PublishPr {
                            branch: format!("work/{}", work_id.0),
                            title: "t".into(),
                            body: String::new(),
                        },
                        status: PrActionStatus::Pending,
                    },
                })?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn github_state_persists_across_ticks_of_one_run() {
        let store = SqliteStore::in_memory(2).unwrap();
        let config = config();
        let installation = installation();
        let (w1, e1) = submit(&store, "tick-1");
        let (w2, e2) = submit(&store, "tick-2");
        for eid in [&e1, &e2] {
            store
                .transact(|tx| admit_queued(tx, &config, eid, now()).map(|_| ()))
                .unwrap();
            force_waiting_on_github_write(&store, eid);
        }

        // One fixture for the whole run, as `run` keeps it across ticks.
        let mut github = FixtureGithub::new();

        // Tick 1: work 1 publishes its PR.
        queue_publish(&store, &w1);
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
        assert_eq!(github.applied(), &["publish_pr:local/repo:1"]);

        // Tick 2: work 2 publishes later — its PR number must continue,
        // not restart at 1 and collide with work 1's.
        queue_publish(&store, &w2);
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
        assert_eq!(
            github.applied(),
            &["publish_pr:local/repo:1", "publish_pr:local/repo:2"]
        );

        // Tick 3: an UpdatePr head recheck matches the head published in
        // tick 1 — impossible against a fixture that forgot tick 1.
        store
            .transact(|tx| {
                let action_id = tx.alloc_id("act")?;
                tx.apply(&Effect::InsertPrActionIntent {
                    intent: PrActionIntent {
                        action_id,
                        work_id: w1.clone(),
                        repository: "local/repo".into(),
                        action: PrAction::UpdatePr {
                            number: 1,
                            expected_head: format!("work/{}-head", w1.0),
                            body: "edited".into(),
                        },
                        status: PrActionStatus::Pending,
                    },
                })?;
                Ok(())
            })
            .unwrap();
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
        assert_eq!(
            github.applied(),
            &[
                "publish_pr:local/repo:1",
                "publish_pr:local/repo:2",
                "update_pr:local/repo:1"
            ]
        );
    }
}
