//! Bind finished v2 InvariantCheck/CodeReview reports to the gate
//! evidence facts they claim — `Assessment` and `Review` observations
//! (`docs/v0.3.md` §6). The report envelope was already validated and
//! retained by `report_v2::collect`; this module re-binds the retained
//! member claims to *current* durable state and emits a gate fact only
//! when every binding holds:
//!
//! - the verified `taisan.worker.request/2` body names the role and
//!   carries the exact task identity (approved invariant set, wire
//!   diff target, review source) the dispatch froze;
//! - the report classification is a validated envelope class —
//!   `invalid`, `absent`, and `failure` claims emit nothing;
//! - for InvariantCheck, the request's artifact ref names an
//!   Acknowledged handoff row (exact `produced` + `manifest_digest`)
//!   whose produced commit is the Work's *current* accepted target
//!   head — the check ran before publication, so its evidence binds
//!   only once the artifact is the accepted head;
//! - for CodeReview, the Work's current accepted target still
//!   corresponds to the wire target (same change, same head, and a
//!   recomputed diff digest over the recorded handoff base), so a
//!   stale head can never inherit a prior review's success.
//!
//! Member claims are evidence, never authority: `findings[]` and
//! `invariant_items[]` parse under the closed grammars in
//! `worker_v2`, malformed coverage degrades to `partial` /
//! `complete: false`, and nothing the Worker writes can mark its own
//! assessment complete or seal its own round.

use workloop_core::domain::{
    ArtifactHandoff, HandoffState, Invocation, InvocationRequest, InvocationRole, Work, WorkId,
};
use workloop_core::identity::{ActorRole, CommitRevision, DiffSnapshot};
use workloop_core::observation::{
    AssessmentItem, AssessmentVerdict, Completeness, ObservationFact, OccurrenceInput,
};
use workloop_core::store::{ReadView, Store, StoreError};
use workloop_core::work::current_target;
use workloop_core::worker_v2::{
    InvariantSetBody, MAX_REPORT_MEMBERS, ReportClassification, ReportFinding, ReportInvariantItem,
    ReportItemVerdict, TaskBody, WORKER_REQUEST_V2_FORMAT, WorkerReportStatus, WorkerReportV2,
    WorkerRequestV2, member_claim_digest,
};

use crate::bodies::{BodyStore, CAS_CATEGORY};
use crate::supervisor::ExecError;

/// Everything the binding decision reads, gathered once per
/// Invocation: the durable request, its verified v2 body, the retained
/// report envelope, and the Work/Execution context the target checks
/// run against.
struct Gathered {
    request: InvocationRequest,
    wire: WorkerRequestV2,
    report: WorkerReportV2,
    classification: ReportClassification,
    work: Work,
    /// The review-lane source identity the emitted fact carries: the
    /// Work's bound review actor when one is configured (the managed
    /// Worker is the transport for that reviewer identity), else the
    /// bound adapter's id for provenance.
    review_source: String,
}

/// Attempt to derive the gate fact for one finished assessment-role
/// Invocation. `Ok(Some((request, fact)))` is the observation the
/// caller reports; `Ok(None)` means nothing binds this pass — wrong
/// role, report not yet collected, a non-evidence classification, or
/// a durable binding that does not currently hold (the sweep retries
/// on later passes, so a pre-publication check binds once its
/// artifact becomes the accepted head).
pub fn bind(
    store: &impl Store,
    bodies: Option<&BodyStore>,
    invocation: &Invocation,
) -> Result<Option<(InvocationRequest, ObservationFact)>, ExecError> {
    let Some(gathered) = gather(store, bodies, invocation)? else {
        return Ok(None);
    };
    // `gather` only returns a report-backed binding when the CAS root
    // is configured, so reaching a role dispatch implies `Some`.
    let Some(bodies) = bodies else {
        return Ok(None);
    };
    let fact = match gathered.wire.role {
        InvocationRole::InvariantCheck => bind_invariant_check(store, bodies, &gathered)?,
        InvocationRole::Review => bind_review(store, &gathered)?,
        _ => return Ok(None),
    };
    Ok(fact.map(|fact| (gathered.request, fact)))
}

/// Read the durable chain the binding stands on: request → verified
/// v2 body → accepted `WorkerReport` fact → CAS-retained report
/// envelope → Work/Execution context. Every early-out is `None`
/// (nothing to bind); corrupt durable state is a fault, matching
/// `report_v2::collect` and `record_claim_check`.
fn gather(
    store: &impl Store,
    bodies: Option<&BodyStore>,
    invocation: &Invocation,
) -> Result<Option<Gathered>, ExecError> {
    let Some(request) = store.transact(|tx| tx.request(&invocation.request_id))? else {
        return Ok(None);
    };
    let Some(body) = store.transact(|tx| tx.worker_request_body(&request.request_id))? else {
        // A v1 dispatch has no v2 report surface — nothing to bind.
        return Ok(None);
    };
    if !body.verifies() || body.format_version != WORKER_REQUEST_V2_FORMAT {
        return Err(ExecError::Store(StoreError::Internal {
            detail: format!(
                "worker request body for {} is corrupt or non-v2",
                request.request_id
            ),
        }));
    }
    let wire = WorkerRequestV2::parse(&body.canonical_body).map_err(|e| {
        ExecError::Store(StoreError::Internal {
            detail: format!("worker request body does not parse: {e}"),
        })
    })?;
    // The wire role and the recorded request role are written from the
    // same dispatch decision; a divergence is corrupt durable state.
    if wire.role != request.role || wire.role != invocation.role {
        return Err(ExecError::Store(StoreError::Internal {
            detail: format!(
                "worker request {} role {:?} disagrees with request {:?} / invocation {:?}",
                request.request_id, wire.role, request.role, invocation.role
            ),
        }));
    }
    if !matches!(
        wire.role,
        InvocationRole::InvariantCheck | InvocationRole::Review
    ) {
        return Ok(None);
    }
    let report_fact = store.transact(|tx| {
        Ok::<_, StoreError>(
            tx.observations_for_execution(&request.execution_id)?
                .into_iter()
                .find(|o| {
                    matches!(
                        &o.fact,
                        ObservationFact::WorkerReport { invocation_id, .. }
                            if invocation_id == &invocation.id
                    )
                })
                .map(|o| o.fact),
        )
    })?;
    let Some(ObservationFact::WorkerReport {
        classification,
        report: Some(report_evidence),
        ..
    }) = report_fact
    else {
        // No accepted report evidence yet (or `absent`): nothing binds.
        return Ok(None);
    };
    // Only validated envelope classes carry claims worth binding —
    // `invalid` retains raw bytes but never a trusted claim, `absent`
    // has none, and a typed failure authorizes no gate evidence.
    if matches!(
        classification,
        ReportClassification::Invalid { .. }
            | ReportClassification::Absent
            | ReportClassification::Failure { .. }
    ) {
        return Ok(None);
    }
    let Some(bodies) = bodies else {
        return Err(ExecError::Fault(
            "worker assessment binding requires the CAS body store".into(),
        ));
    };
    let resolved = bodies
        .resolve(store.body_catalog(), &report_evidence.body_id)
        .map_err(|e| ExecError::Fault(format!("resolve worker report body: {e}")))?;
    let Some(resolved) = resolved else {
        return Err(ExecError::Fault(format!(
            "worker report body {} is missing from the catalog",
            report_evidence.body_id
        )));
    };
    let report: WorkerReportV2 = serde_json::from_slice(&resolved.bytes)
        .map_err(|e| ExecError::Fault(format!("retained worker report does not parse: {e}")))?;
    let work = store.transact(|tx| {
        let execution =
            tx.execution(&request.execution_id)?
                .ok_or_else(|| StoreError::Internal {
                    detail: "invocation without execution".into(),
                })?;
        tx.work(&execution.work_id)?
            .ok_or_else(|| StoreError::Internal {
                detail: "execution without work".into(),
            })
    })?;
    let review_source = work
        .binding
        .actor_for(ActorRole::Review)
        .map(|bound| bound.actor.as_str().to_string())
        .unwrap_or_else(|| {
            request
                .adapter_binding
                .as_ref()
                .map(|b| format!("worker/{}", b.adapter.as_str()))
                .unwrap_or_else(|| "worker/unknown".into())
        });
    Ok(Some(Gathered {
        request,
        wire,
        report,
        classification,
        work,
        review_source,
    }))
}

/// The Work's current accepted diff target, or `None` when no change
/// request/head pair is accepted yet.
fn target_of(
    store: &impl Store,
    work_id: &WorkId,
) -> Result<Option<workloop_core::identity::DiffTarget>, ExecError> {
    store
        .transact(|tx| current_target(tx, work_id))
        .map_err(ExecError::Store)
}

/// Whether an Acknowledged handoff row for the Work records exactly
/// this `produced`/`base` pair — the recorded fork point a review
/// diff was computed from.
fn acknowledged_base(
    store: &impl Store,
    work_id: &WorkId,
    produced: &str,
    base: &str,
) -> Result<bool, ExecError> {
    store
        .transact(|tx| acknowledged_base_tx(tx, work_id, produced, base))
        .map_err(ExecError::Store)
}

fn acknowledged_base_tx(
    tx: &dyn ReadView,
    work_id: &WorkId,
    produced: &str,
    base: &str,
) -> Result<bool, StoreError> {
    for execution in tx.executions_for_work(work_id)? {
        for invocation in tx.invocations_for_execution(&execution.id)? {
            for required in &invocation.required_artifacts {
                let artifact_id = format!("{}/{}", invocation.id.0, required.kind);
                let Some(row) = tx.artifact_handoff(&artifact_id)? else {
                    continue;
                };
                if row.state == HandoffState::Acknowledged
                    && row.work_id == *work_id
                    && row.produced.as_deref() == Some(produced)
                    && row.base == base
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Bind an InvariantCheck report to an `Assessment` fact. The gates,
/// in order: the request's artifact ref names the Acknowledged
/// handoff's exact produced commit and manifest; that commit is the
/// current accepted target head; the approved invariant set body still
/// resolves and verifies. Coverage is then exact-or-incomplete —
/// `complete: false` is recorded evidence, never a pass.
fn bind_invariant_check(
    store: &impl Store,
    bodies: &BodyStore,
    gathered: &Gathered,
) -> Result<Option<ObservationFact>, ExecError> {
    let TaskBody::InvariantCheck {
        artifact,
        invariant_set,
    } = &gathered.wire.task
    else {
        return Err(ExecError::Store(StoreError::Internal {
            detail: "invariant_check request carries a different task".into(),
        }));
    };
    // The assessed artifact must be the acknowledged handoff's exact
    // produced commit — the row the request bound at dispatch.
    let acknowledged = store.transact(|tx| {
        Ok::<_, StoreError>(tx.artifact_handoff(&artifact.artifact_id)?.is_some_and(
            |row: ArtifactHandoff| {
                row.state == HandoffState::Acknowledged
                    && row.work_id == gathered.work.id
                    && row.produced.as_deref() == Some(artifact.produced.as_str())
                    && row.manifest_digest.as_deref() == Some(artifact.manifest_digest.as_str())
            },
        ))
    })?;
    if !acknowledged {
        return Ok(None);
    }
    // The check ran before publication; its evidence binds only once
    // the produced commit is the accepted head. A moved head simply
    // never binds — no copied prior-head success.
    let Some(target) = target_of(store, &gathered.work.id)? else {
        return Ok(None);
    };
    if target.head.object != artifact.produced {
        return Ok(None);
    }
    if matches!(gathered.classification, ReportClassification::Unavailable) {
        // The check could not run — explicit incomplete evidence, never
        // a pass and never silence.
        return Ok(Some(ObservationFact::Assessment {
            target,
            set_digest: invariant_set.digest.clone(),
            complete: false,
            items: vec![],
        }));
    }
    // Resolve the approved set the request bound: the retained body
    // must still verify against the wire ref, or coverage is
    // unverifiable.
    let set = resolve_invariant_set(store, bodies, invariant_set)?;
    let (complete, items) = assess_items(&gathered.report.invariant_items, set.as_ref());
    Ok(Some(ObservationFact::Assessment {
        target,
        set_digest: invariant_set.digest.clone(),
        complete,
        items,
    }))
}

/// Resolve and verify the approved `InvariantSetBody` a request's
/// `invariant_set` ref names. `Ok(None)` means the body is absent or
/// no longer verifies — coverage is then unverifiable, never assumed.
fn resolve_invariant_set(
    store: &impl Store,
    bodies: &BodyStore,
    invariant_set: &workloop_core::worker_v2::InvariantSetRef,
) -> Result<Option<InvariantSetBody>, ExecError> {
    let path = format!("bodies/invariant-set/{}", invariant_set.digest);
    let resolved = bodies
        .resolve_at(store.body_catalog(), CAS_CATEGORY, &path)
        .map_err(|e| ExecError::Fault(format!("resolve invariant set body: {e}")))?;
    let Some(resolved) = resolved else {
        return Ok(None);
    };
    let set = match InvariantSetBody::parse(&resolved.bytes) {
        Ok(set) => set,
        Err(_) => return Ok(None),
    };
    if set.wire_digest() != invariant_set.digest
        || set.id != invariant_set.id
        || set.revision != invariant_set.revision
    {
        return Ok(None);
    }
    Ok(Some(set))
}

/// Map the report's `invariant_items` claims to assessment items.
/// `complete` requires the approved set to resolve AND every member to
/// parse AND coverage to be exact — each approved id exactly once, no
/// unknown ids, no duplicates. Anything less records `complete:
/// false` with whatever valid claims parsed; the gate can never read
/// it as a pass.
fn assess_items(
    members: &[serde_json::Value],
    set: Option<&InvariantSetBody>,
) -> (bool, Vec<AssessmentItem>) {
    let mut malformed = members.len() > MAX_REPORT_MEMBERS;
    let mut parsed: Vec<ReportInvariantItem> = Vec::new();
    if !malformed {
        for value in members {
            match ReportInvariantItem::parse_member(value) {
                Ok(item) => parsed.push(item),
                Err(_) => malformed = true,
            }
        }
    }
    let mut coverage_exact = false;
    if let Some(set) = set {
        coverage_exact = set
            .items
            .iter()
            .all(|id| parsed.iter().filter(|item| &item.id == id).count() == 1)
            && parsed.iter().all(|item| set.items.contains(&item.id))
            && !malformed;
    }
    let complete = !malformed && coverage_exact;
    let ordered: Vec<&ReportInvariantItem> = if complete {
        // Canonical order is the approved set's declared order.
        set.expect("complete implies a resolved set")
            .items
            .iter()
            .filter_map(|id| parsed.iter().find(|item| &item.id == id))
            .collect()
    } else {
        parsed.iter().collect()
    };
    let items = ordered
        .into_iter()
        .map(|item| AssessmentItem {
            id: item.id.clone(),
            verdict: match item.verdict {
                ReportItemVerdict::Conforms => AssessmentVerdict::Positive,
                ReportItemVerdict::Violates => AssessmentVerdict::Negative,
                ReportItemVerdict::Inconclusive => AssessmentVerdict::Inconclusive,
            },
            refs: item.refs.clone(),
        })
        .collect();
    (complete, items)
}

/// Bind a CodeReview report to a `Review` fact. The gates: the current
/// accepted target still corresponds to the wire target (same change,
/// same head, recomputed diff digest), and the wire base is a recorded
/// fork point for that produced head. Member findings then seal —
/// complete only under exact valid coverage.
fn bind_review(
    store: &impl Store,
    gathered: &Gathered,
) -> Result<Option<ObservationFact>, ExecError> {
    let TaskBody::CodeReview {
        target: wire_target,
        review_source,
    } = &gathered.wire.task
    else {
        return Err(ExecError::Store(StoreError::Internal {
            detail: "code_review request carries a different task".into(),
        }));
    };
    let Some(target) = target_of(store, &gathered.work.id)? else {
        return Ok(None);
    };
    if target.change.change_request.as_str() != wire_target.change
        || target.head.object != wire_target.head
    {
        // The accepted head or change moved since dispatch — the
        // review covers a superseded target and binds nothing.
        return Ok(None);
    }
    // The wire triple must be self-consistent: the recorded
    // `diff_digest` is recomputed over `(change, base, head)` exactly
    // as the request builder derived it.
    let (Ok(base), Ok(head)) = (
        CommitRevision::new(target.change.repository.clone(), wire_target.base.clone()),
        CommitRevision::new(target.change.repository.clone(), wire_target.head.clone()),
    ) else {
        return Ok(None);
    };
    if DiffSnapshot::new(target.change.clone(), base, head).snapshot_id() != wire_target.diff_digest
    {
        return Ok(None);
    }
    // The wire base is the workspace fork point recorded at dispatch;
    // it must still be the acknowledged handoff's base for this
    // produced head — never a worker-invented comparison point.
    if !acknowledged_base(
        store,
        &gathered.work.id,
        &wire_target.head,
        &wire_target.base,
    )? {
        return Ok(None);
    }
    let completeness = if matches!(gathered.classification, ReportClassification::Unavailable) {
        Completeness::Unavailable {
            reason: gathered.report.reason.code.clone(),
        }
    } else {
        review_completeness(&gathered.report)
    };
    let occurrences = review_occurrences(&gathered.report.findings);
    Ok(Some(ObservationFact::Review {
        target,
        source: gathered.review_source.clone(),
        source_revision: Some(review_source.source_revision.clone()),
        completeness,
        occurrences,
    }))
}

/// Decide the review's completeness from its member claims. Valid
/// unique members under the closed grammar are `Complete`; a negative
/// report whose reason is not `findings_reported` is a writer-role
/// claim, never a review result; a `findings_reported` reason carrying
/// zero findings is a contradictory claim, and any malformed/duplicate
/// member is `Partial` — recorded evidence that can never seal a
/// round.
fn review_completeness(report: &WorkerReportV2) -> Completeness {
    // A negative report whose reason is not `findings_reported` is a
    // claim about the artifact's checks or invariants — the Worker's
    // writer-role vocabulary — never a review result: it can neither
    // seal a round nor stand in for a clean zero-finding review.
    if report.status == WorkerReportStatus::Negative && report.reason.code != "findings_reported" {
        return Completeness::Partial {
            reason: "negative_report_not_a_review".into(),
        };
    }
    if report.findings.len() > MAX_REPORT_MEMBERS {
        return Completeness::Partial {
            reason: "too_many_findings".into(),
        };
    }
    let mut keys = std::collections::BTreeSet::new();
    for value in &report.findings {
        let Ok(member) = ReportFinding::parse_member(value) else {
            return Completeness::Partial {
                reason: "malformed_findings".into(),
            };
        };
        if !keys.insert(member.key.clone()) {
            return Completeness::Partial {
                reason: "duplicate_finding_keys".into(),
            };
        }
    }
    if report.findings.is_empty() && report.reason.code == "findings_reported" {
        return Completeness::Partial {
            reason: "findings_reported_without_findings".into(),
        };
    }
    Completeness::Complete
}

/// Build the occurrence inputs for a review fact. Under `Complete`
/// every member already parsed — a parse failure here is impossible
/// and drops the member defensively; under `Partial`/`Unavailable`
/// the valid subset is retained as claim evidence.
fn review_occurrences(members: &[serde_json::Value]) -> Vec<OccurrenceInput> {
    let mut occurrences = Vec::new();
    for value in members {
        let Ok(member) = ReportFinding::parse_member(value) else {
            continue;
        };
        let content_digest = member_claim_digest(&member);
        occurrences.push(OccurrenceInput {
            source_key: member.key,
            content_digest: Some(content_digest),
            severity: member.severity,
            position: member.position,
            thread_id: member.thread_id,
        });
    }
    occurrences
}
