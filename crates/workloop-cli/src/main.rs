//! `taisan` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

mod envelope;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use workloop_config::{Installation, load_or_create};
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::{
    CommitRevision, IssueId, IssueRef, RepositoryId, RepositoryRef, SourceSnapshot,
};
use workloop_core::projection::{self, CommandResultLookup};
use workloop_core::store::Store;
use workloop_core::work::{WorkloopConfig, dispatch_once, handle_command, sweep};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

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

/// Print the one JSON envelope for a result and hand back its exit code.
/// Human diagnostics go to stderr only — stdout carries the object.
fn emit(result: (envelope::Envelope, i32)) -> i32 {
    let (env, code) = result;
    if let Some(line) = envelope::diagnostic(&env) {
        eprintln!("{line}");
    }
    print(&env);
    code
}

pub(crate) fn main() {
    let code = match Cli::try_parse() {
        Ok(cli) => run(cli),
        Err(e) => {
            // --help/--version are display requests, not results.
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                print!("{e}");
                0
            } else {
                emit(envelope::error_envelope(
                    None,
                    envelope::ErrorClass::Usage,
                    &e.to_string(),
                ))
            }
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

fn run(cli: Cli) -> i32 {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("config.json"));
    let installation = match load_or_create(&config_path) {
        Ok(i) => i,
        Err(e) => {
            return emit(envelope::error_envelope(
                None,
                envelope::ErrorClass::Config,
                &e,
            ));
        }
    };
    let actor = installation.actor();
    let config = match installation.workloop_config() {
        Ok(c) => c,
        Err(e) => {
            return emit(envelope::error_envelope(
                None,
                envelope::ErrorClass::Config,
                &e,
            ));
        }
    };
    let store = match SqliteStore::open(&cli.db, installation.capacity) {
        Ok(s) => s,
        Err(e) => return emit(envelope::store_error(&e)),
    };
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

    // Input that cannot be shaped into a Command is a usage error: a
    // bounded JSON envelope on stdout, exit 2, nothing recorded.
    let usage = |kind: &str, message: &str| -> i32 {
        emit(envelope::error_envelope(
            Some(envelope::query_identity(
                kind,
                &actor,
                cli.idempotency_key.as_deref(),
            )),
            envelope::ErrorClass::Usage,
            message,
        ))
    };
    // Every mutation runs through the same lost-response path: a handler
    // error is resolved by `command-result` under the caller's replay
    // identity before the CLI concludes anything.
    let mutation = |req: CommandRequest| -> i32 {
        let lookup = CommandResultLookup::ReplayKey {
            origin: req.origin.clone(),
            actor: req.actor.clone(),
            idempotency_key: req.idempotency_key.clone(),
        };
        let outcome = handle_command(&store, &config, &req, now());
        emit(envelope::mutation_envelope(&req, &outcome, || {
            store.transact_deferred(|tx| projection::command_result(tx, &lookup))
        }))
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
                return usage(
                    "submit",
                    &format!(
                        "each --issue needs a paired --issue-revision and --issue-digest \
                         ({} issues, {} revisions, {} digests)",
                        issue.len(),
                        issue_revision.len(),
                        issue_digest.len()
                    ),
                );
            }
            let host = &config.host.repository.host;
            let sources = match issue
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
                .collect::<Result<Vec<_>, String>>()
            {
                Ok(s) => s,
                Err(m) => return usage("submit", &m),
            };
            mutation(request(CommandBody::Submit {
                intent,
                sources,
                constraints: constraint,
                verification_argv: verification.iter().map(|s| shlex_split(s)).collect(),
            }))
        }
        Commands::Retry {
            target,
            expected_execution,
        } => mutation(request(CommandBody::Retry {
            work_id: WorkId(target.work),
            expected_work_version: target.expected_version,
            expected_execution_id: ExecutionId(expected_execution),
        })),
        Commands::Resume { target, execution } => mutation(request(CommandBody::Resume {
            work_id: WorkId(target.work),
            expected_work_version: target.expected_version,
            execution_id: ExecutionId(execution),
        })),
        Commands::Cancel { target, execution } => mutation(request(CommandBody::Cancel {
            work_id: WorkId(target.work),
            expected_work_version: target.expected_version,
            execution_id: execution.map(ExecutionId),
        })),
        Commands::Acknowledge { target, attention } => {
            mutation(request(CommandBody::Acknowledge {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                attention_id: attention,
            }))
        }
        Commands::JudgeFindings {
            target,
            head,
            decision,
        } => {
            let expected_head = match parse_head(&head, &config.host.repository) {
                Ok(h) => h,
                Err(m) => return usage("judge-findings", &m),
            };
            let decisions = match decision
                .iter()
                .map(|s| parse_decision(s))
                .collect::<Result<_, _>>()
            {
                Ok(d) => d,
                Err(m) => return usage("judge-findings", &m),
            };
            mutation(request(CommandBody::JudgeFindings {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_head,
                decisions,
            }))
        }
        Commands::ReleaseJudgment { target, head } => {
            let expected_head = match parse_head(&head, &config.host.repository) {
                Ok(h) => h,
                Err(m) => return usage("release-judgment", &m),
            };
            mutation(request(CommandBody::ReleaseJudgment {
                work_id: WorkId(target.work),
                expected_work_version: target.expected_version,
                expected_head,
            }))
        }
        Commands::ClaimResources {
            resource,
            ttl_seconds,
        } => {
            let resources = match resource
                .iter()
                .map(|s| parse_resource(s, &config.host.repository.host))
                .collect::<Result<_, _>>()
            {
                Ok(r) => r,
                Err(m) => return usage("claim-resources", &m),
            };
            mutation(request(CommandBody::ClaimResources {
                resources,
                ttl_seconds,
            }))
        }
        Commands::ReleaseResources { resource } => {
            let resources = match resource
                .iter()
                .map(|s| parse_resource(s, &config.host.repository.host))
                .collect::<Result<_, _>>()
            {
                Ok(r) => r,
                Err(m) => return usage("release-resources", &m),
            };
            mutation(request(CommandBody::ReleaseResources { resources }))
        }
        Commands::RunOnce => {
            let supervisor = match build_supervisor(&config, &installation) {
                Ok(s) => s,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(envelope::query_identity(
                            "run",
                            &actor,
                            cli.idempotency_key.as_deref(),
                        )),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            };
            let mut github = FixtureGithub::new();
            match run_tick(
                &store,
                &config,
                &installation,
                supervisor.as_ref(),
                &mut github,
            ) {
                Ok(list) => emit(envelope::query_envelope(
                    &list,
                    envelope::query_identity("run", &actor, cli.idempotency_key.as_deref()),
                    None,
                )),
                Err(e) => emit(envelope::error_envelope(
                    Some(envelope::query_identity(
                        "run",
                        &actor,
                        cli.idempotency_key.as_deref(),
                    )),
                    envelope::ErrorClass::Store,
                    &e,
                )),
            }
        }
        Commands::Run { ticks, interval_ms } => {
            // One Supervisor across ticks: the per-request start/stop locks
            // live on it, so recreating it would drop serialization. The
            // FixtureGithub lives for the whole loop for the same reason:
            // its PR numbering and head state must survive between ticks,
            // or publishes in different ticks restart at PR 1 and an
            // UpdatePr/PostPrComment head recheck can never match a PR
            // published in an earlier tick.
            let supervisor = match build_supervisor(&config, &installation) {
                Ok(s) => s,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(envelope::query_identity(
                            "run",
                            &actor,
                            cli.idempotency_key.as_deref(),
                        )),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            };
            let mut github = FixtureGithub::new();
            let identity =
                || envelope::query_identity("run", &actor, cli.idempotency_key.as_deref());
            let mut remaining = ticks;
            let mut tick = 0u32;
            loop {
                // stdout stays single-object: per-pass progress is a
                // stderr diagnostic, and the envelope is emitted once
                // when the bounded run finishes.
                let list = match run_tick(
                    &store,
                    &config,
                    &installation,
                    supervisor.as_ref(),
                    &mut github,
                ) {
                    Ok(list) => list,
                    Err(e) => {
                        return emit(envelope::error_envelope(
                            Some(identity()),
                            envelope::ErrorClass::Store,
                            &e,
                        ));
                    }
                };
                tick += 1;
                if let projection::QueryResult::Found { value, .. } = &list {
                    eprintln!("taisan: tick {tick}: {} work item(s)", value.items.len());
                }
                if remaining == 1 {
                    return emit(envelope::query_envelope(&list, identity(), None));
                }
                remaining = remaining.saturating_sub(1);
                std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            }
        }
        Commands::WorkList => {
            match store.transact_deferred(|tx| projection::work_list(tx, now())) {
                Ok(r) => emit(envelope::query_envelope(
                    &r,
                    envelope::query_identity("work_list", &actor, cli.idempotency_key.as_deref()),
                    None,
                )),
                Err(e) => emit(envelope::error_envelope(
                    Some(envelope::query_identity(
                        "work_list",
                        &actor,
                        cli.idempotency_key.as_deref(),
                    )),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
        Commands::WorkDetail { work } => {
            let identity =
                envelope::query_identity("work_detail", &actor, cli.idempotency_key.as_deref());
            match store
                .transact_deferred(|tx| projection::work_detail(tx, &WorkId(work.clone()), now()))
            {
                Ok(r) => emit(envelope::query_envelope(&r, identity, Some(work))),
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
        Commands::Attention { work } => {
            let identity =
                envelope::query_identity("attention", &actor, cli.idempotency_key.as_deref());
            match store.transact_deferred(|tx| {
                projection::attention(tx, work.clone().map(WorkId).as_ref(), now())
            }) {
                Ok(r) => emit(envelope::query_envelope(&r, identity, work)),
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
        Commands::Capacity => {
            let identity =
                envelope::query_identity("capacity", &actor, cli.idempotency_key.as_deref());
            match store.transact_deferred(|tx| projection::capacity(tx)) {
                Ok(r) => emit(envelope::query_envelope(&r, identity, None)),
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
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
                    return usage(
                        "command-result",
                        "command-result needs --command-id or --origin + --replay-key",
                    );
                }
            };
            let identity =
                envelope::query_identity("command_result", &actor, cli.idempotency_key.as_deref());
            match store.transact_deferred(|tx| projection::command_result(tx, &lookup)) {
                Ok(r) => emit(envelope::query_envelope(&r, identity, None)),
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
    }
}

/// `Ok(None)` only when no worker is configured. Handoff
/// initialization failure is an error, not an absent worker — a `run`
/// that keeps going without a supervisor admits work no worker ever
/// consumes.
fn build_supervisor(
    config: &WorkloopConfig,
    installation: &Installation,
) -> Result<Option<Supervisor<PosixProcessControl>>, String> {
    if installation.worker_argv.is_empty() {
        return Ok(None);
    }
    let workspace_root = PathBuf::from(&installation.workspace_root);
    // The durable artifact handoff needs an installation-owned bare
    // repository: bundles export to it, verification worktrees check
    // out from it. It lives under the workspace root's `.handoff`
    // directory — never inside a `ws-*` workspace.
    let handoff_root = workspace_root.join(".handoff");
    let repository = handoff_root.join("repo.git");
    if !repository.exists() {
        std::fs::create_dir_all(&handoff_root)
            .map_err(|e| format!("handoff dir {}: {e}", handoff_root.display()))?;
        let ok = std::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&repository)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return Err(format!(
                "handoff repository init failed at {}",
                repository.display()
            ));
        }
    }
    Ok(Some(
        Supervisor::new(
            PosixProcessControl,
            workspace_root.clone(),
            installation.worker_argv.clone(),
            config.clone(),
        )
        .with_handoff(workloop_exec::handoff::HandoffLayout {
            repository,
            workspace_root,
            out_root: handoff_root.join("out"),
        }),
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
) -> Result<projection::QueryResult<projection::WorkListView>, String> {
    let now = now();
    store
        .transact(|tx| dispatch_once(tx, config, now).map(|_| ()))
        .map_err(|e| e.to_string())?;
    if let Some(supervisor) = supervisor {
        supervisor.run_once(store, now).map_err(|e| e.to_string())?;
    }
    // The in-process fixture drains the outbox only when the
    // installation declares it. Any other adapter identity names an
    // out-of-process adapter (e.g. the S1 bridge) that performs host
    // effects against the same store — running the fixture here would
    // fabricate applied receipts for real pending intents.
    if config.host.adapter == workloop_core::identity::AdapterId::trusted("fixture") {
        github
            .apply_outbox(store, config, now)
            .map_err(|e| e.to_string())?;
    }
    sweep(store, config, now).map_err(|e| e.to_string())?;
    // Status output goes through the same read-only projection as the
    // query subcommands — never a raw store read.
    store
        .transact_deferred(|tx| projection::work_list(tx, now))
        .map_err(|e| e.to_string())
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
                HostCapability::CommentWrite,
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

    fn installation() -> Installation {
        Installation {
            actor_id: "actor-test".into(),
            actor_kind: "human".into(),
            host_instance: "host-local".into(),
            repository: "local-repo".into(),
            adapter: "fixture".into(),
            contract_version: 1,
            capabilities: vec![],
            host_actors: Default::default(),
            required_checks: vec![],
            capacity: 2,
            worker_argv: vec![],
            workspace_root: "workspaces".into(),
            allowed_effects: None,
            budget: None,
            lanes: None,
            policy: None,
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

    #[test]
    fn host_state_persists_across_ticks_of_one_run() {
        let store = SqliteStore::in_memory(2).unwrap();
        let config = config();
        let installation = installation();
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
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
        assert_eq!(
            github.applied(),
            &["open_change:host-local/local-repo:cr-1"]
        );

        // Tick 2: work 2 opens later — its change number must continue,
        // not restart at 1 and collide with work 1's.
        queue_open_change(&store, &w2);
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
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
        run_tick(&store, &config, &installation, None, &mut github).unwrap();
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
