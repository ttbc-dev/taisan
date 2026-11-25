//! The `work` decision core: evaluates Commands against the store snapshot
//! and emits `Effect`s the store applies in the same transaction. The store
//! enforces structure; everything here is policy — transition legality,
//! versions, budgets, capability guards, and gates.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::command::{
    CommandBody, CommandDecision, CommandEffect, CommandId, CommandRequest, FindingChoice,
    RejectionCode, WorkVersion, canonical_payload, payload_digest,
};
use crate::domain::*;
use crate::identity::{
    ActorRole, CommitRevision, DiffTarget, EvidenceRef, HostActorRef, HostBinding, HostCapability,
    HostInstanceId, IssueRef, RepositoryRef, RoleBinding, SourceGate, SourceSnapshot,
    canonical_host_instance,
};
use crate::observation::{
    ArtifactFact, BranchState, CheckConclusion, Completeness, HostActionStatus, IssueState,
    NormalizedEffectFact, Observation, ObservationFact, Outcome, ProcessStateFact, RefRelation,
};
use crate::store::{
    Effect, HostAction, HostActionIntent, IntakeSettingsRecord, ReadView, RecoveryIntent,
    RecoveryIntentState, Store, StoreError, TxView,
};

/// The resolved installation binding template: which host installation,
/// repository, adapter contract, and role actors every accepted Work
/// binds to. The Work's binding is a frozen copy recorded at acceptance;
/// later installation changes cannot retarget it.
#[derive(Debug, Clone)]
pub struct HostTemplate {
    pub adapter: crate::identity::AdapterId,
    pub repository: RepositoryRef,
    pub contract_version: u32,
    /// One binding per role; `publication` is required for any Work
    /// that may publish.
    pub actors: Vec<RoleBinding>,
}

/// The pending-queue admission bound used when the store holds no
/// active intake-settings record — the documented default from
/// `docs/installation-config.md`. The profile loader defaults to the
/// same value so a configured limit and an unconfigured store can
/// never disagree about the floor.
pub const DEFAULT_PENDING_QUEUE_LIMIT: u32 = 10;

/// Installation-level configuration supplied by the composition root.
#[derive(Debug, Clone)]
pub struct WorkloopConfig {
    pub host: HostTemplate,
    /// Capabilities the installation declares as supported and
    /// authorized. A Work whose required set is not covered is rejected
    /// at submit; runtime capability reports may further deny.
    pub declared_capabilities: BTreeSet<HostCapability>,
    /// Lane policy stamped into every spec snapshot.
    pub lanes: LanePolicy,
    /// Named host checks the completion gate requires on the current
    /// target; stamped into every spec snapshot.
    pub required_checks: Vec<String>,
    pub allowed_effects: AllowedEffects,
    pub budget: ExecutionBudget,
    /// The Work-wide recovery policy stamped at acceptance.
    pub policy: WorkPolicy,
}

/// The capability set every publishable Work needs, plus `issue_read`
/// for source-bound Work.
fn required_capabilities(sources: &[SourceSnapshot]) -> BTreeSet<HostCapability> {
    let mut caps = BTreeSet::from([
        HostCapability::RepositoryRead,
        HostCapability::PushRef,
        HostCapability::ChangeRequestCreate,
        HostCapability::ChangeRequestRead,
        HostCapability::ChecksRead,
        HostCapability::ReviewFindingsRead,
    ]);
    if !sources.is_empty() {
        caps.insert(HostCapability::IssueRead);
    }
    caps
}

/// The capability a host action requires.
fn action_capability(action: &HostAction) -> HostCapability {
    match action {
        HostAction::PushRef { .. } => HostCapability::PushRef,
        HostAction::OpenChangeRequest { .. } => HostCapability::ChangeRequestCreate,
        HostAction::UpdateChangeRequest { .. } => HostCapability::ChangeRequestUpdate,
        HostAction::PostComment { .. } => HostCapability::CommentWrite,
        HostAction::PublishReviewFeedback { .. } => HostCapability::ReviewFeedback,
        HostAction::ReplyToThread { .. } => HostCapability::ThreadReply,
        HostAction::ResolveThread { .. } => HostCapability::ThreadResolve,
        HostAction::MarkReady { .. } => HostCapability::MarkReady,
    }
}

/// Handle one Command: idempotency check → validation → decide → apply →
/// persist the decision. Runs in a single store transaction; a constraint
/// conflict triggers one re-read and re-decide before rejection.
pub fn handle_command(
    store: &impl Store,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Result<CommandDecision, StoreError> {
    let digest = payload_digest(&req.origin, &req.actor, &req.idempotency_key, &req.body);
    let canonical = canonical_payload(&req.origin, &req.actor, &req.idempotency_key, &req.body);

    store.transact(|tx| {
        if let Some((stored_digest, decision)) =
            tx.stored_decision(&req.origin, &req.actor, &req.idempotency_key)?
        {
            if stored_digest == digest {
                return Ok(decision);
            }
            // The original decision keeps the bare key for replay; the
            // conflict is durable under a payload-derived key. An identical
            // conflict replayed returns the saved decision.
            let conflict_key = format!("{}#{digest}", req.idempotency_key);
            if let Some((_, saved)) = tx.stored_decision(&req.origin, &req.actor, &conflict_key)? {
                return Ok(saved);
            }
            let decision = rejected(
                tx,
                RejectionCode::IdempotencyConflict,
                req.body.work_id().cloned(),
                vec!["same idempotency key with a different payload".into()],
            )?;
            tx.save_decision(
                &req.origin,
                &req.actor,
                &conflict_key,
                &digest,
                &canonical,
                &decision,
            )?;
            return Ok(decision);
        }

        let decision = decide_attempts(tx, config, req, now_unix_seconds)?;

        // A capacity refusal is transient, not a decision about facts:
        // persisting it under the caller's idempotency key would replay
        // the refusal forever — for a selector candidate the key is the
        // proposal's replay key, and deferral requires the lookup to
        // find nothing so a later proposal is decided fresh against
        // then-current capacity (docs/intake-identity-storage.md). Every
        // other outcome, accepted or rejected, keeps its durable record.
        if !matches!(
            decision,
            CommandDecision::Rejected {
                reason: RejectionCode::CapacityExhausted,
                ..
            }
        ) {
            tx.save_decision(
                &req.origin,
                &req.actor,
                &req.idempotency_key,
                &digest,
                &canonical,
                &decision,
            )?;
        }
        Ok(decision)
    })
}

/// Persist a new active intake-settings record under the revision
/// guard (#213). `expected_revision` is the revision the caller last
/// observed — `None` asserts the record is absent, `Some(rev)` asserts
/// it still carries `rev`. A stale expectation is a
/// `ConstraintConflict`, so a reload writer holding an older snapshot
/// can never overwrite a newer active limit. This is the only writer
/// of the record submit transactions read.
pub fn set_intake_settings(
    store: &impl Store,
    settings: IntakeSettingsRecord,
    expected_revision: Option<&str>,
) -> Result<(), StoreError> {
    store.transact(|tx| {
        tx.apply(&Effect::SetIntakeSettings {
            settings,
            expected_revision: expected_revision.map(str::to_string),
        })
    })
}

/// The TUI → `work` `preview` port: whether saved facts would admit the
/// Command right now. `unknown` means the facts cannot decide — distinct
/// from `blocked`, where they produce a definite rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewAvailability {
    Available,
    Blocked,
    Unknown,
}

/// Advisory preflight result. `reasons` names the rejection codes a
/// `submit` of the same request would produce; the observed version/head
/// let the caller display what its expected values were compared
/// against. Carrying a preview grants no authority: it reserves neither
/// capacity nor execution ownership, and the Command handler rechecks
/// every precondition at acceptance time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPreview {
    pub availability: PreviewAvailability,
    pub observed_work_version: Option<WorkVersion>,
    pub observed_head: Option<CommitRevision>,
    pub reasons: Vec<RejectionCode>,
    pub evidence: Vec<String>,
}

/// `work.preview(CommandRequest) → ActionPreview`. Runs the exact
/// decide+apply pass `handle_command` would — including effect
/// application inside the attempt savepoint, so store constraint
/// violations surface as the same rejection a submit would produce —
/// and reports the outcome without persisting it. The whole pass sits
/// inside an outer savepoint that is always rolled back, so no
/// decision, effect, ID, or sequence allocation survives. The
/// transaction begins deferred: the decision reads hold no writer
/// lock, so previewing does not busy-wait behind or block a writer —
/// only the simulated effect application upgrades, and a concurrent
/// writer surfaces `Err` rather than a frozen preview. An internal
/// store fault surfaces as `Err`, never as a rejection-looking preview.
pub fn preview(
    store: &impl Store,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Result<ActionPreview, StoreError> {
    store.transact_deferred(|tx| {
        // A stored decision is what a submit under this key returns: the
        // same payload replays it, a changed payload conflicts.
        let digest = payload_digest(&req.origin, &req.actor, &req.idempotency_key, &req.body);
        if let Some((stored_digest, decision)) =
            tx.stored_decision(&req.origin, &req.actor, &req.idempotency_key)?
        {
            if stored_digest == digest {
                return preview_of_decision(tx, req, &decision);
            }
            let mut p = observed(tx, req)?;
            p.availability = PreviewAvailability::Blocked;
            p.reasons = vec![RejectionCode::IdempotencyConflict];
            p.evidence = vec!["same idempotency key with a different payload".into()];
            return Ok(p);
        }
        tx.begin_attempt()?;
        let outcome = decide_attempts(tx, config, req, now_unix_seconds);
        // Roll back every allocation and effect the pass performed; a
        // preview failure still rolls the transaction back via transact.
        tx.revert_attempt()?;
        let decision = outcome?;
        preview_of_decision(tx, req, &decision)
    })
}

/// The decide+apply attempt loop `handle_command` and `preview` share.
/// Re-decide passes let a constraint conflict resolve against fresh
/// state; each attempt runs in a savepoint so partial effects are
/// rolled back before the next read. Returns the decision the Command
/// would produce — the caller decides whether to persist it.
fn decide_attempts(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Result<CommandDecision, StoreError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        tx.begin_attempt()?;
        match decide(tx, config, req, now_unix_seconds) {
            Ok((decision, effects)) => match apply_all(tx, &effects) {
                Ok(()) => {
                    tx.commit_attempt()?;
                    return Ok(decision);
                }
                Err(e) => {
                    tx.revert_attempt()?;
                    match conflict_rejection(tx, req, &e)? {
                        Some(rejection) => return Ok(rejection),
                        None if attempts < 3 => continue,
                        None => {
                            return Ok(rejected_now(
                                tx,
                                RejectionCode::Ineligible,
                                req.body.work_id().cloned(),
                                vec![e.to_string()],
                            ));
                        }
                    }
                }
            },
            Err(e) => {
                tx.revert_attempt()?;
                return Err(e);
            }
        }
    }
}

fn observed(tx: &dyn ReadView, req: &CommandRequest) -> Result<ActionPreview, StoreError> {
    let work_id = req.body.work_id();
    let observed_work_version = work_id
        .map(|w| tx.work(w))
        .transpose()?
        .flatten()
        .map(|w| w.version);
    let observed_head = work_id.map(|w| latest_head(tx, w)).transpose()?.flatten();
    Ok(ActionPreview {
        availability: PreviewAvailability::Unknown,
        observed_work_version,
        observed_head,
        reasons: vec![],
        evidence: vec![],
    })
}

fn preview_of_decision(
    tx: &dyn ReadView,
    req: &CommandRequest,
    decision: &CommandDecision,
) -> Result<ActionPreview, StoreError> {
    let mut p = observed(tx, req)?;
    match decision {
        CommandDecision::Accepted { .. } => p.availability = PreviewAvailability::Available,
        CommandDecision::Rejected {
            reason, evidence, ..
        } => {
            p.availability = match reason {
                // The facts are insufficient to decide — not a refusal
                // the operator could repair in the request.
                RejectionCode::SourceUnavailable | RejectionCode::OwnershipUncertain => {
                    PreviewAvailability::Unknown
                }
                _ => PreviewAvailability::Blocked,
            };
            p.reasons = vec![reason.clone()];
            p.evidence = evidence.clone();
        }
    }
    Ok(p)
}

fn apply_all(tx: &mut dyn TxView, effects: &[Effect]) -> Result<(), StoreError> {
    for e in effects {
        tx.apply(e)?;
    }
    Ok(())
}

/// Decide whether a store conflict is retryable (re-read and re-decide) or
/// a deterministic rejection.
fn conflict_rejection(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    err: &StoreError,
) -> Result<Option<CommandDecision>, StoreError> {
    let code = match err {
        StoreError::VersionConflict { .. } => Some(RejectionCode::StaleWorkVersion),
        // A lost race on reservations, slots, or uniqueness: the world
        // changed under the read — re-decide once before judging.
        StoreError::ConstraintConflict { .. }
        | StoreError::StateConflict { .. }
        | StoreError::NoFreeSlot => None,
        // An internal or format fault is not a decision about the work.
        // Retrying it and then persisting an Ineligible rejection would
        // harden a transient failure into a durable, forever-replayed
        // answer under the caller's idempotency key; it must reach the
        // caller instead.
        StoreError::Internal { .. } | StoreError::UnsupportedFormat { .. } => {
            return Err(err.clone());
        }
    };
    match code {
        Some(c) => Ok(Some(rejected_now(
            tx,
            c,
            req.body.work_id().cloned(),
            vec![err.to_string()],
        ))),
        None => Ok(None),
    }
}

fn rejected(
    tx: &mut dyn TxView,
    code: RejectionCode,
    work_id: Option<WorkId>,
    evidence: Vec<String>,
) -> Result<CommandDecision, StoreError> {
    Ok(rejected_now(tx, code, work_id, evidence))
}

fn rejected_now(
    tx: &mut dyn TxView,
    code: RejectionCode,
    work_id: Option<WorkId>,
    evidence: Vec<String>,
) -> CommandDecision {
    let current = work_id.as_ref().and_then(|w| tx.work(w).ok().flatten());
    CommandDecision::Rejected {
        command_id: CommandId(
            tx.alloc_id("cmd")
                .unwrap_or_else(|_| "cmd-unknown".to_string()),
        ),
        reason: code,
        current_work_id: work_id,
        current_version: current.map(|w| w.version),
        evidence,
        committed_at: tx.last_seq().unwrap_or(0),
    }
}

/// The version precondition shared by every existing-Work Command.
fn check_work(
    tx: &dyn ReadView,
    work_id: &WorkId,
    expected_version: WorkVersion,
) -> Result<Result<Work, CommandDecision>, StoreError> {
    let Some(work) = tx.work(work_id)? else {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::InvalidRequest,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence: vec!["unknown work_id".into()],
            committed_at: tx.last_seq()?,
        }));
    };
    if work.version != expected_version {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::StaleWorkVersion,
            current_work_id: Some(work_id.clone()),
            current_version: Some(work.version),
            evidence: vec![],
            committed_at: tx.last_seq()?,
        }));
    }
    Ok(Ok(work))
}

/// The `{workspace, (repository, branch)}` pair a Work's Executions reserve.
/// The branch reservation binds the Work's recorded repository — the
/// binding, not a caller-supplied path.
pub fn work_resources(work: &Work) -> Vec<WriteResource> {
    vec![
        WriteResource::Workspace {
            workspace_id: format!("ws-{}", work.id.0),
        },
        WriteResource::Branch {
            repository: work.binding.repository.clone(),
            branch: format!("work/{}", work.id.0),
        },
    ]
}

/// The latest head observed for the Work's change request, ordered by
/// observed time with insertion order breaking ties — the same rule
/// every head comparison uses.
fn latest_head(tx: &dyn ReadView, work_id: &WorkId) -> Result<Option<CommitRevision>, StoreError> {
    Ok(current_target(tx, work_id)?.map(|t| t.head))
}

/// The change request and target branch a *verified* receipt opened
/// for this Work: only an effect inside a `HostActionResult` whose
/// `action_id` names a recorded intent reconciled to `applied` counts.
/// A failed, unknown, or unrecorded receipt is evidence of an attempt —
/// never of an opened change.
pub(crate) fn applied_opened_change(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<Option<(crate::identity::ChangeRequestRef, String)>, StoreError> {
    let work = tx.work(work_id)?;
    let Some(work) = work else { return Ok(None) };
    let intents = tx.action_intents()?;
    // Only a receipt naming an applied `open_change_request` intent can
    // establish the diff target: a push or update receipt that happens to
    // carry a `ChangeOpened` fact is still evidence of ITS recorded
    // action, and the fact must match that intent's immutable payload.
    let applied_open: std::collections::HashMap<&str, &HostActionIntent> = intents
        .iter()
        .filter(|i| {
            i.work_id == *work_id
                && i.status == HostActionStatus::Applied
                && matches!(i.action, HostAction::OpenChangeRequest { .. })
        })
        .map(|i| (i.action_id.as_str(), i))
        .collect();
    let mut change: Option<(crate::identity::ChangeRequestRef, String)> = None;
    for o in tx.observations_for_work(work_id)? {
        if let ObservationFact::HostActionResult {
            action_id, effects, ..
        } = &o.fact
            && let Some(intent) = applied_open.get(action_id.as_str())
        {
            for e in effects {
                if let NormalizedEffectFact::ChangeOpened {
                    change: c,
                    target_branch,
                    ..
                } = e
                    && c.repository == work.binding.repository
                    && effect_matches(intent, std::slice::from_ref(e))
                {
                    change = Some((c.clone(), target_branch.clone()));
                }
            }
        }
    }
    Ok(change)
}

/// The Work's current diff target, derived from persisted evidence:
/// the change request its `open_change_request` receipt opened, the
/// latest observed head for it, and the latest target-branch revision.
/// `None` until the change request exists.
pub fn current_target(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<Option<DiffTarget>, StoreError> {
    let observations = tx.observations_for_work(work_id)?;
    let work = tx.work(work_id)?;
    let Some(work) = work else { return Ok(None) };
    let Some((change, target_branch)) = applied_opened_change(tx, work_id)? else {
        return Ok(None);
    };
    // The latest ChangeHead for that change, ordered by observed time
    // with insertion order breaking ties.
    let head = observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            if let ObservationFact::ChangeHead { change: c, head: h } = &o.fact
                && c == &change
            {
                Some((o.observed_unix_seconds, index, h.clone()))
            } else {
                None
            }
        })
        .max_by_key(|(t, i, _)| (*t, *i))
        .map(|(_, _, h)| h);
    let Some(head) = head else {
        return Ok(None);
    };
    // Latest target-branch revision, so a target-only move changes the
    // diff target without any head change.
    let target_revision = observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            if let ObservationFact::BranchState {
                repository,
                branch,
                state: BranchState::At { revision },
            } = &o.fact
                && repository == &work.binding.repository
                && branch == &target_branch
            {
                return Some((o.observed_unix_seconds, index, revision.clone()));
            }
            None
        })
        .max_by_key(|(t, i, _)| (*t, *i))
        .map(|(_, _, r)| r);
    Ok(Some(DiffTarget {
        change,
        head,
        base: None,
        target_revision,
    }))
}

/// The latest sealed round binding this exact diff target, if any.
/// Rounds are immutable per-target evidence: an out-of-order review for
/// a superseded target is recorded, but it never masks the current
/// target's evidence surface.
pub fn latest_round_for_target(
    tx: &dyn ReadView,
    work_id: &WorkId,
    target: &DiffTarget,
) -> Result<Option<ReviewRound>, StoreError> {
    Ok(tx
        .rounds_for_work(work_id)?
        .into_iter()
        .rfind(|r| &r.target == target))
}

type Verdict = Result<(CommandDecision, Vec<Effect>), StoreError>;

fn decide(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    req: &CommandRequest,
    now_unix_seconds: i64,
) -> Verdict {
    match &req.body {
        CommandBody::Submit {
            intent,
            sources,
            constraints,
            verification_argv,
        } => decide_submit(
            tx,
            config,
            req,
            intent,
            sources,
            constraints,
            verification_argv,
        ),
        CommandBody::ClaimResources {
            resources,
            ttl_seconds,
        } => decide_claim(tx, req, resources, *ttl_seconds, now_unix_seconds),
        CommandBody::ReleaseResources { resources } => decide_release(tx, req, resources),
        CommandBody::Retry {
            work_id,
            expected_work_version,
            expected_execution_id,
        } => decide_retry(
            tx,
            config,
            work_id,
            *expected_work_version,
            expected_execution_id,
            now_unix_seconds,
        ),
        CommandBody::Resume {
            work_id,
            expected_work_version,
            execution_id,
        } => decide_resume(
            tx,
            work_id,
            *expected_work_version,
            execution_id,
            now_unix_seconds,
        ),
        CommandBody::Cancel {
            work_id,
            expected_work_version,
            execution_id,
        } => decide_cancel(tx, work_id, *expected_work_version, execution_id.as_ref()),
        CommandBody::Acknowledge {
            work_id,
            expected_work_version,
            attention_id,
        } => decide_acknowledge(
            tx,
            req,
            work_id,
            *expected_work_version,
            attention_id,
            now_unix_seconds,
        ),
        CommandBody::JudgeFindings {
            work_id,
            expected_work_version,
            expected_head,
            decisions,
        } => decide_judge(
            tx,
            req,
            work_id,
            *expected_work_version,
            expected_head,
            decisions,
        ),
        CommandBody::ReleaseJudgment {
            work_id,
            expected_work_version,
            expected_head,
        } => decide_release_judgment(
            tx,
            work_id,
            *expected_work_version,
            expected_head,
            now_unix_seconds,
        ),
    }
}

fn decide_submit(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    req: &CommandRequest,
    intent: &str,
    sources: &[SourceSnapshot],
    constraints: &[String],
    verification_argv: &[Vec<String>],
) -> Verdict {
    // Host-instance identity is canonicalized once, at admission, before
    // any claim check or spec binding: the selector path derives source
    // repositories from profile endpoints (`https://api.github.com`)
    // while manual submission binds the installation's host instance
    // (`github.com`), and adapters may hand in either spelling. Without
    // a single canonical form here, one real Issue exists under two
    // identities and claims, dedupe, and the source gate diverge.
    // Claims and membership persisted before this canonicalization keep
    // their original spelling; the store's claim and owner reads apply
    // the same canonical form, so an upgraded store enforces the same
    // one-Issue-one-claim rule.
    let canonical_sources: Vec<SourceSnapshot> = sources
        .iter()
        .map(|s| {
            let mut s = s.clone();
            let canonical = canonical_host_instance(s.issue.repository.host.as_str());
            if !canonical.is_empty() {
                s.issue.repository.host = HostInstanceId::trusted(canonical);
            }
            s
        })
        .collect();
    let sources = &canonical_sources[..];
    // Capability guard: the binding's required set must be declared as
    // supported by this installation. An undeclared capability can never
    // silently authorize an effect later.
    let required = required_capabilities(sources);
    let missing: Vec<HostCapability> = required
        .iter()
        .filter(|c| !config.declared_capabilities.contains(*c))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Ok((
            rejected(
                tx,
                RejectionCode::UnsupportedCapability,
                None,
                missing.iter().map(|c| c.name().to_string()).collect(),
            )?,
            vec![],
        ));
    }
    // A source issue already claimed by live Work is a conflict — the
    // overlap rule that keeps one owner per issue.
    for s in sources {
        if let Some(owner) = tx.issue_claim(&s.issue)? {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Ineligible,
                    Some(owner.clone()),
                    vec![format!("issue {} already claimed by {}", s.issue, owner.0)],
                ),
                vec![],
            ));
        }
    }
    // Source issue membership must be unique within the submission.
    let seen: BTreeSet<&IssueRef> = sources.iter().map(|s| &s.issue).collect();
    if seen.len() != sources.len() {
        return Ok((
            rejected(
                tx,
                RejectionCode::InvalidRequest,
                None,
                vec!["duplicate source issue".into()],
            )?,
            vec![],
        ));
    }

    // The installation-wide pending-queue limit is enforced here, inside
    // the same transaction that inserts the Work, its queued Execution,
    // and the issue claims — never as a pre-check two concurrent
    // submitters could both pass. Manual and selector submits share
    // this path; the effective limit is the active settings record's,
    // or the documented default when no record exists. A grouped
    // proposal is one submit and so consumes exactly one position.
    let pending_limit = tx
        .intake_settings()?
        .map(|s| s.pending_queue_limit)
        .unwrap_or(DEFAULT_PENDING_QUEUE_LIMIT);
    let pending = tx.pending_queue_count()?;
    if pending >= u64::from(pending_limit) {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::CapacityExhausted,
                None,
                vec![format!(
                    "pending queue limit {pending_limit} reached: \
                     {pending} queued Works hold every position"
                )],
            ),
            vec![],
        ));
    }

    let binding = HostBinding::new(
        config.host.adapter.clone(),
        config.host.repository.clone(),
        config.host.contract_version,
        required,
        config.host.actors.clone(),
    )
    .map_err(|e| StoreError::Internal { detail: e })?;

    let seq = tx.alloc_seq()?;
    let spec_payload = SpecPayload {
        intent: intent.to_string(),
        sources: sources.to_vec(),
        constraints: constraints.to_vec(),
        verification_argv: verification_argv.to_vec(),
        required_checks: config.required_checks.clone(),
        lanes: config.lanes.clone(),
    };
    let payload_json = serde_json::to_value(&spec_payload).map_err(|e| StoreError::Internal {
        detail: format!("spec payload: {e}"),
    })?;
    let spec = SpecSnapshot {
        work_id: WorkId(String::new()),
        revision: 1,
        payload: spec_payload,
        digest: sha256_hex("spec-v1", &canonical_json(&payload_json)),
        approved_by: req.actor.clone(),
        recorded_seq: seq,
    };
    let work = Work {
        id: WorkId(tx.alloc_id("w")?),
        version: 1,
        binding,
        spec: 1,
        approved_spec: 1,
        source_gate: if sources.is_empty() {
            SourceGate::NotApplicable
        } else {
            SourceGate::NeedsCheck
        },
        policy: config.policy.clone(),
        created_seq: seq,
    };
    let mut spec = spec;
    spec.work_id = work.id.clone();
    let execution = new_execution(
        tx,
        config,
        &work,
        &spec.payload,
        0,
        seq,
        InvocationInput {
            input: serde_json::json!({
                "intent": spec.payload.intent,
                "constraints": spec.payload.constraints,
                "verification_argv": spec.payload.verification_argv,
            }),
            resume: false,
            next_execution_input: None,
            previous_execution_outcome: None,
        },
    )?;
    let issue_refs = spec
        .payload
        .sources
        .iter()
        .map(|s| WorkIssueRef {
            work_id: work.id.clone(),
            issue: s.issue.clone(),
        })
        .collect();
    let claims: Vec<IssueRef> = spec
        .payload
        .sources
        .iter()
        .map(|s| s.issue.clone())
        .collect();
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work.id.clone()),
            work_version: Some(work.version),
            effect: CommandEffect::WorkCreated {
                execution_id: execution.id.clone(),
            },
            committed_at: seq,
        },
        vec![
            Effect::InsertWork {
                work: work.clone(),
                spec,
                issue_refs,
                claims,
                usage: WorkUsage::new(&work.id),
            },
            Effect::InsertExecution { execution },
        ],
    ))
}

fn new_execution(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    work: &Work,
    spec: &SpecPayload,
    ordinal: u32,
    seq: EventSequence,
    input: InvocationInput,
) -> Result<Execution, StoreError> {
    let _ = spec;
    Ok(Execution {
        id: ExecutionId(tx.alloc_id("e")?),
        work_id: work.id.clone(),
        ordinal,
        state: ExecutionState::Queued,
        waiting_reason: None,
        desired_action: None,
        terminal_code: None,
        pause_reason: None,
        repair_prerequisites: vec![],
        outcome: None,
        enqueued_seq: seq,
        allowed_effects: config.allowed_effects.clone(),
        input,
        budget: config.budget.clone(),
        invocations_used: 0,
        review_rounds_used: 0,
        started_seq: None,
        started_unix_seconds: None,
        recovery_attempts_used: 0,
        recovery_deadline_unix_seconds: None,
        uncertain_since_unix_seconds: None,
        paused_since_unix_seconds: None,
        next_role: None,
    })
}

/// An external claim names only `(repository, branch)` pairs; the managed
/// workspace root is reserved for `execution`.
fn external_claimable(res: &WriteResource) -> bool {
    matches!(res, WriteResource::Branch { .. })
}

fn decide_claim(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
    ttl_seconds: u64,
    now_unix_seconds: i64,
) -> Verdict {
    if resources.is_empty() || ttl_seconds == 0 || resources.iter().any(|r| !external_claimable(r))
    {
        return Ok((
            rejected_now(tx, RejectionCode::InvalidRequest, None, vec![]),
            vec![],
        ));
    }
    let holder = ReservationHolder::External {
        actor_id: req.actor.external_id.clone(),
    };
    let expires_at = now_unix_seconds + ttl_seconds as i64;
    let mut effects = Vec::new();
    for res in resources {
        let key = res.key();
        match tx.reservation(&key)? {
            Some(existing) if existing.holder == holder => {
                // Same-actor renewal extends the TTL.
                effects.push(Effect::RenewReservation {
                    resource_key: key,
                    holder: holder.clone(),
                    expires_at_unix_seconds: expires_at,
                });
            }
            Some(existing) if !is_expired(&existing, now_unix_seconds) => {
                return Ok((
                    rejected_now(
                        tx,
                        RejectionCode::ResourceConflict,
                        None,
                        vec![serde_json::to_string(&existing).unwrap_or_default()],
                    ),
                    vec![],
                ));
            }
            Some(expired) => {
                // The expired claim has forfeited exclusion; remove it under
                // its own holder before inserting ours.
                effects.push(Effect::DeleteReservations {
                    resource_keys: vec![key.clone()],
                    holder: expired.holder.clone(),
                });
                effects.push(Effect::InsertReservation {
                    reservation: ResourceReservation {
                        resource_key: key,
                        resource: res.clone(),
                        holder: holder.clone(),
                        expires_at_unix_seconds: Some(expires_at),
                    },
                });
            }
            None => {
                effects.push(Effect::InsertReservation {
                    reservation: ResourceReservation {
                        resource_key: key,
                        resource: res.clone(),
                        holder: holder.clone(),
                        expires_at_unix_seconds: Some(expires_at),
                    },
                });
            }
        }
    }
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let seq = tx.alloc_seq()?;
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: None,
            work_version: None,
            effect: CommandEffect::ResourcesClaimed {
                expires_at: Some(expires_at),
            },
            committed_at: seq,
        },
        effects,
    ))
}

fn is_expired(res: &ResourceReservation, now_unix_seconds: i64) -> bool {
    res.expires_at_unix_seconds
        .is_some_and(|e| e <= now_unix_seconds)
}

fn decide_release(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    resources: &[WriteResource],
) -> Verdict {
    if resources.iter().any(|r| !external_claimable(r)) {
        return Ok((
            rejected_now(tx, RejectionCode::InvalidRequest, None, vec![]),
            vec![],
        ));
    }
    let holder = ReservationHolder::External {
        actor_id: req.actor.external_id.clone(),
    };
    for res in resources {
        if let Some(existing) = tx.reservation(&res.key())?
            && existing.holder != holder
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Unauthorized,
                    None,
                    vec![serde_json::to_string(&existing).unwrap_or_default()],
                ),
                vec![],
            ));
        }
    }
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let seq = tx.alloc_seq()?;
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: None,
            work_version: None,
            effect: CommandEffect::ResourcesReleased,
            committed_at: seq,
        },
        vec![Effect::DeleteReservations {
            resource_keys: resources.iter().map(WriteResource::key).collect(),
            holder,
        }],
    ))
}

fn decide_retry(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let executions = tx.executions_for_work(work_id)?;
    let current = executions.last();
    match current {
        Some(e) if &e.id == expected_execution_id => {}
        _ => {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Ineligible,
                    Some(work_id.clone()),
                    vec!["expected_execution_id is not the current Execution".into()],
                ),
                vec![],
            ));
        }
    }
    let current = current.expect("checked above");
    if current.state != ExecutionState::Failed {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["retry requires a confirmed failed Execution".into()],
            ),
            vec![],
        ));
    }
    // The successor Execution re-acquires the Work's source claims;
    // they were released at terminal settlement. A newer Work owning
    // any of them blocks the retry.
    let issue_refs = tx.issue_refs(work_id)?;
    for issue in &issue_refs {
        if let Some(owner) = tx.issue_claim(issue)?
            && &owner != work_id
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::Ineligible,
                    Some(owner.clone()),
                    vec![format!("issue {} now owned by {}", issue, owner.0)],
                ),
                vec![],
            ));
        }
    }
    // Acceptance-time reservation pre-check; dispatch rechecks atomically.
    let resources = work_resources(&work);
    for res in &resources {
        if let Some(existing) = tx.reservation(&res.key())?
            && !is_expired(&existing, now_unix_seconds)
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::ResourceConflict,
                    Some(work_id.clone()),
                    vec![serde_json::to_string(&existing).unwrap_or_default()],
                ),
                vec![],
            ));
        }
    }
    let seq = tx.alloc_seq()?;
    let spec = tx
        .spec(work_id, Some(work.spec))?
        .ok_or_else(|| StoreError::Internal {
            detail: "work without spec".into(),
        })?;
    let execution = new_execution(
        tx,
        config,
        &work,
        &spec.payload,
        current.ordinal + 1,
        seq,
        InvocationInput {
            input: current.input.input.clone(),
            resume: false,
            next_execution_input: current.input.next_execution_input.clone(),
            previous_execution_outcome: current.outcome.clone(),
        },
    )?;
    let mut next_work = work.clone();
    next_work.version += 1;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(next_work.version),
            effect: CommandEffect::ExecutionCreated {
                execution_id: execution.id.clone(),
            },
            committed_at: seq,
        },
        vec![
            Effect::UpdateWork {
                work: next_work,
                expected: work.version,
            },
            Effect::InsertExecution { execution },
            Effect::InsertIssueClaims {
                work_id: work_id.clone(),
                issues: issue_refs,
            },
        ],
    ))
}

/// Whether a paused Execution's typed repair prerequisites are
/// satisfied. An unsatisfiable prerequisite keeps the Work held.
fn prerequisites_satisfied(
    tx: &dyn ReadView,
    work: &Work,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<Vec<String>, StoreError> {
    let mut unmet = Vec::new();
    for p in &execution.repair_prerequisites {
        match p {
            RepairPrerequisite::SpecReapproval => {
                // `resume` can never create a new approved spec revision.
                unmet.push("spec reapproval required".into());
            }
            RepairPrerequisite::SourceRead => {
                if work.source_gate != SourceGate::Matches {
                    unmet.push("source read still does not match the approved spec".into());
                }
            }
            RepairPrerequisite::Capability { capability } => {
                if !capability_held(tx, &work.binding.repository.host, capability)? {
                    unmet.push(format!(
                        "capability {} is not authorized",
                        capability.name()
                    ));
                }
            }
            RepairPrerequisite::Artifact { name } => {
                let present = tx
                    .observations_for_execution(&execution.id)?
                    .iter()
                    .any(|o| {
                        matches!(
                            &o.fact,
                            ObservationFact::Artifact {
                                required_artifact,
                                state: ArtifactFact::Present { .. },
                                ..
                            } if required_artifact == name
                        )
                    });
                if !present {
                    unmet.push(format!("artifact {name} not present"));
                }
            }
            RepairPrerequisite::EffectResolution { action_id } => {
                let outstanding = tx.action_intents()?.iter().any(|i| {
                    &i.action_id == action_id
                        && matches!(
                            i.status,
                            HostActionStatus::Pending | HostActionStatus::Unknown { .. }
                        )
                });
                if outstanding {
                    unmet.push(format!("effect {action_id} still unresolved"));
                }
            }
            RepairPrerequisite::OperatorDecision => {}
        }
    }
    let _ = now_unix_seconds;
    Ok(unmet)
}

fn decide_resume(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no active Execution to resume".into()],
            ),
            vec![],
        ));
    };
    if &execution.id != execution_id || execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["resume requires the named Execution to be paused".into()],
            ),
            vec![],
        ));
    }
    // Typed prerequisites: a repair pause releases only when the recorded
    // prerequisites are satisfied.
    let unmet = prerequisites_satisfied(tx, &work, &execution, now_unix_seconds)?;
    if !unmet.is_empty() {
        return Ok((
            rejected_now(tx, RejectionCode::Ineligible, Some(work_id.clone()), unmet),
            vec![],
        ));
    }
    // A spent budget is terminal, not requeueable: `queued` or `paused` with
    // the bound exhausted goes to `failed` with the limit recorded. The
    // shared predicate keeps this entry point on the same rule as
    // dispatch-time admission.
    //
    // Before that terminal settlement, an outstanding host write must
    // reconcile: intents are delivery-bound and the outbox drain would
    // still apply a pending write under a recorded failure. This is the
    // same rule admission and recovery-intent firing apply.
    if let Some(limit) = execution.budget_exhaustion(&InvocationRole::Coding)
        && defer_terminal_while_outstanding(tx, &work.id, &execution.id, limit.terminal_code())?
    {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec![format!(
                    "terminal {} defers until outstanding host writes reconcile",
                    limit.terminal_code()
                )],
            ),
            vec![],
        ));
    }
    if let Some((effects, seq, next_version)) =
        paused_exhaustion_effects(tx, &work, &execution, now_unix_seconds)?
    {
        let command_id = CommandId(tx.alloc_id("cmd")?);
        return Ok((
            CommandDecision::Accepted {
                command_id,
                work_id: Some(work_id.clone()),
                work_version: Some(next_version),
                effect: CommandEffect::StateChanged,
                committed_at: seq,
            },
            effects,
        ));
    }
    let (effects, seq, next_version) =
        paused_requeue_effects(tx, &work, &execution, now_unix_seconds)?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(next_version),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        effects,
    ))
}

/// Terminal settlement for a `paused` Execution whose coding budget is
/// spent. Shared by `resume` and recovery-intent firing so both apply
/// the identical rule: a spent budget is terminal, never requeueable.
/// Returns `(effects, committed_at, next_work_version)` when the bound
/// is exhausted, `None` otherwise.
fn paused_exhaustion_effects(
    tx: &mut dyn TxView,
    work: &Work,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<Option<(Vec<Effect>, EventSequence, u64)>, StoreError> {
    let Some(limit) = execution.budget_exhaustion(&InvocationRole::Coding) else {
        return Ok(None);
    };
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Failed;
    next.terminal_code = Some(limit.terminal_code().to_string());
    next.outcome = Some(Outcome {
        state: ExecutionState::Failed,
        code: Some(limit.terminal_code().to_string()),
        detail: Some(limit.detail()),
        evidence_refs: vec![],
        recorded_seq: seq,
    });
    let mut effects = close_pause_effects(tx, work, execution, now_unix_seconds)?;
    let mut next_work = work.clone();
    next_work.version += 1;
    let next_version = next_work.version;
    effects.insert(
        0,
        Effect::UpdateWork {
            work: next_work,
            expected: work.version,
        },
    );
    effects.push(Effect::UpdateExecution {
        execution: next,
        expected_state: ExecutionState::Paused,
    });
    effects.push(Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    });
    effects.push(Effect::DeleteReservations {
        resource_keys: resources_keys_of(tx, execution)?,
        holder: ReservationHolder::Execution {
            execution_id: execution.id.clone(),
        },
    });
    effects.push(Effect::ReleaseIssueClaims {
        work_id: work.id.clone(),
    });
    effects.push(Effect::ClearDeferral {
        execution_id: execution.id.clone(),
    });
    Ok(Some((effects, seq, next_version)))
}

/// Requeue a `paused` Execution back through the queue while retaining
/// its reservations and charging the pause as human-decision waiting.
/// Shared by `resume` and recovery-intent firing so an automatic requeue
/// can never apply weaker guards than the Command path. Returns
/// `(effects, enqueued_seq, next_work_version)`.
fn paused_requeue_effects(
    tx: &mut dyn TxView,
    work: &Work,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<(Vec<Effect>, EventSequence, u64), StoreError> {
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Queued;
    next.enqueued_seq = seq;
    next.pause_reason = None;
    next.repair_prerequisites = vec![];
    next.paused_since_unix_seconds = None;
    let mut next_work = work.clone();
    next_work.version += 1;
    let next_version = next_work.version;
    let mut effects = close_pause_effects(tx, work, execution, now_unix_seconds)?;
    effects.push(Effect::UpdateWork {
        work: next_work,
        expected: work.version,
    });
    effects.push(Effect::UpdateExecution {
        execution: next,
        expected_state: ExecutionState::Paused,
    });
    effects.push(Effect::ClearDeferral {
        execution_id: execution.id.clone(),
    });
    Ok((effects, seq, next_version))
}

/// Usage-accounting effects for leaving `paused`: the elapsed pause
/// counts as human-decision waiting, not automatic usage.
fn close_pause_effects(
    tx: &dyn ReadView,
    work: &Work,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<Vec<Effect>, StoreError> {
    if let Some(paused_since) = execution.paused_since_unix_seconds {
        let mut usage = tx
            .work_usage(&work.id)?
            .unwrap_or_else(|| WorkUsage::new(&work.id));
        usage.human_wait_seconds_total += (now_unix_seconds - paused_since).max(0) as u64;
        return Ok(vec![Effect::UpdateWorkUsage { usage }]);
    }
    Ok(vec![])
}

fn decide_cancel(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    execution_id: Option<&ExecutionId>,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        // Nothing to cancel: an already-terminal Work gets an accepted no-op
        // with no new version.
        let command_id = CommandId(tx.alloc_id("cmd")?);
        let seq = tx.alloc_seq()?;
        return Ok((
            CommandDecision::Accepted {
                command_id,
                work_id: Some(work_id.clone()),
                work_version: Some(work.version),
                effect: CommandEffect::StateChanged,
                committed_at: seq,
            },
            vec![],
        ));
    };
    if let Some(named) = execution_id
        && named != &execution.id
    {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["execution_id is not the active Execution".into()],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    let mut next_work = work.clone();
    next_work.version += 1;
    let bump = Effect::UpdateWork {
        work: next_work.clone(),
        expected: work.version,
    };
    let release_claims = Effect::ReleaseIssueClaims {
        work_id: work_id.clone(),
    };
    // A no-op arm reports the stored version — returning a version the
    // commit did not produce would strand the caller's next precondition.
    let accept = |effects: Vec<Effect>, next_version: u64| {
        (
            CommandDecision::Accepted {
                command_id: command_id.clone(),
                work_id: Some(work_id.clone()),
                work_version: Some(next_version),
                effect: CommandEffect::StateChanged,
                committed_at: seq,
            },
            effects,
        )
    };
    match execution.state {
        ExecutionState::Queued => {
            let mut next = execution.clone();
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("cancelled while queued".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            // A resumed Execution is requeued while retaining its
            // reservations; cancelling it must release them here —
            // execution-held claims have no TTL to reclaim them later.
            Ok(accept(
                vec![
                    bump,
                    release_claims,
                    Effect::UpdateExecution {
                        execution: next,
                        expected_state: ExecutionState::Queued,
                    },
                    Effect::ReleaseSlot {
                        execution_id: execution.id.clone(),
                    },
                    Effect::ClearDeferral {
                        execution_id: execution.id.clone(),
                    },
                    Effect::DeleteReservations {
                        resource_keys: resources_keys_of(tx, &execution)?,
                        holder: ReservationHolder::Execution {
                            execution_id: execution.id.clone(),
                        },
                    },
                ],
                next_work.version,
            ))
        }
        ExecutionState::Paused => {
            // No writer can act: direct cancellation, reservation released.
            let mut next = execution.clone();
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("cancelled while paused".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            Ok(accept(
                vec![
                    bump,
                    release_claims,
                    Effect::UpdateExecution {
                        execution: next,
                        expected_state: ExecutionState::Paused,
                    },
                    Effect::DeleteReservations {
                        resource_keys: resources_keys_of(tx, &execution)?,
                        holder: ReservationHolder::Execution {
                            execution_id: execution.id.clone(),
                        },
                    },
                ],
                next_work.version,
            ))
        }
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
            let mut next = execution.clone();
            next.state = ExecutionState::Stopping;
            next.desired_action = Some(DesiredAction::Cancel);
            Ok(accept(
                vec![
                    bump,
                    Effect::UpdateExecution {
                        execution: next,
                        expected_state: execution.state.clone(),
                    },
                ],
                next_work.version,
            ))
        }
        ExecutionState::Uncertain => {
            if matches!(
                execution.desired_action,
                Some(DesiredAction::Cancel) | Some(DesiredAction::Fail)
            ) {
                // Desired fail/cancel already in progress: accepted no-op.
                Ok(accept(vec![], work.version))
            } else {
                let mut next = execution.clone();
                next.desired_action = Some(DesiredAction::Cancel);
                Ok(accept(
                    vec![
                        bump,
                        Effect::UpdateExecution {
                            execution: next,
                            expected_state: ExecutionState::Uncertain,
                        },
                    ],
                    next_work.version,
                ))
            }
        }
        ExecutionState::Stopping => Ok(accept(vec![], work.version)),
        ExecutionState::Succeeded | ExecutionState::Failed | ExecutionState::Cancelled => {
            Ok(accept(vec![], work.version))
        }
    }
}

fn resources_keys_of(tx: &dyn ReadView, execution: &Execution) -> Result<Vec<String>, StoreError> {
    Ok(tx
        .reservations()?
        .into_iter()
        .filter(|r| {
            r.holder
                == ReservationHolder::Execution {
                    execution_id: execution.id.clone(),
                }
        })
        .map(|r| r.resource_key)
        .collect())
}

fn decide_acknowledge(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    work_id: &WorkId,
    expected_version: WorkVersion,
    attention_id: &str,
    now_unix_seconds: i64,
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    // Acknowledgement marks a displayed Attention version as seen
    // (docs/decisions.md); accepting an id the saved state does not
    // currently derive would persist a row that hides the condition once
    // it later surfaces with the same id.
    let derivable = crate::projection::derivable_attention_ids(tx, work_id, now_unix_seconds)?;
    if !derivable.iter().any(|id| id == attention_id) {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec![format!(
                    "attention_id {attention_id} does not name a currently derivable Attention version"
                )],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(work.version),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![Effect::InsertAttentionAck {
            work_id: work_id.clone(),
            attention_id: attention_id.to_string(),
            actor_id: req.actor.external_id.clone(),
        }],
    ))
}

/// The sealed round covering the caller's expected head, or a rejection.
/// A judgment binds the round's exact target — a head that moved, or a
/// target that moved under an identical head, is stale.
fn current_round_for(
    tx: &dyn ReadView,
    work_id: &WorkId,
    expected_head: &CommitRevision,
) -> Result<Result<(ReviewRound, Vec<FindingOccurrence>), CommandDecision>, StoreError> {
    let Some(target) = current_target(tx, work_id)? else {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::SourceUnavailable,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence: vec![format!("no observed change target for {}", work_id.0)],
            committed_at: tx.last_seq()?,
        }));
    };
    if &target.head != expected_head {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::StaleHead,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence: vec![format!("current head {}", target.head)],
            committed_at: tx.last_seq()?,
        }));
    }
    let rounds = tx.rounds_for_work(work_id)?;
    let Some(round) = rounds.iter().rfind(|r| r.target == target) else {
        let (reason, evidence) = if rounds.is_empty() {
            (
                RejectionCode::SourceUnavailable,
                vec!["no sealed review round".into()],
            )
        } else {
            (
                RejectionCode::StaleHead,
                vec![format!(
                    "sealed rounds cover no current target {}",
                    target.head
                )],
            )
        };
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence,
            committed_at: tx.last_seq()?,
        }));
    };
    if !round.completeness.starts_with("complete") {
        return Ok(Err(CommandDecision::Rejected {
            command_id: CommandId("cmd-unknown".into()),
            reason: RejectionCode::Ineligible,
            current_work_id: Some(work_id.clone()),
            current_version: None,
            evidence: vec!["latest review round is incomplete".into()],
            committed_at: tx.last_seq()?,
        }));
    }
    Ok(Ok((round.clone(), tx.occurrences(&round.id)?)))
}

fn decide_judge(
    tx: &mut dyn TxView,
    req: &CommandRequest,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_head: &CommitRevision,
    decisions: &[crate::command::FindingDecision],
) -> Verdict {
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let (round, occurrences) = match current_round_for(tx, work_id, expected_head)? {
        Ok(pair) => pair,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            if let CommandDecision::Rejected {
                current_version, ..
            } = &mut d
            {
                *current_version = Some(work.version);
            }
            return Ok((d, vec![]));
        }
    };
    // Exact-set coverage: the decision set must equal the sealed
    // occurrence id set — omissions and unknown occurrences cannot
    // quietly pass.
    let occurrence_ids: BTreeSet<&str> = occurrences.iter().map(|o| o.id.as_str()).collect();
    let mut decided: BTreeSet<&str> = BTreeSet::new();
    for d in decisions {
        if !occurrence_ids.contains(d.occurrence_id.as_str()) {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::InvalidRequest,
                    Some(work_id.clone()),
                    vec![format!("unknown occurrence {}", d.occurrence_id)],
                ),
                vec![],
            ));
        }
        if !decided.insert(d.occurrence_id.as_str()) {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::InvalidRequest,
                    Some(work_id.clone()),
                    vec![format!("duplicate occurrence {}", d.occurrence_id)],
                ),
                vec![],
            ));
        }
        if matches!(d.choice, FindingChoice::Dismiss | FindingChoice::Accept)
            && d.rationale.as_deref().unwrap_or("").is_empty()
        {
            return Ok((
                rejected_now(
                    tx,
                    RejectionCode::InvalidRequest,
                    Some(work_id.clone()),
                    vec![format!(
                        "occurrence {} requires a rationale for {}",
                        d.occurrence_id, "dismiss/accept"
                    )],
                ),
                vec![],
            ));
        }
    }
    let missing: Vec<&str> = occurrence_ids.difference(&decided).copied().collect();
    if !missing.is_empty() {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::InvalidRequest,
                Some(work_id.clone()),
                missing
                    .iter()
                    .map(|id| format!("undecided occurrence {id}"))
                    .collect(),
            ),
            vec![],
        ));
    }
    if occurrences.is_empty() && !decisions.is_empty() {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::InvalidRequest,
                Some(work_id.clone()),
                vec!["decisions for an empty round".into()],
            ),
            vec![],
        ));
    }
    let seq = tx.alloc_seq()?;
    let judgment = RoundJudgment {
        id: tx.alloc_id("jdg")?,
        work_id: work_id.clone(),
        round_id: round.id.clone(),
        set_digest: round.set_digest.clone(),
        decisions: decisions.to_vec(),
        actor_id: req.actor.external_id.clone(),
        seq,
    };
    let mut next_work = work.clone();
    next_work.version += 1;
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(next_work.version),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        vec![
            Effect::UpdateWork {
                work: next_work,
                expected: work.version,
            },
            Effect::InsertRoundJudgment { judgment },
        ],
    ))
}

fn decide_release_judgment(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    expected_version: WorkVersion,
    expected_head: &CommitRevision,
    now_unix_seconds: i64,
) -> Verdict {
    // Releases the human gate: valid on a paused Execution, requeues it.
    let work = match check_work(tx, work_id, expected_version)? {
        Ok(w) => w,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            return Ok((d, vec![]));
        }
    };
    let Some(execution) = tx.active_execution(work_id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no active Execution".into()],
            ),
            vec![],
        ));
    };
    if execution.state != ExecutionState::Paused {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["release_judgment requires a paused Execution".into()],
            ),
            vec![],
        ));
    }
    // The gate releases only against a recorded judgment on the sealed
    // round for the caller's expected head — and that head must still be
    // the current target, or the pause must not release against a head
    // the operator never saw.
    let (round, _) = match current_round_for(tx, work_id, expected_head)? {
        Ok(pair) => pair,
        Err(mut d) => {
            fix_command_id(tx, &mut d);
            if let CommandDecision::Rejected {
                current_version, ..
            } = &mut d
            {
                *current_version = Some(work.version);
            }
            return Ok((d, vec![]));
        }
    };
    let Some(judgment) = tx.judgment_for_round(&round.id)? else {
        return Ok((
            rejected_now(
                tx,
                RejectionCode::Ineligible,
                Some(work_id.clone()),
                vec!["no recorded judgment on the current round".into()],
            ),
            vec![],
        ));
    };
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Queued;
    next.enqueued_seq = seq;
    next.pause_reason = None;
    next.repair_prerequisites = vec![];
    next.paused_since_unix_seconds = None;
    // `fix` dispositions route the next admission to the bounded fix
    // account; terminal dispositions leave the normal Coding role.
    next.next_role = if judgment
        .decisions
        .iter()
        .any(|d| d.choice == FindingChoice::Fix)
    {
        Some(InvocationRole::Fix)
    } else {
        None
    };
    let mut next_work = work.clone();
    next_work.version += 1;
    let mut effects = close_pause_effects(tx, &work, &execution, now_unix_seconds)?;
    effects.push(Effect::UpdateWork {
        work: next_work.clone(),
        expected: work.version,
    });
    effects.push(Effect::UpdateExecution {
        execution: next,
        expected_state: ExecutionState::Paused,
    });
    effects.push(Effect::ClearDeferral {
        execution_id: execution.id.clone(),
    });
    let command_id = CommandId(tx.alloc_id("cmd")?);
    Ok((
        CommandDecision::Accepted {
            command_id,
            work_id: Some(work_id.clone()),
            work_version: Some(next_work.version),
            effect: CommandEffect::StateChanged,
            committed_at: seq,
        },
        effects,
    ))
}

fn fix_command_id(tx: &mut dyn TxView, decision: &mut CommandDecision) {
    if let CommandDecision::Rejected { command_id, .. } = decision {
        *command_id = CommandId(
            tx.alloc_id("cmd")
                .unwrap_or_else(|_| "cmd-unknown".to_string()),
        );
    }
}

/// The dispatch-time admission: `queued → ready` in one transaction —
/// slot claim, writer-resource reservation, and the first durable request.
/// Returns the admitted Execution, or records a deferral and returns None.
pub fn admit_queued(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Result<Option<Execution>, StoreError> {
    let Some(execution) = tx.execution(execution_id)? else {
        return Ok(None);
    };
    let role = execution
        .next_role
        .clone()
        .unwrap_or(InvocationRole::Coding);
    admit_queued_role(tx, config, execution_id, role, now_unix_seconds)
}

/// Admission for one Invocation role: the single seam every request creation
/// passes, so the budget bounds are enforced no matter which caller emits
/// the request. A Review admission consumes both counters; a Fix one is
/// also charged to the Work-wide fix account. The head-bound judgment
/// gate is rechecked here too, so a judgment recorded for an older target
/// can never authorize a request after the target moves.
pub fn admit_queued_role(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution_id: &ExecutionId,
    role: InvocationRole,
    now_unix_seconds: i64,
) -> Result<Option<Execution>, StoreError> {
    let Some(execution) = tx.execution(execution_id)? else {
        return Ok(None);
    };
    // `queued` is the ordinary admission point. `waiting` admits only
    // the pre-publication invariant check: lifecycle.md's
    // `waiting → ready` transition issues the next step's request under
    // the same Execution and reservation, and the check is the only
    // role the publish gate re-requests from `waiting` today — every
    // other role still requires a Command-driven requeue first.
    if execution.state != ExecutionState::Queued
        && !(execution.state == ExecutionState::Waiting && role == InvocationRole::InvariantCheck)
    {
        return Ok(None);
    }
    let work = tx
        .work(&execution.work_id)?
        .ok_or_else(|| StoreError::Internal {
            detail: format!("execution {} has no work", execution.id.0),
        })?;

    // Budget gate first, before any reservation, deferral, or slot effect:
    // a spent bound is terminal for this Execution and must not be preceded
    // by partial mutation. No counter increments and no request is emitted.
    if !execution.accepts_invocation(&role) {
        if let Some(limit) = execution.budget_exhaustion(&role) {
            // An outstanding host write must reconcile before the terminal
            // outcome is recorded (lifecycle.md): intents are
            // delivery-bound and the outbox drain would still apply a
            // pending write under a recorded failure.
            if defer_terminal_while_outstanding(tx, &work.id, &execution.id, limit.terminal_code())?
            {
                return Ok(None);
            }
            fail_terminal(tx, &execution, limit.terminal_code(), &limit.detail())?;
            tx.apply(&Effect::ClearDeferral {
                execution_id: execution.id.clone(),
            })?;
        }
        return Ok(None);
    }

    // The Work-wide automatic-time deadline gates at this seam too: past
    // it, no new trial is dispatched. The timed sweep records the same
    // terminal decision, but admission runs phases before the sweep —
    // without the gate a pass would admit-and-launch inside one tick.
    // Queue time counts toward the deadline; human-decision waiting does
    // not (already charged into the usage row).
    {
        let usage = tx
            .work_usage(&work.id)?
            .unwrap_or_else(|| WorkUsage::new(&work.id));
        if automatic_seconds_used(&usage, now_unix_seconds)
            .is_some_and(|automatic| automatic > work.policy.automatic_time_limit_seconds)
        {
            if defer_terminal_while_outstanding(
                tx,
                &work.id,
                &execution.id,
                "automatic_time_exhausted",
            )? {
                return Ok(None);
            }
            fail_terminal(
                tx,
                &execution,
                "automatic_time_exhausted",
                "Work-wide automatic time limit reached",
            )?;
            return Ok(None);
        }
    }

    // Source gate: dispatch runs only on approved, matched sources. A
    // needed check is a deferral (the loop owes a source read); a changed
    // or unavailable source holds the Execution for repair.
    match work.source_gate {
        SourceGate::NotApplicable | SourceGate::Matches => {}
        SourceGate::NeedsCheck => {
            tx.apply(&Effect::RecordDeferral {
                execution_id: execution_id.clone(),
                reason: "source_check".into(),
                diagnostic: "awaiting authoritative source read".into(),
            })?;
            return Ok(None);
        }
        SourceGate::Unavailable => {
            return pause_for_repair(
                tx,
                &execution,
                "source_unavailable",
                "source read unavailable; a fresh matching read is required",
                vec![RepairPrerequisite::SourceRead],
                now_unix_seconds,
            );
        }
        SourceGate::Changed => {
            return pause_for_repair(
                tx,
                &execution,
                "source_changed",
                "source changed after spec approval; a new spec revision and reapproval are required",
                vec![RepairPrerequisite::SpecReapproval],
                now_unix_seconds,
            );
        }
    }
    if work.spec != work.approved_spec {
        return pause_for_repair(
            tx,
            &execution,
            "spec_unapproved",
            "current spec is not the approved revision",
            vec![RepairPrerequisite::SpecReapproval],
            now_unix_seconds,
        );
    }

    // The judgment gate binds the exact sealed target: a judgment recorded
    // while this Execution waited must not authorize a writer after the
    // target moves. The gate fails closed whenever the latest sealed round
    // is not the judged round — a moved head, a moved target branch, or a
    // grown finding set all supersede the recorded judgment — or when the
    // judged round's target no longer matches the current diff target.
    if let Some(judgment) = tx.latest_judgment(&work.id)?
        && let Some(target) = current_target(tx, &work.id)?
    {
        let current = latest_round_for_target(tx, &work.id, &target)?
            .is_some_and(|round| round.id == judgment.round_id);
        if !current {
            return pause_for_repair(
                tx,
                &execution,
                "target_moved_after_judgment",
                "the sealed target the recorded judgment binds no longer matches the current diff target; a new judgment is required",
                vec![RepairPrerequisite::OperatorDecision],
                now_unix_seconds,
            );
        }
    }

    // Work-wide fix bound: a fix dispatch beyond the recorded account is a
    // hold for human action, not another automatic attempt.
    if role == InvocationRole::Fix {
        let usage = tx
            .work_usage(&work.id)?
            .unwrap_or_else(|| WorkUsage::new(&work.id));
        if usage.fix_invocations_used >= work.policy.max_fix_invocations {
            return pause_for_repair(
                tx,
                &execution,
                "fix_budget_exhausted",
                "Work-wide fix invocation bound reached; human action required",
                vec![RepairPrerequisite::OperatorDecision],
                now_unix_seconds,
            );
        }
    }

    // Atomic recheck: another writer may have claimed since acceptance. A
    // reservation this Execution itself holds (a paused/resumed Execution
    // retains its claims) is already-held, not a conflict.
    let resources = work_resources(&work);
    for res in &resources {
        if let Some(existing) = tx.reservation(&res.key())? {
            if matches!(
                &existing.holder,
                ReservationHolder::Execution { execution_id: holder } if holder == execution_id
            ) {
                continue;
            }
            if !is_expired(&existing, now_unix_seconds) {
                tx.apply(&Effect::RecordDeferral {
                    execution_id: execution_id.clone(),
                    reason: "resource_conflict".into(),
                    diagnostic: serde_json::to_string(&existing).unwrap_or_default(),
                })?;
                return Ok(None);
            }
            // An expired claim forfeits; remove it under its own holder.
            tx.apply(&Effect::DeleteReservations {
                resource_keys: vec![res.key()],
                holder: existing.holder.clone(),
            })?;
        }
    }
    // A paused/resumed Execution also retains its capacity slot; only a
    // fresh admission must find a free one.
    let holds_slot = tx
        .slot_holders()?
        .iter()
        .any(|(holder, _)| holder == execution_id);
    if !holds_slot && tx.slot_holders()?.len() >= tx.slot_limit()? as usize {
        tx.apply(&Effect::RecordDeferral {
            execution_id: execution_id.clone(),
            reason: "capacity_exhausted".into(),
            diagnostic: "no free capacity slot".into(),
        })?;
        return Ok(None);
    }

    let seq = tx.alloc_seq()?;
    let mut payload = serde_json::to_value(&execution.input).unwrap_or_default();
    if matches!(
        role,
        InvocationRole::InvariantCheck | InvocationRole::Review
    ) && let serde_json::Value::Object(map) = &mut payload
    {
        // Both read-only roles declare no workspace artifact, so their
        // durable record carries an explicitly empty required set
        // rather than the writer default. Their evidence is the
        // Invocation lifecycle plus receipts, never a produced diff.
        map.insert("required_artifacts".into(), serde_json::json!([]));
    }
    let request = InvocationRequest {
        // One durable request per recorded attempt; the attempt counter
        // makes re-admission ids unique.
        request_id: format!("{}:inv:{}", execution.id.0, execution.invocations_used),
        work_id: work.id.clone(),
        execution_id: execution.id.clone(),
        role: role.clone(),
        spec_revision: work.spec,
        payload,
        // Created unbound: the adapter binding is recorded later from a
        // live `describe` declaration — either by the Phase-2
        // dispatch-admission probe, which binds unbound pending
        // requests to the current worker in a transaction following
        // the admission scan, or by `execution` immediately before the
        // first launch. It is never inferred from configuration.
        adapter_binding: None,
    };
    let mut next = execution.clone();
    next.state = ExecutionState::Ready;
    next.waiting_reason = None;
    // The runtime budget anchors at first dispatch; a resumed Execution
    // keeps its original anchor and counters.
    if next.started_seq.is_none() {
        next.started_seq = Some(seq);
        next.started_unix_seconds = Some(now_unix_seconds);
    }
    next.invocations_used += 1;
    if role == InvocationRole::Review {
        next.review_rounds_used += 1;
    }
    next.next_role = None;
    tx.apply(&Effect::UpdateExecution {
        execution: next.clone(),
        expected_state: execution.state.clone(),
    })?;
    // Work-wide accounting: first dispatch anchors automatic time; a fix
    // admission charges the fix account.
    let mut usage = tx
        .work_usage(&work.id)?
        .unwrap_or_else(|| WorkUsage::new(&work.id));
    if usage.first_dispatch_unix_seconds.is_none() {
        usage.first_dispatch_unix_seconds = Some(now_unix_seconds);
    }
    if role == InvocationRole::Fix {
        usage.fix_invocations_used += 1;
    }
    tx.apply(&Effect::UpdateWorkUsage { usage })?;
    if !holds_slot {
        tx.apply(&Effect::ClaimSlot {
            execution_id: execution_id.clone(),
        })?;
    }
    for res in resources {
        // Resources this Execution already holds stay held.
        if tx.reservation(&res.key())?.is_some() {
            continue;
        }
        tx.apply(&Effect::InsertReservation {
            reservation: ResourceReservation {
                resource_key: res.key(),
                resource: res,
                holder: ReservationHolder::Execution {
                    execution_id: execution_id.clone(),
                },
                expires_at_unix_seconds: None,
            },
        })?;
    }
    tx.apply(&Effect::InsertRequest { request })?;
    tx.apply(&Effect::ClearDeferral {
        execution_id: execution_id.clone(),
    })?;
    let _ = config;
    Ok(Some(next))
}

/// The persisted wait for a transport retry attempt (1-indexed): the
/// Work's configured schedule, the last entry repeating when attempts
/// exceed the list.
fn transport_backoff_seconds(policy: &WorkPolicy, attempt: u32) -> u64 {
    let schedule = &policy.transport_backoff_seconds;
    schedule
        .get(attempt.saturating_sub(1) as usize)
        .or_else(|| schedule.last())
        .copied()
        .unwrap_or(0)
}

/// Persist one `recovery_intents` row inside the deciding transaction —
/// written before any pass can act on it, so a crash between the
/// decision and the action can never lose or double-fire the schedule.
fn schedule_recovery_intent(
    tx: &mut dyn TxView,
    work: &Work,
    execution_id: Option<&ExecutionId>,
    kind: &str,
    due_at_unix_seconds: i64,
    evidence_basis: &str,
) -> Result<(), StoreError> {
    let scheduled_seq = tx.alloc_seq()?;
    let intent_id = tx.alloc_id("ri")?;
    tx.apply(&Effect::InsertRecoveryIntent {
        intent: RecoveryIntent {
            intent_id,
            work_id: work.id.clone(),
            execution_id: execution_id.cloned(),
            kind: kind.into(),
            due_at_unix_seconds,
            generation: work.version,
            evidence_basis: evidence_basis.into(),
            state: RecoveryIntentState::Pending,
            retirement_reason: None,
            retirement_generation: None,
            scheduled_seq,
        },
    })
}

/// Pause an Execution for a repairable hold, recording the typed
/// prerequisites and the immutable recovery decision.
///
/// Writer safety comes first (lifecycle.md): a `queued` Execution holds
/// no slot and no writer, so it pauses directly; an active Execution
/// enters `stopping` with a recorded `pause` desired action and settles
/// to `paused` only after writer-stop proof releases the slot. The pause
/// reason and prerequisites are persisted on the row now so the settle
/// lands on the recorded continuation.
///
/// Every repair pause also schedules its potential `requeue` in the
/// same deciding transaction — persisted before any recovery pass acts
/// on it. The firing guards decide whether it ever runs: an explicit
/// hold keeps it `pending`, unmet prerequisites hold it, terminal
/// evidence retires it.
fn pause_for_repair(
    tx: &mut dyn TxView,
    execution: &Execution,
    classification: &str,
    detail: &str,
    prerequisites: Vec<RepairPrerequisite>,
    now_unix_seconds: i64,
) -> Result<Option<Execution>, StoreError> {
    match execution.state {
        ExecutionState::Queued => {
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some(detail.to_string());
            next.repair_prerequisites = prerequisites.clone();
            next.paused_since_unix_seconds = Some(now_unix_seconds);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: ExecutionState::Queued,
            })?;
            tx.apply(&Effect::ClearDeferral {
                execution_id: execution.id.clone(),
            })?;
        }
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
            let mut next = execution.clone();
            next.state = ExecutionState::Stopping;
            next.desired_action = Some(DesiredAction::Pause);
            next.pause_reason = Some(detail.to_string());
            next.repair_prerequisites = prerequisites.clone();
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state.clone(),
            })?;
        }
        ExecutionState::Uncertain => {
            let mut next = execution.clone();
            next.desired_action = Some(DesiredAction::Pause);
            next.pause_reason = Some(detail.to_string());
            next.repair_prerequisites = prerequisites.clone();
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: ExecutionState::Uncertain,
            })?;
        }
        // Stopping with another recorded desire, paused, and terminal
        // states keep their recorded path.
        _ => return Ok(None),
    }
    let usage = tx
        .work_usage(&execution.work_id)?
        .unwrap_or_else(|| WorkUsage::new(&execution.work_id));
    let seq = tx.alloc_seq()?;
    tx.apply(&Effect::InsertRecoveryDecision {
        decision: RecoveryDecision {
            id: new_id("rd"),
            work_id: execution.work_id.clone(),
            execution_id: Some(execution.id.clone()),
            classification: classification.into(),
            signature: None,
            prerequisites,
            detail: detail.into(),
            usage_after: serde_json::to_value(&usage).unwrap_or_default(),
            seq,
        },
    })?;
    // The pause's potential recovery is scheduled in the same deciding
    // transaction — the requeue intent is persisted before any pass can
    // act on it. It is due immediately; the firing guards decide
    // whether it ever consumes — an explicit hold keeps it `pending`,
    // unmet prerequisites hold it, terminal evidence retires it.
    // Dedupe parity with the `retry` guard: one pending `requeue` per
    // Execution, so repeated pause/resume cycles can never accumulate
    // intents that starve the per-tick recovery budget.
    let requeue_pending = tx.recovery_intents()?.iter().any(|i| {
        i.execution_id.as_ref() == Some(&execution.id)
            && i.state == RecoveryIntentState::Pending
            && i.kind == "requeue"
    });
    if !requeue_pending {
        let work = tx
            .work(&execution.work_id)?
            .ok_or_else(|| StoreError::Internal {
                detail: "execution without work".into(),
            })?;
        schedule_recovery_intent(
            tx,
            &work,
            Some(&execution.id),
            "requeue",
            now_unix_seconds,
            classification,
        )?;
    }
    Ok(None)
}

/// The latest capability state a report recorded for (host, capability);
/// `None` means no report exists — the capability is unconfirmed.
fn capability_held(
    tx: &dyn ReadView,
    host: &crate::identity::HostInstanceId,
    capability: &HostCapability,
) -> Result<bool, StoreError> {
    Ok(match capability_state(tx, host, capability)? {
        Some(entry) => entry.usable(),
        None => false,
    })
}

fn capability_state(
    tx: &dyn ReadView,
    host: &crate::identity::HostInstanceId,
    capability: &HostCapability,
) -> Result<Option<crate::observation::CapabilityEntry>, StoreError> {
    // The newest report is the host's complete declaration: a
    // capability absent from it is revoked — falling back to an older
    // report's entry would keep a withdrawn grant usable.
    let observations = tx.all_observations()?;
    let mut latest_report: Option<(i64, usize, &Vec<crate::observation::CapabilityEntry>)> = None;
    for (index, o) in observations.iter().enumerate() {
        if let ObservationFact::CapabilityReport {
            host: report_host,
            capabilities,
        } = &o.fact
            // Reports persisted before unification may carry raw
            // endpoint spellings; the comparison is on canonical
            // host-instance identity, not transport spelling.
            && canonical_host_instance(report_host.as_str())
                == canonical_host_instance(host.as_str())
        {
            let key = (o.observed_unix_seconds, index);
            if latest_report.is_none_or(|old| key > (old.0, old.1)) {
                latest_report = Some((key.0, key.1, capabilities));
            }
        }
    }
    Ok(latest_report.and_then(|(_, _, capabilities)| {
        capabilities
            .iter()
            .find(|entry| &entry.capability == capability)
            .cloned()
    }))
}

/// What happened to an ingested Observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestReceipt {
    Recorded,
    /// A source event delivered twice; the store's dedupe index caught it.
    Duplicate,
    Rejected(String),
}

/// Slice-0+ completion policy: a coding Invocation's required artifacts,
/// the required stage effects applied (ref published, change request
/// opened), and check + review + lane evidence covering the same current
/// target produce `succeeded`.
pub fn handle_observation(
    store: &impl Store,
    config: &WorkloopConfig,
    obs: &Observation,
    now_unix_seconds: i64,
) -> Result<IngestReceipt, StoreError> {
    // Stage records are authored by Core inside its own transaction; the
    // ingest path — adapters, bridges, probes — carries no authority to
    // write pipeline history, so a delivered `StageEvidence` fact is
    // refused before it can enter the durable log.
    if matches!(obs.fact, ObservationFact::StageEvidence { .. }) {
        return Ok(IngestReceipt::Rejected(
            "stage evidence is Core-authored; ingest cannot write pipeline records".into(),
        ));
    }
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertObservation {
                observation: obs.clone(),
            })?;
            assess(tx, config, obs, now_unix_seconds)?;
            Ok(IngestReceipt::Recorded)
        })
        .or_else(|e| {
            if !is_dedupe_conflict(&e) {
                return Err(e);
            }
            // The delivery identity already has a recorded observation.
            // A byte-identical fact is a replay; a different fact under
            // the same identity is a producer conflict — like a changed
            // payload under one idempotency key, it is rejected, never
            // silently folded into the recorded row.
            let stored = store.transact(|tx| {
                Ok(tx
                    .all_observations()?
                    .into_iter()
                    .find(|o| o.delivery == obs.delivery))
            })?;
            let same_fact = stored.is_some_and(|existing| {
                canonical_json(&serde_json::to_value(&existing.fact).unwrap_or_default())
                    == canonical_json(&serde_json::to_value(&obs.fact).unwrap_or_default())
            });
            if same_fact {
                Ok(IngestReceipt::Duplicate)
            } else {
                Ok(IngestReceipt::Rejected(
                    "delivery identity reused with different content".into(),
                ))
            }
        })
}

/// Only the observations dedupe index classifies an insert as a replay;
/// every other uniqueness violation is a real invariant breach and must
/// propagate instead of being mistaken for a duplicate delivery. SQLite
/// reports the violated index by its column list for a column-keyed
/// unique index, and `delivery_event_id` appears in no other
/// constraint.
pub fn is_dedupe_conflict(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::ConstraintConflict { detail }
            if detail.contains("observations.delivery_event_id")
    )
}

/// A committed observation may move the Execution; every transition here is
/// evidence-driven — a worker claim is never an Outcome by itself.
fn assess(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    obs: &Observation,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    // Outbox reconciliation is delivery-bound, not execution-bound: the
    // intent's result must be recorded even when no active Execution can
    // be resolved, otherwise a pending intent blocks the outbox forever.
    if let ObservationFact::HostActionResult {
        action_id,
        authenticated_actor,
        status,
        effects,
    } = &obs.fact
    {
        reconcile_intent(
            tx,
            action_id,
            authenticated_actor.clone(),
            status.clone(),
            effects.clone(),
            now_unix_seconds,
        )?;
    }
    // Source facts move the source gate regardless of execution state.
    if let ObservationFact::Issue { .. } = &obs.fact
        && let Some(work_id) = &obs.subject_work_id
    {
        assess_source_gate(tx, work_id, now_unix_seconds)?;
    }
    // A complete review seals an immutable round; the seal itself bumps
    // the Work's evidence generation when the occurrence set differs.
    if let ObservationFact::Review { .. } = &obs.fact
        && let Some(work_id) = &obs.subject_work_id
    {
        seal_round(tx, obs, work_id, now_unix_seconds)?;
    }
    // A capability report is host-scoped evidence with no work or
    // execution subject of its own, but it is the only event that can
    // turn a capability-blocked publish chain loose: re-drive every
    // Waiting execution on the reported host. `maybe_publish`
    // re-checks each precondition itself, so the sweep is safe on
    // every report — changed or not.
    if let ObservationFact::CapabilityReport { host, .. } = &obs.fact {
        redrive_capability_waiters(tx, config, host, now_unix_seconds)?;
    }
    // Material progress resets the equivalent-failure streak and stamps
    // the no-progress clock with its durable basis.
    if let Some(basis) = material_progress_basis(&obs.fact)
        && let Some(work_id) = &obs.subject_work_id
    {
        let mut usage = tx
            .work_usage(work_id)?
            .unwrap_or_else(|| WorkUsage::new(work_id));
        usage.last_progress_unix_seconds = Some(now_unix_seconds);
        usage.last_progress_basis = Some(basis);
        usage.equivalent_failure_streak = 0;
        usage.equivalent_signature = None;
        tx.apply(&Effect::UpdateWorkUsage { usage })?;
    }
    let execution = match (&obs.subject_execution_id, &obs.subject_work_id) {
        (Some(execution_id), _) => tx.execution(execution_id)?,
        (None, Some(work_id))
            if matches!(
                &obs.fact,
                ObservationFact::Check { .. }
                    | ObservationFact::Review { .. }
                    | ObservationFact::ChangeHead { .. }
                    | ObservationFact::BranchState { .. }
                    | ObservationFact::HostActionResult { .. }
                    | ObservationFact::Issue { .. }
                    | ObservationFact::Assessment { .. }
            ) =>
        {
            crate::projection::current_execution(tx, work_id)?
        }
        (None, _) => None,
    };
    let Some(execution) = execution else {
        return Ok(());
    };
    match &obs.fact {
        ObservationFact::ProcessState {
            invocation_id,
            process_identity,
            state,
            ..
        } => assess_process(
            tx,
            config,
            &execution,
            invocation_id,
            process_identity,
            state,
            now_unix_seconds,
        ),
        ObservationFact::LaunchRefused { invocation_id, .. } => {
            // Capture the refused attempt's role while its Invocation is
            // still an unresolved writer of this Execution: a refusal
            // re-delivered under a fresh delivery id after the attempt
            // already resolved must not refund the accounting a second
            // time inside a later `ready` window.
            let refused_role = tx
                .invocations_for_execution(&execution.id)?
                .into_iter()
                .find(|inv| inv.id == *invocation_id && inv.state.is_unresolved_writer())
                .map(|inv| inv.role);
            mark_invocation(tx, invocation_id, |_| InvocationState::Abandoned)?;
            // Definitely no process exists. lifecycle.md permits this
            // transition only from `ready`: a late refusal delivered after
            // cancel/stop (or a terminal settle) must not resurrect the
            // Execution to `paused`. The refusal still resolves the
            // Invocation itself — a spawn that never happened is not an
            // unresolved writer.
            if execution.state == ExecutionState::Ready {
                let mut next = execution.clone();
                if let Some(role) = refused_role {
                    refund_refused_attempt(tx, &mut next, role)?;
                }
                next.state = ExecutionState::Paused;
                next.pause_reason = Some("repair: launch refused before spawn".into());
                next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
                next.paused_since_unix_seconds = Some(now_unix_seconds);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: ExecutionState::Ready,
                })?;
                tx.apply(&Effect::ReleaseSlot {
                    execution_id: execution.id.clone(),
                })?;
            }
            Ok(())
        }
        ObservationFact::LaunchAbsenceUnprovable {
            invocation_id,
            reason,
        } => {
            // The bounded escalation for an unidentified launch: the
            // absence scans could not be completed a bounded number of
            // consecutive times. The Invocation stays an unresolved
            // writer — a live process may still exist, so no slot is
            // released — and is marked `unknown`: the distinct outcome
            // surfaces the unprovable absence to the operator while
            // the Execution is held for an operator decision. The
            // mark does not end the resolution — the poll loop keeps
            // scanning the `unknown` launch, so a later provable
            // answer (a re-bound writer or a completed clear) still
            // settles it under the recorded desired action; the
            // transient burst is never a permanent wedge. An intent
            // already recorded on the Execution (cancel/fail/pause)
            // is never overwritten, and a terminal Execution is never
            // resurrected — the state guard lives inside
            // `pause_for_repair`.
            mark_invocation(tx, invocation_id, |_| InvocationState::Unknown)?;
            if execution.desired_action.is_none()
                || execution.desired_action == Some(DesiredAction::Continue)
            {
                pause_for_repair(
                    tx,
                    &execution,
                    "absence_unprovable",
                    &format!("repair: launch absence unprovable: {reason}"),
                    vec![RepairPrerequisite::OperatorDecision],
                    now_unix_seconds,
                )?;
            }
            Ok(())
        }
        ObservationFact::DispatchRefused {
            request_id,
            code,
            detail,
        } => {
            // No Invocation ever existed for the request: the refusal
            // is proven before spawn, so — like `LaunchRefused` — it
            // resolves the waiting Execution only from `ready` and
            // never resurrects a cancelled or settled one. The
            // durable refusal record keeps the typed code for audit;
            // the pause reason carries it verbatim for the operator.
            if execution.state == ExecutionState::Ready {
                let mut next = execution.clone();
                // The refund binds to the request the current `ready`
                // admission minted — `<eid>:inv:<invocations_used - 1>`
                // since the counter never decrements. A refusal
                // re-delivered under a fresh delivery id for an earlier
                // attempt's durable request row must not refund a
                // charged attempt a second time.
                let live_request = format!(
                    "{}:inv:{}",
                    execution.id.0,
                    execution.invocations_used.saturating_sub(1)
                );
                if *request_id == live_request
                    && let Some(role) = tx
                        .request(request_id)?
                        .filter(|request| request.execution_id == execution.id)
                        .map(|request| request.role)
                {
                    refund_refused_attempt(tx, &mut next, role)?;
                }
                next.state = ExecutionState::Paused;
                next.pause_reason = Some(format!("repair: dispatch refused {code}: {detail}"));
                next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
                next.paused_since_unix_seconds = Some(now_unix_seconds);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: ExecutionState::Ready,
                })?;
                tx.apply(&Effect::ReleaseSlot {
                    execution_id: execution.id.clone(),
                })?;
            }
            Ok(())
        }
        ObservationFact::Artifact { state, .. } => {
            assess_artifact(tx, config, &execution, state, now_unix_seconds)
        }
        ObservationFact::HostActionResult { status, .. } => {
            // The intent status was already updated above; only the
            // execution-facing transitions remain.
            if status == &HostActionStatus::Applied && execution.state == ExecutionState::Waiting {
                let mut next = execution.clone();
                next.waiting_reason = Some(WaitingReason::Ci);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })?;
            }
            if matches!(status, HostActionStatus::Failed { .. })
                && execution.state == ExecutionState::Waiting
            {
                if let HostActionStatus::Failed { reason } = status
                    && is_authorization_verdict(reason)
                {
                    // An authorization/eligibility verdict is terminal
                    // for the intent: retrying cannot change the
                    // outcome, so it never consumes the transport
                    // account or re-issues. The verdict itself pauses
                    // for an operator decision instead of degrading to
                    // a generic transport exhaustion.
                    pause_for_repair(
                        tx,
                        &execution,
                        "effect_authorization_denied",
                        "the stage effect was denied or the actor was ineligible; \
                         human action required",
                        vec![RepairPrerequisite::OperatorDecision],
                        now_unix_seconds,
                    )?;
                } else {
                    // A definite no-effect failure is safe to retry,
                    // bounded by the Work's transport account.
                    let work =
                        tx.work(&execution.work_id)?
                            .ok_or_else(|| StoreError::Internal {
                                detail: "execution without work".into(),
                            })?;
                    let mut usage = tx
                        .work_usage(&execution.work_id)?
                        .unwrap_or_else(|| WorkUsage::new(&execution.work_id));
                    usage.transport_retries_used += 1;
                    let attempt = usage.transport_retries_used;
                    let over = attempt > work.policy.transport_max_retries;
                    tx.apply(&Effect::UpdateWorkUsage { usage })?;
                    if over {
                        pause_for_repair(
                            tx,
                            &execution,
                            "transport_exhausted",
                            "transport retry bound reached; human action required",
                            vec![RepairPrerequisite::OperatorDecision],
                            now_unix_seconds,
                        )?;
                    } else {
                        // Persisted backoff: the retry intent is written
                        // in this deciding transaction, before any
                        // re-issue acts on it — `maybe_publish` defers
                        // to the scheduled wait instead of re-issuing
                        // immediately. The observed failure classifies to
                        // its `recovery.*` budget key, which the intent
                        // records as its evidence basis.
                        if let Some(class) = classify_failure(&obs.fact) {
                            schedule_recovery_intent(
                                tx,
                                &work,
                                Some(&execution.id),
                                "retry",
                                now_unix_seconds
                                    + transport_backoff_seconds(&work.policy, attempt) as i64,
                                class.budget_key(),
                            )?;
                        }
                    }
                }
            }
            if matches!(status, HostActionStatus::Unsupported { .. })
                && execution.state == ExecutionState::Waiting
            {
                // The operation is outside the host's capability map:
                // terminal and explicit, never a transport failure and
                // never retried. Pause for an operator decision instead
                // of wedging on an effect that can never land or
                // re-issuing the intent without bound.
                pause_for_repair(
                    tx,
                    &execution,
                    "unsupported_effect",
                    "the stage effect is outside the host's capability map; \
                     human action required",
                    vec![RepairPrerequisite::OperatorDecision],
                    now_unix_seconds,
                )?;
            }
            // A terminal settle that deferred on this outstanding effect
            // may complete now — but only when every writer is already
            // resolved, so the stop proof is on record (the same rule
            // drive_stops applies before reporting the exit).
            let stop_pending = matches!(execution.state, ExecutionState::Stopping)
                || (execution.state == ExecutionState::Uncertain
                    && matches!(
                        execution.desired_action,
                        Some(DesiredAction::Cancel)
                            | Some(DesiredAction::Fail)
                            | Some(DesiredAction::Pause)
                    ));
            if stop_pending {
                let writers_unresolved = tx
                    .invocations_for_execution(&execution.id)?
                    .iter()
                    .any(|i| i.state.is_unresolved_writer());
                if !writers_unresolved {
                    settle_after_writer_gone(tx, config, &execution, now_unix_seconds)?;
                }
            }
            // A reconciled receipt is the only event that can advance the
            // publish chain or free a bounded retry: `push_ref` applied
            // unlocks `open_change_request`, and a failed receipt with
            // transport retries left needs a re-issued intent. Re-read the
            // Execution so a pause/settle recorded above is honored;
            // `maybe_publish` re-checks every precondition itself.
            if let Some(current) = tx.execution(&execution.id)?
                && current.state == ExecutionState::Waiting
            {
                maybe_publish(tx, config, &current, now_unix_seconds)?;
            }
            Ok(())
        }
        ObservationFact::Review { .. } => {
            // A fresh complete round with undecided occurrences pauses
            // for human judgment. Only a round covering the current
            // diff target counts — a stale-target review is recorded but
            // cannot suspend the work.
            if execution.state == ExecutionState::Waiting
                && let Some(target) = current_target(tx, &execution.work_id)?
                && let Some(round) = latest_round_for_target(tx, &execution.work_id, &target)?
                && round.completeness == "complete"
            {
                let occurrences = tx.occurrences(&round.id)?;
                let judged = tx.judgment_for_round(&round.id)?.is_some_and(|j| {
                    j.decisions
                        .iter()
                        .all(|d| matches!(d.choice, FindingChoice::Accept | FindingChoice::Dismiss))
                });
                if !occurrences.is_empty() && !judged {
                    pause_for_repair(
                        tx,
                        &execution,
                        "review_findings",
                        "review findings require human disposition",
                        vec![RepairPrerequisite::OperatorDecision],
                        now_unix_seconds,
                    )?;
                }
            }
            Ok(())
        }
        ObservationFact::Check { .. } => assess_check_failure(tx, &execution, now_unix_seconds),
        // The handoff acknowledgement these facts ride on is the
        // moment the produced commit became durable: publication may
        // now name it, so a waiting Execution re-evaluates its stage
        // intents.
        ObservationFact::ClaimedHeadCheck { .. } | ObservationFact::HandoffAcknowledged { .. }
            if execution.state == ExecutionState::Waiting =>
        {
            maybe_publish(tx, config, &execution, now_unix_seconds)
        }
        _ => Ok(()),
    }?;
    let Some(current) = tx.execution(&execution.id)? else {
        return Ok(());
    };
    assess_completion(tx, &current)
}

/// Record the reconciled intent status. `applied` requires a normalized
/// effect fact matching the intent's immutable payload; an unsupported
/// `applied` claim is recorded as `unknown` — invalid evidence can never
/// mark an intent applied.
fn reconcile_intent(
    tx: &mut dyn TxView,
    action_id: &str,
    authenticated_actor: Option<HostActorRef>,
    status: HostActionStatus,
    effects: Vec<NormalizedEffectFact>,
    _now: i64,
) -> Result<(), StoreError> {
    let Some(intent) = tx
        .action_intents()?
        .into_iter()
        .find(|i| i.action_id == action_id)
    else {
        return Ok(());
    };
    // Terminal reconciliation sticks: a late receipt cannot resurrect a
    // failed or unsupported intent or downgrade an applied one — with
    // one drift exception. An `applied` intent whose remote fact later
    // diverges is re-armable: an `unknown` receipt re-opens
    // reconciliation, but only when authenticated by the intent's bound
    // actor *and* the receipt's effects carry the drift fact itself. A
    // stale or out-of-order transport-`unknown` without drift evidence
    // is not a re-arm — it would demote a settled intent behind a
    // receipt that proves nothing about the remote. An unauthenticated
    // or unbound receipt is invalid evidence and can never re-open a
    // reconciled intent. `unknown` stays reconcilable — a later
    // receipt for the same action_id is what resolves it.
    if intent.status == HostActionStatus::Applied {
        let drift_rearm = matches!(status, HostActionStatus::Unknown { .. })
            && authenticated_actor.as_ref() == Some(&intent.actor)
            && is_drift_evidence(&intent, &effects);
        if !drift_rearm {
            return Ok(());
        }
        return tx.apply(&Effect::UpdateHostActionIntent {
            action_id: action_id.to_string(),
            status,
            authenticated_actor,
            effects,
        });
    }
    if matches!(
        intent.status,
        HostActionStatus::Failed { .. } | HostActionStatus::Unsupported { .. }
    ) {
        return Ok(());
    }
    // A receipt authenticated as a different actor than the binding
    // authorized is invalid evidence — whatever status it claims, it
    // settles nothing. The intent stays outstanding as
    // `unknown{actor_mismatch}`, never `applied` and never `failed`.
    if authenticated_actor
        .as_ref()
        .is_some_and(|a| a != &intent.actor)
    {
        return tx.apply(&Effect::UpdateHostActionIntent {
            action_id: action_id.to_string(),
            status: HostActionStatus::Unknown {
                reason: "actor_mismatch".into(),
            },
            authenticated_actor,
            effects,
        });
    }
    let recorded = match status {
        HostActionStatus::Applied if effect_matches(&intent, &effects) => HostActionStatus::Applied,
        HostActionStatus::Applied => HostActionStatus::Unknown {
            reason: "applied claim carried no matching effect fact".into(),
        },
        other => other,
    };
    tx.apply(&Effect::UpdateHostActionIntent {
        action_id: action_id.to_string(),
        status: recorded,
        authenticated_actor,
        effects,
    })
}

/// Whether the receipt evidences real drift for this intent — a
/// remote ref observation proving the pushed revision is no longer
/// contained. Only a `push_ref` intent has a drift fact; for every
/// other action an `unknown` receipt against an `applied` intent is a
/// stale or out-of-order transport record, never re-arm evidence.
fn is_drift_evidence(intent: &HostActionIntent, effects: &[NormalizedEffectFact]) -> bool {
    let HostAction::PushRef {
        branch, produced, ..
    } = &intent.action
    else {
        return false;
    };
    effects.iter().any(|e| {
        matches!(
            e,
            NormalizedEffectFact::RefPublished {
                repository,
                branch: ref_branch,
                produced: ref_produced,
                relation: RefRelation::Diverged,
                ..
            } if repository == &intent.repository
                && ref_branch == branch
                && ref_produced == produced
        )
    })
}

/// Whether the receipt's normalized facts contain one matching the
/// intent's immutable payload.
fn effect_matches(intent: &HostActionIntent, effects: &[NormalizedEffectFact]) -> bool {
    effects.iter().any(|e| match (&intent.action, e) {
        (
            HostAction::PushRef {
                branch, produced, ..
            },
            NormalizedEffectFact::RefPublished {
                repository,
                branch: ref_branch,
                produced: ref_produced,
                observed,
                relation,
            },
        ) => {
            repository == &intent.repository
                && ref_branch == branch
                && ref_produced == produced
                && observed.repository == produced.repository
                && matches!(relation, RefRelation::Equal | RefRelation::Ancestor)
        }
        (
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                ..
            },
            NormalizedEffectFact::ChangeOpened {
                change,
                source_branch: opened_source,
                target_branch: opened_target,
                ..
            },
        ) => {
            change.repository == intent.repository
                && opened_source == source_branch
                && opened_target == target_branch
        }
        (
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                ..
            },
            NormalizedEffectFact::MetadataUpdated {
                change: updated,
                head,
                ..
            },
        ) => updated == change && head == expected_head,
        (
            HostAction::PostComment {
                change,
                expected_head,
                marker,
                body,
            },
            NormalizedEffectFact::CommentPosted {
                change: commented,
                marker: comment_marker,
                body_digest,
                target,
                ..
            },
        ) => {
            // A marker match alone must not adopt a foreign comment:
            // the observed body must digest to the intent's own body.
            commented == change
                && comment_marker == marker
                && *body_digest
                    == sha256_hex(
                        "comment-v1",
                        &canonical_json(&serde_json::json!({ "body": body })),
                    )
                && target.as_ref().is_none_or(|t| &t.head == expected_head)
        }
        (
            HostAction::PublishReviewFeedback { change, head, .. },
            NormalizedEffectFact::ReviewPublished {
                change: reviewed,
                head: reviewed_head,
                ..
            },
        ) => reviewed == change && reviewed_head == head,
        (
            HostAction::ReplyToThread {
                change,
                thread_id,
                marker,
                ..
            },
            NormalizedEffectFact::ReplyPosted {
                change: replied,
                thread_id: replied_thread,
                marker: reply_marker,
                ..
            },
        ) => replied == change && replied_thread == thread_id && reply_marker == marker,
        (
            HostAction::ResolveThread {
                change,
                thread_id,
                head,
            },
            NormalizedEffectFact::ThreadResolved {
                change: resolved,
                thread_id: resolved_thread,
                target,
                observed,
                relation,
            },
        ) => {
            resolved == change
                && resolved_thread == thread_id
                && &target.head == head
                && observed.repository == head.repository
                && matches!(relation, RefRelation::Equal | RefRelation::Ancestor)
        }
        (
            HostAction::MarkReady {
                change,
                expected_head,
            },
            NormalizedEffectFact::ReadyObserved {
                change: readied,
                head,
                observed,
                relation,
            },
        ) => {
            readied == change
                && head == expected_head
                && observed.repository == expected_head.repository
                && matches!(relation, RefRelation::Equal | RefRelation::Ancestor)
        }
        _ => false,
    })
}

/// Whether a failed host-action receipt reports an authorization or
/// eligibility verdict — `denied` when the actor lacks the grant,
/// `ineligible` when the actor's class cannot perform the operation.
/// These are terminal for the intent and never transport failures:
/// a retry cannot change the verdict, so it must not consume the
/// transport retry account or re-issue the intent.
fn is_authorization_verdict(reason: &str) -> bool {
    reason == "denied" || reason == "ineligible"
}

/// The durable basis an observed fact commits as material progress —
/// the recovery-policy progress definition: an accepted artifact, a
/// required check now passing, or an unknown effect resolved. Log
/// volume and new commits alone never count.
pub fn material_progress_basis(fact: &ObservationFact) -> Option<MaterialProgressBasis> {
    match fact {
        ObservationFact::Artifact {
            state: ArtifactFact::Present { .. },
            ..
        } => Some(MaterialProgressBasis::AcceptedArtifact),
        ObservationFact::Check {
            conclusion: CheckConclusion::Success,
            ..
        } => Some(MaterialProgressBasis::CheckNowPassing),
        ObservationFact::HostActionResult {
            status: HostActionStatus::Applied,
            ..
        } => Some(MaterialProgressBasis::UnknownEffectResolved),
        _ => None,
    }
}

/// The Work's material-progress clock, derived from the durable usage
/// row. `last_progress_at` is the committed timestamp of the last
/// material-progress record; a Work with no commit anchors at first
/// dispatch. A stored signal that cannot be read — a timestamp
/// without its recorded basis, or one preceding first dispatch — is
/// `Unknown`: surfaced, never treated as progress or as death.
pub fn material_progress(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<MaterialProgressSignal, StoreError> {
    let Some(usage) = tx.work_usage(work_id)? else {
        return Ok(MaterialProgressSignal::NotStarted);
    };
    let first = usage.first_dispatch_unix_seconds;
    match usage.last_progress_unix_seconds {
        None => match first {
            Some(at) => Ok(MaterialProgressSignal::Reading(MaterialProgress {
                work_id: work_id.clone(),
                last_progress_at: at,
                basis: None,
            })),
            None => Ok(MaterialProgressSignal::NotStarted),
        },
        Some(at) if usage.last_progress_basis.is_some() && first.is_none_or(|f| at >= f) => {
            Ok(MaterialProgressSignal::Reading(MaterialProgress {
                work_id: work_id.clone(),
                last_progress_at: at,
                basis: usage.last_progress_basis,
            }))
        }
        Some(_) => Ok(MaterialProgressSignal::Unknown),
    }
}

/// The observed failure's class — which `recovery.*` budget key the
/// Work's policy charges. `None` for facts that are not failures and
/// for terminal verdicts a retry cannot change.
pub fn classify_failure(fact: &ObservationFact) -> Option<FailureClass> {
    match fact {
        ObservationFact::HostActionResult { status, .. } => match status {
            HostActionStatus::Failed { reason } if !is_authorization_verdict(reason) => {
                Some(FailureClass::Transport)
            }
            HostActionStatus::Unknown { .. } => Some(FailureClass::UnknownEffect),
            _ => None,
        },
        ObservationFact::Check {
            conclusion: CheckConclusion::Failure,
            ..
        } => Some(FailureClass::Repair),
        _ => None,
    }
}

/// Recompute the source gate from the latest authoritative reads.
/// `complete` coverage requires every spec source to carry a matching
/// fresh read; a mismatch holds as `changed` (new spec + reapproval),
/// an unavailable read holds as `unavailable`.
fn assess_source_gate(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    let Some(work) = tx.work(work_id)? else {
        return Ok(());
    };
    if matches!(work.source_gate, SourceGate::NotApplicable) {
        return Ok(());
    }
    let spec = tx
        .spec(work_id, Some(work.approved_spec))?
        .ok_or_else(|| StoreError::Internal {
            detail: "work without approved spec".into(),
        })?;
    let observations = tx.observations_for_work(work_id)?;
    let mut latest: BTreeMap<&IssueRef, (i64, usize, &Observation)> = BTreeMap::new();
    for (index, o) in observations.iter().enumerate() {
        if let ObservationFact::Issue { issue, .. } = &o.fact {
            let replace = latest
                .get(issue)
                .is_none_or(|(t, i, _)| (*t, *i) < (o.observed_unix_seconds, index));
            if replace {
                latest.insert(issue, (o.observed_unix_seconds, index, o));
            }
        }
    }
    let mut gate = SourceGate::Matches;
    for source in &spec.payload.sources {
        match latest.get(&source.issue) {
            None => {
                gate = gate.min_gate(SourceGate::NeedsCheck);
            }
            Some((_, _, o)) => match &o.fact {
                ObservationFact::Issue {
                    source_revision,
                    content_digest,
                    state,
                    ..
                } => {
                    if state == &IssueState::Unavailable {
                        gate = gate.min_gate(SourceGate::Unavailable);
                    } else if state == &IssueState::Closed
                        || source_revision != &source.source_revision
                        || content_digest != &source.content_digest
                    {
                        gate = gate.min_gate(SourceGate::Changed);
                    }
                }
                _ => unreachable!(),
            },
        }
    }
    if gate != work.source_gate {
        let mut next_work = work.clone();
        next_work.source_gate = gate;
        next_work.version += 1;
        tx.apply(&Effect::UpdateWork {
            work: next_work,
            expected: work.version,
        })?;
        // A changed/unavailable source also pauses a queued or running
        // Execution for repair; `Matches` unblocks through the normal
        // dispatch scan.
        if matches!(gate, SourceGate::Changed | SourceGate::Unavailable)
            && let Some(execution) = tx.active_execution(work_id)?
            && matches!(
                execution.state,
                ExecutionState::Queued
                    | ExecutionState::Ready
                    | ExecutionState::Running
                    | ExecutionState::Waiting
                    | ExecutionState::Uncertain
            )
        {
            let (classification, detail, prereq) = match gate {
                SourceGate::Changed => (
                    "source_changed",
                    "source changed after spec approval; a new spec revision and reapproval are required",
                    RepairPrerequisite::SpecReapproval,
                ),
                _ => (
                    "source_unavailable",
                    "source read unavailable; a fresh matching read is required",
                    RepairPrerequisite::SourceRead,
                ),
            };
            pause_for_repair(
                tx,
                &execution,
                classification,
                detail,
                vec![prereq],
                now_unix_seconds,
            )?;
        }
    }
    Ok(())
}

/// Seal an immutable review round for a complete review fact. An
/// identical set on the same target is idempotent; a changed set — the
/// same-head finding growth case — installs a new round and bumps the
/// Work's evidence generation.
fn seal_round(
    tx: &mut dyn TxView,
    obs: &Observation,
    work_id: &WorkId,
    _now: i64,
) -> Result<(), StoreError> {
    let ObservationFact::Review {
        target,
        source,
        source_revision,
        completeness,
        occurrences,
    } = &obs.fact
    else {
        return Ok(());
    };
    // The review lane binds the configured reviewer identity: when the
    // Work's host binding names a review actor, only a review reported
    // under that actor seals a round. A review from any other source —
    // the generic automation identity included — stays recorded as an
    // observation but is never authoritative review evidence, so it can
    // satisfy no lane and trigger no findings pause.
    if let Some(work) = tx.work(work_id)?
        && let Some(bound) = work.binding.actor_for(ActorRole::Review)
        && source.as_str() != bound.actor.as_str()
    {
        return Ok(());
    }
    if !completeness.is_complete() {
        // An incomplete review is recorded as evidence but never seals a
        // round; it can satisfy nothing.
        return Ok(());
    }
    let mut set_material: Vec<(String, String)> = occurrences
        .iter()
        .map(|o| {
            (
                o.source_key.clone(),
                o.content_digest.clone().unwrap_or_default(),
            )
        })
        .collect();
    set_material.sort();
    set_material.dedup();
    let set_digest = sha256_hex(
        "round-v1",
        canonical_json(&serde_json::json!({
            "target": target,
            "occurrences": set_material,
        }))
        .as_slice(),
    );
    if let Some(latest) = latest_round_for_target(tx, work_id, target)?
        && latest.set_digest == set_digest
        && latest.completeness == "complete"
    {
        // Idempotent redelivery of the identical set for this target;
        // an unrelated newer round must not force a duplicate seal.
        return Ok(());
    }
    let seq = tx.alloc_seq()?;
    let generation = tx
        .latest_round(work_id)?
        .map(|r| r.generation + 1)
        .unwrap_or(0);
    let round = ReviewRound {
        id: new_id("round"),
        work_id: work_id.clone(),
        target: target.clone(),
        source: source.clone(),
        source_revision: source_revision.clone(),
        completeness: match completeness {
            Completeness::Complete => "complete".to_string(),
            Completeness::Partial { reason } => format!("partial:{reason}"),
            Completeness::Unavailable { reason } => format!("unavailable:{reason}"),
        },
        set_digest,
        generation,
        seq,
    };
    let occurrences: Vec<FindingOccurrence> = occurrences
        .iter()
        .enumerate()
        .map(|(i, o)| FindingOccurrence {
            id: format!("occ-{}-{i:06}", round.id),
            round_id: round.id.clone(),
            source_key: o.source_key.clone(),
            content_digest: o.content_digest.clone(),
            severity: o.severity.clone(),
            position: o.position.clone(),
            thread_id: o.thread_id.clone(),
            summary: o.summary.clone(),
        })
        .collect();
    tx.apply(&Effect::InsertReviewRound { round, occurrences })?;
    // Evidence for the CURRENT diff target changed: bump the Work
    // version so every precondition on the old evidence surface is
    // stale. A round sealed for a superseded or not-yet-current target
    // is recorded evidence, not a current-state change — it invalidates
    // nothing.
    if current_target(tx, work_id)?.as_ref() == Some(target)
        && let Some(work) = tx.work(work_id)?
    {
        let mut next_work = work.clone();
        next_work.version += 1;
        tx.apply(&Effect::UpdateWork {
            work: next_work,
            expected: work.version,
        })?;
    }
    Ok(())
}

/// A failed check on the current head counts toward the equivalent-
/// failure bound; reaching it pauses for human action.
fn assess_check_failure(
    tx: &mut dyn TxView,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    let work = tx
        .work(&execution.work_id)?
        .ok_or_else(|| StoreError::Internal {
            detail: "execution without work".into(),
        })?;
    let Some(target) = current_target(tx, &work.id)? else {
        return Ok(());
    };
    let observations = tx.observations_for_work(&work.id)?;
    // The latest conclusion per named check on the current head.
    let mut latest: BTreeMap<&str, (i64, usize, CheckConclusion)> = BTreeMap::new();
    for (index, o) in observations.iter().enumerate() {
        if let ObservationFact::Check {
            change,
            head,
            check_id,
            conclusion,
            ..
        } = &o.fact
            && change == &target.change
            && head == &target.head
        {
            let replace = latest
                .get(check_id.as_str())
                .is_none_or(|(t, i, _)| (*t, *i) < (o.observed_unix_seconds, index));
            if replace {
                latest.insert(
                    check_id.as_str(),
                    (o.observed_unix_seconds, index, conclusion.clone()),
                );
            }
        }
    }
    // Equivalent-failure signature: stage + stable criterion + category.
    // The streak tracks ONE failure shape — the whole set of failing
    // required checks. Signing each check separately would alternate the
    // stored signature under multiple failures and never reach the limit.
    let spec = tx.spec(&work.id, Some(work.approved_spec))?;
    let Some(spec) = spec else { return Ok(()) };
    let mut failing: Vec<&str> = spec
        .payload
        .required_checks
        .iter()
        .map(|id| id.as_str())
        .filter(|check_id| {
            latest
                .get(*check_id)
                .is_some_and(|(_, _, c)| c == &CheckConclusion::Failure)
        })
        .collect();
    failing.sort_unstable();
    if failing.is_empty() {
        return Ok(());
    }
    let signature = format!("checks:{}:failure", failing.join("+"));
    let mut usage = tx
        .work_usage(&work.id)?
        .unwrap_or_else(|| WorkUsage::new(&work.id));
    if usage.equivalent_signature.as_deref() == Some(signature.as_str()) {
        usage.equivalent_failure_streak += 1;
    } else {
        usage.equivalent_failure_streak = 1;
        usage.equivalent_signature = Some(signature.clone());
    }
    let over = usage.equivalent_failure_streak >= work.policy.equivalent_failure_limit;
    tx.apply(&Effect::UpdateWorkUsage { usage })?;
    if over {
        let detail = format!(
            "equivalent failures for {} reached the policy limit",
            failing.join(", ")
        );
        pause_for_repair(
            tx,
            execution,
            "equivalent_failure",
            &detail,
            vec![RepairPrerequisite::OperatorDecision],
            now_unix_seconds,
        )?;
    }
    Ok(())
}

fn mark_invocation(
    tx: &mut dyn TxView,
    invocation_id: &InvocationId,
    state: impl Fn(&Invocation) -> InvocationState,
) -> Result<(), StoreError> {
    for exec in tx.nonterminal_executions()? {
        for inv in tx.invocations_for_execution(&exec.id)? {
            if inv.id == *invocation_id && inv.state.is_unresolved_writer() {
                let next_state = state(&inv);
                if inv.state != next_state {
                    let mut next = inv.clone();
                    next.state = next_state;
                    tx.apply(&Effect::UpdateInvocation {
                        invocation: next,
                        expected_state: inv.state.clone(),
                    })?;
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Roll back the accounting a refused pre-spawn attempt consumed at
/// admission. Admission cleared `next_role` into the minted request and
/// charged the role-scoped ledgers — the Work-wide fix account and the
/// Execution review-round ledger — so a launch proven never to have
/// spawned hands both back: `next_role` re-arms so a `resume` re-issues
/// the recorded role instead of degrading to `Coding`, and the bounded
/// ledgers refund so the re-issued dispatch is charged exactly once.
/// `invocations_used` deliberately stays charged: every request-creation
/// attempt counts against `max_invocations`, and the counter mints
/// request ids that must never repeat.
fn refund_refused_attempt(
    tx: &mut dyn TxView,
    next: &mut Execution,
    role: InvocationRole,
) -> Result<(), StoreError> {
    next.next_role = Some(role.clone());
    match role {
        InvocationRole::Fix => {
            let mut usage = tx
                .work_usage(&next.work_id)?
                .unwrap_or_else(|| WorkUsage::new(&next.work_id));
            usage.fix_invocations_used = usage.fix_invocations_used.saturating_sub(1);
            tx.apply(&Effect::UpdateWorkUsage { usage })?;
        }
        InvocationRole::Review => {
            next.review_rounds_used = next.review_rounds_used.saturating_sub(1);
        }
        InvocationRole::Coding | InvocationRole::InvariantCheck => {}
    }
    Ok(())
}

fn assess_process(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    invocation_id: &InvocationId,
    reported_identity: &str,
    state: &ProcessStateFact,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    match state {
        ProcessStateFact::Running => match execution.state {
            ExecutionState::Ready => {
                let mut next = execution.clone();
                next.state = ExecutionState::Running;
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            // A writer re-confirmed alive resolves `uncertain(continue)`.
            ExecutionState::Uncertain
                if execution.desired_action == Some(DesiredAction::Continue) =>
            {
                let mut next = execution.clone();
                next.state = ExecutionState::Running;
                next.desired_action = None;
                next.uncertain_since_unix_seconds = None;
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            _ => Ok(()),
        },
        ProcessStateFact::Exited => {
            // A proven exit resolves the writer. An Invocation that
            // never recorded a process identity — the launch outcome
            // was lost before the identity commit — is `abandoned`
            // (proven unable to write), not `done`: nothing ran to
            // finish. The fact must also carry no identity of its own:
            // an exit reported against a named process is evidence a
            // writer existed.
            mark_invocation(tx, invocation_id, |inv| {
                if inv.process_identity.is_none() && reported_identity.is_empty() {
                    InvocationState::Abandoned
                } else {
                    InvocationState::Done
                }
            })?;
            match execution.state {
                // A proven exit for a still-`ready` Execution is the
                // lost-Running-report case: the writer ran and is gone,
                // so it follows the same reconciliation path instead of
                // holding the slot until a sweep fails the completed work.
                ExecutionState::Ready | ExecutionState::Running => {
                    let mut next = execution.clone();
                    next.state = ExecutionState::Waiting;
                    next.waiting_reason = Some(WaitingReason::Reconciliation);
                    tx.apply(&Effect::UpdateExecution {
                        execution: next.clone(),
                        expected_state: execution.state.clone(),
                    })?;
                    maybe_publish(tx, config, &next, now_unix_seconds)
                }
                ExecutionState::Waiting => maybe_publish(tx, config, execution, now_unix_seconds),
                ExecutionState::Stopping | ExecutionState::Uncertain => {
                    settle_after_writer_gone(tx, config, execution, now_unix_seconds)
                }
                _ => Ok(()),
            }
        }
        ProcessStateFact::Unknown => match execution.state {
            ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
                let mut next = execution.clone();
                next.state = ExecutionState::Uncertain;
                next.desired_action = Some(DesiredAction::Continue);
                next.uncertain_since_unix_seconds = Some(now_unix_seconds);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            // An unverifiable writer during a stop: the requested outcome
            // cannot be proven, so the Execution is uncertain — keeping
            // the recorded desired action — until attention resolves it.
            ExecutionState::Stopping => {
                let mut next = execution.clone();
                next.state = ExecutionState::Uncertain;
                if next.uncertain_since_unix_seconds.is_none() {
                    next.uncertain_since_unix_seconds = Some(now_unix_seconds);
                }
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })
            }
            _ => Ok(()),
        },
    }
}

/// The writer is gone: resolve `stopping`/`uncertain` to the recorded
/// desired action — terminal states release slot and reservation, a
/// recoverable pause releases only the slot.
fn settle_after_writer_gone(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    let desired = execution
        .desired_action
        .clone()
        .unwrap_or(DesiredAction::Fail);
    // Terminal settlement waits for outstanding outbox effects to
    // reconcile (lifecycle.md): a pending or unknown host write must
    // not be drained after a recorded cancellation/failure. A pending
    // intent counts only while the Work's latest Execution could still
    // attempt it — under a denied `host_write` grant it can never
    // deliver an effect, so deferring on it stalls the outcome
    // forever. The `host_action_result` observation re-drives this
    // settle once the intent is resolved.
    if matches!(desired, DesiredAction::Cancel | DesiredAction::Fail)
        && !crate::projection::settlement_blocking_actions(tx, &execution.work_id)?.is_empty()
    {
        return Ok(());
    }
    let mut next = execution.clone();
    let seq = tx.alloc_seq()?;
    let release_reservations = match desired {
        DesiredAction::Cancel => {
            next.state = ExecutionState::Cancelled;
            next.terminal_code = Some("cancelled".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Cancelled,
                code: Some("cancelled".into()),
                detail: Some("writer stop proven".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            true
        }
        DesiredAction::Fail => {
            next.state = ExecutionState::Failed;
            next.terminal_code = Some("stopped".into());
            next.outcome = Some(Outcome {
                state: ExecutionState::Failed,
                code: Some("stopped".into()),
                detail: Some("writer stop proven; recorded failure".into()),
                evidence_refs: vec![],
                recorded_seq: seq,
            });
            true
        }
        DesiredAction::Continue => {
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("recoverable_stop".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(now_unix_seconds);
            false
        }
        DesiredAction::Pause => {
            // A repair pause recorded its reason and prerequisites when
            // the Execution entered `stopping`; settle to `paused` now
            // that writer safety is proven.
            next.state = ExecutionState::Paused;
            if next.pause_reason.is_none() {
                next.pause_reason = Some("recoverable_stop".into());
            }
            if next.repair_prerequisites.is_empty() {
                next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            }
            next.paused_since_unix_seconds = Some(now_unix_seconds);
            false
        }
    };
    next.desired_action = None;
    next.uncertain_since_unix_seconds = None;
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    if release_reservations {
        tx.apply(&Effect::DeleteReservations {
            resource_keys: resources_keys_of(tx, execution)?,
            holder: ReservationHolder::Execution {
                execution_id: execution.id.clone(),
            },
        })?;
        tx.apply(&Effect::ReleaseIssueClaims {
            work_id: execution.work_id.clone(),
        })?;
    }
    let _ = config;
    Ok(())
}

fn assess_artifact(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    state: &ArtifactFact,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    match state {
        // Required evidence missing after a confirmed exit → bounded
        // recovery. The deadline is durable from the first transition in.
        ArtifactFact::Missing | ArtifactFact::Corrupt { .. }
            if execution.state == ExecutionState::Waiting
                || execution.state == ExecutionState::Running =>
        {
            let mut next = execution.clone();
            next.state = ExecutionState::Waiting;
            next.waiting_reason = Some(WaitingReason::ArtifactRecovery);
            if next.recovery_deadline_unix_seconds.is_none() {
                next.recovery_deadline_unix_seconds =
                    Some(now_unix_seconds + execution.budget.artifact_recovery_seconds as i64);
            }
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state.clone(),
            })
        }
        ArtifactFact::Present { .. }
            if execution.state == ExecutionState::Waiting
                || execution.state == ExecutionState::Running =>
        {
            maybe_publish(tx, config, execution, now_unix_seconds)
        }
        _ => Ok(()),
    }
}

/// Re-drive `maybe_publish` for every Waiting execution whose Work
/// binds the reported host. A capability report carries no work or
/// execution subject, so without this sweep a publish chain blocked
/// on `capability_held` would wait for an unrelated observation to
/// re-fire — the level-triggered gap that left authorized work
/// parked at `waiting` with zero intents.
fn redrive_capability_waiters(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    host: &HostInstanceId,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    for work in tx.works()? {
        if canonical_host_instance(work.binding.repository.host.as_str())
            != canonical_host_instance(host.as_str())
        {
            continue;
        }
        let Some(execution) = crate::projection::current_execution(tx, &work.id)? else {
            continue;
        };
        if execution.state == ExecutionState::Waiting {
            maybe_publish(tx, config, &execution, now_unix_seconds)?;
        }
    }
    Ok(())
}

/// Once the producing Invocation's evidence is complete — and, when
/// the invariant lane is enabled, a completed invariant-check
/// Invocation recorded after it — record the next stage effect intent:
/// the outbox row exists before any host write is attempted. Stage
/// effects chain on each other's receipts: `push_ref` first, then
/// `open_change_request` once the ref is published.
fn maybe_publish(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<(), StoreError> {
    if execution.state != ExecutionState::Waiting {
        return Ok(());
    }
    let invocations = tx.invocations_for_execution(&execution.id)?;
    if invocations.iter().any(|i| i.state.is_unresolved_writer()) {
        return Ok(());
    }
    // The published artifact belongs to the latest producing
    // Invocation (coding/fix). A check or review Invocation never
    // re-publishes its own output as the produced revision.
    let Some(producing) = invocations
        .iter()
        .rposition(|i| matches!(i.role, InvocationRole::Coding | InvocationRole::Fix))
    else {
        return Ok(());
    };
    let latest = &invocations[producing];
    if latest.state != InvocationState::Done {
        return Ok(());
    }
    let artifact_digests = artifact_digests(tx, &execution.id, latest)?;
    if artifact_digests.is_empty() {
        return Ok(());
    }
    // `push_ref` publishes the produced *commit*, so the revision must
    // be the one the durable handoff names — an artifact fact's
    // payload digest identifies bytes, never a git object, and a
    // remote ref can never target it. Only an invocation with no
    // handoff rows at all publishes the recorded digest token.
    let Some(produced_object) = produced_revision(tx, latest, &artifact_digests)? else {
        return Ok(());
    };
    let outstanding = crate::projection::outstanding_actions(tx, &execution.work_id)?;
    if !outstanding.is_empty() {
        return Ok(());
    }
    // A pending transport-retry intent owns the next failed-effect
    // re-issue: an unrelated drive of this chain can never outrun the
    // persisted backoff. The scheduled intent exists only while some
    // stage's latest effect is a definite failure, so the chain is
    // already blocked at that stage.
    let retry_pending = tx.recovery_intents()?.iter().any(|i| {
        i.execution_id.as_ref() == Some(&execution.id)
            && i.state == RecoveryIntentState::Pending
            && i.kind == "retry"
    });
    if retry_pending {
        return Ok(());
    }
    let work = tx
        .work(&execution.work_id)?
        .ok_or_else(|| StoreError::Internal {
            detail: "execution without work".into(),
        })?;
    let spec = tx
        .spec(&work.id, Some(work.approved_spec))?
        .ok_or_else(|| StoreError::Internal {
            detail: "work without spec".into(),
        })?;
    // Invariant lane: publication waits for the check's own durable
    // Invocation evidence — a `Done` invariant-check recorded after
    // the producing Invocation — never for an assessment fact alone.
    // `Done` alone only records that the process resolved: the check
    // must also carry a proven exit status of 0, so a failed or
    // unverifiable check exit is resolved-but-not-conforming evidence
    // and never opens the gate. A missing or superseded check
    // re-requests under the same Execution reservation
    // (`waiting → ready`); the attempt is budget-bounded like every
    // other Invocation.
    if spec.payload.lanes.invariant_check {
        let checked = satisfied_invariant_check(tx, &execution.id, &invocations[producing + 1..])?;
        if !checked {
            admit_queued_role(
                tx,
                config,
                &execution.id,
                InvocationRole::InvariantCheck,
                now_unix_seconds,
            )?;
            return Ok(());
        }
    }
    let intents = tx.action_intents()?;
    let work_intents: Vec<&HostActionIntent> =
        intents.iter().filter(|i| i.work_id == work.id).collect();
    // Stage 1: push_ref. Produced revision = the produced commit the
    // acknowledged handoff names (or, with no handoff rows, the
    // recorded artifact token non-git worlds publish).
    let branch = format!("work/{}", work.id.0);
    // The stage is satisfied only by an applied push that published the
    // *current* produced revision — a fix Invocation's new produced
    // commit re-arms the stage so the branch republishes.
    let push_applied = work_intents.iter().any(|i| {
        matches!(
            &i.action,
            HostAction::PushRef { produced, .. }
                if produced.object == produced_object
        ) && i.status == HostActionStatus::Applied
    });
    if !push_applied {
        // A definitely-failed latest attempt re-issues while the
        // transport account has retries left (the receipt handler pauses
        // when spent). Pending/unknown pushes are already caught by the
        // outstanding check above; an applied push for a stale produced
        // revision is exactly the republish case — fall through and
        // record a fresh intent. An unsupported push is terminal: its
        // receipt pauses the execution for an operator decision, and the
        // intent is never re-issued.
        if let Some(latest_push) = work_intents
            .iter()
            .rfind(|i| matches!(i.action, HostAction::PushRef { .. }))
        {
            match &latest_push.status {
                HostActionStatus::Failed { reason } => {
                    // An authorization/eligibility verdict is terminal:
                    // its receipt already paused for an operator
                    // decision, and a re-issued push would be refused
                    // the same way.
                    if is_authorization_verdict(reason) {
                        return Ok(());
                    }
                    let usage = tx
                        .work_usage(&work.id)?
                        .unwrap_or_else(|| WorkUsage::new(&work.id));
                    if usage.transport_retries_used > work.policy.transport_max_retries {
                        return Ok(());
                    }
                }
                HostActionStatus::Unsupported { .. } => return Ok(()),
                _ => {}
            }
        }
        let produced =
            CommitRevision::new(work.binding.repository.clone(), produced_object.clone())
                .map_err(|e| StoreError::Internal { detail: e })?;
        // The guard's expected prior is the freshest remote position
        // this installation observed for the branch, ordered by
        // (observed_unix_seconds, index) — the same ordering
        // `current_target` applies to the same facts. A delayed older
        // `BranchState` delivered after a newer applied read-back never
        // moves the position backward; an explicit absent re-arms
        // create, and a first publish expects an absent ref.
        enum BranchPosition {
            Unknown,
            Absent,
            At(CommitRevision),
        }
        let applied_pushes: std::collections::BTreeSet<&str> = work_intents
            .iter()
            .filter(|i| i.status == HostActionStatus::Applied)
            .map(|i| i.action_id.as_str())
            .collect();
        let mut latest: Option<(i64, usize, BranchPosition)> = None;
        for (index, o) in tx.observations_for_work(&work.id)?.iter().enumerate() {
            let candidate = match &o.fact {
                ObservationFact::BranchState {
                    repository,
                    branch: state_branch,
                    state,
                } if state_branch == &branch && repository == &work.binding.repository => {
                    Some(match state {
                        BranchState::At { revision } => BranchPosition::At(revision.clone()),
                        BranchState::Absent => BranchPosition::Absent,
                    })
                }
                ObservationFact::HostActionResult {
                    action_id,
                    status: HostActionStatus::Applied,
                    effects,
                    ..
                } if applied_pushes.contains(action_id.as_str()) => effects.iter().find_map(|e| {
                    if let NormalizedEffectFact::RefPublished {
                        repository,
                        branch: ref_branch,
                        observed,
                        ..
                    } = e
                        && ref_branch == &branch
                        && repository == &work.binding.repository
                    {
                        Some(BranchPosition::At(observed.clone()))
                    } else {
                        None
                    }
                }),
                _ => None,
            };
            if let Some(candidate) = candidate
                && latest
                    .as_ref()
                    .is_none_or(|(t, i, _)| (*t, *i) < (o.observed_unix_seconds, index))
            {
                latest = Some((o.observed_unix_seconds, index, candidate));
            }
        }
        let position = latest.map(|(_, _, p)| p).unwrap_or(BranchPosition::Unknown);
        let expected_prior = match position {
            BranchPosition::At(revision) => Some(revision),
            BranchPosition::Absent | BranchPosition::Unknown => None,
        };
        return record_intent(
            tx,
            &work,
            ActorRole::Publication,
            HostAction::PushRef {
                branch: branch.clone(),
                produced,
                expected_prior,
            },
            execution,
        );
    }
    // Stage 2: open_change_request once the ref is published.
    let opened = work_intents.iter().any(|i| {
        matches!(i.action, HostAction::OpenChangeRequest { .. })
            && i.status == HostActionStatus::Applied
    });
    if opened {
        return Ok(());
    }
    if work_intents.iter().any(|i| {
        matches!(i.action, HostAction::OpenChangeRequest { .. })
            && !matches!(
                i.status,
                HostActionStatus::Failed { .. } | HostActionStatus::Unsupported { .. }
            )
    }) {
        // A pending/unknown open intent is outstanding; wait for its receipt.
        return Ok(());
    }
    if work_intents.iter().any(|i| {
        matches!(i.action, HostAction::OpenChangeRequest { .. })
            && matches!(i.status, HostActionStatus::Unsupported { .. })
    }) {
        // An unsupported open is terminal — its receipt pauses the
        // execution for an operator decision; never re-issue it.
        return Ok(());
    }
    record_intent(
        tx,
        &work,
        ActorRole::Publication,
        HostAction::OpenChangeRequest {
            source_branch: branch,
            target_branch: "main".into(),
            title: spec.payload.intent.chars().take(70).collect(),
            body: String::new(),
        },
        execution,
    )
}

/// Record one stage effect intent. The capability guard runs here — an
/// effect is only authorized when the latest capability report holds
/// the required capability; missing, denied, or unknown reports record
/// nothing and the Execution keeps waiting.
fn record_intent(
    tx: &mut dyn TxView,
    work: &Work,
    role: ActorRole,
    action: HostAction,
    execution: &Execution,
) -> Result<(), StoreError> {
    let capability = action_capability(&action);
    if !capability_held(tx, &work.binding.repository.host, &capability)? {
        // The capability is unconfirmed or denied: the effect is not
        // authorized. Keep waiting for a report (or a repair decision);
        // never record an unauthorized intent.
        return Ok(());
    }
    let actor = work
        .binding
        .actor_for(role)
        .ok_or_else(|| StoreError::Internal {
            detail: format!("binding has no actor for role {}", role.name()),
        })?
        .clone();
    let payload = serde_json::to_value(&action).unwrap_or_default();
    let seq = tx.alloc_seq()?;
    tx.apply(&Effect::InsertHostActionIntent {
        intent: HostActionIntent {
            action_id: new_id("act"),
            work_id: work.id.clone(),
            host: work.binding.repository.host.clone(),
            repository: work.binding.repository.clone(),
            role,
            actor,
            action,
            payload_digest: sha256_hex("intent-v1", &canonical_json(&payload)),
            status: HostActionStatus::Pending,
            authenticated_actor: None,
            effects: vec![],
            created_seq: seq,
        },
    })?;
    let mut next = execution.clone();
    next.waiting_reason = Some(WaitingReason::HostWrite);
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })
}

/// The revision `push_ref` must publish for `invocation`: the produced
/// commit the newest acknowledged handoff names, or — for a v1
/// invocation that can never grow a handoff row — the first recorded
/// artifact digest (the only produced identity non-git artifact
/// worlds carry). `None` when handoff rows exist but none is
/// acknowledged yet, and `None` while a v2 invocation has no rows at
/// all: the row is written by the handoff drive after the exit fact
/// lands, so an empty row set at that moment means pending, never
/// absent — falling back would publish the payload digest (a byte
/// hash, not a git object) as the produced commit. The
/// acknowledgement's claim check re-drives this stage once the row
/// is durable.
fn produced_revision(
    tx: &dyn ReadView,
    invocation: &Invocation,
    artifact_digests: &[String],
) -> Result<Option<String>, StoreError> {
    let mut rows = false;
    let mut newest: Option<ArtifactHandoff> = None;
    for required in &invocation.required_artifacts {
        let artifact_id = format!("{}/{}", invocation.id.0, required.kind);
        let Some(row) = tx.artifact_handoff(&artifact_id)? else {
            continue;
        };
        rows = true;
        if row.state == HandoffState::Acknowledged
            && row.produced.is_some()
            && newest
                .as_ref()
                .is_none_or(|b: &ArtifactHandoff| row.recorded_seq > b.recorded_seq)
        {
            newest = Some(row);
        }
    }
    Ok(match (rows, newest) {
        (true, Some(row)) => row.produced,
        // Handoff rows exist but none is acknowledged: wait for the
        // sweep's claim check to re-drive publication.
        (true, None) => None,
        // A v2 invocation always grows a handoff row after the exit
        // fact — no rows yet is the pending window between finish and
        // the handoff drive, not a non-git world. Only a legacy v1
        // dispatch (no request body row) publishes the recorded
        // digest token.
        (false, _) if tx.worker_request_body(&invocation.request_id)?.is_some() => None,
        (false, _) => artifact_digests.first().cloned(),
    })
}

/// The digests of `Present` artifacts covering every required artifact
/// of the invocation; empty when the set is not yet complete and valid.
/// A `Present` fact with a missing or mismatched digest is invalid and
/// never completes the set.
fn artifact_digests(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
    invocation: &Invocation,
) -> Result<Vec<String>, StoreError> {
    let observations = tx.observations_for_execution(execution_id)?;
    let mut digests = Vec::new();
    for required in &invocation.required_artifacts {
        let mut latest: Option<(i64, usize, &ArtifactFact)> = None;
        for (index, o) in observations.iter().enumerate() {
            if let ObservationFact::Artifact {
                invocation_id,
                required_artifact,
                state,
            } = &o.fact
                && invocation_id == &invocation.id
                && required_artifact == &required.kind
            {
                let replace =
                    latest.is_none_or(|(t, i, _)| (t, i) < (o.observed_unix_seconds, index));
                if replace {
                    latest = Some((o.observed_unix_seconds, index, state));
                }
            }
        }
        match latest {
            Some((_, _, ArtifactFact::Present { digest }))
                if !digest.is_empty()
                    && required
                        .expected_digest
                        .as_ref()
                        .is_none_or(|expected| expected == digest) =>
            {
                digests.push(digest.clone())
            }
            _ => return Ok(vec![]),
        }
    }
    Ok(digests)
}

/// The check's positive process evidence: its Invocation resolved
/// `Done` on a proven exit status of 0. `Done` alone records only
/// that the process stopped — a failed, signaled, or status-unproven
/// check exit is resolved-but-not-conforming and never counts.
fn check_exited_zero(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
    invocation: &Invocation,
) -> Result<bool, StoreError> {
    Ok(tx
        .observations_for_execution(execution_id)?
        .iter()
        .any(|o| match &o.fact {
            ObservationFact::ProcessState {
                invocation_id,
                state: ProcessStateFact::Exited,
                exit_code,
                ..
            } => invocation_id == &invocation.id && exit_code == &Some(0),
            _ => false,
        }))
}

/// Whether the *latest* invariant-check Invocation's own evidence is
/// positive — `Done` on a proven exit status of 0. A superseded pass
/// never satisfies the gate: once a newer check is recorded its
/// result supersedes every earlier one, so a failed or unresolved
/// newest check holds the lane closed. This is the only check result
/// publication and completion may count; both gates share it so they
/// can never drift apart.
fn satisfied_invariant_check(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
    candidates: &[Invocation],
) -> Result<bool, StoreError> {
    let Some(latest) = candidates
        .iter()
        .rev()
        .find(|i| i.role == InvocationRole::InvariantCheck)
    else {
        return Ok(false);
    };
    Ok(latest.state == InvocationState::Done && check_exited_zero(tx, execution_id, latest)?)
}

/// `succeeded` requires the required stage effects applied, the declared
/// checks passing on the current target, the enabled lanes satisfied by
/// their exact evidence, and no unresolved writer, gate, or outgoing
/// action. Evidence references are typed and nonempty.
fn assess_completion(tx: &mut dyn TxView, execution: &Execution) -> Result<(), StoreError> {
    if execution.state != ExecutionState::Waiting {
        return Ok(());
    }
    // An unresolved writer may still act — its later evidence, not this
    // stale pass, owns the completion decision.
    let invocations = tx.invocations_for_execution(&execution.id)?;
    if invocations.iter().any(|i| i.state.is_unresolved_writer()) {
        return Ok(());
    }
    if !invocations.iter().any(|i| {
        matches!(i.role, InvocationRole::Coding | InvocationRole::Fix)
            && i.state == InvocationState::Done
    }) {
        return Ok(());
    }
    for invocation in &invocations {
        // Only declared artifacts gate completion: a read-only role
        // (the invariant check) records an explicitly empty required
        // set — its lifecycle and receipts are the evidence.
        if invocation.state == InvocationState::Done
            && !invocation.required_artifacts.is_empty()
            && artifact_digests(tx, &execution.id, invocation)?.is_empty()
        {
            return Ok(());
        }
    }
    let work = tx
        .work(&execution.work_id)?
        .ok_or_else(|| StoreError::Internal {
            detail: "execution without work".into(),
        })?;
    let spec = tx
        .spec(&work.id, Some(work.approved_spec))?
        .ok_or_else(|| StoreError::Internal {
            detail: "work without spec".into(),
        })?;
    // Required stage effects: a `push_ref` whose receipt proves the
    // produced revision landed, and an `open_change_request` whose
    // receipt opened the change. Both must be `applied` with matching
    // normalized facts — recorded at receipt time.
    let intents = tx.action_intents()?;
    let push_applied = intents.iter().any(|i| {
        i.work_id == work.id
            && matches!(i.action, HostAction::PushRef { .. })
            && i.status == HostActionStatus::Applied
    });
    let open_applied = intents.iter().any(|i| {
        i.work_id == work.id
            && matches!(i.action, HostAction::OpenChangeRequest { .. })
            && i.status == HostActionStatus::Applied
    });
    if !(push_applied && open_applied) {
        return Ok(());
    }
    if intents.iter().any(|i| {
        i.work_id == work.id
            && matches!(
                i.status,
                HostActionStatus::Pending | HostActionStatus::Unknown { .. }
            )
    }) {
        return Ok(());
    }
    let Some(target) = current_target(tx, &work.id)? else {
        return Ok(());
    };
    let observations = tx.observations_for_work(&work.id)?;
    // Declared checks: latest conclusion per named check on the current
    // head must be `success`.
    let mut checks: BTreeMap<&str, (i64, usize, CheckConclusion)> = BTreeMap::new();
    for (index, o) in observations.iter().enumerate() {
        if let ObservationFact::Check {
            change,
            head,
            check_id,
            conclusion,
            ..
        } = &o.fact
            && change == &target.change
            && head == &target.head
        {
            let replace = checks
                .get(check_id.as_str())
                .is_none_or(|(t, i, _)| (*t, *i) < (o.observed_unix_seconds, index));
            if replace {
                checks.insert(
                    check_id.as_str(),
                    (o.observed_unix_seconds, index, conclusion.clone()),
                );
            }
        }
    }
    for check_id in &spec.payload.required_checks {
        if !checks
            .get(check_id.as_str())
            .is_some_and(|(_, _, c)| c == &CheckConclusion::Success)
        {
            return Ok(());
        }
    }
    // Review lane: the current sealed complete round for this target has
    // zero occurrences, or every occurrence is dispositioned
    // accept/dismiss in a judgment on that round.
    if spec.payload.lanes.code_review {
        let Some(round) = latest_round_for_target(tx, &work.id, &target)? else {
            return Ok(());
        };
        if round.completeness != "complete" {
            return Ok(());
        }
        let occurrences = tx.occurrences(&round.id)?;
        if !occurrences.is_empty() {
            let Some(judgment) = tx.judgment_for_round(&round.id)? else {
                return Ok(());
            };
            let all_resolved = judgment
                .decisions
                .iter()
                .all(|d| matches!(d.choice, FindingChoice::Accept | FindingChoice::Dismiss));
            if !all_resolved {
                return Ok(());
            }
        }
    }
    // Invariant lane: the *latest* assessment on the current target is
    // complete with every item positive AND the newest invariant-check
    // Invocation recorded after the latest producing Invocation exited
    // 0. A superseded positive assessment never counts — a newer check
    // reporting a violation, an incomplete run, or nothing at all
    // displaces it as the lane's deciding evidence. The assessment
    // fact alone never substitutes for the check's own durable record.
    // Disabled lanes count nothing either way.
    if spec.payload.lanes.invariant_check {
        let ok = observations
            .iter()
            .rev()
            .find_map(|o| match &o.fact {
                ObservationFact::Assessment {
                    target: t,
                    complete,
                    items,
                    ..
                } if t == &target => Some(
                    *complete
                        && !items.is_empty()
                        && items
                            .iter()
                            .all(|i| i.verdict == crate::observation::AssessmentVerdict::Positive),
                ),
                _ => None,
            })
            .unwrap_or(false);
        let checked = match invocations.iter().rposition(|i| {
            matches!(i.role, InvocationRole::Coding | InvocationRole::Fix)
                && i.state == InvocationState::Done
        }) {
            Some(producing) => {
                satisfied_invariant_check(tx, &execution.id, &invocations[producing + 1..])?
            }
            None => false,
        };
        if !(ok && checked) {
            return Ok(());
        }
    }
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Succeeded;
    next.terminal_code = Some("succeeded".into());
    next.outcome = Some(Outcome {
        state: ExecutionState::Succeeded,
        code: Some("succeeded".into()),
        detail: Some(format!("verified target {}", target.head)),
        evidence_refs: completion_evidence(tx, &work, &spec, execution, &target)?,
        recorded_seq: seq,
    });
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    tx.apply(&Effect::DeleteReservations {
        resource_keys: resources_keys_of(tx, execution)?,
        holder: ReservationHolder::Execution {
            execution_id: execution.id.clone(),
        },
    })?;
    tx.apply(&Effect::ReleaseIssueClaims {
        work_id: work.id.clone(),
    })?;
    // A terminal Execution is never a deferred one: any stale stall
    // explanation recorded against it would otherwise keep surfacing
    // on the `capacity` Query after settlement — the same rule
    // `fail_terminal` applies.
    tx.apply(&Effect::ClearDeferral {
        execution_id: execution.id.clone(),
    })?;
    Ok(())
}

/// The typed evidence references a success Outcome must carry: the
/// approved spec, each valid artifact, each applied stage effect, the
/// current-target check and round/assessment records.
fn completion_evidence(
    tx: &dyn ReadView,
    work: &Work,
    spec: &SpecSnapshot,
    execution: &Execution,
    target: &DiffTarget,
) -> Result<Vec<EvidenceRef>, StoreError> {
    let mut refs = vec![
        EvidenceRef {
            id: format!("spec:{}:{}", work.id.0, spec.revision),
            digest: spec.digest.clone(),
        },
        EvidenceRef {
            id: format!("target:{}", work.id.0),
            digest: sha256_hex(
                "target-v1",
                &canonical_json(&serde_json::to_value(target).unwrap_or_default()),
            ),
        },
    ];
    for invocation in tx.invocations_for_execution(&execution.id)? {
        if invocation.state != InvocationState::Done {
            continue;
        }
        for (required, digest) in invocation
            .required_artifacts
            .iter()
            .zip(artifact_digests(tx, &execution.id, &invocation)?.iter())
        {
            refs.push(EvidenceRef {
                id: format!("artifact:{}:{}", invocation.id.0, required.kind),
                digest: digest.clone(),
            });
        }
    }
    for intent in tx.action_intents()? {
        if intent.work_id == work.id && intent.status == HostActionStatus::Applied {
            refs.push(EvidenceRef {
                id: format!("action:{}", intent.action_id),
                digest: intent.payload_digest.clone(),
            });
        }
    }
    if let Some(round) = latest_round_for_target(tx, &work.id, target)? {
        refs.push(EvidenceRef {
            id: format!("round:{}", round.id),
            digest: round.set_digest.clone(),
        });
    }
    Ok(refs)
}

/// The internal dispatch scan: queued Executions in committed enqueue
/// order. A deferred head never blocks later items; each deferral is
/// recorded for the `capacity` Query. Each admission runs in its own
/// savepoint, so a dispatch-time constraint, state, or capacity conflict
/// (a request-ID/slot/reservation race with another process) rolls back
/// only that item, records a retryable diagnostic, and the scan moves on
/// — the conflict rule in lifecycle.md: the loop rereads and decides
/// again instead of aborting the whole tick.
pub fn dispatch_once(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    now_unix_seconds: i64,
) -> Result<Vec<ExecutionId>, StoreError> {
    let mut clock = || now_unix_seconds;
    Ok(dispatch_scan(tx, config, &mut clock, None, None)?.admitted)
}

/// One bounded dispatch pass. The loop supplies the clock and the tick
/// deadline as facts; admission legality stays in `admit_queued`. Items
/// the deadline stops before admission stay queued — reported, never
/// silently dropped, never re-ordered. `scope` restricts the scan to one
/// Work for a diagnostic pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchScan {
    pub admitted: Vec<ExecutionId>,
    /// Queued items left for the next pass because the tick deadline
    /// (or a scoped pass excluding them) stopped the scan first.
    pub deferred_deadline: Vec<ExecutionId>,
}

pub fn dispatch_scan(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    clock: &mut dyn FnMut() -> i64,
    deadline: Option<i64>,
    scope: Option<&WorkId>,
) -> Result<DispatchScan, StoreError> {
    let mut scan = DispatchScan::default();
    for execution in tx.queued_executions()? {
        if let Some(work_id) = scope
            && execution.work_id != *work_id
        {
            continue;
        }
        let now_unix_seconds = clock();
        if deadline.is_some_and(|d| now_unix_seconds >= d) {
            scan.deferred_deadline.push(execution.id.clone());
            continue;
        }
        tx.begin_attempt()?;
        match admit_queued(tx, config, &execution.id, now_unix_seconds) {
            Ok(admitted_execution) => {
                tx.commit_attempt()?;
                if admitted_execution.is_some() {
                    scan.admitted.push(execution.id.clone());
                }
            }
            Err(
                e @ (StoreError::ConstraintConflict { .. }
                | StoreError::StateConflict { .. }
                | StoreError::NoFreeSlot),
            ) => {
                // The world changed under the read; partial effects from
                // this admission are rolled back and the item stays
                // queued with a diagnostic the next pass re-decides.
                tx.revert_attempt()?;
                tx.apply(&Effect::RecordDeferral {
                    execution_id: execution.id.clone(),
                    reason: "dispatch_conflict".into(),
                    diagnostic: e.to_string(),
                })?;
            }
            Err(e) => {
                tx.revert_attempt()?;
                return Err(e);
            }
        }
    }
    Ok(scan)
}

/// What one timed sweep examined and decided. Clock-derived Core
/// decisions only — the loop supplies the clock, never the transition.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub examined: usize,
    /// Executions moved to `stopping` by a runtime or time bound.
    pub stops_recorded: usize,
    /// Executions settled terminal by an exhausted bound.
    pub terminal_failures: usize,
    /// Waiting Executions whose recorded evidence already satisfied
    /// every completion gate and were settled `succeeded` by the
    /// level-triggered re-assessment — no new observation required.
    pub completed: usize,
    /// Executions whose own transaction hit a store conflict; recorded
    /// against them as a `sweep_conflict` deferral and left for the
    /// next pass — one Work's conflict never aborts the phase.
    pub conflicts: Vec<ExecutionId>,
}

/// A per-item store conflict: the world changed under this item's read
/// (constraint, state, version, or capacity), so its attempt is rolled
/// back and re-decided next pass. Everything else — a store that
/// cannot be read or written at all — is a phase-level infrastructure
/// failure and still aborts the pass.
fn is_item_conflict(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::ConstraintConflict { .. }
            | StoreError::StateConflict { .. }
            | StoreError::VersionConflict { .. }
            | StoreError::NoFreeSlot
    )
}

/// Cumulative automatic seconds charged against the Work-wide ceiling:
/// elapsed since first dispatch minus recorded human-decision waiting.
/// `None` when the Work never dispatched.
fn automatic_seconds_used(usage: &WorkUsage, now_unix_seconds: i64) -> Option<u64> {
    let first = usage.first_dispatch_unix_seconds?;
    let elapsed = (now_unix_seconds - first).max(0) as u64;
    Some(elapsed - usage.human_wait_seconds_total.min(elapsed))
}

/// Timed decisions the loop owes the store: runtime limits, recovery
/// bounds, the Work-wide automatic-time ceiling, and budget exhaustion.
/// Each is a committed decision checked against the stored limit and
/// the observed clock — never silent.
pub fn sweep(
    store: &impl Store,
    config: &WorkloopConfig,
    now_unix_seconds: i64,
) -> Result<SweepReport, StoreError> {
    sweep_scoped(store, config, now_unix_seconds, None)
}

/// The sweep restricted to one Work for a diagnostic `--work` pass.
///
/// Each execution is processed under its OWN store transaction (the
/// per-Work isolation the loop design requires): a conflict on one
/// Work's attempt is rolled back, recorded against that Work as a
/// `sweep_conflict` deferral, and the pass continues with the others.
/// Only an infrastructure failure — the store itself unreadable —
/// returns `Err` and aborts the phase.
pub fn sweep_scoped(
    store: &impl Store,
    config: &WorkloopConfig,
    now_unix_seconds: i64,
    scope: Option<&WorkId>,
) -> Result<SweepReport, StoreError> {
    let executions = store.transact(|tx| tx.nonterminal_executions())?;
    let mut report = SweepReport::default();
    for listed in executions {
        if let Some(work_id) = scope
            && listed.work_id != *work_id
        {
            continue;
        }
        report.examined += 1;
        let execution_id = listed.id.clone();
        match store.transact(|tx| sweep_one(tx, &execution_id, now_unix_seconds)) {
            Ok(SweepOutcome::Absent | SweepOutcome::Unchanged) => {}
            Ok(SweepOutcome::StopRecorded) => report.stops_recorded += 1,
            Ok(SweepOutcome::TerminalFailure) => report.terminal_failures += 1,
            Ok(SweepOutcome::Completed) => report.completed += 1,
            Err(e) if is_item_conflict(&e) => {
                store.transact(|tx| {
                    tx.apply(&Effect::RecordDeferral {
                        execution_id: execution_id.clone(),
                        reason: "sweep_conflict".into(),
                        diagnostic: e.to_string(),
                    })
                })?;
                report.conflicts.push(execution_id);
            }
            Err(e) => return Err(e),
        }
    }
    let _ = config;
    Ok(report)
}

/// What one execution's sweep attempt decided, or that it vanished
/// between the list and its own transaction.
#[derive(Debug, PartialEq, Eq)]
enum SweepOutcome {
    Absent,
    Unchanged,
    StopRecorded,
    TerminalFailure,
    Completed,
}

/// Drop any stall explanation recorded against an Execution that the
/// sweep has now resolved (a successful read, a stop, or settlement).
fn clear_deferral(tx: &mut dyn TxView, execution_id: &ExecutionId) -> Result<(), StoreError> {
    tx.apply(&Effect::ClearDeferral {
        execution_id: execution_id.clone(),
    })
}

/// Clear only a previous `sweep_conflict`: a clean sweep read resolves
/// that transient conflict, while a `dispatch_conflict` on a still-queued
/// Execution is dispatch's explanation to keep until admission re-decides.
fn clear_sweep_conflict(tx: &mut dyn TxView, execution_id: &ExecutionId) -> Result<(), StoreError> {
    let stale = tx
        .deferrals()?
        .iter()
        .any(|d| d.execution_id == *execution_id && d.reason == "sweep_conflict");
    if stale {
        clear_deferral(tx, execution_id)?;
    }
    Ok(())
}

/// A terminal decision (spent budget or a passed deadline) must wait
/// for outstanding host writes to reconcile (lifecycle.md): intents are
/// delivery-bound and would still apply under a recorded failure, so the
/// stall is recorded as `outbox_reconciliation` instead of the terminal
/// outcome. An intent that can never be attempted — `pending` under a
/// denied `host_write` grant — does not defer: there is no in-flight
/// effect to wait for. Returns true while the deferral stands.
fn defer_terminal_while_outstanding(
    tx: &mut dyn TxView,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    terminal_code: &str,
) -> Result<bool, StoreError> {
    if crate::projection::settlement_blocking_actions(tx, work_id)?.is_empty() {
        return Ok(false);
    }
    tx.apply(&Effect::RecordDeferral {
        execution_id: execution_id.clone(),
        reason: "outbox_reconciliation".into(),
        diagnostic: format!(
            "terminal {terminal_code} defers until outstanding host writes reconcile"
        ),
    })?;
    Ok(true)
}

/// One execution's sweep decision inside its own transaction. The
/// execution is re-read here so a change committed after the list was
/// taken is observed fresh, never acted on from a stale snapshot.
fn sweep_one(
    tx: &mut dyn TxView,
    execution_id: &ExecutionId,
    now_unix_seconds: i64,
) -> Result<SweepOutcome, StoreError> {
    let Some(execution) = tx.execution(execution_id)? else {
        clear_deferral(tx, execution_id)?;
        return Ok(SweepOutcome::Absent);
    };
    match execution.state {
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => {
            // Recovery bound is separate from the runtime budget.
            if matches!(
                execution.waiting_reason,
                Some(WaitingReason::ArtifactRecovery)
            ) {
                let exhausted = execution.recovery_attempts_used
                    >= execution.budget.artifact_recovery_attempts
                    || execution
                        .recovery_deadline_unix_seconds
                        .is_some_and(|d| now_unix_seconds > d);
                if exhausted {
                    fail_terminal(
                        tx,
                        &execution,
                        "artifact_recovery_exhausted",
                        "bounded artifact recovery exhausted; WIP preserved",
                    )?;
                    return Ok(SweepOutcome::TerminalFailure);
                }
            }
            let over_runtime = execution.started_unix_seconds.is_some_and(|s| {
                now_unix_seconds - s > execution.budget.runtime_limit_seconds as i64
            });
            if over_runtime {
                let mut next = execution.clone();
                next.state = ExecutionState::Stopping;
                next.desired_action = Some(DesiredAction::Fail);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })?;
                clear_deferral(tx, execution_id)?;
                return Ok(SweepOutcome::StopRecorded);
            }
        }
        ExecutionState::Queued => {
            // An absolute deadline would be checked here; v0.1 has
            // none configured. Queue time never consumes runtime.
        }
        _ => {}
    }
    // The Work-wide automatic-time ceiling: cumulative automatic
    // seconds across the whole Execution chain. It supplements
    // the per-Execution runtime bound and prohibits new attempts;
    // it does not itself establish termination.
    if matches!(
        execution.state,
        ExecutionState::Queued
            | ExecutionState::Ready
            | ExecutionState::Running
            | ExecutionState::Waiting
    ) {
        let work = tx.work(&execution.work_id)?;
        let usage = tx.work_usage(&execution.work_id)?;
        if let (Some(work), Some(usage)) = (work, usage)
            && let Some(automatic) = automatic_seconds_used(&usage, now_unix_seconds)
            && automatic > work.policy.automatic_time_limit_seconds
        {
            match execution.state {
                ExecutionState::Queued => {
                    // The same rule admission applies at the
                    // request-creation seam: the terminal outcome waits
                    // for outstanding host writes to reconcile.
                    if defer_terminal_while_outstanding(
                        tx,
                        &work.id,
                        &execution.id,
                        "automatic_time_exhausted",
                    )? {
                        return Ok(SweepOutcome::Unchanged);
                    }
                    fail_terminal(
                        tx,
                        &execution,
                        "automatic_time_exhausted",
                        "Work-wide automatic time limit reached",
                    )?;
                    return Ok(SweepOutcome::TerminalFailure);
                }
                _ => {
                    let mut next = execution.clone();
                    next.state = ExecutionState::Stopping;
                    next.desired_action = Some(DesiredAction::Fail);
                    tx.apply(&Effect::UpdateExecution {
                        execution: next,
                        expected_state: execution.state.clone(),
                    })?;
                    clear_deferral(tx, execution_id)?;
                    return Ok(SweepOutcome::StopRecorded);
                }
            }
        }
    }
    // Level-triggered completion re-assessment: a Waiting Execution
    // whose evidence set became complete through a path that produced
    // no new observation — a deduplicated or lost delivery, a
    // command-side decision like `judge-findings`, or any producer
    // gap — would otherwise park forever, because `assess_completion`
    // runs only inside observation ingest. Re-running it here
    // re-derives every gate from the durable record under the same
    // transaction; it can never succeed on stale, partial, or
    // superseded-target evidence. Protective bounds above keep
    // precedence: an Execution this sweep stops or fails never
    // reaches re-assessment in the same pass.
    if execution.state == ExecutionState::Waiting {
        assess_completion(tx, &execution)?;
        if tx
            .execution(&execution.id)?
            .is_some_and(|e| e.state == ExecutionState::Succeeded)
        {
            return Ok(SweepOutcome::Completed);
        }
    }
    clear_sweep_conflict(tx, execution_id)?;
    Ok(SweepOutcome::Unchanged)
}

fn fail_terminal(
    tx: &mut dyn TxView,
    execution: &Execution,
    code: &str,
    detail: &str,
) -> Result<(), StoreError> {
    let seq = tx.alloc_seq()?;
    let mut next = execution.clone();
    next.state = ExecutionState::Failed;
    next.terminal_code = Some(code.to_string());
    next.outcome = Some(Outcome {
        state: ExecutionState::Failed,
        code: Some(code.to_string()),
        detail: Some(detail.to_string()),
        evidence_refs: vec![],
        recorded_seq: seq,
    });
    tx.apply(&Effect::UpdateExecution {
        execution: next,
        expected_state: execution.state.clone(),
    })?;
    tx.apply(&Effect::ReleaseSlot {
        execution_id: execution.id.clone(),
    })?;
    tx.apply(&Effect::DeleteReservations {
        resource_keys: resources_keys_of(tx, execution)?,
        holder: ReservationHolder::Execution {
            execution_id: execution.id.clone(),
        },
    })?;
    tx.apply(&Effect::ReleaseIssueClaims {
        work_id: execution.work_id.clone(),
    })?;
    // A terminal Execution is never a deferred one: any stale stall
    // explanation recorded against it would otherwise keep surfacing on
    // the `capacity` Query after settlement.
    tx.apply(&Effect::ClearDeferral {
        execution_id: execution.id.clone(),
    })?;
    let usage = tx
        .work_usage(&execution.work_id)?
        .unwrap_or_else(|| WorkUsage::new(&execution.work_id));
    tx.apply(&Effect::InsertRecoveryDecision {
        decision: RecoveryDecision {
            id: new_id("rd"),
            work_id: execution.work_id.clone(),
            execution_id: Some(execution.id.clone()),
            classification: "budget".into(),
            signature: None,
            prerequisites: vec![],
            detail: format!("{code}: {detail}"),
            usage_after: serde_json::to_value(&usage).unwrap_or_default(),
            seq,
        },
    })?;
    Ok(())
}

/// Outcome of one bounded recovery-scheduling pass. Nothing here is
/// inferred: every transition is decided from recorded state under the
/// same guards the Command path applies.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryScan {
    /// Intents fired (`consumed` recorded before any re-entry).
    pub consumed: usize,
    /// Of those, Executions requeued through the queue.
    pub requeued: usize,
    /// Of those, Executions settled terminal instead (spent budget or
    /// Work-wide automatic-time ceiling).
    pub failed_terminal: usize,
    /// Intents retired because recorded evidence shows they can never
    /// fire — terminal evidence, a progressing Execution, or a missing
    /// target.
    pub retired: usize,
    /// Intents left `pending`, with the recorded reason.
    pub held: Vec<(String, String)>,
    /// Due intents the tick deadline stopped before evaluation; they
    /// fire on a later pass, never silently dropped.
    pub deferred: Vec<String>,
}

/// One bounded recovery-scheduling pass (loop phase 6): consume due
/// `recovery_intents` with revalidation before any firing, ≤ `limit`
/// items per tick. A due `requeue` intent re-enters the same paused
/// Execution only when every recorded hold still allows it; evidence
/// that the intent can never fire — terminal evidence, or an
/// Execution already progressing under another path — retires it
/// instead. Intents whose hold cannot be discharged (operator
/// decision, unmet repair prerequisite, a pause still settling) stay
/// `pending` with a reason — an automatic pass never overrules an
/// explicit hold, never guesses an unknown `kind`, and never fires an
/// intent twice.
///
/// Every intent fires under its OWN store transaction (the per-Work
/// isolation the loop design requires): a conflict on one intent's
/// attempt is rolled back, recorded against that Work's Execution as a
/// `recovery_conflict` deferral, and the intent stays `pending` for
/// the next pass while the remaining intents still fire. Only an
/// infrastructure failure — the store itself unreadable — returns
/// `Err` and aborts the phase.
pub fn consume_due_recovery_intents(
    store: &impl Store,
    config: &WorkloopConfig,
    clock: &mut dyn FnMut() -> i64,
    deadline: Option<i64>,
    scope: Option<&WorkId>,
    limit: usize,
) -> Result<RecoveryScan, StoreError> {
    let mut scan = RecoveryScan::default();
    let now = clock();
    let intents = store.transact(|tx| tx.recovery_intents())?;
    // Every due intent evaluated this pass consumes the per-tick budget,
    // including the ones held by a guard (`operator_hold`, unmet
    // prerequisites, an active Execution, an unsupported kind). Only the
    // deadline-deferred intents are exempt: they were never evaluated.
    let mut evaluated: usize = 0;
    for listed in intents {
        if listed.state != crate::store::RecoveryIntentState::Pending {
            continue;
        }
        if let Some(work_id) = scope
            && listed.work_id != *work_id
        {
            continue;
        }
        if listed.due_at_unix_seconds > now {
            continue;
        }
        if let Some(d) = deadline {
            let tick_now = clock();
            if tick_now >= d {
                scan.deferred.push(listed.intent_id.clone());
                continue;
            }
        }
        if evaluated >= limit {
            scan.held
                .push((listed.intent_id.clone(), "recovery_limit".into()));
            continue;
        }
        evaluated += 1;
        if !matches!(listed.kind.as_str(), "requeue" | "retry") {
            // Only the documented kinds fire automatically; anything
            // else waits for its owning slice rather than being guessed.
            scan.held
                .push((listed.intent_id.clone(), "unsupported_kind".into()));
            continue;
        }
        let intent_id = listed.intent_id.clone();
        match store.transact(|tx| fire_due_intent(tx, config, &intent_id, now)) {
            Ok(FireOutcome::Ignored) => {}
            Ok(FireOutcome::Retired) => scan.retired += 1,
            Ok(FireOutcome::Suppressed(reason)) | Ok(FireOutcome::Held(reason)) => {
                scan.held.push((intent_id, reason))
            }
            Ok(FireOutcome::Consumed {
                requeued,
                failed_terminal,
            }) => {
                scan.consumed += 1;
                scan.requeued += usize::from(requeued);
                scan.failed_terminal += usize::from(failed_terminal);
            }
            Err(e) if is_item_conflict(&e) => {
                // The world changed under this intent's read: the
                // conflict is recorded against the Work's Execution
                // when it has one, and the intent stays pending for
                // the next pass — never silently dropped, never
                // allowed to abort the other items.
                if let Some(execution_id) = &listed.execution_id {
                    store.transact(|tx| {
                        tx.apply(&Effect::RecordDeferral {
                            execution_id: execution_id.clone(),
                            reason: "recovery_conflict".into(),
                            diagnostic: e.to_string(),
                        })
                    })?;
                }
                scan.held.push((intent_id, "recovery_conflict".into()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(scan)
}

/// What firing one intent inside its own transaction decided.
#[derive(Debug, PartialEq, Eq)]
enum FireOutcome {
    /// The intent vanished or left `pending` between the list and this
    /// transaction — another path already owned it.
    Ignored,
    Retired,
    Held(String),
    /// The Work's equivalent-failure budget is spent: the automatic
    /// requeue is refused and recorded as suppressed. The intent stays
    /// `pending`; it is never silently consumed.
    Suppressed(String),
    Consumed {
        requeued: bool,
        failed_terminal: bool,
    },
}

/// Fire one intent, keeping the recorded suppression state honest:
/// any outcome other than `Suppressed` (and other than `Ignored`,
/// which means nothing was learned) drops a stale
/// `equivalent_failure_suppressed` deferral for this intent's
/// Execution, so the capacity Query never shows a stall explanation
/// for a condition that no longer holds. The re-read and revalidation
/// happens in `fire_due_intent_inner`.
fn fire_due_intent(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    intent_id: &str,
    now: i64,
) -> Result<FireOutcome, StoreError> {
    let outcome = fire_due_intent_inner(tx, config, intent_id, now)?;
    if !matches!(outcome, FireOutcome::Suppressed(_) | FireOutcome::Ignored)
        && let Some(execution_id) = tx
            .recovery_intents()?
            .into_iter()
            .find(|i| i.intent_id == intent_id)
            .and_then(|i| i.execution_id)
        && tx
            .deferrals()?
            .iter()
            .any(|d| d.execution_id == execution_id && d.reason == "equivalent_failure_suppressed")
    {
        tx.apply(&Effect::ClearDeferral { execution_id })?;
    }
    Ok(outcome)
}

/// Re-read one intent inside the firing transaction and apply the
/// documented revalidation guards before any transition, so a stale
/// list snapshot can never fire an intent twice or against a state
/// that changed since the list was taken. `consumed` is recorded
/// before the action the intent gates — within the same transaction —
/// so a crash can never fire an intent twice.
fn fire_due_intent_inner(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    intent_id: &str,
    now: i64,
) -> Result<FireOutcome, StoreError> {
    let Some(intent) = tx
        .recovery_intents()?
        .into_iter()
        .find(|i| i.intent_id == intent_id)
    else {
        return Ok(FireOutcome::Ignored);
    };
    if intent.state != crate::store::RecoveryIntentState::Pending
        || intent.due_at_unix_seconds > now
    {
        return Ok(FireOutcome::Ignored);
    }
    let Some(execution_id) = intent.execution_id.clone() else {
        retire_intent(tx, &intent, "no_execution")?;
        return Ok(FireOutcome::Retired);
    };
    let Some(execution) = tx.execution(&execution_id)? else {
        retire_intent(tx, &intent, "execution_missing")?;
        return Ok(FireOutcome::Retired);
    };
    if matches!(
        execution.state,
        ExecutionState::Succeeded | ExecutionState::Failed | ExecutionState::Cancelled
    ) {
        // Terminal evidence invalidates the intent and any stall
        // explanation a prior conflict recorded against the Execution.
        clear_deferral(tx, &execution_id)?;
        retire_intent(tx, &intent, "execution_terminal")?;
        return Ok(FireOutcome::Retired);
    }
    match intent.kind.as_str() {
        "requeue" => fire_requeue_intent(tx, &intent, &execution, now),
        "retry" => fire_retry_intent(tx, config, &intent, &execution, now),
        // The outer filter already excluded undocumented kinds; a
        // kind that arrives here anyway is held, never guessed.
        _ => Ok(FireOutcome::Held("unsupported_kind".into())),
    }
}

/// Fire a due `requeue` intent: re-enter the same `paused` Execution
/// through the queue when every recorded hold still allows it.
fn fire_requeue_intent(
    tx: &mut dyn TxView,
    intent: &RecoveryIntent,
    execution: &Execution,
    now: i64,
) -> Result<FireOutcome, StoreError> {
    if execution.state != ExecutionState::Paused {
        // A pause still resolving — `stopping`/`uncertain` heading for
        // `paused` — keeps its intent: it is the schedule for the pause
        // being settled, not a stale row. An Execution already
        // progressing again (queued/ready/running/waiting) was resumed
        // by another path; this intent can never fire and is retired
        // rather than left pending to consume the per-tick recovery
        // budget on every pass.
        return match execution.state {
            ExecutionState::Stopping | ExecutionState::Uncertain => {
                Ok(FireOutcome::Held("execution_active".into()))
            }
            _ => {
                retire_intent(tx, intent, "execution_progressing")?;
                Ok(FireOutcome::Retired)
            }
        };
    }
    // Equivalent-failure suppression: the Work's failure budget is
    // spent, so the automatic requeue is refused and recorded as
    // suppressed — never consumed, never silently held.
    if let Some(usage) = tx.work_usage(&intent.work_id)?
        && let Some(work) = tx.work(&intent.work_id)?
        && usage.equivalent_failure_streak >= work.policy.equivalent_failure_limit
    {
        // Durably record the suppression — the intent stays Pending and
        // the deferral on the Execution carries the recorded reason
        // (visible through the capacity Query), the same mechanism the
        // dispatch recovery_conflict path uses.
        tx.apply(&Effect::RecordDeferral {
            execution_id: execution.id.clone(),
            reason: "equivalent_failure_suppressed".into(),
            diagnostic: format!(
                "automatic requeue refused: equivalent-failure streak {} reached the policy limit {}",
                usage.equivalent_failure_streak, work.policy.equivalent_failure_limit
            ),
        })?;
        return Ok(FireOutcome::Suppressed(
            "equivalent_failure_suppressed".into(),
        ));
    }
    if execution
        .repair_prerequisites
        .iter()
        .any(|p| matches!(p, RepairPrerequisite::OperatorDecision))
    {
        // An explicit human hold: automatic recovery must not
        // release it, only an operator Command may.
        return Ok(FireOutcome::Held("operator_hold".into()));
    }
    let Some(work) = tx.work(&intent.work_id)? else {
        retire_intent(tx, intent, "work_missing")?;
        return Ok(FireOutcome::Retired);
    };
    let unmet = prerequisites_satisfied(tx, &work, execution, now)?;
    if !unmet.is_empty() {
        return Ok(FireOutcome::Held("prerequisites_unmet".into()));
    }
    // Work-wide automatic-time ceiling: an exhausted budget can only
    // hand off, never requeue (same rule as sweep and resume). The
    // current pause has not been closed yet, so charge it as
    // human-decision waiting here — exactly what `close_pause_effects`
    // will persist on the transition. Otherwise a long human/repair
    // pause would count as automatic time and terminally fail the
    // Execution the design says must be excluded.
    let usage = tx.work_usage(&work.id)?;
    let automatic_exhausted = usage.is_some_and(|mut usage| {
        if let Some(paused_since) = execution.paused_since_unix_seconds {
            usage.human_wait_seconds_total = usage
                .human_wait_seconds_total
                .saturating_add((now - paused_since).max(0) as u64);
        }
        automatic_seconds_used(&usage, now)
            .is_some_and(|a| a > work.policy.automatic_time_limit_seconds)
    });
    if automatic_exhausted {
        if defer_terminal_while_outstanding(
            tx,
            &work.id,
            &execution.id,
            "automatic_time_exhausted",
        )? {
            return Ok(FireOutcome::Held("outbox_reconciliation".into()));
        }
        consume_intent(tx, &intent.intent_id)?;
        fail_terminal(
            tx,
            execution,
            "automatic_time_exhausted",
            "Work-wide automatic time limit reached",
        )?;
        return Ok(FireOutcome::Consumed {
            requeued: false,
            failed_terminal: true,
        });
    }
    if let Some(limit) = execution.budget_exhaustion(&InvocationRole::Coding)
        && defer_terminal_while_outstanding(tx, &work.id, &execution.id, limit.terminal_code())?
    {
        return Ok(FireOutcome::Held("outbox_reconciliation".into()));
    }
    if let Some((effects, _seq, _version)) = paused_exhaustion_effects(tx, &work, execution, now)? {
        consume_intent(tx, &intent.intent_id)?;
        for effect in &effects {
            tx.apply(effect)?;
        }
        return Ok(FireOutcome::Consumed {
            requeued: false,
            failed_terminal: true,
        });
    }
    let (effects, _seq, _version) = paused_requeue_effects(tx, &work, execution, now)?;
    consume_intent(tx, &intent.intent_id)?;
    for effect in &effects {
        tx.apply(effect)?;
    }
    Ok(FireOutcome::Consumed {
        requeued: true,
        failed_terminal: false,
    })
}

/// Fire a due `retry` intent: re-drive the publication chain for a
/// still-`waiting` Execution so the failed stage effect re-issues on
/// the persisted backoff. Every guard a `requeue` applies that can
/// block an automatic action applies here too — an explicit hold and
/// an active Execution hold the intent, terminal evidence retires it,
/// and the transport budget is re-checked against the durable counter.
fn fire_retry_intent(
    tx: &mut dyn TxView,
    config: &WorkloopConfig,
    intent: &RecoveryIntent,
    execution: &Execution,
    now: i64,
) -> Result<FireOutcome, StoreError> {
    if execution.state == ExecutionState::Paused {
        if execution
            .repair_prerequisites
            .iter()
            .any(|p| matches!(p, RepairPrerequisite::OperatorDecision))
        {
            // An explicit human hold: automatic recovery must not
            // release it, only an operator Command may.
            return Ok(FireOutcome::Held("operator_hold".into()));
        }
        // A repair pause owns the next move; the effect cannot be
        // re-issued while the Execution is held.
        return Ok(FireOutcome::Held("execution_paused".into()));
    }
    if execution.state != ExecutionState::Waiting {
        // Progressing or settling elsewhere; the intent stays visible
        // and pending, never forced.
        return Ok(FireOutcome::Held("execution_active".into()));
    }
    let Some(work) = tx.work(&intent.work_id)? else {
        retire_intent(tx, intent, "work_missing")?;
        return Ok(FireOutcome::Retired);
    };
    // The transport account is re-checked against the durable counter:
    // a budget spent after scheduling means the receipt path already
    // paused for a human — the intent is refused, never consumed.
    if let Some(usage) = tx.work_usage(&intent.work_id)?
        && usage.transport_retries_used > work.policy.transport_max_retries
    {
        tx.apply(&Effect::RecordDeferral {
            execution_id: execution.id.clone(),
            reason: "transport_budget_exhausted".into(),
            diagnostic: format!(
                "automatic retry refused: transport retries {} exceed the policy limit {}",
                usage.transport_retries_used, work.policy.transport_max_retries
            ),
        })?;
        return Ok(FireOutcome::Suppressed("transport_budget_exhausted".into()));
    }
    // `consumed` is recorded before the re-issue acts: a crash between
    // firing and the next receipt can never fire the intent twice.
    consume_intent(tx, &intent.intent_id)?;
    maybe_publish(tx, config, execution, now)?;
    Ok(FireOutcome::Consumed {
        requeued: false,
        failed_terminal: false,
    })
}

/// Record the intent consumed before the action it gates — the mark
/// precedes the action inside the same transaction.
fn consume_intent(tx: &mut dyn TxView, intent_id: &str) -> Result<(), StoreError> {
    tx.apply(&Effect::UpdateRecoveryIntent {
        intent_id: intent_id.to_string(),
        state: crate::store::RecoveryIntentState::Consumed,
        retirement_reason: None,
        evidence_generation: None,
    })
}

/// The evidence generation a retirement is decided from: the Work's
/// current version when it is readable, otherwise the intent's own
/// scheduling generation (the only evidence left when its Work is
/// gone). Never older than the intent's generation in normal operation,
/// so the store's stale-generation guard only refuses decisions built
/// from an older snapshot.
fn intent_retirement_generation(
    tx: &mut dyn TxView,
    intent: &RecoveryIntent,
) -> Result<u64, StoreError> {
    Ok(tx
        .work(&intent.work_id)?
        .map(|work| work.version)
        .unwrap_or(intent.generation))
}

fn retire_intent(
    tx: &mut dyn TxView,
    intent: &RecoveryIntent,
    reason: &str,
) -> Result<(), StoreError> {
    let evidence_generation = intent_retirement_generation(tx, intent)?;
    tx.apply(&Effect::UpdateRecoveryIntent {
        intent_id: intent.intent_id.clone(),
        state: crate::store::RecoveryIntentState::Retired,
        retirement_reason: Some(reason.to_string()),
        evidence_generation: Some(evidence_generation),
    })
}
