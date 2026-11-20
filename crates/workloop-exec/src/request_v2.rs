//! Materialize immutable per-Invocation `taisan.worker.request/2`
//! bodies from durable state (`docs/v0.3.md` §4) and lay out the
//! per-Invocation input/output tree the launch runs against.
//!
//! The builder is the only place a v2 request body comes from: it
//! reads the durable `InvocationRequest` row, its Work/Execution/spec
//! revision, the acknowledged artifact handoffs, sealed review
//! rounds/judgments, and verification results — never live
//! configuration or remote state — resolves every required retained
//! body through the fail-closed catalogue, retains the derived bodies
//! (goal, invariant set, review source, failed evidence) in the CAS
//! store, and produces the canonical bytes the consume commit stores
//! verbatim. A restart replays the stored row; the builder never runs
//! twice for one `request_id`.
//!
//! Every failure is typed: a [`DispatchRefusal`] becomes the durable
//! `dispatch_refused` fact before any process exists; an infrastructure
//! fault ([`BuildError::Fault`]) fails the pass so the request stays
//! pending for the next one.

use std::path::{Path, PathBuf};

use workloop_core::command::FindingChoice;
use workloop_core::domain::{
    ArtifactHandoff, Execution, FindingOccurrence, HandoffState, InvocationRequest, InvocationRole,
    ReviewRound, RoundJudgment, SpecSnapshot, VerificationOutcome, VerificationResult, Work,
    WorkId, issue_content_digest,
};
use workloop_core::identity::{
    CommitRevision, DiffSnapshot, DiffTarget as CoreDiffTarget, IssueRef, RepositoryRef,
};
use workloop_core::store::{BodyCatalog, ReadView, Store, StoreError};
use workloop_core::worker_v2::{
    ArtifactRef, DispatchRefusal, FAILED_EVIDENCE_FORMAT, FailedEvidence, FailedEvidenceBody,
    FixOrigin, GoalRef, GroupedIssue, InvariantSetBody, InvariantSetRef, IssueBodyRef,
    IssueSnapshotBody, RequestBase, RequestBudget, RequiredCapabilities, ReviewSourceBody,
    ReviewSourceJudgment, ReviewSourceOccurrence, ReviewSourceRef, ReviewSourceRound, SpecRef,
    TaskBody, WORKER_REQUEST_V2_FORMAT, WireIssue, WireRepository, WorkerRequestBody,
    WorkerRequestV2, goal_body_bytes, goal_body_digest,
};
use workloop_store::retention::CategoryLimits;

use crate::bodies::{BodyError, BodyStore, CAS_CATEGORY, ResolvedBody, content_id};
use crate::isolation;
use crate::workspace::{self, PreparationError, RepositorySource};

/// Per-body input bound: a retained body larger than this is
/// `request.body_oversize` — refused before the bytes are ever paid
/// for twice (catalogue read plus materialized copy).
const MAX_INPUT_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// Total retained-input bound across one request: the sum of every
/// resolved body the Invocation will read.
const MAX_INPUT_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

/// The static budget bounds every v2 request carries; the deadline
/// and remaining-runtime members are derived per request from the
/// Execution's recorded budget, never from this table.
#[derive(Debug, Clone)]
pub struct V2BudgetBounds {
    pub max_turns: u64,
    pub max_report_bytes: u64,
    pub max_log_bytes: u64,
    pub max_verification_commands: u64,
    pub max_continuation_refs: u64,
}

/// The installation-approved invariant set an InvariantCheck request
/// binds. A changed set is a new `(id, revision)` pair — the retained
/// body is immutable once written.
#[derive(Debug, Clone)]
pub struct InvariantSetConfig {
    pub id: String,
    pub revision: u64,
    pub items: Vec<String>,
}

/// The v2 dispatch profile the composition root supplies: where the
/// CAS root lives, which local repository sources base objects and
/// workspaces resolve against, the approved invariant set, the static
/// budget bounds, and the capability/egress surface every request
/// requires.
#[derive(Debug, Clone)]
pub struct V2Dispatch {
    /// The content-addressed retained-body store
    /// (`<state_root>/bodies`).
    pub bodies: BodyStore,
    /// Local repository sources matched by the Work's recorded
    /// repository binding — the supervisor-side read path for base
    /// resolution and workspace preparation. The Worker never sees
    /// these paths.
    pub sources: Vec<RepositorySource>,
    /// The approved invariant set; `None` refuses every
    /// InvariantCheck dispatch as `request.invariant_set_missing`.
    pub invariant_set: Option<InvariantSetConfig>,
    pub budget: V2BudgetBounds,
    /// Capabilities every v2 request requires the declaration to
    /// cover; writer roles additionally require `workspace_write`.
    pub capabilities: Vec<String>,
    /// Hostnames the request permits verification egress to. Empty
    /// denies all verification egress — the CONNECT proxy profile
    /// that gives these names meaning lands with the credential slice.
    pub verification_egress_hosts: Vec<String>,
}

impl V2Dispatch {
    /// The configured local source bound to this repository, if any —
    /// the supervisor-side read path base resolution and workspace
    /// preparation both resolve through.
    pub fn source_for(&self, repository: &RepositoryRef) -> Option<&RepositorySource> {
        self.sources.iter().find(|s| s.repository == *repository)
    }
}

/// Why a durable request could not be materialized into a v2 body.
#[derive(Debug)]
pub enum BuildError {
    /// A typed dispatch refusal — recorded as `dispatch_refused`.
    Refusal(DispatchRefusal),
    /// An infrastructure fault (store, filesystem, git): the pass
    /// fails and the request stays pending for the next pass.
    Fault(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refusal(r) => write!(f, "{r}"),
            Self::Fault(detail) => write!(f, "request materialization fault: {detail}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<DispatchRefusal> for BuildError {
    fn from(r: DispatchRefusal) -> Self {
        Self::Refusal(r)
    }
}

impl From<StoreError> for BuildError {
    fn from(e: StoreError) -> Self {
        Self::Fault(e.to_string())
    }
}

fn refusal(r: DispatchRefusal) -> BuildError {
    BuildError::Refusal(r)
}

fn invalid(detail: impl Into<String>) -> DispatchRefusal {
    DispatchRefusal::RequestInvalid {
        detail: detail.into(),
    }
}

fn is_hex_sha1(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn wire_repository(repository: &RepositoryRef) -> WireRepository {
    WireRepository {
        host: repository.host.as_str().to_string(),
        repository: repository.repository.as_str().to_string(),
    }
}

fn wire_issue(issue: &IssueRef) -> WireIssue {
    WireIssue {
        host: issue.repository.host.as_str().to_string(),
        repository: issue.repository.repository.as_str().to_string(),
        issue: issue.issue.as_str().to_string(),
    }
}

/// Map a catalogue/filesystem failure to a dispatch refusal: absent or
/// reclaimed bodies take the caller's `missing` code, bytes that fail
/// verification are `body_corrupt`, and infrastructure faults stay
/// faults so the pass retries rather than refusing the request.
fn body_error_to_refusal(
    error: BodyError,
    missing: impl FnOnce(String) -> DispatchRefusal,
) -> BuildError {
    match error {
        BodyError::Missing { detail } => refusal(missing(detail)),
        BodyError::Unavailable { body_id, state } => {
            refusal(missing(format!("body {body_id} is {state}")))
        }
        BodyError::Corrupt { detail, .. } | BodyError::UnsafePath { detail } => {
            refusal(DispatchRefusal::BodyCorrupt { detail })
        }
        BodyError::Fault(detail) => BuildError::Fault(detail),
    }
}

/// The newest acknowledged artifact handoff for the Work — the
/// produced artifact every follow-on role binds. `produced` and
/// `manifest_digest` must both be recorded: an acknowledged row
/// without them cannot name an exact artifact.
pub(crate) fn latest_acknowledged_handoff(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<Option<ArtifactHandoff>, StoreError> {
    let mut best: Option<ArtifactHandoff> = None;
    for execution in tx.executions_for_work(work_id)? {
        for invocation in tx.invocations_for_execution(&execution.id)? {
            for required in &invocation.required_artifacts {
                let artifact_id = format!("{}/{}", invocation.id.0, required.kind);
                let Some(row) = tx.artifact_handoff(&artifact_id)? else {
                    continue;
                };
                if row.state != HandoffState::Acknowledged
                    || row.work_id != *work_id
                    || row.produced.is_none()
                    || row.manifest_digest.is_none()
                {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|b| row.recorded_seq > b.recorded_seq)
                {
                    best = Some(row);
                }
            }
        }
    }
    Ok(best)
}

fn artifact_ref(handoff: &ArtifactHandoff) -> Result<ArtifactRef, DispatchRefusal> {
    Ok(ArtifactRef {
        artifact_id: handoff.artifact_id.clone(),
        manifest_digest: handoff.manifest_digest.clone().ok_or_else(|| {
            DispatchRefusal::ArtifactUnavailable {
                detail: format!("handoff {} has no manifest digest", handoff.artifact_id),
            }
        })?,
        produced: handoff
            .produced
            .clone()
            .ok_or_else(|| DispatchRefusal::ArtifactUnavailable {
                detail: format!("handoff {} has no produced commit", handoff.artifact_id),
            })?,
    })
}

/// The wire review target for the Work's current accepted head —
/// `change`/`base`/`head` exact, `diff_digest` derived from the same
/// `(change, base, head)` triple `DiffSnapshot` digests everywhere
/// else. `base` is the recorded workspace fork point (the
/// acknowledged handoff's base), never a live re-read.
fn wire_target(
    work: &Work,
    target: &CoreDiffTarget,
    base: &str,
) -> Result<workloop_core::worker_v2::DiffTarget, DispatchRefusal> {
    if target.change.repository != work.binding.repository {
        return Err(DispatchRefusal::HeadStale {
            detail: "the current target names a foreign repository".into(),
        });
    }
    if !is_hex_sha1(base) || !is_hex_sha1(&target.head.object) {
        return Err(DispatchRefusal::HeadStale {
            detail: "the current target does not carry exact commit ids".into(),
        });
    }
    let repository = work.binding.repository.clone();
    let snapshot = DiffSnapshot::new(
        target.change.clone(),
        CommitRevision::new(repository.clone(), base)
            .map_err(|e| DispatchRefusal::HeadStale { detail: e })?,
        CommitRevision::new(repository, target.head.object.clone())
            .map_err(|e| DispatchRefusal::HeadStale { detail: e })?,
    );
    Ok(workloop_core::worker_v2::DiffTarget {
        change: target.change.change_request.as_str().to_string(),
        base: base.to_string(),
        head: target.head.object.clone(),
        diff_digest: snapshot.snapshot_id(),
    })
}

/// Resolve one ordered spec source snapshot to its retained body and
/// verify it against the approved spec identity — the §4.3 Coding/Fix
/// freshness check. Order is the spec's declared order; it is never
/// sorted or deduplicated here (duplicate identities are refused by
/// `WorkerRequestV2::validate`).
fn resolve_grouped_issue(
    bodies: &BodyStore,
    catalog: &dyn BodyCatalog,
    source: &workloop_core::identity::SourceSnapshot,
    order: u64,
    total: &mut u64,
) -> Result<GroupedIssue, BuildError> {
    let expected = wire_issue(&source.issue);
    let path =
        IssueSnapshotBody::lookup_path(&expected, &source.source_revision, &source.content_digest);
    let resolved = bodies
        .resolve_at(catalog, CAS_CATEGORY, &path)
        .map_err(|e| {
            body_error_to_refusal(e, |detail| DispatchRefusal::SnapshotMissing { detail })
        })?
        .ok_or_else(|| {
            refusal(DispatchRefusal::SnapshotMissing {
                detail: format!(
                    "no retained snapshot for issue {} at {}",
                    source.issue.issue, source.source_revision
                ),
            })
        })?;
    if resolved.bytes.len() as u64 > MAX_INPUT_BODY_BYTES {
        return Err(refusal(DispatchRefusal::BodyOversize {
            detail: format!(
                "issue {} snapshot body is {} bytes",
                source.issue.issue,
                resolved.bytes.len()
            ),
        }));
    }
    *total = total.saturating_add(resolved.bytes.len() as u64);
    if *total > MAX_INPUT_TOTAL_BYTES {
        return Err(refusal(DispatchRefusal::BodyOversize {
            detail: format!("issue snapshot inputs exceed {MAX_INPUT_TOTAL_BYTES} bytes total"),
        }));
    }
    let snapshot = IssueSnapshotBody::parse(&resolved.bytes).map_err(|e| {
        refusal(DispatchRefusal::BodyCorrupt {
            detail: format!("issue {} snapshot: {e}", source.issue.issue),
        })
    })?;
    // The retained body must be exactly the spec-bound read: identity,
    // revision, and content digest all agree, and the body's own
    // title/body recompute to the recorded content digest.
    if snapshot.issue != expected
        || snapshot.source_revision != source.source_revision
        || snapshot.content_digest != source.content_digest
        || issue_content_digest(&snapshot.title, &snapshot.body) != snapshot.content_digest
    {
        return Err(refusal(DispatchRefusal::SnapshotStale {
            detail: format!(
                "issue {} retained snapshot disagrees with the approved spec snapshot",
                source.issue.issue
            ),
        }));
    }
    Ok(GroupedIssue {
        order,
        issue: expected,
        source_revision: source.source_revision.clone(),
        snapshot_digest: resolved.body_id.clone(),
        body: IssueBodyRef {
            body_ref: path,
            bytes: resolved.bytes.len() as u64,
            digest: snapshot.wire_digest(),
        },
    })
}

/// Retain one derived body at its deterministic `<type>/<digest>`
/// lookup path and return the resolved row the wire refs bind.
fn retain_derived(
    bodies: &BodyStore,
    catalog: &dyn BodyCatalog,
    prefix: &str,
    digest: &str,
    bytes: &[u8],
    now_unix_seconds: i64,
    horizon: Option<i64>,
) -> Result<ResolvedBody, BuildError> {
    if bytes.len() as u64 > MAX_INPUT_BODY_BYTES {
        return Err(refusal(DispatchRefusal::BodyOversize {
            detail: format!("retained {prefix} body is {} bytes", bytes.len()),
        }));
    }
    let path = format!("{prefix}/{digest}");
    bodies
        .retain_at(
            catalog,
            CAS_CATEGORY,
            &path,
            bytes,
            now_unix_seconds,
            horizon,
        )
        .map_err(|e| match e {
            BodyError::Fault(detail) => BuildError::Fault(detail),
            other => BuildError::Fault(other.to_string()),
        })?;
    bodies
        .resolve_at(catalog, CAS_CATEGORY, &path)
        .map_err(|e| BuildError::Fault(e.to_string()))?
        .ok_or_else(|| BuildError::Fault(format!("retained {prefix} body did not resolve")))
}

/// Everything the durable reads produced for one request — collected
/// inside one transaction so the body is built against a single
/// consistent view.
struct Durable {
    store_id: String,
    work: Work,
    execution: Execution,
    spec: SpecSnapshot,
    handoff: Option<ArtifactHandoff>,
    target: Option<CoreDiffTarget>,
    /// The work's latest sealed round across all targets — prior-review
    /// context for a CodeReview source.
    latest_round: Option<ReviewRound>,
    /// `(round, judgment, occurrences)` when the current target's
    /// latest sealed round carries an exact-set judgment — the
    /// review-origin fix evidence.
    judged_round: Option<(ReviewRound, RoundJudgment, Vec<FindingOccurrence>)>,
    /// Failed mechanical verification results on the predecessor
    /// artifact — the pre-publication fix evidence.
    failed_verification: Vec<VerificationResult>,
}

fn gather(
    tx: &mut dyn workloop_core::store::TxView,
    request: &InvocationRequest,
) -> Result<Durable, BuildError> {
    let store_id = tx.store_id()?;
    let work = tx
        .work(&request.work_id)?
        .ok_or_else(|| refusal(invalid("request names no work")))?;
    let execution = tx
        .execution(&request.execution_id)?
        .ok_or_else(|| refusal(invalid("request names no execution")))?;
    if execution.work_id != work.id {
        return Err(refusal(invalid("execution belongs to another work")));
    }
    // The request must bind the spec revision the Work currently runs
    // on and the operator last approved — anything else is a spec the
    // approval gate no longer stands behind.
    if request.spec_revision != work.spec || request.spec_revision != work.approved_spec {
        return Err(refusal(DispatchRefusal::SpecMismatch {
            detail: format!(
                "request spec revision {} is not the work's approved {}",
                request.spec_revision, work.approved_spec
            ),
        }));
    }
    let spec = tx
        .spec(&work.id, Some(request.spec_revision))?
        .ok_or_else(|| refusal(invalid("request spec revision is not recorded")))?;

    let needs_handoff = matches!(
        request.role,
        InvocationRole::Fix | InvocationRole::InvariantCheck | InvocationRole::Review
    );
    let handoff = if needs_handoff {
        latest_acknowledged_handoff(tx, &work.id)?
    } else {
        None
    };

    let (target, latest_round, judged_round, failed_verification) = match request.role {
        InvocationRole::Coding => (None, None, None, vec![]),
        InvocationRole::Fix => {
            let Some(handoff) = &handoff else {
                return Err(refusal(DispatchRefusal::ArtifactUnavailable {
                    detail: "no acknowledged artifact handoff for this work".into(),
                }));
            };
            let failed = tx
                .verification_results(&handoff.artifact_id)?
                .into_iter()
                .filter(|r| r.result == VerificationOutcome::Fail)
                .collect();
            let target = workloop_core::work::current_target(tx, &work.id)?;
            let latest_round = tx.latest_round(&work.id)?;
            // The review origin exists only when the current target's
            // latest sealed round carries an exact-set judgment —
            // anything else (no target, no round, no judgment, a
            // judgment over a different occurrence set) is not
            // review-driven evidence and the fix falls back to
            // mechanical failure evidence.
            let judged_round = match &target {
                Some(t) => match workloop_core::work::latest_round_for_target(tx, &work.id, t)? {
                    Some(round) => match tx.judgment_for_round(&round.id)? {
                        Some(judgment) if judgment.set_digest == round.set_digest => {
                            let occurrences = tx.occurrences(&round.id)?;
                            Some((round, judgment, occurrences))
                        }
                        Some(_) => {
                            return Err(refusal(DispatchRefusal::JudgmentStale {
                                detail: format!(
                                    "judgment for round {} does not bind its sealed occurrence set",
                                    round.id
                                ),
                            }));
                        }
                        None => None,
                    },
                    None => None,
                },
                None => None,
            };
            (target, latest_round, judged_round, failed)
        }
        InvocationRole::InvariantCheck => {
            if handoff.is_none() {
                return Err(refusal(DispatchRefusal::ArtifactUnavailable {
                    detail: "no acknowledged artifact handoff for this work".into(),
                }));
            }
            (None, None, None, vec![])
        }
        InvocationRole::Review => {
            let target = workloop_core::work::current_target(tx, &work.id)?;
            let latest_round = tx.latest_round(&work.id)?;
            (target, latest_round, None, vec![])
        }
    };

    Ok(Durable {
        store_id: store_id.0,
        work,
        execution,
        spec,
        handoff,
        target,
        latest_round,
        judged_round,
        failed_verification,
    })
}

/// Build the durable v2 request body for one pending `InvocationRequest`.
/// `invocation_id` is allocated by the caller so the consume commit can
/// record the Invocation and the body atomically.
pub fn build_request_body(
    store: &impl Store,
    dispatch: &V2Dispatch,
    request: &InvocationRequest,
    invocation_id: &str,
    now_unix_seconds: i64,
) -> Result<WorkerRequestBody, BuildError> {
    // The gather runs inside one read transaction; a typed refusal is
    // carried out through the captured slot because `transact`'s
    // closure may only fail with `StoreError`. The transaction is
    // read-only, so committing after a refusal writes nothing.
    let mut gathered: Option<Result<Durable, BuildError>> = None;
    store.transact(|tx| {
        gathered = Some(gather(tx, request));
        Ok(())
    })?;
    let durable = gathered.expect("transact ran the gather closure")?;

    let catalog = store.body_catalog();
    let horizon = CategoryLimits::documented_default(CAS_CATEGORY)
        .and_then(|limits| limits.max_age)
        .map(|max_age| now_unix_seconds.saturating_add(max_age));

    // --- shared inputs -------------------------------------------------
    // Grouped issue snapshots are Coding/Fix inputs only: the read-only
    // roles bind artifacts and sets, never issue bodies, so a reclaimed
    // snapshot must not refuse a request that would never read it.
    let mut input_total: u64 = 0;
    let mut issues = Vec::new();
    if matches!(request.role, InvocationRole::Coding | InvocationRole::Fix) {
        for (order, source) in durable.spec.payload.sources.iter().enumerate() {
            issues.push(resolve_grouped_issue(
                &dispatch.bodies,
                catalog,
                source,
                order as u64,
                &mut input_total,
            )?);
        }
    }

    // The goal body: the approved intent and constraints rendered as
    // Markdown, retained at its type-tagged digest path. The role is
    // part of the render so a Coding and a Fix goal over one spec are
    // distinct immutable bodies.
    let goal_bytes = goal_body_bytes(
        request.role.clone(),
        &durable.spec.payload.intent,
        &durable.spec.payload.constraints,
    );
    let goal_digest = goal_body_digest(&goal_bytes);
    let goal = if matches!(request.role, InvocationRole::Coding | InvocationRole::Fix) {
        let resolved = retain_derived(
            &dispatch.bodies,
            catalog,
            "goal",
            &goal_digest,
            &goal_bytes,
            now_unix_seconds,
            horizon,
        )?;
        input_total = input_total.saturating_add(resolved.bytes.len() as u64);
        if input_total > MAX_INPUT_TOTAL_BYTES {
            return Err(refusal(DispatchRefusal::BodyOversize {
                detail: format!("request inputs exceed {MAX_INPUT_TOTAL_BYTES} bytes total"),
            }));
        }
        Some(GoalRef {
            body_ref: format!("goal/{goal_digest}"),
            media_type: "text/markdown".into(),
            bytes: resolved.bytes.len() as u64,
            digest: goal_digest,
        })
    } else {
        None
    };

    // --- base object ----------------------------------------------------
    // Every workspace-writing role resolves its objects through a
    // configured local source: without one the request could never be
    // prepared, so the refusal lands here — before the consume commit —
    // rather than as a launch failure after an Invocation exists.
    if matches!(request.role, InvocationRole::Coding | InvocationRole::Fix) {
        let source = dispatch
            .source_for(&durable.work.binding.repository)
            .ok_or_else(|| {
                refusal(DispatchRefusal::BaseUnavailable {
                    detail: "no repository source is configured for this work".into(),
                })
            })?;
        if !source.path.is_dir() {
            return Err(refusal(DispatchRefusal::BaseUnavailable {
                detail: format!("repository source {} is unavailable", source.path.display()),
            }));
        }
    }
    // Coding pins the source repository's current HEAD — the workspace
    // is built at exactly this commit. Follow-on roles pin the
    // acknowledged handoff's recorded base, which is the workspace's
    // durable start and what `workspace::prepare` re-validates.
    let base_object = match request.role {
        InvocationRole::Coding => {
            let source = dispatch
                .source_for(&durable.work.binding.repository)
                .expect("writer source checked above");
            workspace::git_head(&source.path).map_err(|e| match e {
                PreparationError::MissingBase { detail } | PreparationError::Git { detail } => {
                    refusal(DispatchRefusal::BaseUnavailable { detail })
                }
                other => BuildError::Fault(other.to_string()),
            })?
        }
        _ => durable
            .handoff
            .as_ref()
            .map(|h| h.base.clone())
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                refusal(DispatchRefusal::BaseUnavailable {
                    detail: "no acknowledged handoff records the workspace base".into(),
                })
            })?,
    };

    // --- task body ------------------------------------------------------
    let task = match request.role {
        InvocationRole::Coding => TaskBody::Coding {
            goal: goal.expect("coding always builds a goal"),
            issues,
            constraints: durable.spec.payload.constraints.clone(),
            verification_argv: durable.spec.payload.verification_argv.clone(),
        },
        InvocationRole::Fix => {
            let handoff = durable
                .handoff
                .as_ref()
                .expect("fix requires a handoff — gathered above");
            let predecessor = artifact_ref(handoff)?;
            // A review origin wins when the current target's latest
            // sealed round carries a judgment selecting Fix
            // dispositions; otherwise the fix binds failed mechanical
            // evidence. A fix with neither is refused — it has no
            // durable basis to repair against.
            let origin = match &durable.judged_round {
                Some((round, judgment, occurrences)) => {
                    let mut findings = Vec::new();
                    for (order, decision) in judgment
                        .decisions
                        .iter()
                        .filter(|d| d.choice == FindingChoice::Fix)
                        .enumerate()
                    {
                        let Some(occurrence) =
                            occurrences.iter().find(|o| o.id == decision.occurrence_id)
                        else {
                            return Err(refusal(DispatchRefusal::FindingMissing {
                                detail: format!(
                                    "judgment {} selects occurrence {} which is not in round {}",
                                    judgment.id, decision.occurrence_id, round.id
                                ),
                            }));
                        };
                        // The wire `digest` binds the finding's content
                        // identity; an occurrence the review source
                        // recorded without one has nothing immutable to
                        // bind — refuse rather than substitute the
                        // source key into a digest field.
                        let Some(content_digest) = occurrence.content_digest.clone() else {
                            return Err(refusal(DispatchRefusal::FindingMissing {
                                detail: format!(
                                    "judgment {} selects occurrence {} which carries no content digest",
                                    judgment.id, decision.occurrence_id
                                ),
                            }));
                        };
                        findings.push(workloop_core::worker_v2::FindingOccurrenceRef {
                            occurrence: occurrence.id.clone(),
                            source: occurrence.source_key.clone(),
                            digest: content_digest,
                            order: order as u64,
                        });
                    }
                    if findings.is_empty() {
                        None
                    } else {
                        let target = durable.target.as_ref().ok_or_else(|| {
                            refusal(DispatchRefusal::HeadStale {
                                detail: "the judged target is no longer current".into(),
                            })
                        })?;
                        Some(FixOrigin::Review {
                            target: wire_target(&durable.work, target, &handoff.base)?,
                            round: round.id.clone(),
                            judgment: judgment.id.clone(),
                            findings,
                        })
                    }
                }
                None => None,
            };
            let origin = match origin {
                Some(o) => o,
                None => {
                    if durable.failed_verification.is_empty() {
                        return Err(refusal(DispatchRefusal::EvidenceMissing {
                            detail:
                                "no failed verification evidence or current review judgment binds this fix"
                                    .into(),
                        }));
                    }
                    let mut failed_evidence = Vec::new();
                    for result in &durable.failed_verification {
                        let body = FailedEvidenceBody {
                            format: FAILED_EVIDENCE_FORMAT.into(),
                            work_id: durable.work.id.0.clone(),
                            artifact_id: result.artifact_id.clone(),
                            evidence_id: result.id.clone(),
                            kind: "verification".into(),
                            requirement: result.requirement.clone(),
                            argv: result.argv.clone(),
                            produced: result.produced.clone(),
                            result: match result.result {
                                VerificationOutcome::Fail => "fail".into(),
                                VerificationOutcome::Pass => "pass".into(),
                                VerificationOutcome::Inconclusive => "inconclusive".into(),
                            },
                            output: result.output.clone(),
                        };
                        let bytes = body.canonical_bytes();
                        let digest = body.wire_digest();
                        let resolved = retain_derived(
                            &dispatch.bodies,
                            catalog,
                            "failed-evidence",
                            &digest,
                            &bytes,
                            now_unix_seconds,
                            horizon,
                        )?;
                        input_total = input_total.saturating_add(resolved.bytes.len() as u64);
                        if input_total > MAX_INPUT_TOTAL_BYTES {
                            return Err(refusal(DispatchRefusal::BodyOversize {
                                detail: format!(
                                    "request inputs exceed {MAX_INPUT_TOTAL_BYTES} bytes total"
                                ),
                            }));
                        }
                        failed_evidence.push(FailedEvidence {
                            evidence_id: result.id.clone(),
                            kind: "verification".into(),
                            digest,
                        });
                    }
                    FixOrigin::Prepublication { failed_evidence }
                }
            };
            TaskBody::Fix {
                goal: goal.expect("fix always builds a goal"),
                issues,
                predecessor,
                origin: Box::new(origin),
                constraints: durable.spec.payload.constraints.clone(),
                verification_argv: durable.spec.payload.verification_argv.clone(),
            }
        }
        InvocationRole::InvariantCheck => {
            let handoff = durable
                .handoff
                .as_ref()
                .expect("invariant check requires a handoff — gathered above");
            let set = dispatch.invariant_set.as_ref().ok_or_else(|| {
                refusal(DispatchRefusal::InvariantSetMissing {
                    detail: "no approved invariant set is configured".into(),
                })
            })?;
            let body = InvariantSetBody::new(set.id.clone(), set.revision, set.items.clone());
            let bytes = body.canonical_bytes();
            let digest = body.wire_digest();
            let resolved = retain_derived(
                &dispatch.bodies,
                catalog,
                "invariant-set",
                &digest,
                &bytes,
                now_unix_seconds,
                horizon,
            )?;
            input_total = input_total.saturating_add(resolved.bytes.len() as u64);
            if input_total > MAX_INPUT_TOTAL_BYTES {
                return Err(refusal(DispatchRefusal::BodyOversize {
                    detail: format!("request inputs exceed {MAX_INPUT_TOTAL_BYTES} bytes total"),
                }));
            }
            TaskBody::InvariantCheck {
                artifact: artifact_ref(handoff)?,
                invariant_set: InvariantSetRef {
                    id: set.id.clone(),
                    revision: set.revision,
                    digest,
                },
            }
        }
        InvocationRole::Review => {
            let target = durable.target.as_ref().ok_or_else(|| {
                refusal(DispatchRefusal::HeadStale {
                    detail: "no current accepted diff target for this work".into(),
                })
            })?;
            // The review base is the recorded fork point: the
            // acknowledged handoff's base — the same durable start the
            // producing workspace pinned.
            let handoff = durable.handoff.as_ref().ok_or_else(|| {
                refusal(DispatchRefusal::ReviewSourceUnavailable {
                    detail: "no acknowledged handoff records the review base".into(),
                })
            })?;
            let wire = wire_target(&durable.work, target, &handoff.base)?;
            // Prior-review context: the newest sealed round across all
            // targets, with its occurrence set and exact-set judgment —
            // read back fresh because the gather collected identities
            // only. Rounds are immutable, so "now" equals "then".
            let prior_round = match &durable.latest_round {
                Some(round) => Some(round_source_round(store, round)?),
                None => None,
            };
            let source_body = ReviewSourceBody::new(
                durable.work.id.0.clone(),
                serde_json::to_value(&wire).unwrap_or(serde_json::Value::Null),
                prior_round,
            );
            let bytes = source_body.canonical_bytes();
            let digest = source_body.wire_digest();
            let resolved = retain_derived(
                &dispatch.bodies,
                catalog,
                "review-source",
                &digest,
                &bytes,
                now_unix_seconds,
                horizon,
            )?;
            input_total = input_total.saturating_add(resolved.bytes.len() as u64);
            if input_total > MAX_INPUT_TOTAL_BYTES {
                return Err(refusal(DispatchRefusal::BodyOversize {
                    detail: format!("request inputs exceed {MAX_INPUT_TOTAL_BYTES} bytes total"),
                }));
            }
            TaskBody::CodeReview {
                target: wire,
                review_source: ReviewSourceRef {
                    source: "supervisor".into(),
                    bytes: resolved.bytes.len() as u64,
                    digest,
                    source_revision: content_id(&bytes),
                },
            }
        }
    };

    // --- budget ---------------------------------------------------------
    // The deadline anchors at first dispatch and is the Execution's
    // recorded runtime limit; `max_runtime_seconds` is what remains.
    let started = durable
        .execution
        .started_unix_seconds
        .unwrap_or(now_unix_seconds);
    let runtime_limit =
        i64::try_from(durable.execution.budget.runtime_limit_seconds).unwrap_or(i64::MAX);
    let deadline = started.saturating_add(runtime_limit);
    if deadline <= now_unix_seconds {
        return Err(refusal(invalid("request deadline has already elapsed")));
    }
    let budget = RequestBudget {
        deadline_unix_seconds: deadline,
        max_runtime_seconds: (deadline - now_unix_seconds) as u64,
        max_turns: dispatch.budget.max_turns,
        max_report_bytes: dispatch.budget.max_report_bytes,
        max_log_bytes: dispatch.budget.max_log_bytes,
        max_verification_commands: dispatch.budget.max_verification_commands,
        max_continuation_refs: dispatch.budget.max_continuation_refs,
    };

    // --- capabilities ----------------------------------------------------
    let mut capabilities = dispatch.capabilities.clone();
    if matches!(request.role, InvocationRole::Coding | InvocationRole::Fix)
        && !capabilities.iter().any(|c| c == "workspace_write")
    {
        capabilities.push("workspace_write".into());
    }
    let required_capabilities = RequiredCapabilities {
        role: request.role.clone(),
        protocol_versions: vec![WORKER_REQUEST_V2_FORMAT.to_string()],
        capabilities,
        resume: "unsupported".into(),
        verification_egress_hosts: dispatch.verification_egress_hosts.clone(),
    };

    let read_only = matches!(
        request.role,
        InvocationRole::InvariantCheck | InvocationRole::Review
    );
    let repository = wire_repository(&durable.work.binding.repository);
    let mut wire = WorkerRequestV2 {
        format_version: WORKER_REQUEST_V2_FORMAT.into(),
        request_id: request.request_id.clone(),
        invocation_id: invocation_id.to_string(),
        request_digest: String::new(),
        store_id: durable.store_id.clone(),
        work_id: durable.work.id.0.clone(),
        execution_id: durable.execution.id.0.clone(),
        role: request.role.clone(),
        repository: repository.clone(),
        base: RequestBase {
            repository,
            object: base_object,
        },
        spec: SpecRef {
            revision: request.spec_revision,
            digest: durable.spec.digest.clone(),
        },
        workspace: workloop_core::worker_v2::WorkspaceGrant {
            workspace_id: format!("ws-{}", durable.work.id.0),
            mount: "/workspace".into(),
            read_only,
        },
        budget,
        required_capabilities,
        continuation: vec![],
        task,
    };
    wire.request_digest = wire.digest();
    // The builder's own output must pass the same closed-envelope
    // validation the worker and `workspace::prepare` apply — a body
    // that cannot parse is a build fault, never a stored refusal.
    WorkerRequestV2::parse(&wire.to_canonical_bytes())
        .map_err(|e| BuildError::Fault(format!("built request failed validation: {e}")))?;
    Ok(WorkerRequestBody {
        request_id: request.request_id.clone(),
        format_version: WORKER_REQUEST_V2_FORMAT.into(),
        canonical_body: wire.to_canonical_bytes(),
        body_digest: wire.digest(),
    })
}

/// Map one durable `ReviewRound` to its retained review-source
/// context: the sealed occurrence set plus the exact-set judgment,
/// read back fresh so the materialized source binds what is recorded
/// now — rounds are immutable, so "now" equals "then".
fn round_source_round(
    store: &impl Store,
    round: &ReviewRound,
) -> Result<ReviewSourceRound, StoreError> {
    store.transact(|tx| {
        let occurrences = tx
            .occurrences(&round.id)?
            .into_iter()
            .map(|o| ReviewSourceOccurrence {
                id: o.id,
                source_key: o.source_key,
                content_digest: o.content_digest,
                severity: o.severity,
                position: o.position,
                thread_id: o.thread_id,
            })
            .collect();
        let judgment = tx
            .judgment_for_round(&round.id)?
            .map(|j| ReviewSourceJudgment {
                id: j.id,
                set_digest: j.set_digest,
                decisions: j.decisions,
            });
        Ok(ReviewSourceRound {
            id: round.id.clone(),
            source: round.source.clone(),
            source_revision: round.source_revision.clone(),
            completeness: round.completeness.clone(),
            set_digest: round.set_digest.clone(),
            target: serde_json::to_value(&round.target).unwrap_or(serde_json::Value::Null),
            occurrences,
            judgment,
        })
    })
}

/// The per-Invocation input/output layout: `request.json` plus the
/// resolved input bodies under `input/`, all read-only, and the
/// Invocation-owned `out/` the Worker writes its report and artifacts
/// into. Nothing here is shared between Invocations.
#[derive(Debug, Clone)]
pub struct InvocationIo {
    /// `<root>/<invocation_id>/` — the whole per-Invocation tree.
    pub dir: PathBuf,
    /// The canonical request file the `--request` argv names.
    pub request_path: PathBuf,
    /// The read-only materialized input directory.
    pub input_dir: PathBuf,
    /// The writable output directory the `--out` argv names.
    pub out_dir: PathBuf,
}

/// Why per-Invocation materialization refused: typed dispatch reasons
/// where a body fails verification, plain faults for filesystem errors.
#[derive(Debug)]
pub enum MaterializeError {
    Refusal(DispatchRefusal),
    Fault(String),
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refusal(r) => write!(f, "{r}"),
            Self::Fault(detail) => write!(f, "invocation io fault: {detail}"),
        }
    }
}

impl std::error::Error for MaterializeError {}

impl From<DispatchRefusal> for MaterializeError {
    fn from(r: DispatchRefusal) -> Self {
        Self::Refusal(r)
    }
}

fn mat_fault(detail: impl Into<String>) -> MaterializeError {
    MaterializeError::Fault(detail.into())
}

/// The caller's missing-body refusal, with catalogue detail folded in
/// when the code is the generic `snapshot_missing`; kind-specific
/// codes (`artifact_unavailable`, `evidence_missing`, ...) keep their
/// own detail because the path already names the body.
fn mat_missing(missing: &DispatchRefusal, detail: String) -> MaterializeError {
    match missing {
        DispatchRefusal::SnapshotMissing { .. } => {
            MaterializeError::Refusal(DispatchRefusal::SnapshotMissing { detail })
        }
        other => MaterializeError::Refusal(other.clone()),
    }
}

/// Resolve one catalogue path to verified bytes and check the wire
/// fields the ref binds: byte count (when the ref carries one) and
/// the type-tagged digest the `verify` closure recomputes. A body
/// that resolves but disagrees is `body_corrupt`; an absent one is the
/// caller's `missing` refusal.
fn resolve_ref(
    bodies: &BodyStore,
    catalog: &dyn BodyCatalog,
    path: &str,
    expected_bytes: Option<u64>,
    verify: impl FnOnce(&[u8]) -> Result<(), String>,
    missing: DispatchRefusal,
) -> Result<ResolvedBody, MaterializeError> {
    let resolved = match bodies.resolve_at(catalog, CAS_CATEGORY, path) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Err(MaterializeError::Refusal(missing)),
        Err(e) => {
            return Err(match e {
                BodyError::Missing { detail } => mat_missing(&missing, detail),
                BodyError::Unavailable { body_id, state } => {
                    mat_missing(&missing, format!("body {body_id} is {state}"))
                }
                BodyError::Corrupt { detail, .. } | BodyError::UnsafePath { detail } => {
                    MaterializeError::Refusal(DispatchRefusal::BodyCorrupt { detail })
                }
                BodyError::Fault(detail) => mat_fault(detail),
            });
        }
    };
    if let Some(expected) = expected_bytes
        && resolved.bytes.len() as u64 != expected
    {
        return Err(MaterializeError::Refusal(DispatchRefusal::BodyCorrupt {
            detail: format!(
                "body {path} holds {} bytes, ref records {expected}",
                resolved.bytes.len()
            ),
        }));
    }
    verify(&resolved.bytes)
        .map_err(|detail| MaterializeError::Refusal(DispatchRefusal::BodyCorrupt { detail }))?;
    Ok(resolved)
}

/// Materialize the per-Invocation input/output tree for a recorded
/// request body. Every required body reference is re-resolved through
/// the catalogue and re-verified against its wire digest before
/// anything is written — a body reclaimed or corrupted between the
/// consume commit and the launch refuses here, before any process
/// exists. Optional (`required: false`) continuation bodies that are
/// absent are simply not materialized; required ones refuse.
pub fn materialize_invocation_io(
    store: &impl Store,
    dispatch: &V2Dispatch,
    root: &Path,
    invocation_id: &str,
    body: &WorkerRequestBody,
) -> Result<InvocationIo, MaterializeError> {
    if !body.verifies() {
        return Err(MaterializeError::Refusal(DispatchRefusal::BodyCorrupt {
            detail: "durable request body fails verification".into(),
        }));
    }
    let request = WorkerRequestV2::parse(&body.canonical_body).map_err(|e| {
        MaterializeError::Refusal(DispatchRefusal::BodyCorrupt {
            detail: format!("durable request body does not parse: {e}"),
        })
    })?;

    let catalog = store.body_catalog();
    let dir = root.join(isolation::sanitize_key(invocation_id));
    // A leftover tree from an interrupted earlier attempt is rebuilt
    // from scratch — the layout is deterministic per Invocation.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| mat_fault(format!("clear {}: {e}", dir.display())))?;
    }
    let input_dir = dir.join("input");
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&input_dir)
        .map_err(|e| mat_fault(format!("create {}: {e}", input_dir.display())))?;
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| mat_fault(format!("create {}: {e}", out_dir.display())))?;

    // The request file itself: the exact durable bytes, read-only.
    let request_path = dir.join("request.json");
    {
        use std::os::unix::fs::PermissionsExt;
        let tmp = dir.join("request.json.tmp");
        std::fs::write(&tmp, &body.canonical_body)
            .map_err(|e| mat_fault(format!("write {}: {e}", tmp.display())))?;
        let written =
            std::fs::read(&tmp).map_err(|e| mat_fault(format!("read {}: {e}", tmp.display())))?;
        if written != body.canonical_body {
            let _ = std::fs::remove_file(&tmp);
            return Err(mat_fault("request file write verification failed"));
        }
        // fsync before the rename and the directory after it: the
        // durable body row is the source of truth, but a crash that
        // loses the materialized file must never leave a partial or
        // missing request.json the next pass would trust.
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| mat_fault(format!("sync {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &request_path)
            .map_err(|e| mat_fault(format!("rename {}: {e}", request_path.display())))?;
        let mut perms = std::fs::metadata(&request_path)
            .map_err(|e| mat_fault(e.to_string()))?
            .permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&request_path, perms)
            .map_err(|e| mat_fault(format!("chmod {}: {e}", request_path.display())))?;
        crate::bodies::fsync_dir(&dir).map_err(|e| mat_fault(e.to_string()))?;
    }

    // Continuation refs: a required body that does not resolve is a
    // dispatch refusal; an optional one is recorded-unavailable
    // context and simply has no file. No producer emits continuation
    // refs yet, but the gate is enforced so a future producer cannot
    // smuggle an unverifiable required body past launch.
    for entry in &request.continuation {
        let required = match entry {
            workloop_core::worker_v2::ContinuationRef::WorkspaceCommit { required, .. }
            | workloop_core::worker_v2::ContinuationRef::Artifact { required, .. }
            | workloop_core::worker_v2::ContinuationRef::Report { required, .. } => *required,
        };
        if required {
            return Err(MaterializeError::Refusal(
                DispatchRefusal::ContinuationMissing {
                    detail: "a required continuation body cannot be resolved".into(),
                },
            ));
        }
    }

    // Task inputs, in wire order. Each resolves through its own
    // deterministic catalogue path and re-verifies the type-tagged
    // digest the ref carries.
    match &request.task {
        TaskBody::Coding { goal, issues, .. } | TaskBody::Fix { goal, issues, .. } => {
            let resolved = resolve_ref(
                &dispatch.bodies,
                catalog,
                &goal.body_ref,
                Some(goal.bytes),
                |bytes| {
                    if goal_body_digest(bytes) != goal.digest {
                        return Err(format!("goal body digest mismatch at {}", goal.body_ref));
                    }
                    Ok(())
                },
                DispatchRefusal::SnapshotMissing {
                    detail: format!("goal body {}", goal.body_ref),
                },
            )?;
            dispatch
                .bodies
                .materialize_input(&input_dir, "goal.md", &resolved)
                .map_err(|e| mat_fault(e.to_string()))?;
            for (index, entry) in issues.iter().enumerate() {
                let resolved = resolve_ref(
                    &dispatch.bodies,
                    catalog,
                    &entry.body.body_ref,
                    Some(entry.body.bytes),
                    |bytes| {
                        let snapshot = IssueSnapshotBody::parse(bytes)
                            .map_err(|e| format!("issue snapshot malformed: {e}"))?;
                        if snapshot.wire_digest() != entry.body.digest {
                            return Err(format!(
                                "issue {} snapshot digest mismatch",
                                entry.issue.issue
                            ));
                        }
                        Ok(())
                    },
                    DispatchRefusal::SnapshotMissing {
                        detail: format!("issue {} snapshot", entry.issue.issue),
                    },
                )?;
                dispatch
                    .bodies
                    .materialize_input(&input_dir, &format!("issue-{index}.json"), &resolved)
                    .map_err(|e| mat_fault(e.to_string()))?;
            }
            // The predecessor artifact and review-origin findings are
            // durable store rows, not retained bodies — their freshness
            // was proven at build time and the durable request body
            // freezes the identities. Nothing to materialize.
        }
        TaskBody::InvariantCheck { invariant_set, .. } => {
            let path = format!("invariant-set/{}", invariant_set.digest);
            let resolved = resolve_ref(
                &dispatch.bodies,
                catalog,
                &path,
                None,
                |bytes| {
                    let set = InvariantSetBody::parse(bytes)
                        .map_err(|e| format!("invariant set malformed: {e}"))?;
                    if set.wire_digest() != invariant_set.digest
                        || set.id != invariant_set.id
                        || set.revision != invariant_set.revision
                    {
                        return Err(format!(
                            "invariant set {}@{} disagrees with the request ref",
                            invariant_set.id, invariant_set.revision
                        ));
                    }
                    Ok(())
                },
                DispatchRefusal::InvariantSetMissing {
                    detail: format!(
                        "invariant set {}@{}",
                        invariant_set.id, invariant_set.revision
                    ),
                },
            )?;
            dispatch
                .bodies
                .materialize_input(&input_dir, "invariant-set.json", &resolved)
                .map_err(|e| mat_fault(e.to_string()))?;
        }
        TaskBody::CodeReview { review_source, .. } => {
            let path = format!("review-source/{}", review_source.digest);
            let resolved = resolve_ref(
                &dispatch.bodies,
                catalog,
                &path,
                Some(review_source.bytes),
                |bytes| {
                    let source = ReviewSourceBody::parse(bytes)
                        .map_err(|e| format!("review source malformed: {e}"))?;
                    if source.wire_digest() != review_source.digest {
                        return Err("review source digest mismatch".into());
                    }
                    if content_id(bytes) != review_source.source_revision {
                        return Err("review source revision mismatch".into());
                    }
                    Ok(())
                },
                DispatchRefusal::ReviewSourceUnavailable {
                    detail: format!("review source {}", review_source.digest),
                },
            )?;
            dispatch
                .bodies
                .materialize_input(&input_dir, "review-source.json", &resolved)
                .map_err(|e| mat_fault(e.to_string()))?;
        }
    }

    // Failed-evidence bodies for a pre-publication fix: each resolves
    // at its own digest path and re-verifies identity and kind.
    if let TaskBody::Fix { origin, .. } = &request.task
        && let FixOrigin::Prepublication { failed_evidence } = origin.as_ref()
    {
        for (index, evidence) in failed_evidence.iter().enumerate() {
            let path = format!("failed-evidence/{}", evidence.digest);
            let resolved = resolve_ref(
                &dispatch.bodies,
                catalog,
                &path,
                None,
                |bytes| {
                    let body = FailedEvidenceBody::parse(bytes)
                        .map_err(|e| format!("failed evidence malformed: {e}"))?;
                    if body.wire_digest() != evidence.digest
                        || body.evidence_id != evidence.evidence_id
                        || body.kind != evidence.kind
                    {
                        return Err(format!(
                            "failed evidence {} disagrees with the request ref",
                            evidence.evidence_id
                        ));
                    }
                    Ok(())
                },
                DispatchRefusal::EvidenceMissing {
                    detail: format!("failed evidence {}", evidence.evidence_id),
                },
            )?;
            dispatch
                .bodies
                .materialize_input(&input_dir, &format!("evidence-{index}.json"), &resolved)
                .map_err(|e| mat_fault(e.to_string()))?;
        }
    }

    Ok(InvocationIo {
        dir,
        request_path,
        input_dir,
        out_dir,
    })
}

/// The root every per-Invocation input/output tree lives under —
/// `<workspace_root>/.invocations`, a sibling of the managed
/// workspaces, the `.prepare` control area, and the `.isolation`
/// scratch root.
pub fn invocation_io_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".invocations")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_io_root_sits_beside_managed_state() {
        assert_eq!(
            invocation_io_root(Path::new("/state/ws")),
            PathBuf::from("/state/ws/.invocations")
        );
    }
}
