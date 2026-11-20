//! `s1-bridge` — scenario-local real-host bridge for the authorized
//! U8.2 live run (docs/s1). It is NOT the production GitHub adapter
//! (U3.x, unimplemented): a bounded scenario tool that drains pending
//! `HostActionIntent`s against the real isolated repository through
//! `git`/`gh` and ingests the real host facts it observes through
//! `work::handle_observation` — the same ingest pipeline a production
//! adapter would use. Remote identity: `gh` CLI credentials of the
//! operator's account; every read-back is re-fetched, never assumed.
//!
//! One `tick` performs, in order: a capability report grounded in real
//! permission reads — emitted once, then re-emitted whenever the
//! recorded report's support surface differs from this build's —
//! source reads for every Work's declared spec sources, one bounded
//! outbox drain, and a remote-fact poll (branch state, change head,
//! check runs, reviews, and the real-diff invariant assessment).
//! `issue-ref` prints the authoritative revision/digest pair `submit`
//! requires.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};
use workloop_config::load_or_create;
use workloop_core::domain::*;
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, EvidenceRef, HostActorId,
    HostActorRef, HostCapability,
};
use workloop_core::observation::*;
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};
use workloop_store::sqlite::SqliteStore;

#[derive(Parser)]
#[command(name = "s1-bridge", about = "S1 scenario real-host bridge")]
struct Cli {
    /// SQLite store path — the same store the `taisan` CLI drives.
    #[arg(long, global = true, default_value = "workloop.db")]
    db: PathBuf,
    /// Installation config path shared with `taisan`.
    #[arg(long, global = true, default_value = "installation.json")]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// One bridge pass; --ticks N repeats with --interval-ms between.
    Tick {
        #[arg(long, default_value_t = 1)]
        ticks: u32,
        #[arg(long, default_value_t = 3000)]
        interval_ms: u64,
        /// Recorded-attempt bound for reconciling an `unknown` intent:
        /// once one action_id accumulates this many attempt receipts,
        /// the reconcile pass holds it for operator action instead of
        /// reconciling forever.
        #[arg(long, default_value_t = DEFAULT_RECONCILE_BOUND)]
        reconcile_bound: usize,
    },
    /// Print the authoritative revision/digest for one issue — the
    /// exact pair `taisan submit --issue-revision/--issue-digest` needs.
    IssueRef {
        #[arg(long)]
        issue: u64,
    },
}

/// Default `--reconcile-bound`: an initial attempt plus three reconcile
/// passes per intent before it retires.
const DEFAULT_RECONCILE_BOUND: usize = 4;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn gh(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| format!("gh spawn: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn gh_json(args: &[&str]) -> Result<serde_json::Value, String> {
    let text = gh(args)?;
    serde_json::from_str(&text).map_err(|e| format!("gh {}: bad json: {e}", args.join(" ")))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The issue identity convention shared with the selector side:
/// `source_revision` is the host's `updatedAt` token; `content_digest`
/// is the canonical issue-content digest — `s1-issue-v1` over
/// `title\nbody` (`workloop_selector::issue_content_digest`). Both are real reads —
/// the bridge recomputes them on every source poll, so an edited issue
/// surfaces as `changed` while an unchanged one matches the spec the
/// admitting selector bound.
fn issue_identity(repo: &str, number: u64) -> Result<(String, String, IssueState, u64), String> {
    let v = gh_json(&[
        "issue",
        "view",
        &number.to_string(),
        "--repo",
        repo,
        "--json",
        "number,updatedAt,title,body,state",
    ])?;
    let revision = v["updatedAt"].as_str().unwrap_or_default().to_string();
    let title = v["title"].as_str().unwrap_or_default();
    let body = v["body"].as_str().unwrap_or_default();
    let digest = workloop_selector::issue_content_digest(title, body);
    let state = match v["state"].as_str().unwrap_or("") {
        "CLOSED" => IssueState::Closed,
        _ => IssueState::Open,
    };
    let num = v["number"].as_u64().unwrap_or(number);
    Ok((revision, digest, state, num))
}

struct Ctx {
    store: SqliteStore,
    config: WorkloopConfig,
    workspace_root: PathBuf,
    remote: String,
    repo_display: String,
    /// The real `gh`-authenticated identity — what receipts name as the
    /// actor that performed a host effect. Never the intent's claim.
    authenticated: HostActorRef,
}

impl Ctx {
    fn workspace_of(&self, work_id: &WorkId) -> PathBuf {
        self.workspace_root.join(format!("ws-{}", work_id.0))
    }

    /// Feed one real fact through the same ingest pipeline a production
    /// adapter uses. `event_id` is the stable source identity — replays
    /// dedupe at the store.
    fn emit(
        &self,
        work: Option<(&WorkId, Option<&ExecutionId>)>,
        event_id: String,
        fact: ObservationFact,
    ) -> Result<IngestReceipt, StoreError> {
        let obs = Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Host {
                    host: self.config.host.repository.host.clone(),
                },
                source: "github.s1-bridge".into(),
                event_id: event_id.clone(),
            }),
            subject_work_id: work.map(|(w, _)| w.clone()),
            subject_execution_id: work.and_then(|(_, e)| e.cloned()),
            fact,
            observed_unix_seconds: now(),
        };
        let receipt = handle_observation(&self.store, &self.config, &obs, now());
        match &receipt {
            Ok(r) => eprintln!("s1-bridge: {event_id} -> {r:?}"),
            Err(e) => eprintln!("s1-bridge: {event_id} -> store error: {e}"),
        }
        receipt
    }
}

/// U8.4 fault hooks are scenario-only and must never be armed by a
/// stray inherited variable. They are inert unless the process
/// explicitly opts in with `S1_BRIDGE_SCENARIO_FAULTS` naming the exact
/// repository this bridge is configured for, and the build is a debug
/// build (the scenario bridge is never a release artifact). Only then
/// is the hook's own value consulted. The predicate is split out so the
/// arming rules are unit-tested without mutating process-global env.
fn fault_hook_armed(ctx: &Ctx, hook: &str, expected: &str) -> bool {
    fault_hook_armed_with(
        &ctx.repo_display,
        std::env::var("S1_BRIDGE_SCENARIO_FAULTS").ok().as_deref(),
        std::env::var(hook).ok().as_deref(),
        expected,
        cfg!(debug_assertions),
    )
}

fn fault_hook_armed_with(
    repo_display: &str,
    opted_in_repo: Option<&str>,
    hook_value: Option<&str>,
    expected: &str,
    debug_build: bool,
) -> bool {
    debug_build && opted_in_repo == Some(repo_display) && hook_value == Some(expected)
}

/// A partial-effect injection is only honest when a push for this Work's
/// source branch already landed and was recorded `Applied` with a
/// `RefPublished` effect. A stray armed variable with no such receipt
/// must fall through to the real create path instead of fabricating a
/// "landed push" claim.
fn push_landed_in(intents: &[HostActionIntent], work_id: &WorkId, branch: &str) -> bool {
    intents.iter().any(|intent| {
        intent.work_id == *work_id
            && intent.status == HostActionStatus::Applied
            && matches!(&intent.action, HostAction::PushRef { branch: b, .. } if b.as_str() == branch)
            && intent
                .effects
                .iter()
                .any(|e| matches!(e, NormalizedEffectFact::RefPublished { .. }))
    })
}

fn push_landed_for(ctx: &Ctx, work_id: &WorkId, branch: &str) -> bool {
    ctx.store
        .transact_deferred(|tx| tx.action_intents())
        .map(|intents| push_landed_in(&intents, work_id, branch))
        .unwrap_or(false)
}

/// The capability support surface this build implements — the
/// bridge-local half of the report an older binary's recorded report
/// may predate. Authorization stays a live permission read.
const IMPLEMENTED: &[(HostCapability, bool)] = &[
    (HostCapability::RepositoryRead, true),
    (HostCapability::BranchRead, true),
    (HostCapability::ChangeRequestRead, true),
    (HostCapability::IssueRead, true),
    (HostCapability::ChecksRead, true),
    (HostCapability::ReviewFindingsRead, true),
    (HostCapability::PushRef, true),
    (HostCapability::ChangeRequestCreate, true),
    (HostCapability::ChangeRequestUpdate, false),
    (HostCapability::CommentWrite, false),
];

/// The capabilities this bridge can honestly report after a real
/// permission read: support is the bridge's implementation surface,
/// authorization is the credential's real `permissions` answer. An
/// operation `attempt` reports `Unsupported` for is denied support
/// here — declaring it supported would authorize intents the bridge
/// can never execute.
fn capability_report(ctx: &Ctx) -> Result<ObservationFact, String> {
    let perms = gh_json(&["api", &format!("repos/{}", ctx.repo_display)])?;
    let write = perms["permissions"]["push"].as_bool() == Some(true);
    let read = perms["permissions"]["pull"].as_bool() == Some(true);
    let entry = |capability: HostCapability, implemented: bool, granted: bool| CapabilityEntry {
        capability,
        // `support` is the bridge's implementation surface, not the
        // credential's: an operation `attempt` can only ever record
        // `unsupported` for is not supported here, whatever the
        // credential may authorize.
        support: if implemented {
            CapabilityState::Supported
        } else {
            CapabilityState::Denied
        },
        authorization: if granted {
            CapabilityState::Authorized
        } else {
            CapabilityState::Denied
        },
    };
    Ok(ObservationFact::CapabilityReport {
        host: ctx.config.host.repository.host.clone(),
        capabilities: IMPLEMENTED
            .iter()
            .map(|(capability, implemented)| {
                entry(
                    *capability,
                    *implemented,
                    match capability {
                        HostCapability::PushRef
                        | HostCapability::ChangeRequestCreate
                        | HostCapability::ChangeRequestUpdate
                        | HostCapability::CommentWrite => write,
                        _ => read,
                    },
                )
            })
            .collect(),
    })
}

/// The capability report is emitted whenever the recorded report's
/// support surface is not this build's: stores created before this
/// change can carry an older report that marked unimplemented writes
/// `supported`, and Core reads the latest report — never re-emitting
/// would leave the stale support state authoritative forever. A store
/// already holding a current report skips the permission read
/// entirely; later permission drift is left to a production adapter's
/// reporting cadence.
fn ensure_capability_report(ctx: &Ctx) -> Result<(), String> {
    let (count, latest) = ctx
        .store
        .transact_deferred(|tx| {
            let observations = tx.all_observations()?;
            let count = observations
                .iter()
                .filter(|o| matches!(o.fact, ObservationFact::CapabilityReport { .. }))
                .count();
            let latest = observations
                .iter()
                .enumerate()
                .filter_map(|(index, o)| match &o.fact {
                    ObservationFact::CapabilityReport { host, capabilities }
                        if host == &ctx.config.host.repository.host =>
                    {
                        Some((o.observed_unix_seconds, index, capabilities.clone()))
                    }
                    _ => None,
                })
                .max_by_key(|(t, i, _)| (*t, *i))
                .map(|(_, _, capabilities)| capabilities);
            Ok((count, latest))
        })
        .map_err(|e| e.to_string())?;
    let stale = latest.as_deref().is_none_or(|latest| {
        latest.len() != IMPLEMENTED.len()
            || latest.iter().any(|e| {
                IMPLEMENTED
                    .iter()
                    .find(|(capability, _)| *capability == e.capability)
                    .map(|(_, implemented)| *implemented)
                    != Some(matches!(e.support, CapabilityState::Supported))
            })
    });
    if !stale {
        return Ok(());
    }
    let fact = capability_report(ctx)?;
    ctx.emit(None, format!("cap:{count}"), fact)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Authoritative read of every declared spec source issue.
fn source_reads(ctx: &Ctx) -> Result<(), String> {
    let works = ctx
        .store
        .transact_deferred(|tx| tx.works())
        .map_err(|e| e.to_string())?;
    for work in works {
        let spec = ctx
            .store
            .transact_deferred(|tx| tx.spec(&work.id, Some(work.approved_spec)))
            .map_err(|e| e.to_string())?;
        let Some(spec) = spec else { continue };
        for source in &spec.payload.sources {
            match issue_identity(
                source.issue.repository.repository.as_str(),
                source.issue.display_number.unwrap_or(0),
            ) {
                Ok((revision, digest, state, num)) => {
                    let fact = ObservationFact::Issue {
                        issue: source.issue.clone(),
                        source_revision: revision.clone(),
                        content_digest: digest,
                        state,
                    };
                    // Delivery ids are deduped globally per
                    // `(scope, source, event_id)`, so the Work identity
                    // is part of the id: two Works reading the same
                    // issue are distinct subject observations (#186).
                    let _ = ctx.emit(
                        Some((&work.id, None)),
                        format!("issue:{}:{num}:{revision}", work.id.0),
                        fact,
                    );
                }
                Err(e) => {
                    let _ = ctx.emit(
                        Some((&work.id, None)),
                        format!(
                            "issue:{}:{}:unavailable:{}",
                            work.id.0,
                            source.issue.display_number.unwrap_or(0),
                            now()
                        ),
                        ObservationFact::Issue {
                            issue: source.issue.clone(),
                            source_revision: String::new(),
                            content_digest: String::new(),
                            state: IssueState::Unavailable,
                        },
                    );
                    eprintln!("s1-bridge: issue read failed: {e}");
                }
            }
        }
    }
    Ok(())
}

/// Drain pending intents in outbox order — real effects only, each
/// followed by a real read-back before any `applied` claim. Reconcile
/// `unknown` intents after the drain: a pending-only pass leaves them
/// outstanding forever.
fn drain_outbox(ctx: &Ctx, reconcile_bound: usize) -> Result<(), String> {
    if !ctx.config.allowed_effects.host_write {
        return Ok(());
    }
    let mut attempted = std::collections::HashSet::new();
    loop {
        let next = ctx
            .store
            .transact_deferred(|tx| {
                Ok(tx.action_intents()?.into_iter().find(|i| {
                    i.status == HostActionStatus::Pending && !attempted.contains(&i.action_id)
                }))
            })
            .map_err(|e| e.to_string())?;
        let Some(intent) = next else { break };
        attempted.insert(intent.action_id.clone());
        // One intent's transient failure must not starve the rest of
        // the drain or skip the pass's reconcile/verify.
        if let Err(e) = attempt(ctx, &intent, &format!("act:{}", intent.action_id)) {
            eprintln!("s1-bridge: attempt {} failed: {e}", intent.action_id);
        }
    }
    reconcile_unknown(ctx, reconcile_bound)?;
    reverify_applied_pushes(ctx)
}

/// Re-verify every applied `push_ref` against the real remote ref: the
/// receipt is emitted again under a state-keyed delivery id whenever
/// the observed head moved since the last report. This is what a
/// production adapter's drift reconciliation does — an applied claim is
/// re-observed, never trusted from memory. The duplicate-deduped replay
/// is what lets Core re-run the publish chain after a receipt.
fn reverify_applied_pushes(ctx: &Ctx) -> Result<(), String> {
    let intents = ctx
        .store
        .transact_deferred(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .filter(|i| {
                    i.status == HostActionStatus::Applied
                        && matches!(i.action, HostAction::PushRef { .. })
                })
                .collect::<Vec<_>>())
        })
        .map_err(|e| e.to_string())?;
    for intent in intents {
        let HostAction::PushRef {
            branch, produced, ..
        } = &intent.action
        else {
            continue;
        };
        let Ok(sha) = gh(&[
            "api",
            &format!("repos/{}/branches/{branch}", ctx.repo_display),
            "--jq",
            ".commit.sha",
        ]) else {
            continue;
        };
        let Ok(observed) = CommitRevision::new(intent.repository.clone(), sha.clone()) else {
            continue;
        };
        // `produced` is the artifact payload digest — a content
        // identity, never a git object — so the relation is checked
        // against the last head this receipt chain recorded as remotely
        // verified: the concrete commit the push landed.
        let base = intent.effects.iter().rev().find_map(|e| match e {
            NormalizedEffectFact::RefPublished { observed, .. } => Some(observed.object.clone()),
            _ => None,
        });
        let relation = match base.as_deref() {
            Some(base) if base == sha => RefRelation::Equal,
            Some(base) => match gh(&[
                "api",
                &format!("repos/{}/compare/{base}...{sha}", ctx.repo_display),
                "--jq",
                ".status",
            ]) {
                Ok(status) if status == "ahead" => RefRelation::Ancestor,
                Ok(_) => RefRelation::Diverged,
                Err(_) => continue,
            },
            None => continue,
        };
        let execution = ctx
            .store
            .transact_deferred(|tx| {
                Ok(tx
                    .executions_for_work(&intent.work_id)?
                    .into_iter()
                    .max_by_key(|e| e.ordinal))
            })
            .map_err(|e| e.to_string())?;
        // A ref that no longer contains the pushed revision is drift,
        // not a fresh applied claim — recording it unknown lets the
        // publish chain re-arm instead of trusting a stale receipt.
        let status = if relation == RefRelation::Diverged {
            HostActionStatus::Unknown {
                reason: format!("remote {branch} no longer contains the pushed revision"),
            }
        } else {
            HostActionStatus::Applied
        };
        let relation_key = match relation {
            RefRelation::Equal => "equal",
            RefRelation::Ancestor => "ancestor",
            RefRelation::Diverged => "diverged",
        };
        // The applied-settlement ordinal keeps a *repeat* drift to an
        // already-reported head a new record while an unchanged push
        // dedupes: the generation advances only when an attempt receipt
        // settles the intent `applied` again, so a second
        // `verify:<sha>:diverged` after a reconcile lands in a later
        // generation instead of reusing the recorded delivery identity.
        // A per-tick ordinal would mint a fresh receipt on every
        // re-observation — one new `applied` observation per tick would
        // grow the observation table without bound and keep resetting
        // the Work's no-progress clock.
        let generation = applied_attempt_count(ctx, &intent.action_id)?;
        let _ = ctx.emit(
            Some((&intent.work_id, execution.as_ref().map(|e| &e.id))),
            format!(
                "act:{}:verify:{sha}:{relation_key}:{generation}",
                intent.action_id
            ),
            ObservationFact::HostActionResult {
                action_id: intent.action_id.clone(),
                authenticated_actor: Some(ctx.authenticated.clone()),
                status,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                    produced: produced.clone(),
                    observed,
                    relation,
                }],
            },
        );
    }
    Ok(())
}

/// The work's latest recorded Execution — receipts bind to it when one
/// exists.
fn latest_execution_id(ctx: &Ctx, work_id: &WorkId) -> Option<ExecutionId> {
    ctx.store
        .transact_deferred(|tx| {
            Ok(tx
                .executions_for_work(work_id)?
                .into_iter()
                .max_by_key(|e| e.ordinal))
        })
        .ok()
        .flatten()
        .map(|e| e.id)
}

/// One `host_action_result` receipt under a caller-chosen delivery id.
/// The id must differ per reconcile attempt — a resend under a reused
/// id dedupes and never records.
fn emit_result_keyed(
    ctx: &Ctx,
    intent: &HostActionIntent,
    event_id: &str,
    status: HostActionStatus,
    effects: Vec<NormalizedEffectFact>,
) {
    let execution = latest_execution_id(ctx, &intent.work_id);
    let _ = ctx.emit(
        Some((&intent.work_id, execution.as_ref())),
        event_id.to_string(),
        ObservationFact::HostActionResult {
            action_id: intent.action_id.clone(),
            authenticated_actor: Some(ctx.authenticated.clone()),
            status,
            effects,
        },
    );
}

/// Whether one recorded observation is an *attempt* receipt for this
/// action — a drain `act:{id}` or reconcile `act:{id}:reconcile:{n}`
/// emission. State-keyed verify receipts are evidence refreshes, not
/// tries at the effect.
fn is_attempt_receipt(o: &Observation, action_id: &str) -> bool {
    let drain_key = format!("act:{action_id}");
    let reconcile_prefix = format!("{drain_key}:reconcile:");
    matches!(
        &o.fact,
        ObservationFact::HostActionResult { action_id: id, .. }
            if id == action_id
    ) && o.delivery.as_ref().is_some_and(|d| {
        d.event_id == drain_key
            || d.event_id
                .strip_prefix(reconcile_prefix.as_str())
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

/// Recorded *attempt* receipts for one action. The reconcile bound
/// applies to attempts: state-keyed verify receipts re-observing an
/// applied push are evidence refreshes and must not spend the bound.
fn attempt_count(ctx: &Ctx, action_id: &str) -> Result<usize, String> {
    ctx.store
        .transact_deferred(|tx| {
            Ok(tx
                .all_observations()?
                .iter()
                .filter(|o| is_attempt_receipt(o, action_id))
                .count())
        })
        .map_err(|e| e.to_string())
}

/// The applied generation of one action — how many attempt receipts
/// have already settled it `applied`. The verify delivery id keys on
/// the generation so a drift after a re-apply is a new record while a
/// re-verified unchanged push dedupes.
fn applied_attempt_count(ctx: &Ctx, action_id: &str) -> Result<usize, String> {
    ctx.store
        .transact_deferred(|tx| {
            Ok(tx
                .all_observations()?
                .iter()
                .filter(|o| {
                    is_attempt_receipt(o, action_id)
                        && matches!(
                            &o.fact,
                            ObservationFact::HostActionResult {
                                status: HostActionStatus::Applied,
                                ..
                            }
                        )
                })
                .count())
        })
        .map_err(|e| e.to_string())
}

/// Bounded reconcile for `unknown` intents — the pass a pending-only
/// drain cannot perform. Each intent re-reads the remote fact: when
/// the fact proves the recorded effect landed the intent settles
/// `applied` with no second write, otherwise the same guarded write
/// path re-attempts. Every pass emits at most one attempt receipt per
/// intent, so at `--reconcile-bound` attempt receipts the intent is
/// held `unknown` for operator action instead of reconciling forever —
/// an unverified effect is never reported `failed`, which Core would
/// charge to the transport account and blind-retry as a definite
/// no-effect.
fn reconcile_unknown(ctx: &Ctx, bound: usize) -> Result<(), String> {
    let intents = ctx
        .store
        .transact_deferred(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .filter(|i| matches!(i.status, HostActionStatus::Unknown { .. }))
                .collect::<Vec<_>>())
        })
        .map_err(|e| e.to_string())?;
    for intent in intents {
        // One intent's transient read/parse failure must not abort the
        // pass — every other intent still reconciles and the caller's
        // verify pass still runs.
        if let Err(e) = reconcile_one(ctx, &intent, bound) {
            eprintln!("s1-bridge: reconcile {} failed: {e}", intent.action_id);
        }
    }
    Ok(())
}

/// One intent's reconcile pass: re-read the remote fact and settle by
/// it, or re-attempt through the guarded write path.
fn reconcile_one(ctx: &Ctx, intent: &HostActionIntent, bound: usize) -> Result<(), String> {
    let receipts = attempt_count(ctx, &intent.action_id)?;
    if receipts >= bound {
        // A fixed delivery id records the hold once and dedupes on
        // later passes; `unknown` keeps the effect outstanding so no
        // dependent write or terminal settle proceeds unverified.
        emit_result_keyed(
            ctx,
            intent,
            &format!("act:{}:reconcile:exhausted", intent.action_id),
            HostActionStatus::Unknown {
                reason: format!(
                    "reconcile bound {bound} reached; remote effect still unverified — held for operator action"
                ),
            },
            vec![],
        );
        return Ok(());
    }
    // The delivery id keys on the attempt ordinal so each reconcile
    // pass records a new receipt; an emit that failed before
    // recording safely reuses the ordinal.
    let key = format!("act:{}:reconcile:{receipts}", intent.action_id);
    match &intent.action {
        HostAction::PushRef {
            branch, produced, ..
        } => reconcile_push(ctx, intent, branch, produced, &key),
        HostAction::OpenChangeRequest {
            source_branch,
            target_branch,
            ..
        } => reconcile_open(ctx, intent, source_branch, target_branch, &key),
        // Operations outside the bridge's implementation surface
        // reconcile through `attempt`'s explicit `unsupported` receipt —
        // terminal and never an applied receipt or a permission
        // verdict.
        HostAction::UpdateChangeRequest { .. }
        | HostAction::PostComment { .. }
        | HostAction::PublishReviewFeedback { .. }
        | HostAction::ReplyToThread { .. }
        | HostAction::ResolveThread { .. }
        | HostAction::MarkReady { .. } => reattempt(ctx, intent, &key),
    }
}

/// Reconcile one `push_ref`: re-read the remote head first — when it
/// provably contains the pushed commit the intent settles `applied`
/// with no resend — otherwise re-attempt through the lease-guarded
/// push path. The read goes through `git ls-remote`, the same ref read
/// the attempt path guards its push on: empty output is the definite
/// "branch absent" fact — the recorded effect did not land — while a
/// transport error leaves the fact unreadable. A `gh api` failure
/// conflates the two and would burn the bound holding an intent whose
/// effect provably never landed.
fn reconcile_push(
    ctx: &Ctx,
    intent: &HostActionIntent,
    branch: &str,
    produced: &CommitRevision,
    key: &str,
) -> Result<(), String> {
    let repo_dir = ctx.workspace_of(&intent.work_id);
    // `ls-remote` needs no repository context, but `git` runs against a
    // real directory — the workspace clone when it exists.
    let read_dir = if repo_dir.join(".git").exists() {
        repo_dir.as_path()
    } else {
        ctx.workspace_root.as_path()
    };
    let remote = git(
        read_dir,
        &["ls-remote", &ctx.remote, &format!("refs/heads/{branch}")],
    );
    let remote_sha = match remote {
        Ok(out) => out.split_whitespace().next().map(str::to_string),
        Err(_) => {
            // The remote fact itself is unreadable: the attempt counts
            // toward the bound but the intent stays reconcilable.
            emit_result_keyed(
                ctx,
                intent,
                key,
                HostActionStatus::Unknown {
                    reason: "remote head still unreadable".into(),
                },
                vec![],
            );
            return Ok(());
        }
    };
    let Some(remote_sha) = remote_sha else {
        // The branch is provably absent — the recorded effect did not
        // land — so the guarded write re-attempts instead of recording
        // another `unknown` and burning the bound.
        return reattempt(ctx, intent, key);
    };
    let pushed = repo_dir
        .join(".git")
        .exists()
        .then(|| git(&repo_dir, &["rev-parse", "HEAD"]).ok())
        .flatten();
    if let Some(pushed) = pushed {
        let relation = if remote_sha == pushed {
            Some(RefRelation::Equal)
        } else {
            match gh(&[
                "api",
                &format!("repos/{}/compare/{pushed}...{remote_sha}", ctx.repo_display),
                "--jq",
                ".status",
            ])
            .as_deref()
            {
                // The remote provably contains the pushed commit.
                Ok("ahead") | Ok("identical") => Some(RefRelation::Ancestor),
                // `behind` proves the remote is a strict ancestor of
                // the pushed commit — the re-attempt's lease push is a
                // fast-forward, the only re-push an existing branch may
                // take.
                Ok("behind") => None,
                // `diverged` means re-pushing would rewrite remote
                // history — the publication contract forbids a
                // non-fast-forward to an existing branch. And a failed
                // or unrecognized compare read is unavailable evidence.
                // Either way: record the pass unverified, never write.
                Ok("diverged") => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason: format!(
                                "remote {branch} diverged from the pushed revision — a re-push would not fast-forward; effect unverified"
                            ),
                        },
                        vec![],
                    );
                    return Ok(());
                }
                _ => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason:
                                "compare read-back unavailable or unrecognized; effect unverified"
                                    .into(),
                        },
                        vec![],
                    );
                    return Ok(());
                }
            }
        };
        if let Some(relation) = relation {
            // An unparseable remote read-back leaves the effect
            // unverified — recorded, so the bound still advances
            // instead of letting the intent retry the same erroring
            // read every pass.
            let observed = match CommitRevision::new(intent.repository.clone(), remote_sha) {
                Ok(observed) => observed,
                Err(e) => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason: format!("remote head read-back was unparseable: {e}"),
                        },
                        vec![],
                    );
                    return Ok(());
                }
            };
            emit_result_keyed(
                ctx,
                intent,
                key,
                HostActionStatus::Applied,
                vec![NormalizedEffectFact::RefPublished {
                    repository: intent.repository.clone(),
                    branch: branch.to_string(),
                    produced: produced.clone(),
                    observed,
                    relation,
                }],
            );
            return Ok(());
        }
    }
    reattempt(ctx, intent, key)
}

/// The marker the bridge embeds in a created change's body —
/// `ActionMarker` in the effect schema. Adoption of a remote change
/// requires it: without a marker tying the entry to this intent's
/// identity, a same head/base match only proves *some* change exists,
/// and adopting its number into `ChangeOpened` borrows another
/// change's identity for an effect this intent cannot be shown to
/// have made.
fn action_marker(intent: &HostActionIntent) -> String {
    format!("<!-- workloop-action:{} -->", intent.action_id)
}

/// Does this pulls-list entry carry this intent's marker? The read is
/// authoritative only for a change this intent itself created — a
/// pre-existing or foreign change on the same branches is ambiguity,
/// not adoption.
fn marker_matches(intent: &HostActionIntent, entry: &serde_json::Value) -> bool {
    entry["body"]
        .as_str()
        .is_some_and(|body| body.contains(&action_marker(intent)))
}

/// Reconcile one `open_change_request` by read only: an open change
/// under the intent's source branch, target base *and* action marker
/// settles `applied`; without one the intent stays `unknown` and
/// burns the bound. A create is not idempotent — re-issuing
/// `gh pr create` after a response may have been lost is exactly the
/// duplicate R-4 forbids, so the reconcile never resends the write;
/// and a pre-existing open change on the same branches without this
/// intent's marker is ambiguity, never this intent's effect.
fn reconcile_open(
    ctx: &Ctx,
    intent: &HostActionIntent,
    source_branch: &str,
    target_branch: &str,
    key: &str,
) -> Result<(), String> {
    let owner = ctx.repo_display.split('/').next().unwrap_or_default();
    let pr = gh_json(&[
        "api",
        &format!(
            "repos/{}/pulls?head={owner}:{source_branch}&state=open",
            ctx.repo_display
        ),
    ]);
    match pr {
        Ok(pr) => {
            // Adoption needs exactly one matching change: none means
            // the effect never verified; more than one is ambiguity —
            // either way the intent stays `unknown`.
            let matches: Vec<serde_json::Value> = pr
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|p| {
                            p["base"]["ref"].as_str() == Some(target_branch)
                                && marker_matches(intent, p)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let [first] = matches.as_slice() else {
                let reason = if matches.is_empty() {
                    format!(
                        "no open change on base {target_branch} for {source_branch} carrying this action's marker; create effect unverified"
                    )
                } else {
                    format!(
                        "{} open changes on base {target_branch} for {source_branch} carry this action's marker — ambiguous; create effect unverified",
                        matches.len()
                    )
                };
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Unknown { reason },
                    vec![],
                );
                return Ok(());
            };
            let observed_target = first["base"]["ref"]
                .as_str()
                .unwrap_or(target_branch)
                .to_string();
            let (change, head) = match opened_change(intent, first) {
                Ok(read) => read,
                Err(e) => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason: format!("matching change read-back was unparseable: {e}"),
                        },
                        vec![],
                    );
                    return Ok(());
                }
            };
            let execution = latest_execution_id(ctx, &intent.work_id);
            let _ = ctx.emit(
                Some((&intent.work_id, execution.as_ref())),
                format!("head:{}:{head}", change.change_request.as_str()),
                ObservationFact::ChangeHead {
                    change: change.clone(),
                    head: head.clone(),
                },
            );
            emit_result_keyed(
                ctx,
                intent,
                key,
                HostActionStatus::Applied,
                vec![NormalizedEffectFact::ChangeOpened {
                    change,
                    head,
                    source_branch: source_branch.to_string(),
                    target_branch: observed_target,
                }],
            );
            Ok(())
        }
        Err(_) => {
            // The remote fact itself is unreadable: the attempt counts
            // toward the bound but the intent stays reconcilable.
            emit_result_keyed(
                ctx,
                intent,
                key,
                HostActionStatus::Unknown {
                    reason: "open change read still unreadable".into(),
                },
                vec![],
            );
            Ok(())
        }
    }
}

/// Re-attempt through the ordinary guarded write path. When the
/// attempt cannot run at all — no resolvable Execution or writes not
/// allowed on it — the pass still records one reconcile receipt so the
/// bound keeps advancing.
fn reattempt(ctx: &Ctx, intent: &HostActionIntent, key: &str) -> Result<(), String> {
    let before = attempt_count(ctx, &intent.action_id)?;
    // An errored re-attempt recorded nothing on its way out — emit the
    // reconcile receipt so the bound still advances.
    if let Err(e) = attempt(ctx, intent, key) {
        emit_result_keyed(
            ctx,
            intent,
            key,
            HostActionStatus::Unknown {
                reason: format!("reconcile re-attempt errored: {e}"),
            },
            vec![],
        );
        return Ok(());
    }
    if attempt_count(ctx, &intent.action_id)? == before {
        emit_result_keyed(
            ctx,
            intent,
            key,
            HostActionStatus::Unknown {
                reason: "reconcile re-attempt could not run".into(),
            },
            vec![],
        );
    }
    Ok(())
}

/// The change identity a pull-list entry records — the read-back both
/// the create path and the reconcile settle path share.
fn opened_change(
    intent: &HostActionIntent,
    pr: &serde_json::Value,
) -> Result<(ChangeRequestRef, CommitRevision), String> {
    let node = pr["node_id"].as_str().unwrap_or_default().to_string();
    let number = pr["number"].as_u64().unwrap_or(0);
    let head_sha = pr["head"]["sha"].as_str().unwrap_or_default().to_string();
    let change = ChangeRequestRef {
        repository: intent.repository.clone(),
        change_request: ChangeRequestId::trusted(node),
        display_number: Some(number),
    };
    let head = CommitRevision::new(intent.repository.clone(), head_sha)?;
    Ok((change, head))
}

/// One guarded write attempt. A `push_ref` that cannot run or cannot
/// be verified records `unknown` in every mode: even `pending` does
/// not prove the intent was never attempted — a crash after the push
/// landed but before the receipt wrote leaves it `pending` — so a
/// push failure never proves a definite no-effect (R42). `failed` is
/// recorded only where the no-effect is provable before any write:
/// the `stale_head` expected-prior refusal and the scenario-injected
/// definite refusal. Operations outside the bridge's implementation
/// surface record `unsupported` — terminal in Core, never an applied
/// receipt or a permission verdict.
fn attempt(ctx: &Ctx, intent: &HostActionIntent, key: &str) -> Result<(), String> {
    let execution = ctx
        .store
        .transact_deferred(|tx| {
            Ok(tx
                .executions_for_work(&intent.work_id)?
                .into_iter()
                .max_by_key(|e| e.ordinal))
        })
        .map_err(|e| e.to_string())?;
    let Some(execution) = execution else {
        return Ok(());
    };
    if !execution.allowed_effects.host_write {
        return Ok(());
    }
    match &intent.action {
        HostAction::PushRef {
            branch,
            produced,
            expected_prior,
        } => {
            let repo_dir = ctx.workspace_of(&intent.work_id);
            if !repo_dir.join(".git").exists() {
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Unknown {
                        reason: format!(
                            "no workspace clone at {}; effect unverified",
                            repo_dir.display()
                        ),
                    },
                    vec![],
                );
                return Ok(());
            }
            let local_head = match git(&repo_dir, &["rev-parse", "HEAD"]) {
                Ok(head) => head,
                Err(e) => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason: format!("local head read failed: {e}; effect unverified"),
                        },
                        vec![],
                    );
                    return Ok(());
                }
            };
            // Expected-prior-ref guard: read the remote ref, then push
            // under a lease on that exact value. A remote that moved
            // since the read rejects the push instead of being
            // fast-forwarded silently.
            let prior_sha = match git(
                &repo_dir,
                &["ls-remote", &ctx.remote, &format!("refs/heads/{branch}")],
            ) {
                Ok(out) => out
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                Err(e) => {
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Unknown {
                            reason: format!("prior-ref read failed: {e}; effect unverified"),
                        },
                        vec![],
                    );
                    return Ok(());
                }
            };
            // The read-back records the prior ref the lease guards on;
            // with the applied receipt's `observed` it shows the prior →
            // new head pair the republish evidence requires.
            let prior_state = if prior_sha.is_empty() {
                BranchState::Absent
            } else {
                let revision = match CommitRevision::new(
                    intent.repository.clone(),
                    prior_sha.clone(),
                ) {
                    Ok(revision) => revision,
                    Err(e) => {
                        emit_result_keyed(
                            ctx,
                            intent,
                            key,
                            HostActionStatus::Unknown {
                                reason: format!(
                                    "prior-ref read-back was unparseable: {e}; effect unverified"
                                ),
                            },
                            vec![],
                        );
                        return Ok(());
                    }
                };
                BranchState::At { revision }
            };
            let _ = ctx.emit(
                Some((&intent.work_id, Some(&execution.id))),
                branch_event_id(
                    &intent.work_id,
                    branch,
                    if prior_sha.is_empty() {
                        "absent"
                    } else {
                        prior_sha.as_str()
                    },
                ),
                ObservationFact::BranchState {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                    state: prior_state,
                },
            );
            // The guard binds the write to the position recorded at
            // intent time, not to whatever a fresh read happens to
            // return: a remote ref that moved since the intent was
            // persisted refuses the write before it is attempted.
            let expected = expected_prior
                .as_ref()
                .map(|p| p.object.as_str())
                .unwrap_or_default();
            if prior_sha != expected {
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Failed {
                        reason: "stale_head".into(),
                    },
                    vec![],
                );
                return Ok(());
            }
            let lease = format!("--force-with-lease=refs/heads/{branch}:{prior_sha}");
            let push = git(
                &repo_dir,
                &[
                    "push",
                    &lease,
                    &ctx.remote,
                    &format!("HEAD:refs/heads/{branch}"),
                ],
            );
            // U8.4 fault hook: S1_BRIDGE_KILL_AFTER_APPLY=push_ref dies
            // only after the push actually succeeded — a lease-rejected or
            // otherwise failed push is recorded through the ordinary
            // Failed path, never fault-injected. Arming additionally
            // requires the scenario opt-in; see `fault_hook_armed`.
            if push.is_ok() && fault_hook_armed(ctx, "S1_BRIDGE_KILL_AFTER_APPLY", "push_ref") {
                eprintln!(
                    "s1-bridge: injected fault kill_after_apply push_ref {}",
                    intent.action_id
                );
                std::process::exit(137);
            }
            let observed = gh(&[
                "api",
                &format!("repos/{}/branches/{branch}", ctx.repo_display),
                "--jq",
                ".commit.sha",
            ])
            .ok();
            if observed.as_deref() == Some(local_head.as_str()) {
                let observed = match CommitRevision::new(
                    intent.repository.clone(),
                    observed.expect("checked"),
                ) {
                    Ok(observed) => observed,
                    Err(e) => {
                        emit_result_keyed(
                            ctx,
                            intent,
                            key,
                            HostActionStatus::Unknown {
                                reason: format!(
                                    "remote head read-back was unparseable: {e}; effect unverified"
                                ),
                            },
                            vec![],
                        );
                        return Ok(());
                    }
                };
                // `produced` is the recorded artifact payload digest — a
                // content identity, not a git object on this revision.
                // `ancestor` records that the observed real head contains
                // the produced change; binding produced to a pushed
                // commit is the production adapter's job (U3).
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Applied,
                    vec![NormalizedEffectFact::RefPublished {
                        repository: intent.repository.clone(),
                        branch: branch.clone(),
                        produced: produced.clone(),
                        observed,
                        relation: RefRelation::Ancestor,
                    }],
                );
            } else {
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    match push {
                        Ok(_) => HostActionStatus::Unknown {
                            reason: format!(
                                "push reported success but remote head is {:?}, expected {local_head}",
                                observed
                            ),
                        },
                        Err(e) => HostActionStatus::Unknown {
                            reason: format!("push failed: {e}; effect unverified"),
                        },
                    },
                    vec![],
                );
            }
        }
        HostAction::OpenChangeRequest {
            source_branch,
            target_branch,
            title,
            body,
        } => {
            // U8.4 fault hook: S1_BRIDGE_INJECT_FAIL_OPEN_PR=1 records a
            // definite no-effect failure for the create leg (partial effect:
            // push landed, create refused). Arming additionally requires
            // the scenario opt-in; see `fault_hook_armed`. It also requires
            // a recorded `Applied` push for this Work and source branch, so
            // an armed variable left set across ticks/stores cannot fabricate
            // a "landed push" receipt where no push happened.
            if fault_hook_armed(ctx, "S1_BRIDGE_INJECT_FAIL_OPEN_PR", "1")
                && push_landed_for(ctx, &intent.work_id, source_branch)
            {
                emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Failed {
                        reason:
                            "injected fault: partial effect (landed push, change_request_create refused)"
                                .into(),
                    },
                    vec![],
                );
                return Ok(());
            }
            // The marker embeds this intent's identity in the change
            // body: it is what lets a lost-response reconcile later
            // adopt *this* intent's change rather than any pre-existing
            // one on the same branches.
            let marker = action_marker(intent);
            match gh(&[
                "pr",
                "create",
                "--repo",
                &ctx.repo_display,
                "--head",
                source_branch,
                "--base",
                target_branch,
                "--title",
                title,
                "--body",
                &format!("{body}\n\n{marker}"),
            ]) {
                Ok(url) => {
                    let owner = ctx.repo_display.split('/').next().unwrap_or_default();
                    // The create reported success: anything short of a
                    // matching read-back leaves the effect unverified —
                    // recording `unknown` hands the intent to the
                    // reconcile pass, whose read settles or re-attempts
                    // without a blind duplicate create (a failed or
                    // propagated outcome would let the drain re-issue
                    // `gh pr create` after a create that may have
                    // landed — the lost-response case R-4 forbids).
                    let pr = match gh_json(&[
                        "api",
                        &format!(
                            "repos/{}/pulls?head={owner}:{source_branch}&state=open",
                            ctx.repo_display
                        ),
                    ]) {
                        Ok(pr) => pr,
                        Err(e) => {
                            emit_result_keyed(
                                ctx,
                                intent,
                                key,
                                HostActionStatus::Unknown {
                                    reason: format!(
                                        "pr create reported {url} but read-back failed, effect unverified: {e}"
                                    ),
                                },
                                vec![],
                            );
                            return Ok(());
                        }
                    };
                    // The read-back must show exactly one change on the
                    // intent's own base carrying this intent's marker —
                    // none, or several, leave the effect unverified
                    // rather than borrowing a change's identity.
                    let matches: Vec<serde_json::Value> = pr
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter(|p| {
                                    p["base"]["ref"].as_str() == Some(target_branch)
                                        && marker_matches(intent, p)
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let [first] = matches.as_slice() else {
                        let reason = if matches.is_empty() {
                            format!("pr create reported {url} but no matching change read back")
                        } else {
                            format!(
                                "pr create reported {url} but {} marker-bearing changes read back — ambiguous",
                                matches.len()
                            )
                        };
                        // The create reported success but no matching
                        // change reads back: the effect is unverified,
                        // never failed.
                        emit_result_keyed(
                            ctx,
                            intent,
                            key,
                            HostActionStatus::Unknown { reason },
                            vec![],
                        );
                        return Ok(());
                    };
                    let observed_target = first["base"]["ref"]
                        .as_str()
                        .unwrap_or(target_branch)
                        .to_string();
                    let (change, head) = match opened_change(intent, first) {
                        Ok(read) => read,
                        Err(e) => {
                            emit_result_keyed(
                                ctx,
                                intent,
                                key,
                                HostActionStatus::Unknown {
                                    reason: format!(
                                        "pr create reported {url} but read-back was unparseable, effect unverified: {e}"
                                    ),
                                },
                                vec![],
                            );
                            return Ok(());
                        }
                    };
                    let execution_id = Some(execution.id.clone());
                    let _ = ctx.emit(
                        Some((&intent.work_id, execution_id.as_ref())),
                        format!("head:{}:{head}", change.change_request.as_str()),
                        ObservationFact::ChangeHead {
                            change: change.clone(),
                            head: head.clone(),
                        },
                    );
                    emit_result_keyed(
                        ctx,
                        intent,
                        key,
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::ChangeOpened {
                            change,
                            head,
                            source_branch: source_branch.clone(),
                            target_branch: observed_target,
                        }],
                    );
                }
                Err(e) => emit_result_keyed(
                    ctx,
                    intent,
                    key,
                    HostActionStatus::Unknown {
                        reason: format!("pr create failed, effect unverified: {e}"),
                    },
                    vec![],
                ),
            }
        }
        // Operations the bridge does not implement stay explicit — an
        // `unsupported` receipt is terminal in Core, never an applied
        // receipt or a permission verdict.
        HostAction::UpdateChangeRequest { .. }
        | HostAction::PostComment { .. }
        | HostAction::PublishReviewFeedback { .. }
        | HostAction::ReplyToThread { .. }
        | HostAction::ResolveThread { .. }
        | HostAction::MarkReady { .. } => emit_result_keyed(
            ctx,
            intent,
            key,
            HostActionStatus::Unsupported {
                reason: format!("unsupported operation: {}", intent.action.name()),
            },
            vec![],
        ),
    }
    Ok(())
}

/// The change each Work's applied open receipt recorded, plus its
/// target branch — derived only from persisted evidence.
fn opened_changes(ctx: &Ctx, work_id: &WorkId) -> Result<Vec<(ChangeRequestRef, String)>, String> {
    let observations = ctx
        .store
        .transact_deferred(|tx| tx.observations_for_work(work_id))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for o in &observations {
        if let ObservationFact::HostActionResult { effects, .. } = &o.fact {
            for e in effects {
                if let NormalizedEffectFact::ChangeOpened {
                    change,
                    target_branch,
                    ..
                } = e
                {
                    out.push((change.clone(), target_branch.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// Latest emitted `ChangeHead` per change, for no-force-push evidence.
fn emitted_heads(ctx: &Ctx, work_id: &WorkId) -> Result<Vec<(String, String)>, String> {
    let observations = ctx
        .store
        .transact_deferred(|tx| tx.observations_for_work(work_id))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for o in &observations {
        if let ObservationFact::ChangeHead { change, head } = &o.fact {
            out.push((
                change.change_request.as_str().to_string(),
                head.object.clone(),
            ));
        }
    }
    Ok(out)
}

/// Real-diff invariant assessment for the current head: writable paths
/// bounded to the seeded files, and the observed head descends from the
/// previously observed head (no force-push). Real checks against the
/// real clone — a positive verdict is never assumed.
fn assess_invariants(
    ctx: &Ctx,
    work: &Work,
    target: &DiffTarget,
    repo_dir: &Path,
) -> Result<ObservationFact, String> {
    let mut items = Vec::new();
    let allowed = ["sample.py", "tests/test_sample.py"];
    let names = git(
        repo_dir,
        &[
            "diff",
            "--name-only",
            &format!("origin/main..{}", target.head.object),
        ],
    )
    .unwrap_or_default();
    let bounded = !names.is_empty() && names.lines().all(|n| allowed.contains(&n.trim()));
    items.push(AssessmentItem {
        id: "s1.writable-paths-bounded".into(),
        verdict: if bounded {
            AssessmentVerdict::Positive
        } else {
            AssessmentVerdict::Negative
        },
        refs: vec![EvidenceRef {
            id: format!("diff:{}", target.head.object),
            digest: sha256_hex("s1-diff-v1", names.as_bytes()),
        }],
    });
    let prev = emitted_heads(ctx, &work.id)?
        .into_iter()
        .filter(|(cr, h)| cr == target.change.change_request.as_str() && h != &target.head.object)
        .map(|(_, h)| h)
        .next_back();
    let ff = match prev {
        Some(prev) => git(
            repo_dir,
            &["merge-base", "--is-ancestor", &prev, &target.head.object],
        )
        .is_ok(),
        None => true,
    };
    items.push(AssessmentItem {
        id: "s1.no-force-push".into(),
        verdict: if ff {
            AssessmentVerdict::Positive
        } else {
            AssessmentVerdict::Negative
        },
        refs: vec![],
    });
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    Ok(ObservationFact::Assessment {
        target: target.clone(),
        set_digest: sha256_hex(
            "s1-invariants-v1",
            canonical_json(&serde_json::json!(ids)).as_slice(),
        ),
        complete: true,
        items,
    })
}

/// Parse the machine-readable finding set a posted review body carries.
/// A missing block is a clean review; a present-but-malformed block is
/// an error, so the review can never be recorded as complete-clean by
/// failing open.
fn parse_findings(body: &str) -> Result<Vec<OccurrenceInput>, String> {
    let Some(start) = body.find("```s1-findings") else {
        return Ok(vec![]);
    };
    let rest = &body[start + "```s1-findings".len()..];
    let Some(end) = rest.find("```") else {
        return Err("s1-findings block has no closing fence".into());
    };
    let items = serde_json::from_str::<Vec<serde_json::Value>>(rest[..end].trim())
        .map_err(|e| format!("s1-findings block is not valid json: {e}"))?;
    Ok(items
        .iter()
        .map(|it| {
            let summary = it["summary"].as_str().unwrap_or_default();
            OccurrenceInput {
                source_key: it["source_key"].as_str().unwrap_or_default().to_string(),
                content_digest: Some(sha256_hex("s1-finding-v1", summary.as_bytes())),
                severity: it["severity"].as_str().map(str::to_string),
                position: it["position"].as_str().map(str::to_string),
                thread_id: None,
                summary: if summary.is_empty() {
                    None
                } else {
                    Some(summary.to_string())
                },
            }
        })
        .collect())
}

/// Delivery id for a Work-subject branch observation. The store dedupes
/// deliveries globally per `(scope, source, event_id)` — the subject
/// Work is not part of the key — so the id must carry the Work identity.
/// Two Works observing the same `(branch, position)` are distinct
/// subject observations; an unscoped id drops the later one as
/// `Duplicate`, starving its publish guard or target revision (#196).
///
/// Every `BranchState` the bridge emits goes through this helper: the
/// target-branch poll that feeds `current_target`, and the push prior-ref
/// read-back that feeds the next publish guard. Neither may reuse
/// another Work's delivery identity.
fn branch_event_id(work_id: &WorkId, branch: &str, position: &str) -> String {
    format!("branch:{}:{branch}:{position}", work_id.0)
}

/// Delivery id for a polled target-branch observation — the branch-poll
/// caller of [`branch_event_id`], kept named for the regression test
/// that pins the target-revision path (#186, #196).
fn branch_poll_event_id(work_id: &WorkId, target_branch: &str, sha: &str) -> String {
    branch_event_id(work_id, target_branch, sha)
}

/// Poll the real remote facts for every Work that has an opened change:
/// target-branch revision, current head, check runs, submitted reviews,
/// and the invariant assessment — each bound to the exact head it
/// describes.
fn poll_remote(ctx: &Ctx) -> Result<(), String> {
    let works = ctx
        .store
        .transact_deferred(|tx| tx.works())
        .map_err(|e| e.to_string())?;
    for work in works {
        let changes = opened_changes(ctx, &work.id)?;
        let execution = ctx
            .store
            .transact_deferred(|tx| tx.active_execution(&work.id))
            .map_err(|e| e.to_string())?;
        for (change, target_branch) in changes {
            let Some(pr_number) = change.display_number else {
                continue;
            };
            // Target-branch revision first, so every later fact's diff
            // target binds the same real base revision.
            if let Ok(main_sha) = gh(&[
                "api",
                &format!("repos/{}/branches/{target_branch}", ctx.repo_display),
                "--jq",
                ".commit.sha",
            ]) && let Ok(revision) =
                CommitRevision::new(work.binding.repository.clone(), main_sha.clone())
            {
                let _ = ctx.emit(
                    Some((&work.id, execution.as_ref().map(|e| &e.id))),
                    branch_poll_event_id(&work.id, &target_branch, &main_sha),
                    ObservationFact::BranchState {
                        repository: work.binding.repository.clone(),
                        branch: target_branch.clone(),
                        state: BranchState::At {
                            revision: revision.clone(),
                        },
                    },
                );
                let main_revision = Some(revision);
                let pr = gh_json(&[
                    "api",
                    &format!("repos/{}/pulls/{pr_number}", ctx.repo_display),
                ])?;
                let head_sha = pr["head"]["sha"].as_str().unwrap_or_default().to_string();
                if head_sha.is_empty() {
                    continue;
                }
                let head = CommitRevision::new(work.binding.repository.clone(), head_sha.clone())?;
                let _ = ctx.emit(
                    Some((&work.id, execution.as_ref().map(|e| &e.id))),
                    format!("head:{}:{head_sha}", change.change_request.as_str()),
                    ObservationFact::ChangeHead {
                        change: change.clone(),
                        head: head.clone(),
                    },
                );
                let check_runs = gh_json(&[
                    "api",
                    &format!("repos/{}/commits/{head_sha}/check-runs", ctx.repo_display),
                    "--jq",
                    ".check_runs",
                ])
                .unwrap_or(serde_json::json!([]));
                if let Some(runs) = check_runs.as_array() {
                    for run in runs {
                        let name = run["name"].as_str().unwrap_or_default();
                        let status = run["status"].as_str().unwrap_or_default();
                        let conclusion = match (status, run["conclusion"].as_str()) {
                            ("completed", Some("success")) => CheckConclusion::Success,
                            ("completed", _) => CheckConclusion::Failure,
                            _ => CheckConclusion::Pending,
                        };
                        let _ = ctx.emit(
                            Some((&work.id, execution.as_ref().map(|e| &e.id))),
                            format!(
                                "check:{head_sha}:{name}:{}",
                                run["conclusion"].as_str().unwrap_or(status)
                            ),
                            ObservationFact::Check {
                                change: change.clone(),
                                head: head.clone(),
                                check_id: name.to_string(),
                                producer: HostActorRef {
                                    host: ctx.config.host.repository.host.clone(),
                                    actor: HostActorId::trusted("github-actions[bot]"),
                                },
                                conclusion,
                            },
                        );
                    }
                }
                let reviews = gh_json(&[
                    "api",
                    &format!("repos/{}/pulls/{pr_number}/reviews", ctx.repo_display),
                ])
                .unwrap_or(serde_json::json!([]));
                if let Some(list) = reviews.as_array() {
                    for review in list {
                        let id = review["id"].as_u64().unwrap_or(0);
                        let submitted = review["submitted_at"].as_str().unwrap_or_default();
                        let commit_id = review["commit_id"].as_str().unwrap_or_default();
                        if id == 0 || submitted.is_empty() || commit_id.is_empty() {
                            continue;
                        }
                        let reviewer = review["user"]["login"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_string();
                        let Ok(rev_head) = CommitRevision::new(
                            work.binding.repository.clone(),
                            commit_id.to_string(),
                        ) else {
                            continue;
                        };
                        // A malformed findings block makes the review
                        // partial — never a clean complete round.
                        let (completeness, occurrences) =
                            match parse_findings(review["body"].as_str().unwrap_or_default()) {
                                Ok(occurrences) => (Completeness::Complete, occurrences),
                                Err(reason) => (Completeness::Partial { reason }, vec![]),
                            };
                        let _ = ctx.emit(
                            Some((&work.id, execution.as_ref().map(|e| &e.id))),
                            format!("review:{}:{id}", change.change_request.as_str()),
                            ObservationFact::Review {
                                target: DiffTarget {
                                    change: change.clone(),
                                    head: rev_head,
                                    base: None,
                                    target_revision: main_revision.clone(),
                                },
                                source: reviewer,
                                source_revision: Some(id.to_string()),
                                completeness,
                                occurrences,
                            },
                        );
                    }
                }
                // Invariant assessment on the current real head: real
                // diff checks, verdict never assumed.
                let repo_dir = ctx.workspace_of(&work.id);
                if repo_dir.join(".git").exists() {
                    let _ = git(
                        &repo_dir,
                        &[
                            "fetch",
                            "--quiet",
                            "origin",
                            target_branch.as_str(),
                            head_sha.as_str(),
                        ],
                    );
                    let target = DiffTarget {
                        change: change.clone(),
                        head: head.clone(),
                        base: None,
                        target_revision: main_revision.clone(),
                    };
                    if let Ok(fact) = assess_invariants(ctx, &work, &target, &repo_dir) {
                        // The delivery id keys on the full fact content,
                        // not just the item-id set: a re-assessment with
                        // a changed verdict or a moved target revision is
                        // a new fact to record, never a delivery-id reuse
                        // the store must reject.
                        let key = sha256_hex(
                            "s1-assess-event-v1",
                            canonical_json(&serde_json::to_value(&fact).unwrap_or_default())
                                .as_slice(),
                        );
                        let _ = ctx.emit(
                            Some((&work.id, execution.as_ref().map(|e| &e.id))),
                            format!("assess:{head_sha}:{key}"),
                            fact,
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn tick(ctx: &Ctx, reconcile_bound: usize) -> Result<(), String> {
    ensure_capability_report(ctx)?;
    source_reads(ctx)?;
    drain_outbox(ctx, reconcile_bound)?;
    poll_remote(ctx)
}

fn main() {
    let cli = Cli::parse();
    let installation = match load_or_create(&cli.config) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("s1-bridge: config: {e}");
            std::process::exit(2);
        }
    };
    let config = match installation.workloop_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("s1-bridge: config: {e}");
            std::process::exit(2);
        }
    };
    let store = match SqliteStore::open(&cli.db, installation.capacity) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("s1-bridge: store: {e}");
            std::process::exit(2);
        }
    };
    let repo_display = config.host.repository.repository.as_str().to_string();
    let login = gh(&["api", "user", "--jq", ".login"]).unwrap_or_else(|_| "unknown".to_string());
    let authenticated = HostActorRef {
        host: config.host.repository.host.clone(),
        actor: HostActorId::trusted(login),
    };
    let ctx = Ctx {
        store,
        config,
        workspace_root: PathBuf::from(&installation.workspace_root),
        remote: format!("https://github.com/{repo_display}.git"),
        repo_display,
        authenticated,
    };
    match cli.command {
        Commands::IssueRef { issue } => match issue_identity(&ctx.repo_display, issue) {
            Ok((revision, digest, state, num)) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "issue": format!("{}:{}", ctx.repo_display, num),
                        "source_revision": revision,
                        "content_digest": digest,
                        "state": match state { IssueState::Open => "open", IssueState::Closed => "closed", IssueState::Unavailable => "unavailable" },
                    }))
                    .expect("serializes")
                );
            }
            Err(e) => {
                eprintln!("s1-bridge: issue read failed: {e}");
                std::process::exit(1);
            }
        },
        Commands::Tick { .. } => {}
    }
    if let Commands::Tick {
        ticks,
        interval_ms,
        reconcile_bound,
    } = cli.command
    {
        let mut remaining = ticks;
        loop {
            if let Err(e) = tick(&ctx, reconcile_bound) {
                eprintln!("s1-bridge: tick failed: {e}");
            }
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
    use workloop_core::identity::{
        ActorRole, AdapterId, CredentialRef, HostInstanceId, RepositoryId, RepositoryRef,
        RoleBinding,
    };
    use workloop_core::work::{HostTemplate, handle_command};

    const SCENARIO_REPO: &str = "shimpeiws/coding-worker-smoke";

    fn test_ctx() -> Ctx {
        let host = HostInstanceId::trusted("test-host");
        let repository = RepositoryRef::new(host.clone(), RepositoryId::trusted("test-repo"));
        let publisher = HostActorRef {
            host: host.clone(),
            actor: HostActorId::trusted("publisher"),
        };
        Ctx {
            store: SqliteStore::in_memory(1).unwrap(),
            config: WorkloopConfig {
                host: HostTemplate {
                    adapter: AdapterId::trusted("s1-bridge"),
                    repository: repository.clone(),
                    contract_version: 1,
                    actors: vec![RoleBinding {
                        role: ActorRole::Publication,
                        actor: publisher.clone(),
                        credential_ref: CredentialRef::trusted("cred-publication"),
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
            },
            workspace_root: PathBuf::from("/tmp"),
            remote: "https://example.invalid/repo.git".into(),
            repo_display: "example/repo".into(),
            authenticated: publisher,
        }
    }

    fn submit_work(ctx: &Ctx, key: &str) -> WorkId {
        let decision = handle_command(
            &ctx.store,
            &ctx.config,
            &CommandRequest {
                actor: Actor {
                    kind: ActorKind::Human,
                    external_id: "op".into(),
                },
                origin: Origin("test".into()),
                idempotency_key: key.into(),
                body: CommandBody::Submit {
                    intent: "poll the target branch".into(),
                    sources: vec![],
                    constraints: vec![],
                    verification_argv: vec![],
                },
            },
            now(),
        )
        .unwrap();
        match decision {
            CommandDecision::Accepted { work_id, .. } => work_id.expect("accepted work"),
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    /// Regression (#186): two Works polling the same repository while
    /// the target branch sits at the same commit must each record their
    /// own `BranchState` observation — the delivery id is scoped per
    /// Work because store dedupe is global per `(scope, source,
    /// event_id)` and does not include the subject.
    #[test]
    fn target_branch_poll_records_each_work_at_the_same_sha() {
        let ctx = test_ctx();
        let w1 = submit_work(&ctx, "k-w1");
        let w2 = submit_work(&ctx, "k-w2");
        let repository = ctx.config.host.repository.clone();
        let sha = "30f43fa40dfa0cac4b4455c245516b10374285b0";
        let fact = || ObservationFact::BranchState {
            repository: repository.clone(),
            branch: "main".into(),
            state: BranchState::At {
                revision: CommitRevision::new(repository.clone(), sha).unwrap(),
            },
        };
        let poll = |work: &WorkId| {
            ctx.emit(
                Some((work, None)),
                branch_poll_event_id(work, "main", sha),
                fact(),
            )
            .unwrap()
        };
        assert_eq!(poll(&w1), IngestReceipt::Recorded);
        assert_eq!(
            poll(&w2),
            IngestReceipt::Recorded,
            "a second Work's identical target-branch fact must not dedupe \
             against the first Work's delivery"
        );
        assert_eq!(
            poll(&w1),
            IngestReceipt::Duplicate,
            "a single Work's unchanged re-poll still dedupes"
        );
        for work in [&w1, &w2] {
            let observations = ctx
                .store
                .transact_deferred(|tx| tx.observations_for_work(work))
                .unwrap();
            assert!(
                observations.iter().any(|o| matches!(
                    &o.fact,
                    ObservationFact::BranchState {
                        branch,
                        state: BranchState::At { revision },
                        ..
                    } if branch.as_str() == "main" && revision.object.as_str() == sha
                )),
                "work {} is missing its own target-branch observation",
                work.0
            );
        }
    }

    /// Regression (#196): the push prior-ref read-back is also a
    /// Work-subject `BranchState`. Two Works observing the same source
    /// branch at the same position must each record it; an unscoped id
    /// would leave the later Work's publish guard with no known prior.
    /// This drives the production `attempt` push path — not a copy of
    /// its emit — so restoring the unscoped delivery id there fails the
    /// test. The absent position and a different branch are distinct
    /// identities, so a branch that moves is never mistaken for a
    /// duplicate of its own earlier position.
    #[test]
    fn publish_prior_state_records_each_work_at_the_same_branch_position() {
        let root =
            std::env::temp_dir().join(format!("workloop-s1-bridge-prior-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let remote = root.join("remote.git");
        let remote_str = remote.to_str().unwrap();
        let run = |dir: &Path, args: &[&str]| {
            git(dir, args).unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")))
        };
        run(&root, &["init", "--bare", remote_str]);
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run(&seed, &["init"]);
        run(
            &seed,
            &["config", "user.email", "bridge-test@example.invalid"],
        );
        run(&seed, &["config", "user.name", "bridge test"]);
        std::fs::write(seed.join("README.md"), "seed\n").unwrap();
        run(&seed, &["add", "README.md"]);
        run(&seed, &["commit", "-m", "seed"]);
        let branch = "work/shared";
        run(
            &seed,
            &["push", remote_str, &format!("HEAD:refs/heads/{branch}")],
        );
        let sha = run(&seed, &["rev-parse", "HEAD"]);

        let mut ctx = test_ctx();
        ctx.workspace_root = root.clone();
        ctx.remote = remote_str.to_string();
        let w1 = submit_work(&ctx, "k-p1");
        let w2 = submit_work(&ctx, "k-p2");
        let repository = ctx.config.host.repository.clone();
        for work in [&w1, &w2] {
            let workspace = ctx.workspace_of(work);
            run(
                &root,
                &[
                    "clone",
                    "--branch",
                    branch,
                    remote_str,
                    workspace.to_str().unwrap(),
                ],
            );
            let mut intent = push_intent(&work.0, HostActionStatus::Pending, branch, false);
            intent.action_id = format!("act-{}", work.0);
            // The prior-ref observation is emitted before the
            // expected-prior guard. A deliberately mismatched guard
            // still exercises the real push attempt's read-back while
            // refusing the write, so the test needs no live `gh`.
            if let HostAction::PushRef { expected_prior, .. } = &mut intent.action {
                *expected_prior =
                    Some(CommitRevision::new(repository.clone(), "different-prior").unwrap());
            }
            attempt(&ctx, &intent, &format!("act:{}", work.0)).unwrap();
        }
        let all = ctx
            .store
            .transact_deferred(|tx| tx.all_observations())
            .unwrap();
        for work in [&w1, &w2] {
            assert!(
                all.iter().any(|o| o.subject_work_id.as_ref() == Some(work)
                    && matches!(
                        &o.fact,
                        ObservationFact::HostActionResult {
                            status: HostActionStatus::Failed { reason },
                            ..
                        } if reason == "stale_head"
                    )),
                "the mismatched guard must still refuse the write for work {}",
                work.0
            );
            let observations = ctx
                .store
                .transact_deferred(|tx| tx.observations_for_work(work))
                .unwrap();
            assert!(
                observations.iter().any(|o| matches!(
                    &o.fact,
                    ObservationFact::BranchState {
                        branch: b,
                        state: BranchState::At { revision },
                        ..
                    } if b == branch && revision.object.as_str() == sha
                )),
                "work {} is missing its own publish prior-ref observation",
                work.0
            );
        }
        assert_ne!(
            branch_event_id(&w1, branch, "absent"),
            branch_event_id(&w1, branch, &sha),
            "absent and present positions of one branch are distinct facts"
        );
        assert_ne!(
            branch_event_id(&w1, branch, "1111111111111111111111111111111111111111"),
            branch_event_id(&w1, branch, &sha),
            "a branch that moves to a new revision is a new fact"
        );
        assert_ne!(
            branch_event_id(&w1, "work/other", &sha),
            branch_event_id(&w1, branch, &sha),
            "the same position on another branch is a distinct fact"
        );
        assert_eq!(
            branch_poll_event_id(&w1, "main", &sha),
            branch_event_id(&w1, "main", &sha),
            "the target-branch poll shares the Work-scoped identity rule"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn push_intent(
        work: &str,
        status: HostActionStatus,
        branch: &str,
        with_effect: bool,
    ) -> HostActionIntent {
        let host = HostInstanceId::trusted("test-host");
        let repository = RepositoryRef::new(host.clone(), RepositoryId::trusted("test-repo"));
        let produced = CommitRevision::new(repository.clone(), "produced").unwrap();
        let effects = if with_effect {
            vec![NormalizedEffectFact::RefPublished {
                repository: repository.clone(),
                branch: branch.into(),
                produced: produced.clone(),
                observed: CommitRevision::new(repository.clone(), "observed").unwrap(),
                relation: RefRelation::Equal,
            }]
        } else {
            vec![]
        };
        HostActionIntent {
            action_id: "act-test".into(),
            work_id: WorkId(work.into()),
            host: host.clone(),
            repository,
            role: ActorRole::Publication,
            actor: HostActorRef {
                host: host.clone(),
                actor: HostActorId::trusted("publisher"),
            },
            action: HostAction::PushRef {
                branch: branch.into(),
                produced,
                expected_prior: None,
            },
            payload_digest: "digest".into(),
            status,
            authenticated_actor: None,
            effects,
            created_seq: 1,
        }
    }

    #[test]
    fn partial_effect_injection_requires_a_recorded_landed_push() {
        let work = WorkId("work-test".into());
        let applied = push_intent("work-test", HostActionStatus::Applied, "work/test", true);
        assert!(push_landed_in(
            std::slice::from_ref(&applied),
            &work,
            "work/test"
        ));

        let pending = push_intent("work-test", HostActionStatus::Pending, "work/test", true);
        assert!(!push_landed_in(&[pending], &work, "work/test"));
        assert!(!push_landed_in(
            std::slice::from_ref(&applied),
            &work,
            "work/other"
        ));
        assert!(!push_landed_in(
            std::slice::from_ref(&applied),
            &WorkId("work-other".into()),
            "work/test"
        ));
        let no_effect = push_intent("work-test", HostActionStatus::Applied, "work/test", false);
        assert!(!push_landed_in(&[no_effect], &work, "work/test"));
    }

    #[test]
    fn fault_hooks_require_scenario_opt_in_for_this_repo() {
        assert!(fault_hook_armed_with(
            SCENARIO_REPO,
            Some(SCENARIO_REPO),
            Some("push_ref"),
            "push_ref",
            true,
        ));
        assert!(!fault_hook_armed_with(
            SCENARIO_REPO,
            None,
            Some("push_ref"),
            "push_ref",
            true,
        ));
        assert!(!fault_hook_armed_with(
            SCENARIO_REPO,
            Some("someone/else"),
            Some("push_ref"),
            "push_ref",
            true,
        ));
        assert!(!fault_hook_armed_with(
            SCENARIO_REPO,
            Some(SCENARIO_REPO),
            Some("open_pr"),
            "push_ref",
            true,
        ));
        assert!(!fault_hook_armed_with(
            SCENARIO_REPO,
            Some(SCENARIO_REPO),
            None,
            "push_ref",
            true,
        ));
        assert!(!fault_hook_armed_with(
            SCENARIO_REPO,
            Some(SCENARIO_REPO),
            Some("push_ref"),
            "push_ref",
            false,
        ));
    }
}
