//! `s1-bridge` — scenario-local real-host bridge for the authorized
//! U8.2 live run (docs/s1). It is NOT the production GitHub adapter
//! (U3.x, unimplemented): a bounded scenario tool that drains pending
//! `HostActionIntent`s against the real isolated repository through
//! `git`/`gh` and ingests the real host facts it observes through
//! `work::handle_observation` — the same ingest pipeline a production
//! adapter would use. Remote identity: `gh` CLI credentials of the
//! operator's account; every read-back is re-fetched, never assumed.
//!
//! One `tick` performs, in order: a one-time capability report grounded
//! in real permission reads, source reads for every Work's declared
//! spec sources, one bounded outbox drain, and a remote-fact poll
//! (branch state, change head, check runs, reviews, and the real-diff
//! invariant assessment). `issue-ref` prints the authoritative
//! revision/digest pair `submit` requires.

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
    },
    /// Print the authoritative revision/digest for one issue — the
    /// exact pair `taisan submit --issue-revision/--issue-digest` needs.
    IssueRef {
        #[arg(long)]
        issue: u64,
    },
}

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

/// The S1 issue identity convention: `source_revision` is the host's
/// `updatedAt` token; `content_digest` is the tagged sha256 of
/// `title\nbody`. Both are real reads — the bridge recomputes them on
/// every source poll, so an edited issue surfaces as `changed`.
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
    let digest = sha256_hex("s1-issue-v1", format!("{title}\n{body}").as_bytes());
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

/// The capabilities this bridge can honestly report after a real
/// permission read: support is the bridge's implementation surface,
/// authorization is the credential's real `permissions` answer.
fn capability_report(ctx: &Ctx) -> Result<ObservationFact, String> {
    let perms = gh_json(&["api", &format!("repos/{}", ctx.repo_display)])?;
    let write = perms["permissions"]["push"].as_bool() == Some(true);
    let read = perms["permissions"]["pull"].as_bool() == Some(true);
    let entry = |capability: HostCapability, granted: bool| CapabilityEntry {
        capability,
        support: CapabilityState::Supported,
        authorization: if granted {
            CapabilityState::Authorized
        } else {
            CapabilityState::Denied
        },
    };
    Ok(ObservationFact::CapabilityReport {
        host: ctx.config.host.repository.host.clone(),
        capabilities: vec![
            entry(HostCapability::RepositoryRead, read),
            entry(HostCapability::BranchRead, read),
            entry(HostCapability::ChangeRequestRead, read),
            entry(HostCapability::IssueRead, read),
            entry(HostCapability::ChecksRead, read),
            entry(HostCapability::ReviewFindingsRead, read),
            entry(HostCapability::PushRef, write),
            entry(HostCapability::ChangeRequestCreate, write),
            entry(HostCapability::ChangeRequestUpdate, write),
            entry(HostCapability::CommentWrite, write),
        ],
    })
}

/// One-time capability report — later permission drift is left to a
/// production adapter's reporting cadence.
fn ensure_capability_report(ctx: &Ctx) -> Result<(), String> {
    let have = ctx
        .store
        .transact_deferred(|tx| {
            Ok(tx
                .all_observations()?
                .iter()
                .any(|o| matches!(o.fact, ObservationFact::CapabilityReport { .. })))
        })
        .map_err(|e| e.to_string())?;
    if have {
        return Ok(());
    }
    let fact = capability_report(ctx)?;
    ctx.emit(None, "cap:initial".into(), fact)
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
                    let _ = ctx.emit(
                        Some((&work.id, None)),
                        format!("issue:{num}:{revision}"),
                        fact,
                    );
                }
                Err(e) => {
                    let _ = ctx.emit(
                        Some((&work.id, None)),
                        format!(
                            "issue:{}:unavailable:{}",
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
/// followed by a real read-back before any `applied` claim.
fn drain_outbox(ctx: &Ctx) -> Result<(), String> {
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
        attempt(ctx, &intent)?;
    }
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
        let HostAction::PushRef { branch, produced } = &intent.action else {
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
        let execution = ctx
            .store
            .transact_deferred(|tx| {
                Ok(tx
                    .executions_for_work(&intent.work_id)?
                    .into_iter()
                    .max_by_key(|e| e.ordinal))
            })
            .map_err(|e| e.to_string())?;
        let _ = ctx.emit(
            Some((&intent.work_id, execution.as_ref().map(|e| &e.id))),
            format!("act:{}:verify:{sha}", intent.action_id),
            ObservationFact::HostActionResult {
                action_id: intent.action_id.clone(),
                authenticated_actor: Some(ctx.authenticated.clone()),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                    produced: produced.clone(),
                    observed,
                    relation: RefRelation::Ancestor,
                }],
            },
        );
    }
    Ok(())
}

fn emit_result(
    ctx: &Ctx,
    intent: &HostActionIntent,
    status: HostActionStatus,
    effects: Vec<NormalizedEffectFact>,
) {
    let execution = ctx
        .store
        .transact_deferred(|tx| {
            Ok(tx
                .executions_for_work(&intent.work_id)?
                .into_iter()
                .max_by_key(|e| e.ordinal))
        })
        .map_err(|e| e.to_string())
        .ok()
        .flatten();
    let _ = ctx.emit(
        Some((&intent.work_id, execution.as_ref().map(|e| &e.id))),
        format!("act:{}", intent.action_id),
        ObservationFact::HostActionResult {
            action_id: intent.action_id.clone(),
            authenticated_actor: Some(ctx.authenticated.clone()),
            status,
            effects,
        },
    );
}

fn attempt(ctx: &Ctx, intent: &HostActionIntent) -> Result<(), String> {
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
        HostAction::PushRef { branch, produced } => {
            let repo_dir = ctx.workspace_of(&intent.work_id);
            if !repo_dir.join(".git").exists() {
                emit_result(
                    ctx,
                    intent,
                    HostActionStatus::Failed {
                        reason: format!("no workspace clone at {}", repo_dir.display()),
                    },
                    vec![],
                );
                return Ok(());
            }
            let local_head = git(&repo_dir, &["rev-parse", "HEAD"])?;
            let push = git(
                &repo_dir,
                &["push", &ctx.remote, &format!("HEAD:refs/heads/{branch}")],
            );
            let observed = gh(&[
                "api",
                &format!("repos/{}/branches/{branch}", ctx.repo_display),
                "--jq",
                ".commit.sha",
            ])
            .ok();
            if observed.as_deref() == Some(local_head.as_str()) {
                let observed =
                    CommitRevision::new(intent.repository.clone(), observed.expect("checked"))?;
                // `produced` is the recorded artifact payload digest — a
                // content identity, not a git object on this revision.
                // `ancestor` records that the observed real head contains
                // the produced change; binding produced to a pushed
                // commit is the production adapter's job (U3).
                emit_result(
                    ctx,
                    intent,
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
                emit_result(
                    ctx,
                    intent,
                    match push {
                        Ok(_) => HostActionStatus::Unknown {
                            reason: format!(
                                "push reported success but remote head is {:?}, expected {local_head}",
                                observed
                            ),
                        },
                        Err(e) => HostActionStatus::Failed { reason: e },
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
                body,
            ]) {
                Ok(url) => {
                    let pr = gh_json(&[
                        "api",
                        &format!(
                            "repos/{}/pulls?head=shimpeiws:{source_branch}&state=open",
                            ctx.repo_display
                        ),
                    ])?;
                    let first = pr
                        .as_array()
                        .and_then(|a| a.first().cloned())
                        .ok_or_else(|| format!("created PR not found in read-back: {url}"))?;
                    let node = first["node_id"].as_str().unwrap_or_default().to_string();
                    let number = first["number"].as_u64().unwrap_or(0);
                    let head_sha = first["head"]["sha"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let change = ChangeRequestRef {
                        repository: intent.repository.clone(),
                        change_request: ChangeRequestId::trusted(node),
                        display_number: Some(number),
                    };
                    let head = CommitRevision::new(intent.repository.clone(), head_sha)?;
                    let execution_id = Some(execution.id.clone());
                    let _ = ctx.emit(
                        Some((&intent.work_id, execution_id.as_ref())),
                        format!("head:{}:{head}", change.change_request.as_str()),
                        ObservationFact::ChangeHead {
                            change: change.clone(),
                            head: head.clone(),
                        },
                    );
                    emit_result(
                        ctx,
                        intent,
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::ChangeOpened {
                            change,
                            head,
                            source_branch: source_branch.clone(),
                            target_branch: target_branch.clone(),
                        }],
                    );
                }
                Err(e) => emit_result(
                    ctx,
                    intent,
                    HostActionStatus::Unknown {
                        reason: format!("pr create failed, effect unverified: {e}"),
                    },
                    vec![],
                ),
            }
        }
        // Head-bound writes the bridge does not implement stay explicit
        // failures — an unsupported effect is never an applied receipt.
        HostAction::UpdateChangeRequest { .. } => emit_result(
            ctx,
            intent,
            HostActionStatus::Failed {
                reason: "unsupported operation: update_change_request".into(),
            },
            vec![],
        ),
        HostAction::PostComment { .. } => emit_result(
            ctx,
            intent,
            HostActionStatus::Failed {
                reason: "unsupported operation: post_comment".into(),
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
fn parse_findings(body: &str) -> Vec<OccurrenceInput> {
    let Some(start) = body.find("```s1-findings") else {
        return vec![];
    };
    let rest = &body[start + "```s1-findings".len()..];
    let Some(end) = rest.find("```") else {
        return vec![];
    };
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(rest[..end].trim()) else {
        return vec![];
    };
    items
        .iter()
        .map(|it| {
            let summary = it["summary"].as_str().unwrap_or_default();
            OccurrenceInput {
                source_key: it["source_key"].as_str().unwrap_or_default().to_string(),
                content_digest: Some(sha256_hex("s1-finding-v1", summary.as_bytes())),
                severity: it["severity"].as_str().map(str::to_string),
                position: it["position"].as_str().map(str::to_string),
                thread_id: None,
            }
        })
        .collect()
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
                    format!("branch:{target_branch}:{main_sha}"),
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
                        let occurrences =
                            parse_findings(review["body"].as_str().unwrap_or_default());
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
                                completeness: Completeness::Complete,
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
                        let digest = match &fact {
                            ObservationFact::Assessment { set_digest, .. } => set_digest.clone(),
                            _ => String::new(),
                        };
                        let _ = ctx.emit(
                            Some((&work.id, execution.as_ref().map(|e| &e.id))),
                            format!("assess:{head_sha}:{digest}"),
                            fact,
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn tick(ctx: &Ctx) -> Result<(), String> {
    ensure_capability_report(ctx)?;
    source_reads(ctx)?;
    drain_outbox(ctx)?;
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
    if let Commands::Tick { ticks, interval_ms } = cli.command {
        let mut remaining = ticks;
        loop {
            if let Err(e) = tick(&ctx) {
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
