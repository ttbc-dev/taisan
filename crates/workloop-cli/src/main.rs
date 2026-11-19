//! `taisan` — the command/query surface and composition-root run loop.
//! The CLI validates and shapes input only; every decision goes through
//! `work::handle_command`, and every read goes through the projection
//! Queries — the CLI is a replaceable interface, not an authority.

mod envelope;
mod evidence;
mod intake;
mod run_loop;

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use run_loop::{
    Pass, PassSettings, RunLeaseGuard, install_shutdown_handler, run_pass, shutdown_requested,
};
use workloop_config::preflight::{
    CheckOptions, CheckReport, InitHost, InitObservation, InitRepository, InitRole, InitSpec,
    ProbeMode, check as preflight_check, init as preflight_init,
};
use workloop_config::profile::resolve_paths_from_env;
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
    /// State root override for `check` (store, workspaces, bodies).
    /// Absolute; resolved as flag → `TAISAN_STATE_DIR` → XDG default.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
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
    /// Scaffold the installation profile (`config.toml`) from observed
    /// evidence: hosts, repositories, roles with credential references,
    /// the egress allowlist, and one observed identity per bound role.
    /// Never fabricates identity; never rewrites an existing config.
    Init {
        /// Host instance identity the adapter reported — the stable endpoint.
        #[arg(long)]
        endpoint: String,
        /// Adapter id from the adapter registry.
        #[arg(long, default_value = "github")]
        adapter: String,
        /// The host table alias to create.
        #[arg(long, default_value = "h1")]
        host: String,
        /// The adapter contract version this installation speaks.
        #[arg(long, default_value_t = 1)]
        contract_version: u32,
        /// `<repository-id>:<publication|source>`; repeatable, ≥ 1.
        #[arg(long = "repo", value_name = "ID:USE")]
        repo: Vec<String>,
        /// `<role>:<actor>:<credential-ref>`; repeatable, ≥ 1.
        #[arg(long = "role", value_name = "ROLE:ACTOR:CREDENTIAL_REF")]
        role: Vec<String>,
        /// Bare egress host name for `verification.egress.allowlist`; repeatable.
        #[arg(long = "egress")]
        egress: Vec<String>,
        /// `<role>:<login>@<host-instance>/<repository-id>` — one
        /// observed identity per bound role; `init` refuses to publish
        /// without it (it never fabricates identity). Repeatable.
        #[arg(long = "observe", value_name = "ROLE:LOGIN@HOST/REPO")]
        observe: Vec<String>,
    },
    /// Ordered, offline, fail-closed preflight over the installation
    /// profile: config parse → identity tuples → credential-reference
    /// presence → paths → worker runtime → isolation primitives → store,
    /// stopping at the first failing stage with a stage-scoped
    /// diagnostic. Read-only; never a lasting grant.
    Check {
        /// Opt-in read-only adapter probe (stage 8): identity and
        /// capability reads per bound actor, zero remote writes. A
        /// plain `check` never touches remote state.
        #[arg(long)]
        probe: bool,
    },
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
    /// Run one intake invocation against the configured
    /// `[intake.selector]` binding — the same coordinator path the
    /// scheduler drives, under the `manual` trigger. Requires the
    /// TOML installation profile; a missing profile or selector
    /// binding is a bounded config refusal.
    Intake,
    /// Bounded intake run history, newest-first: per-run trigger,
    /// state, bounded detail code, and per-status candidate counts,
    /// plus the durable status facts (active run, last completed,
    /// last failed) and the profile's enabled/revision state.
    IntakeRuns {
        /// Maximum runs returned; clamped to the query bound.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// One intake run with its candidates in returned order:
    /// proposal/replay identity, recorded status and bounded reason,
    /// and the command/Work decision links.
    IntakeDetail {
        #[arg(long)]
        run: String,
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
    /// Bounded read-only anomaly census: every stuck lane state as
    /// structured output — the lane-diagnosis Phase-1 input. Report
    /// only: no remediation, no worker launch, no source refresh.
    Anomalies {
        /// Maximum items reported per detector section.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Deferral age, in event-sequence distance, that marks
        /// selector starvation.
        #[arg(long, default_value_t = 256)]
        deferral_age_events: i64,
    },
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
    /// Read-only `rui-evidence/1` evidence queries over recorded
    /// history: snapshot-pinned page export and exact artifact
    /// retrieval. Never invokes a worker or refreshes a host.
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommands,
    },
}

#[derive(Subcommand)]
enum EvidenceCommands {
    /// One page of the recorded-history export. `--after` resumes from
    /// a committed checkpoint, `--cursor` continues an open page
    /// sequence; both tokens are opaque strings from a prior page.
    Export {
        /// Committed checkpoint token from a completed export.
        #[arg(long, conflicts_with = "cursor")]
        after: Option<String>,
        /// Page cursor from the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum records on this page (1..=500).
        #[arg(long, default_value_t = 100)]
        limit: u16,
    },
    /// Write one retained artifact's exact bytes to a new file after
    /// digest verification. Refuses an existing destination.
    Artifact {
        /// Artifact id from a page's `artifacts` list.
        #[arg(long)]
        id: String,
        /// Expected SHA-256 digest (64 lowercase hex characters).
        #[arg(long)]
        sha256: String,
        /// New file to receive the exact retained bytes.
        #[arg(long)]
        output: PathBuf,
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
        /// `--apply` is accepted as a synonym (the retention policy's
        /// documented spelling for the same flag).
        #[arg(long, alias = "apply")]
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
    // Setup and preflight operate on the TOML installation profile via
    // the fixed flag → env → XDG precedence. They must never touch the
    // legacy JSON installation record (which would auto-create it) or
    // the store — `check` is read-only and `init` publishes only the
    // profile — so they dispatch before any store-backed setup.
    if let Some(code) = preflight_dispatch(&cli) {
        return code;
    }
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
            // Intake recovery before the scheduler starts: close runs
            // whose recorded owner is proven gone. Idempotent — a live
            // or uninspectable owner leaves its run untouched.
            if let Err(e) = workloop_intake::recover_intake(&store, &actor, now()) {
                eprintln!("taisan: intake recovery: {e}");
            }
            // The intake driver ticks its own 60-second settings check
            // inside the pass loop and idle sleep — profile reloads,
            // due decisions, and skip-while-active all live there.
            // Invocations run on their own thread with their own store
            // connection, so a pending selector never blocks a phase.
            // The TOML profile resolves through `TAISAN_CONFIG`/XDG
            // only: `--config` on a store-backed command is the JSON
            // installation record, never the profile path.
            // `run --work` is a diagnostic scope: it restricts the
            // execution phases to one Work and does not evaluate
            // intake scheduling — no driver exists for it at all.
            let mut intake_driver = scope.is_none().then(|| {
                intake::IntakeDriver::new(
                    None,
                    cli.state_dir.clone(),
                    actor.clone(),
                    config.clone(),
                    db_abs.clone(),
                    installation.capacity,
                    state_root.clone(),
                )
            });
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
                // Every exit route joins a live invocation first — a
                // store fault must not orphan the selector child past
                // the process boundary either.
                let join_invocation = |driver: &mut Option<intake::IntakeDriver>| {
                    if let Some(driver) = driver.as_mut() {
                        driver.finish();
                    }
                };
                let payload = match serde_json::to_string(&report) {
                    Ok(p) => p,
                    Err(e) => {
                        join_invocation(&mut intake_driver);
                        return emit(envelope::error_envelope(
                            Some(identity()),
                            envelope::ErrorClass::Store,
                            &format!("tick report: {e}"),
                        ));
                    }
                };
                if let Err(e) = store.append_event("tick_report", &payload) {
                    join_invocation(&mut intake_driver);
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
                    // Join a still-running invocation before exiting —
                    // otherwise its selector child is orphaned past the
                    // process boundary.
                    if let Some(driver) = intake_driver.as_mut() {
                        driver.finish();
                    }
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &reason,
                    ));
                }
                // A pass that aborted starts no new intake work — the
                // invocation would only sit under finish()'s join on
                // the error path. `--once` still evaluates scheduling:
                // the startup-due intake is part of the one pass the
                // run documents, joined by finish() before the terminal
                // envelope.
                if let Some(driver) = intake_driver.as_mut() {
                    driver.maybe_tick(&store, now());
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
                    if shutdown_requested() {
                        break;
                    }
                    // The settings-check tick is 60 seconds regardless
                    // of the pass interval — evaluate it inside the
                    // idle gap so a long interval cannot starve it. A
                    // shutdown observed inside the slice must not spawn
                    // an invocation finish() then has to join.
                    if let Some(driver) = intake_driver.as_mut() {
                        driver.maybe_tick(&store, now());
                    }
                }
            }
            // No hidden child outlives the run: a still-running
            // invocation is joined here, bounded by the configured
            // adapter timeout.
            if let Some(driver) = intake_driver.as_mut() {
                driver.finish();
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
            // A capacity-deferred proposal recorded nothing, so its
            // key is not a recorded decision and stays out.
            let recorded_keys = |decided: &[serde_json::Value]| -> Vec<String> {
                decided
                    .iter()
                    .filter(|i| i["status"] != "capacity_deferred")
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
                        reason: RejectionCode::CapacityExhausted,
                        evidence,
                        ..
                    } => serde_json::json!({
                        // Transient capacity refusal: deferrable, not a
                        // recorded rejection — no command_id exists and
                        // the replay key resolves to nothing until a
                        // later proposal is decided fresh.
                        "key": proposal.key,
                        "replay_key": req.idempotency_key,
                        "status": "capacity_deferred",
                        "reason_code": serde_json::to_value(RejectionCode::CapacityExhausted).ok(),
                        "evidence": evidence,
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
        Commands::Intake => {
            // `intake` reports through the query envelope deliberately:
            // its durable result is the intake run itself (projectable
            // via `intake-runs`/`intake-detail`), duplicate submission
            // is refused structurally by the intake lease and the
            // active-run guard rather than by a command-ledger replay
            // key, and candidate-level idempotency lives in each
            // proposal's replay key inside the coordinator. The run's
            // `state`/`detail_code` — not the envelope status — carries
            // per-run outcome, so a converged `interrupted` run still
            // exits accepted: the invocation did its job.
            let identity =
                || envelope::query_identity("intake", &actor, cli.idempotency_key.as_deref());
            // Close runs a proven-dead owner left behind before the
            // coordinator can refuse on an active run — idempotent and
            // owner-proof-gated inside `recover_intake`.
            if let Err(e) = workloop_intake::recover_intake(&store, &actor, now()) {
                return emit(envelope::error_envelope(
                    Some(identity()),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                ));
            }
            // The TOML profile resolves through `TAISAN_CONFIG`/XDG
            // only — `cli.config` is the JSON installation record on
            // store-backed commands, never the profile path.
            let resolved = match intake::resolve_intake(None, cli.state_dir.as_deref()) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        "no installation profile — intake is not configured",
                    ));
                }
                Err(e) => {
                    return emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        &e,
                    ));
                }
            };
            let Some(selector) = resolved.selector.clone() else {
                return emit(envelope::error_envelope(
                    Some(identity()),
                    envelope::ErrorClass::Config,
                    "intake.selector binding is required for a manual intake",
                ));
            };
            let revision = resolved.settings.revision();
            let params = workloop_intake::IntakeParams {
                trigger: workloop_core::store::IntakeTrigger::Manual,
                binding: &selector.binding,
                actor: &actor,
                work_config: &config,
                repositories: &resolved.repositories,
                task_constraints: &selector.task_constraints,
                verification_argv: &selector.verification_argv,
                required_permissions: &selector.required_permissions,
                settings_revision: Some(revision.as_str()),
                cwd: &state_root,
                timeout: std::time::Duration::from_secs(resolved.settings.timeout_seconds),
            };
            let epoch_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            match workloop_intake::run_intake(&store, &params, now(), epoch_ms) {
                Ok(report) => {
                    let as_of = store
                        .transact_deferred(|tx| tx.last_seq())
                        .unwrap_or_default();
                    let value = serde_json::json!({
                        "run_id": report.run_id,
                        "state": report.state,
                        "detail_code": report.detail_code,
                        "launched": report.launched,
                        "candidates": report
                            .candidates
                            .iter()
                            .map(|c| serde_json::json!({
                                "ordinal": c.ordinal,
                                "proposal_key": c.proposal_key,
                                "replay_key": c.replay_key,
                                "status": c.status,
                                "status_reason": c.status_reason,
                                "command_id": c.command_id.as_ref().map(|id| id.0.clone()),
                                "work_id": c.work_id.as_ref().map(|w| w.0.clone()),
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
                Err(workloop_intake::IntakeError::Busy { owner_pid }) => {
                    emit(envelope::rejected_envelope(
                        Some(identity()),
                        "intake_busy",
                        &format!("another intake invocation holds the lease (pid {owner_pid})"),
                    ))
                }
                Err(workloop_intake::IntakeError::ActiveRun { run_id }) => {
                    emit(envelope::rejected_envelope(
                        Some(identity()),
                        "intake_active",
                        &format!("an intake run is already active: {run_id}"),
                    ))
                }
                Err(workloop_intake::IntakeError::Request { rule }) => {
                    emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Config,
                        &format!("intake request could not be built: {rule}"),
                    ))
                }
                Err(workloop_intake::IntakeError::Lease { reason }) => {
                    emit(envelope::error_envelope(
                        Some(identity()),
                        envelope::ErrorClass::Store,
                        &format!("intake lease: {reason}"),
                    ))
                }
                Err(workloop_intake::IntakeError::Store(e)) => emit(envelope::error_envelope(
                    Some(identity()),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
        Commands::IntakeRuns { limit } => {
            let identity =
                envelope::query_identity("intake_runs", &actor, cli.idempotency_key.as_deref());
            let status = intake_status_value(None, cli.state_dir.as_deref());
            match store.transact_deferred(|tx| projection::intake_runs(tx, limit)) {
                Ok(workloop_core::projection::QueryResult::Found {
                    value,
                    as_of,
                    sources,
                }) => {
                    let mut value = serde_json::to_value(&value).unwrap_or(serde_json::json!({}));
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("status".into(), status);
                    }
                    emit(envelope::query_envelope(
                        &workloop_core::projection::QueryResult::Found {
                            value,
                            as_of,
                            sources,
                        },
                        identity,
                        None,
                    ))
                }
                Ok(other) => {
                    type V = serde_json::Value;
                    let mapped: workloop_core::projection::QueryResult<V> = match other {
                        workloop_core::projection::QueryResult::NotFound => {
                            workloop_core::projection::QueryResult::NotFound
                        }
                        workloop_core::projection::QueryResult::Forbidden => {
                            workloop_core::projection::QueryResult::Forbidden
                        }
                        workloop_core::projection::QueryResult::StoreUnavailable => {
                            workloop_core::projection::QueryResult::StoreUnavailable
                        }
                        workloop_core::projection::QueryResult::Found { .. } => unreachable!(),
                    };
                    emit(envelope::query_envelope(&mapped, identity, None))
                }
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
                    envelope::ErrorClass::Store,
                    &e.to_string(),
                )),
            }
        }
        Commands::IntakeDetail { run } => {
            let identity =
                envelope::query_identity("intake_detail", &actor, cli.idempotency_key.as_deref());
            match store.transact_deferred(|tx| projection::intake_run_detail(tx, &run)) {
                // Run-scoped: the run id lives inside `result`, and the
                // envelope's `work_id` is reserved for Work identities.
                Ok(r) => emit(envelope::query_envelope(&r, identity, None)),
                Err(e) => emit(envelope::error_envelope(
                    Some(identity),
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
        Commands::Anomalies {
            limit,
            deferral_age_events,
        } => {
            let identity =
                envelope::query_identity("anomalies", &actor, cli.idempotency_key.as_deref());
            let bounds = workloop_core::census::CensusBounds {
                item_limit: limit,
                deferral_age_events,
            };
            let host = config.host.repository.host.clone();
            match store.transact_deferred(|tx| {
                workloop_core::census::anomaly_census(tx, &bounds, &host).map(|census| {
                    let as_of = census.as_of;
                    workloop_core::projection::QueryResult::Found {
                        value: census,
                        as_of,
                        sources: vec![SourceStatus {
                            source: "store".into(),
                            availability: Availability::Known,
                            observed_unix_seconds: None,
                        }],
                    }
                })
            }) {
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
        Commands::Evidence { command } => {
            let actor_label = envelope::actor_label(&actor);
            match command {
                EvidenceCommands::Export {
                    after,
                    cursor,
                    limit,
                } => evidence::export(&store, &actor_label, after, cursor, limit),
                EvidenceCommands::Artifact { id, sha256, output } => {
                    evidence::artifact(&store, &actor_label, id, sha256, output)
                }
            }
        }
        Commands::Retention { command } => {
            // Per-category limits are not configurable yet: the closed
            // documented defaults apply (retention-policy.md). The
            // installation config owns the sweep bounds only.
            let category_settings = workloop_store::retention::CategorySettings::new();
            let finish =
                |kind: &str, value: serde_json::Value, as_of: Option<EventSequence>| -> i32 {
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
                            as_of,
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
                    let as_of = store.transact_deferred(|tx| tx.last_seq()).ok();
                    match store.retention_report(
                        &category_settings,
                        &state_root,
                        &installation_files(&db_abs, &config_path),
                        now(),
                    ) {
                        Ok(report) => finish(
                            "retention_report",
                            serde_json::to_value(report).unwrap_or(serde_json::json!({})),
                            as_of,
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
                                None,
                            ),
                            Err(e) => store_error("retention_cleanup", e),
                        };
                    }
                    // Single-owner maintenance: the installation
                    // maintenance lease (loop-independent) serializes
                    // explicit cleanup and any other maintenance pass;
                    // the loser reports already_running. The loop's
                    // driver lease is unaffected.
                    let _lease = match RunLeaseGuard::acquire_maintenance(&store, now()) {
                        Ok(lease) => lease,
                        Err(e) => {
                            // A live driver is `already_running`; a
                            // lease whose owner cannot be inspected is
                            // reported as the unknown lock state it is,
                            // never as a concurrent run.
                            let message = match &e {
                                workloop_store::sqlite::RunLeaseError::Held { .. } => {
                                    format!("already_running: {e}")
                                }
                                workloop_store::sqlite::RunLeaseError::Unavailable { .. } => {
                                    format!("lock.unavailable: {e}")
                                }
                                workloop_store::sqlite::RunLeaseError::Store(_) => e.to_string(),
                            };
                            return emit(envelope::error_envelope(
                                Some(envelope::query_identity(
                                    "retention_cleanup",
                                    &actor,
                                    cli.idempotency_key.as_deref(),
                                )),
                                envelope::ErrorClass::Store,
                                &message,
                            ));
                        }
                    };
                    // Phase 1 (persist the bounded plan), then phase 2
                    // execute it; an interrupted run resumes the open
                    // plan and skips already-recorded entries. Bodies
                    // phase 1 held out of the plan (over-budget,
                    // protected) are carried into the result so the
                    // operator sees them on execution too.
                    let plan = match store.reclaim_plan_create(
                        &category_settings,
                        &bounds,
                        &state_root,
                        &protected,
                        now(),
                    ) {
                        Ok(plan) => plan,
                        Err(e) => return store_error("retention_cleanup", e),
                    };
                    match store.reclaim_plan_execute(
                        &category_settings,
                        &state_root,
                        &protected,
                        retention.delete_error_limit,
                        now(),
                    ) {
                        Ok(Some(execution)) => {
                            let mut value =
                                serde_json::to_value(execution).unwrap_or(serde_json::json!({}));
                            // Phase-2 holds recorded by the execution
                            // stay under `held`; the phase-1 plan's
                            // skipped/held lists go under distinct keys
                            // so they never clobber each other (nor
                            // the phase-2 holds).
                            value["planned_skipped"] = serde_json::to_value(&plan.skipped)
                                .unwrap_or(serde_json::json!([]));
                            value["planned_held"] =
                                serde_json::to_value(&plan.held).unwrap_or(serde_json::json!([]));
                            finish("retention_cleanup", value, None)
                        }
                        // Phase 1 just ensured an open plan exists.
                        Ok(None) => finish(
                            "retention_cleanup",
                            serde_json::json!({ "completed": true, "no_open_plan": true }),
                            None,
                        ),
                        Err(e) => store_error("retention_cleanup", e),
                    }
                }
            }
        }
        // `init`/`check` returned at the preflight dispatch above —
        // they never reach store-backed setup.
        Commands::Init { .. } | Commands::Check { .. } => {
            unreachable!("setup commands dispatch before store initialization")
        }
    }
}

/// The profile-side intake status the `intake-runs` query reports
/// alongside the durable run facts: whether intake is configured,
/// enabled, under which settings revision, and the bounded loader
/// diagnostic when the profile cannot be resolved this read.
/// `next_due` is scheduler-process state and is never fabricated
/// here — it stays absent from a one-shot query.
fn intake_status_value(config_flag: Option<&Path>, state_flag: Option<&Path>) -> serde_json::Value {
    match intake::profile_state(config_flag, state_flag) {
        intake::ProfileState::Absent => serde_json::json!({ "configured": false }),
        intake::ProfileState::Failed(e) => serde_json::json!({
            "configured": true,
            "enabled": null,
            "diagnostic": e,
        }),
        intake::ProfileState::Loaded(profile) => match profile.intake_settings() {
            Ok(settings) => serde_json::json!({
                "configured": true,
                "enabled": settings.enabled,
                "settings_revision": settings.revision(),
                "interval_minutes": settings.interval_minutes,
                "timeout_seconds": settings.timeout_seconds,
                "pending_queue_limit": settings.pending_queue_limit,
            }),
            Err(e) => serde_json::json!({
                "configured": true,
                "enabled": null,
                "diagnostic": e,
            }),
        },
    }
}

/// The raw `init` flag values, after clap parsing, before semantic
/// parsing into the preflight spec.
struct InitArgs {
    endpoint: String,
    adapter: String,
    host: String,
    contract_version: u32,
    repo: Vec<String>,
    role: Vec<String>,
    egress: Vec<String>,
    observe: Vec<String>,
}

/// `<repository-id>:<publication|source>`. Shape errors are usage
/// errors; the use value's domain is validated by `init` itself.
fn parse_init_repo(spec: &str) -> Result<InitRepository, String> {
    let error = || format!("--repo '{spec}': expected <repository-id>:<publication|source>");
    let (id, use_) = spec.rsplit_once(':').ok_or_else(error)?;
    if id.is_empty() || use_.is_empty() {
        return Err(error());
    }
    Ok(InitRepository {
        id: id.into(),
        display: None,
        use_: use_.into(),
    })
}

/// `<role>:<actor>:<credential-ref>` — the credential reference keeps
/// its own `:` separators (`env:NAME`, `file:/abs/path`). The refusal
/// never echoes the argument: a malformed spec may carry a pasted
/// secret, and usage errors land in captured logs.
fn parse_init_role(spec: &str) -> Result<InitRole, String> {
    let error = || {
        "--role: expected <role>:<actor>:<credential-ref> \
         (e.g. publication:my-bot:env:TAISAN_GH_TOKEN)"
            .to_string()
    };
    let mut parts = spec.splitn(3, ':');
    let role = parts.next().unwrap_or_default();
    let actor = parts.next().unwrap_or_default();
    let credential_ref = parts.next().unwrap_or_default();
    if role.is_empty() || actor.is_empty() || credential_ref.is_empty() {
        return Err(error());
    }
    Ok(InitRole {
        role: role.into(),
        // Single-host `init`: every role binds the host this run creates.
        host: String::new(), // replaced by the caller with --host
        actor: actor.into(),
        credential_ref: credential_ref.into(),
    })
}

/// `<role>:<login>@<host-instance>/<repository-id>` — the observed
/// identity pointers an operator read from the adapter.
fn parse_init_observe(spec: &str) -> Result<InitObservation, String> {
    let error =
        || format!("--observe '{spec}': expected <role>:<login>@<host-instance>/<repository-id>");
    let (role, rest) = spec.split_once(':').ok_or_else(error)?;
    let (login, context) = rest.rsplit_once('@').ok_or_else(error)?;
    let (host_instance, repository) = context.rsplit_once('/').ok_or_else(error)?;
    if role.is_empty() || login.is_empty() || host_instance.is_empty() || repository.is_empty() {
        return Err(error());
    }
    Ok(InitObservation {
        role: role.into(),
        login: login.into(),
        host_instance: host_instance.into(),
        repository: repository.into(),
    })
}

/// The one JSON envelope for a successful setup command.
fn setup_envelope(kind: &'static str, mut result: serde_json::Value) -> (envelope::Envelope, i32) {
    result["kind"] = serde_json::json!(kind);
    (
        envelope::Envelope {
            schema_version: "1",
            status: "accepted",
            command: None,
            work_id: None,
            execution_id: None,
            result: Some(result),
            reason: None,
            evidence_refs: vec![],
            sources: None,
            as_of: None,
        },
        envelope::EXIT_OK,
    )
}

/// `check` failures are config-class (exit 3); the summary names the
/// failed stage and its first stage-scoped diagnostic, and the whole
/// per-stage report rides in `result`.
fn check_failure(report: &CheckReport) -> (envelope::Envelope, i32) {
    let summary = match report.failed_stage() {
        Some(stage) => match stage.diagnostics.first() {
            Some(diagnostic) => format!(
                "check failed at {}: {} — {}",
                stage.stage.id(),
                diagnostic.code,
                diagnostic.message
            ),
            None => format!("check failed at {}", stage.stage.id()),
        },
        None => "check failed".into(),
    };
    let (mut env, code) = envelope::error_envelope(None, envelope::ErrorClass::Config, &summary);
    env.result = serde_json::to_value(report).ok();
    if let Some(result) = &mut env.result {
        result["kind"] = serde_json::json!("check");
    }
    (env, code)
}

/// Dispatch for the two profile-scope setup commands; every other
/// command continues into the store-backed surface.
fn preflight_dispatch(cli: &Cli) -> Option<i32> {
    match &cli.command {
        Commands::Init {
            endpoint,
            adapter,
            host,
            contract_version,
            repo,
            role,
            egress,
            observe,
        } => Some(run_init(
            cli,
            InitArgs {
                endpoint: endpoint.clone(),
                adapter: adapter.clone(),
                host: host.clone(),
                contract_version: *contract_version,
                repo: repo.clone(),
                role: role.clone(),
                egress: egress.clone(),
                observe: observe.clone(),
            },
        )),
        Commands::Check { probe } => Some(run_check(cli, *probe)),
        _ => None,
    }
}

fn run_init(cli: &Cli, args: InitArgs) -> i32 {
    let paths = match resolve_paths_from_env(cli.config.as_deref(), cli.state_dir.as_deref()) {
        Ok(paths) => paths,
        Err(e) => {
            return emit(envelope::error_envelope(
                None,
                envelope::ErrorClass::Config,
                &e,
            ));
        }
    };
    let usage = |message: String| {
        emit(envelope::error_envelope(
            None,
            envelope::ErrorClass::Usage,
            &message,
        ))
    };
    let repositories = match args
        .repo
        .iter()
        .map(|spec| parse_init_repo(spec))
        .collect::<Result<Vec<_>, String>>()
    {
        Ok(repositories) => repositories,
        Err(message) => return usage(message),
    };
    if repositories.is_empty() {
        return usage("init needs at least one --repo <id:use>".into());
    }
    let roles = match args
        .role
        .iter()
        .map(|spec| parse_init_role(spec))
        .collect::<Result<Vec<_>, String>>()
    {
        Ok(roles) => roles,
        Err(message) => return usage(message),
    };
    if roles.is_empty() {
        return usage("init needs at least one --role <role:actor:credential-ref>".into());
    }
    let observations = match args
        .observe
        .iter()
        .map(|spec| parse_init_observe(spec))
        .collect::<Result<Vec<_>, String>>()
    {
        Ok(observations) => observations,
        Err(message) => return usage(message),
    };
    let spec = InitSpec {
        hosts: vec![InitHost {
            name: args.host.clone(),
            adapter: args.adapter,
            endpoint: args.endpoint,
            contract_version: args.contract_version,
            repositories,
        }],
        roles: roles
            .into_iter()
            .map(|role| InitRole {
                host: args.host.clone(),
                ..role
            })
            .collect(),
        egress_allowlist: args.egress,
        observations,
        observed_unix_seconds: now(),
    };
    match preflight_init(&paths.config_path, &spec) {
        Ok(outcome) => emit(setup_envelope(
            "init",
            serde_json::to_value(outcome).unwrap_or_else(|_| serde_json::json!({})),
        )),
        Err(e) => emit(envelope::error_envelope(
            None,
            envelope::ErrorClass::Config,
            &e,
        )),
    }
}

fn run_check(cli: &Cli, probe: bool) -> i32 {
    let paths = match resolve_paths_from_env(cli.config.as_deref(), cli.state_dir.as_deref()) {
        Ok(paths) => paths,
        Err(e) => {
            return emit(envelope::error_envelope(
                None,
                envelope::ErrorClass::Config,
                &e,
            ));
        }
    };
    let mut options = CheckOptions::new(paths.state_root.clone());
    if probe {
        // No adapter probe backend is wired into this runtime yet, so
        // the requested probe stage fails closed (`capability.unknown`)
        // rather than fabricating a result or silently skipping the
        // request. The stage runs after (1)–(7), performs no reads and
        // no writes, and the report carries an empty operation log.
        options.probe = ProbeMode::Unavailable;
    }
    let report = preflight_check(&paths.config_path, &mut options);
    if report.success {
        let value = serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}));
        emit(setup_envelope("check", value))
    } else {
        emit(check_failure(&report))
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
    let supervisor = Supervisor::new(
        PosixProcessControl,
        workspace_root.clone(),
        installation.worker_argv.clone(),
        config.clone(),
    );
    // The isolation profile resolves fail-closed against the approved
    // registry: an unapproved name in the installation file refuses
    // supervisor construction — no dispatch can run unprofiled (U2.3).
    let supervisor = supervisor.with_isolation_profile(installation.worker_isolation_profile()?);
    Ok(Some(supervisor.with_handoff(
        workloop_exec::handoff::HandoffLayout {
            repository,
            workspace_root,
            out_root: handoff_root.join("out"),
        },
    )))
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

    /// The protected set the sweep receives at every entry point is the
    /// live store, its three SQLite sidecars and the config — the files
    /// the retention review flagged as reachable. The sidecars are
    /// spelled `<db>-<suffix>` (not `<db>.<suffix>`), and the config is
    /// absolutized so a relative `--config` still names the real file.
    #[test]
    fn installation_files_covers_the_store_sidecars_and_config() {
        let db = std::path::Path::new("/state/workloop.db");
        let config = std::path::Path::new("rel/config.json");
        let paths = installation_files(db, config);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/state/workloop.db"),
                PathBuf::from("/state/workloop.db-wal"),
                PathBuf::from("/state/workloop.db-shm"),
                PathBuf::from("/state/workloop.db-journal"),
                std::path::absolute(config).unwrap(),
            ]
        );
        for sidecar in &paths[1..4] {
            assert!(
                sidecar.starts_with("/state"),
                "sidecars stay under the state root: {sidecar:?}"
            );
        }
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
