//! `taisan` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

mod envelope;

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
use workloop_core::projection::{self, Availability, CommandResultLookup, SourceStatus};
use workloop_core::store::Store;
use workloop_core::work::{WorkloopConfig, handle_command};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::retention::PlanBounds;
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
    /// Run the configured selector over a snapshot file and admit each
    /// proposal through the normal submit path. Requires the
    /// `selector` section in the installation config.
    Propose {
        /// A JSON snapshot of available source facts, in the adapter's
        /// output shape.
        #[arg(long)]
        snapshot: PathBuf,
    },
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
    /// Storage-pressure accounting and two-phase bounded cleanup.
    Retention {
        #[command(subcommand)]
        command: RetentionCommands,
    },
}

#[derive(Subcommand)]
enum RetentionCommands {
    /// Read-only report: per-category usage against max_bytes/max_age,
    /// reclaim status and the bounded unavailable body list.
    Report,
    /// Two-phase bounded cleanup. Dry-run by default: computes and
    /// shows the bounded plan, deletes nothing, persists nothing.
    /// `--execute` persists the plan and performs the reclaim; an
    /// interrupted run resumes the open plan, skipping entries whose
    /// outcome is already recorded.
    Cleanup {
        /// Perform the phase-2 reclaim plan. Without it, changes nothing.
        #[arg(long)]
        execute: bool,
        /// Override the configured per-pass body bound.
        #[arg(long)]
        max_bodies: Option<usize>,
        /// Override the configured per-pass byte bound.
        #[arg(long)]
        max_bytes: Option<u64>,
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
    // State lives beside the store file: CAS bodies, workspaces, out
    // directories and the handoff repository all resolve under this one
    // root. Resolve it once, at startup, so no pass depends on the
    // process working directory.
    let db_abs = match std::path::absolute(&cli.db) {
        Ok(p) => p,
        Err(e) => {
            return emit(envelope::error_envelope(
                None,
                envelope::ErrorClass::Usage,
                &format!("db path: {e}"),
            ));
        }
    };
    let state_root = db_abs
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let store = match SqliteStore::open(&db_abs, installation.capacity) {
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
        Commands::Run {
            once,
            work,
            interval,
            ticks,
        } => {
            let identity =
                || envelope::query_identity("run", &actor, cli.idempotency_key.as_deref());
            // SIGINT/SIGTERM finish the current phase boundary and
            // exit; without the handler the default disposition would
            // kill this process mid-phase.
            if let Err(e) = install_shutdown_handler() {
                return emit(envelope::error_envelope(
                    Some(identity()),
                    envelope::ErrorClass::Store,
                    &e,
                ));
            }
            let settings = match PassSettings::from_installation(
                &installation,
                installation_files(&db_abs, &config_path),
            ) {
                Ok(s) => s,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            };
            let interval = interval.unwrap_or(settings.loop_config.interval_seconds);
            if interval == 0 {
                return usage("run", "run --interval must be > 0");
            }
            let scope = work.map(WorkId);
            // One Supervisor across ticks: the per-request start/stop
            // locks live on it, so recreating it would drop
            // serialization. The FixtureGithub lives for the whole run
            // for the same reason: its PR numbering and head state must
            // survive between ticks, or publishes in different ticks
            // restart at PR 1 and a head recheck can never match a PR
            // published in an earlier tick.
            let supervisor = match build_supervisor(&config, &installation, &state_root) {
                Ok(s) => s,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            };
            let mut github = FixtureGithub::new();
            // Single-flight: one lease covers the whole run, taken
            // before the first pass and released when this process ends.
            // A second `run` fails closed with `lock.held`; a crashed
            // run's lease is reclaimed only after its recorded process
            // identity is inspected as gone.
            let _lease = match RunLeaseGuard::acquire(&store, now()) {
                Ok(l) => l,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &e.to_string(),
                    ));
                }
            };
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
                // The pass leaves a durable tick report first; stdout
                // stays single-object — per-pass progress is a stderr
                // diagnostic and the run's one envelope is emitted once
                // when the bounded run finishes.
                let payload = match serde_json::to_string(&report) {
                    Ok(p) => p,
                    Err(e) => {
                        return emit(envelope::error_envelope(
                            Some(identity()),
                            envelope::ErrorClass::Store,
                            &format!("tick report: {e}"),
                        ));
                    }
                };
                if let Err(e) = store.append_event("tick_report", &payload) {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &format!("tick report: {e}"),
                    ));
                }
                passes_done += 1;
                eprintln!(
                    "taisan: pass {passes_done}: {} phase(s)",
                    report.phases.len()
                );
                if let Some(reason) = report.abort_reason() {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &reason,
                    ));
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
            // The run's terminal result is exactly one envelope — kind
            // `run`, carrying the final work-list — no matter how many
            // passes it drove.
            let list = match store.transact_deferred(|tx| projection::work_list(tx, now())) {
                Ok(list) => list,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &e.to_string(),
                    ));
                }
            };
            emit(envelope::query_envelope(&list, identity(), None))
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
        Commands::Propose { snapshot } => {
            let identity =
                || envelope::query_identity("propose", &actor, cli.idempotency_key.as_deref());
            // Alternate admission is configuration-selected: no
            // selector section is a bounded refusal, never a guess.
            let Some((selector, policy)) = (match installation.selection_policy() {
                Ok(p) => p,
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            }) else {
                return emit(envelope::error_envelope(
                    Some(identity()),
                    envelope::ErrorClass::Config,
                    "selector admission is not configured",
                ));
            };
            let text = match std::fs::read_to_string(&snapshot) {
                Ok(t) => t,
                Err(e) => {
                    return usage("propose", &format!("snapshot {}: {e}", snapshot.display()));
                }
            };
            let facts: workloop_selector::SelectorSnapshot = match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    return usage("propose", &format!("snapshot {}: {e}", snapshot.display()));
                }
            };
            let set = workloop_selector::propose(&selector, &facts, &policy);
            // Every proposal is admitted through the same
            // `handle_command` path a manual `submit` uses — the
            // selector holds no authority of its own. Per-proposal
            // outcomes are data inside one batch envelope.
            let mut items = Vec::new();
            // Replay keys of the proposals decided so far, in batch
            // order — what a faulted or truncated run must still
            // surface so every recorded decision stays recoverable.
            let recorded_keys = |decided: &[serde_json::Value]| -> Vec<String> {
                decided
                    .iter()
                    .filter_map(|i| i["replay_key"].as_str().map(str::to_string))
                    .collect()
            };
            for proposal in &set.proposals {
                let req = workloop_selector::admission::submit_command(proposal, actor.clone());
                let decision = match handle_command(&store, &config, &req, now()) {
                    Ok(d) => d,
                    Err(store_error) => {
                        // The batch shares the single-mutation
                        // lost-response path: a handler error is
                        // resolved under this proposal's replay
                        // identity before the CLI concludes anything.
                        let lookup = CommandResultLookup::ReplayKey {
                            origin: req.origin.clone(),
                            actor: req.actor.clone(),
                            idempotency_key: req.idempotency_key.clone(),
                        };
                        let found =
                            store.transact_deferred(|tx| projection::command_result(tx, &lookup));
                        // The interrupted proposal's own key rides the
                        // envelope alongside the committed ones: its
                        // fate must be recoverable by `command-result`
                        // exactly like a settled decision. The list is
                        // bounded like every other evidence list.
                        let mut recoverable = recorded_keys(&items);
                        recoverable.push(req.idempotency_key.clone());
                        match envelope::resolve_lost_response(
                            &req.origin,
                            &req.idempotency_key,
                            &store_error,
                            || found,
                        ) {
                            // The decision is durable — replaying it is
                            // the recovery; the batch continues with it
                            // as this proposal's outcome.
                            envelope::LostResponse::Recorded(d) => d,
                            // A batch is not atomic: earlier proposals
                            // may already hold recorded decisions. Their
                            // replay keys — and this proposal's — ride
                            // the error envelope so the operator can
                            // recover each via `command-result` — or
                            // simply re-run `propose`, which replays
                            // them. (Per the CLI contract's batch note,
                            // `error` here means the *batch* stopped:
                            // nothing was recorded for the interrupted
                            // proposal, while `evidence_refs` names
                            // what was recorded.)
                            envelope::LostResponse::Absent(message) => {
                                let (mut env, code) = envelope::error_envelope(
                                    Some(identity()),
                                    envelope::ErrorClass::Store,
                                    &format!(
                                        "batch interrupted at proposal {} ({message}); replay \
                                         keys for committed and interrupted decisions are listed",
                                        proposal.key
                                    ),
                                );
                                env.evidence_refs = envelope::bounded_evidence(recoverable);
                                return emit((env, code));
                            }
                            // The fate is undetermined — never a
                            // definite store error about a mutation
                            // that may have committed. Recovery keys on
                            // the interrupted proposal's own mutation
                            // identity, exactly like a single-mutation
                            // `unknown`, never on the propose run's
                            // query identity.
                            envelope::LostResponse::Undetermined(detail) => {
                                let (mut env, code) = envelope::unknown_envelope(
                                    envelope::request_identity(&req),
                                    &detail,
                                );
                                env.evidence_refs = envelope::bounded_evidence(recoverable);
                                return emit((env, code));
                            }
                        }
                    }
                };
                // The replay key rides the envelope so an operator can
                // recover the decision — `command-result --origin
                // selector --replay-key <key>` — without re-running
                // selection.
                let item = match &decision {
                    CommandDecision::Accepted {
                        work_id,
                        command_id,
                        ..
                    } => serde_json::json!({
                        "key": proposal.key,
                        "replay_key": req.idempotency_key,
                        "command_id": command_id.0,
                        "status": "accepted",
                        "work_id": work_id.as_ref().map(|w| w.0.clone()),
                    }),
                    CommandDecision::Rejected {
                        command_id,
                        reason,
                        evidence,
                        ..
                    } => serde_json::json!({
                        "key": proposal.key,
                        "replay_key": req.idempotency_key,
                        "command_id": command_id.0,
                        "status": "rejected",
                        "reason_code": serde_json::to_value(reason).ok(),
                        "evidence": evidence,
                    }),
                };
                items.push(item);
            }
            let as_of = match store.transact_deferred(|tx| tx.last_seq()) {
                Ok(s) => s,
                Err(e) => {
                    // Every proposal in this batch is already
                    // recorded; only the bookkeeping read failed. The
                    // envelope names the recorded decisions' replay
                    // keys so a consumer never reads this store error
                    // as "nothing was recorded".
                    let recorded = recorded_keys(&items);
                    let (mut env, code) = envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &format!(
                            "batch admissions are recorded but the final sequence read failed \
                             ({e}); replay keys for the recorded decisions are listed"
                        ),
                    );
                    env.evidence_refs = envelope::bounded_evidence(recorded);
                    return emit((env, code));
                }
            };
            let value = serde_json::json!({
                "selector": selector.as_str(),
                "policy_revision": policy.revision,
                "proposals": items,
                "truncated": set.truncated,
                "skipped": set
                    .skipped
                    .iter()
                    .map(|s| serde_json::json!({
                        "issue": s.issue.to_string(),
                        "reason": serde_json::to_value(s.reason).ok(),
                    }))
                    .collect::<Vec<_>>(),
            });
            emit(envelope::query_envelope(
                &workloop_core::projection::QueryResult::Found {
                    value,
                    as_of,
                    sources: vec![],
                },
                identity(),
                None,
            ))
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
        Commands::Retention { command } => {
            // Per-category limits are not configurable yet: the closed
            // documented defaults apply (retention-policy.md). The
            // installation config owns the sweep bounds only.
            let category_settings = workloop_store::retention::CategorySettings::new();
            let finish = |kind: &str, value: serde_json::Value| -> i32 {
                emit((
                    envelope::Envelope {
                        schema_version: "1",
                        status: "accepted",
                        command: Some(envelope::query_identity(
                            kind,
                            &actor,
                            cli.idempotency_key.as_deref(),
                        )),
                        work_id: None,
                        execution_id: None,
                        result: Some(value),
                        reason: None,
                        evidence_refs: vec![],
                        sources: Some(vec![SourceStatus {
                            source: "store".into(),
                            availability: Availability::Known,
                            observed_unix_seconds: None,
                        }]),
                        as_of: None,
                    },
                    envelope::EXIT_OK,
                ))
            };
            let store_error = |kind: &str, e: workloop_core::store::StoreError| -> i32 {
                emit(envelope::error_envelope(
                    Some(envelope::query_identity(
                        kind,
                        &actor,
                        cli.idempotency_key.as_deref(),
                    )),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                ))
            };
            match command {
                RetentionCommands::Report => {
                    match store.retention_report(&category_settings, now()) {
                        Ok(report) => finish(
                            "retention_report",
                            serde_json::to_value(report).unwrap_or(serde_json::json!({})),
                        ),
                        Err(e) => store_error("retention_report", e),
                    }
                }
                RetentionCommands::Cleanup {
                    execute,
                    max_bodies,
                    max_bytes,
                } => {
                    let retention = installation.retention_settings();
                    let bounds = PlanBounds {
                        max_bodies: max_bodies.unwrap_or(retention.max_bodies),
                        max_bytes: max_bytes.unwrap_or(retention.max_bytes),
                    };
                    let protected = installation_files(&db_abs, &config_path);
                    if !execute {
                        // Dry-run: nothing is persisted, nothing is
                        // marked, nothing is deleted.
                        return match store.reclaim_plan_candidates(
                            &category_settings,
                            &bounds,
                            &state_root,
                            &protected,
                            now(),
                        ) {
                            Ok(plan) => finish(
                                "retention_cleanup",
                                serde_json::to_value(plan).unwrap_or(serde_json::json!({})),
                            ),
                            Err(e) => store_error("retention_cleanup", e),
                        };
                    }
                    // Phase 1 (persist the bounded plan), then phase 2
                    // execute it; an interrupted run resumes the open
                    // plan and skips already-recorded entries.
                    if let Err(e) = store.reclaim_plan_create(
                        &category_settings,
                        &bounds,
                        &state_root,
                        &protected,
                        now(),
                    ) {
                        return store_error("retention_cleanup", e);
                    }
                    match store.reclaim_plan_execute(
                        &state_root,
                        &protected,
                        retention.delete_error_limit,
                        now(),
                    ) {
                        Ok(Some(execution)) => finish(
                            "retention_cleanup",
                            serde_json::to_value(execution).unwrap_or(serde_json::json!({})),
                        ),
                        // Phase 1 just ensured an open plan exists.
                        Ok(None) => finish(
                            "retention_cleanup",
                            serde_json::json!({ "completed": true, "no_open_plan": true }),
                        ),
                        Err(e) => store_error("retention_cleanup", e),
                    }
                }
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
    state_root: &std::path::Path,
) -> Result<Option<Supervisor<PosixProcessControl>>, String> {
    if installation.worker_argv.is_empty() {
        return Ok(None);
    }
    let workspace_root = installation_path(state_root, &installation.workspace_root);
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
        ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CredentialRef, HostActorId,
        HostActorRef, HostCapability, HostInstanceId, RoleBinding,
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
                    credential_ref: CredentialRef::trusted("default"),
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
