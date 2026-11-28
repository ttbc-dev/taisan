//! Read-only derived state over the store. Queries never refresh the host
//! or start a worker; every view is shaped from saved Work/Execution/
//! Invocation records and Observations — there is no separate card state.

use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId};
use crate::domain::*;
use crate::identity::{ChangeRequestRef, CommitRevision, HostActorRef, IssueRef};
use crate::observation::{
    ArtifactFact, HostActionStatus, ObservationFact, Outcome, ProcessStateFact, ReadRevision,
    ReadSubject,
};
use crate::store::{
    Deferral, IntakeCandidateStatus, IntakeRunState, IntakeSourceOwner, IntakeTrigger, ReadView,
    StoreError,
};
use crate::work::DEFAULT_PENDING_QUEUE_LIMIT;

/// Whether a source's evidence could actually be read. An unavailable or
/// unknown source is not evidence of a clean result — an empty finding
/// list with an unavailable review source proves nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Known,
    Unknown,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub source: String,
    pub availability: Availability,
    pub observed_unix_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryResult<T> {
    Found {
        value: T,
        as_of: EventSequence,
        sources: Vec<SourceStatus>,
    },
    NotFound,
    Forbidden,
    StoreUnavailable,
}

/// Kanban-style phase derived from the current Execution — not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase {
    Queued,
    Implementing,
    Reviewing,
    Converging,
    Waiting,
    Paused,
    Stopping,
    Completed,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkSummary {
    pub work_id: WorkId,
    pub version: u64,
    pub phase: WorkPhase,
    pub current_execution_id: Option<ExecutionId>,
    pub attention_count: usize,
    pub outcome: Option<Outcome>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkListView {
    pub items: Vec<WorkSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionView {
    pub execution_id: ExecutionId,
    pub state: ExecutionState,
    /// `uncertain` while ownership of a writer is unproven.
    pub ownership: String,
    pub invocation_ids: Vec<InvocationId>,
    /// The execution's durable request rows explained for inspection
    /// (#379): role, executor identity, liveness class, and any
    /// dispatch-refusal code.
    pub requests: Vec<RequestView>,
    pub outcome: Option<Outcome>,
}

/// The bound endpoint's public descriptor — transport kind/host/port
/// and the authenticated identity digest. Credential references are
/// opaque provider names and never appear in inspect output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEndpointView {
    pub endpoint_id: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub expected_identity_digest: String,
}

/// One durable request row the inspect surface explains (#379): which
/// executor it is bound to, its role, whether it is still pending /
/// launched / refused, and the refusal code when it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestView {
    pub request_id: String,
    pub role: InvocationRole,
    /// The recorded executor identity — `local` or the bound
    /// endpoint's id. A row recorded before placement existed carries
    /// no binding; its executor is the coordinator host's local one.
    pub executor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<RequestEndpointView>,
    /// The placement schema version + resolved-policy digest the
    /// binding was stamped with at admission; `None` on a
    /// pre-placement row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_schema_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_config_digest: Option<String>,
    /// `pending` while unconsumed, `launched` once an Invocation
    /// exists for the request, `refused` once a durable
    /// dispatch-refusal fact names it, and `consumed` when the row was
    /// consumed with neither (a dispatch gate that can never launch it).
    pub liveness: String,
    /// The closed `request.*` refusal code when refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<InvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_state: Option<InvocationState>,
}

/// One finding the detail pane displays: the occurrence id a
/// `JudgeFindings` decision names, plus the source-supplied key and
/// the sealed claim's human-readable text for display. Identity is
/// the occurrence id, never the display text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FindingView {
    pub occurrence_id: String,
    pub source_key: String,
    /// The sealed claim's finding text — display only.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDetailView {
    pub work_id: WorkId,
    pub version: u64,
    pub current_execution_id: Option<ExecutionId>,
    pub executions: Vec<ExecutionView>,
    /// The change request the Work opened and its latest observed head.
    pub current_change: Option<(ChangeRequestRef, Option<CommitRevision>)>,
    /// The Work's latest sealed review round's occurrences; older
    /// revisions stay in evidence.
    pub findings: Vec<FindingView>,
    /// The sealed round id those findings belong to.
    pub current_round_id: Option<String>,
    /// Judgments already recorded on the latest round's occurrence ids.
    pub judged: Vec<String>,
    pub outcome: Option<Outcome>,
    pub attention: Vec<AttentionView>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionView {
    /// Identifies the acknowledged version; a condition change resurfaces
    /// under a different id.
    pub attention_id: String,
    pub work_id: WorkId,
    pub reason: String,
    pub evidence: Vec<String>,
    /// The threshold that produced this Attention, where one applies.
    pub investigation_threshold_seconds: Option<u64>,
    pub next_actor: String,
    pub next_action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotHolder {
    pub execution_id: ExecutionId,
    pub state: ExecutionState,
    pub since: EventSequence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityView {
    pub used: usize,
    pub limit: u32,
    pub holders: Vec<SlotHolder>,
    /// Held reservations including external claims and their expiry.
    pub reservations: Vec<ResourceReservation>,
    /// Queued Executions deferred with their diagnostic.
    pub deferred: Vec<Deferral>,
}

fn store_source() -> SourceStatus {
    SourceStatus {
        source: "store".into(),
        availability: Availability::Known,
        observed_unix_seconds: None,
    }
}

/// Phase from the Work's current Execution and its latest Invocation role.
pub fn phase_of(execution: &Execution, invocations: &[Invocation]) -> WorkPhase {
    match execution.state {
        ExecutionState::Queued => WorkPhase::Queued,
        ExecutionState::Ready | ExecutionState::Running => {
            match invocations.last().map(|i| &i.role) {
                Some(InvocationRole::Review) => WorkPhase::Reviewing,
                _ => WorkPhase::Implementing,
            }
        }
        ExecutionState::Waiting => WorkPhase::Waiting,
        ExecutionState::Paused => WorkPhase::Paused,
        ExecutionState::Stopping => WorkPhase::Stopping,
        ExecutionState::Uncertain => WorkPhase::Unknown,
        ExecutionState::Succeeded => WorkPhase::Completed,
        ExecutionState::Failed | ExecutionState::Cancelled => WorkPhase::Stopped,
    }
}

/// The Work's attention-carrying Execution: the nonterminal one, else the
/// most recent (its terminal failure may still need a retry decision).
pub(crate) fn current_execution(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<Option<Execution>, StoreError> {
    if let Some(active) = tx.active_execution(work_id)? {
        return Ok(Some(active));
    }
    Ok(tx.executions_for_work(work_id)?.into_iter().last())
}

/// Attention versions the saved state derives right now, before the
/// acknowledgement filter — the only ids an `acknowledge` may name.
/// Acknowledging an id the state does not currently derive could hide a
/// condition that has not surfaced yet.
pub(crate) fn derivable_attention_ids(
    tx: &dyn ReadView,
    work_id: &WorkId,
    now_unix_seconds: i64,
) -> Result<Vec<String>, StoreError> {
    let Some(execution) = current_execution(tx, work_id)? else {
        return Ok(vec![]);
    };
    Ok(attention_views(tx, work_id, &execution, now_unix_seconds)?
        .into_iter()
        .map(|a| a.attention_id)
        .collect())
}

/// Attention derived from a decidable condition: paused human judgment,
/// confirmed failure with an eligible retry, or `uncertain` unresolved
/// beyond its investigation threshold. Queue/CI waits are not Attention.
fn derive_attention(
    tx: &dyn ReadView,
    work_id: &WorkId,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<Vec<AttentionView>, StoreError> {
    let mut out = attention_views(tx, work_id, execution, now_unix_seconds)?;
    let acked = tx.attention_acks(work_id)?;
    out.retain(|a| !acked.contains(&a.attention_id));
    Ok(out)
}

/// The Attention versions the current Execution state derives, with no
/// acknowledgement filtering.
fn attention_views(
    tx: &dyn ReadView,
    work_id: &WorkId,
    execution: &Execution,
    now_unix_seconds: i64,
) -> Result<Vec<AttentionView>, StoreError> {
    let execution_evidence = tx.observations_for_execution(&execution.id)?.len();
    let work_evidence = tx
        .observations_for_work(work_id)?
        .into_iter()
        .filter(|observation| {
            // A merge-detection check is ordering bookkeeping — evidence
            // of the read itself, minted fresh every pass — so it must
            // not count as new Work evidence: its per-pass record would
            // mint a new occurrence id and resurface an acknowledged
            // Attention on every pass.
            observation.subject_execution_id.is_none()
                && !matches!(observation.fact, ObservationFact::MergeCheck { .. })
        })
        .count();
    let occurrence = format!(
        "{}:{}:{}",
        execution.id.0,
        execution.enqueued_seq,
        execution_evidence + work_evidence
    );
    let mut out = Vec::new();
    let outcome_evidence: Vec<String> = execution
        .outcome
        .as_ref()
        .map(|o| {
            o.evidence_refs
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut push = |reason: &str,
                    anchor: String,
                    threshold: Option<u64>,
                    action: &str,
                    evidence: Vec<String>| {
        out.push(AttentionView {
            attention_id: format!("att:{}:{}:{}:{}", work_id.0, reason, anchor, occurrence),
            work_id: work_id.clone(),
            reason: reason.to_string(),
            evidence,
            investigation_threshold_seconds: threshold,
            next_actor: "operator".into(),
            next_action: action.to_string(),
        });
    };
    match execution.state {
        ExecutionState::Paused => {
            let anchor = execution
                .pause_reason
                .clone()
                .unwrap_or_else(|| "paused".into());
            push(
                "paused_judgment",
                anchor,
                None,
                "submit a judgment or resume",
                outcome_evidence.clone(),
            );
        }
        ExecutionState::Failed => {
            let anchor = execution
                .outcome
                .as_ref()
                .map(|o| o.recorded_seq.to_string())
                .unwrap_or_else(|| execution.id.0.clone());
            push(
                "confirmed_failure",
                anchor,
                None,
                "decide retry or repair for the failed Execution",
                outcome_evidence.clone(),
            );
        }
        ExecutionState::Uncertain => {
            let threshold = execution.budget.uncertain_threshold_seconds;
            let since = execution
                .uncertain_since_unix_seconds
                .unwrap_or(now_unix_seconds);
            if now_unix_seconds - since >= threshold as i64 {
                push(
                    "uncertain_beyond_threshold",
                    format!("{since}+{threshold}"),
                    Some(threshold),
                    "investigate the unresolved writer and reconcile process evidence",
                    outcome_evidence.clone(),
                );
            }
        }
        _ => {}
    }
    // A conflict escalation that could not move the Execution — a held
    // pause or a stop already in flight — is recorded on the decision
    // ledger instead of the pause reason; the ledger is the only place
    // its reason and paths live, so Attention derives from it or the
    // escalation never surfaces. The derivation only holds while the
    // Execution remains in a state the escalation could not move:
    // resume clears the hold and the recorded decision must not
    // resurface as a ghost Attention for the rest of its lifetime.
    // Every resume/requeue re-stamps `enqueued_seq`, so a decision made
    // before the current queueing belongs to an already-cleared hold —
    // only a decision recorded after it can describe the hold the
    // operator still owes. A decision whose detail is already the
    // recorded pause reason is carried by `paused_judgment` and is not
    // repeated.
    if matches!(
        execution.state,
        ExecutionState::Paused | ExecutionState::Stopping
    ) {
        for decision in tx.recovery_decisions(work_id)? {
            if decision.execution_id.as_ref() != Some(&execution.id)
                || decision.classification != "conflict_unresolvable"
                || decision.seq <= execution.enqueued_seq
                || execution.pause_reason.as_deref() == Some(decision.detail.as_str())
            {
                continue;
            }
            push(
                "conflict_unresolvable",
                decision.id.clone(),
                None,
                "decide the escalated conflict episode or dismiss it",
                vec![decision.detail.clone()],
            );
        }
    }
    // No-progress: durable usage says nothing material happened inside the
    // policy window. Attention only — never proof of termination. A
    // progress record that cannot be classified (no basis, or predating
    // first dispatch) surfaces as `progress_unknown` — never counted as
    // progress, never as death.
    if matches!(
        execution.state,
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting
    ) && let (Some(work), Some(usage)) = (tx.work(work_id)?, tx.work_usage(work_id)?)
    {
        match crate::work::material_progress(tx, work_id)? {
            MaterialProgressSignal::Reading(progress) => {
                if now_unix_seconds - progress.last_progress_at
                    >= work.policy.no_progress_seconds as i64
                {
                    push(
                        "no_progress",
                        format!(
                            "{}+{}",
                            progress.last_progress_at, work.policy.no_progress_seconds
                        ),
                        Some(work.policy.no_progress_seconds),
                        "investigate progress; the Work has produced no material evidence",
                        outcome_evidence.clone(),
                    );
                }
            }
            MaterialProgressSignal::NotStarted => {}
            MaterialProgressSignal::Unknown => {
                push(
                    "progress_unknown",
                    format!("{}", usage.last_progress_unix_seconds.unwrap_or(0)),
                    None,
                    "inspect the progress evidence; it cannot be classified and proves neither progress nor termination",
                    outcome_evidence.clone(),
                );
            }
        }
    }
    Ok(out)
}

pub fn work_list(
    tx: &dyn ReadView,
    now_unix_seconds: i64,
) -> Result<QueryResult<WorkListView>, StoreError> {
    let mut items = Vec::new();
    for work in tx.works()? {
        let current = current_execution(tx, &work.id)?;
        let (phase, exec_id, outcome, attention_count) = match &current {
            Some(e) => {
                let invs = tx.invocations_for_execution(&e.id)?;
                let att = derive_attention(tx, &work.id, e, now_unix_seconds)?.len();
                (
                    phase_of(e, &invs),
                    Some(e.id.clone()),
                    e.outcome.clone(),
                    att,
                )
            }
            None => (WorkPhase::Unknown, None, None, 0),
        };
        items.push(WorkSummary {
            work_id: work.id.clone(),
            version: work.version,
            phase,
            current_execution_id: exec_id,
            attention_count,
            outcome,
            evidence: vec![],
        });
    }
    Ok(QueryResult::Found {
        value: WorkListView {
            items,
            next_cursor: None,
        },
        as_of: tx.last_seq()?,
        sources: vec![store_source()],
    })
}

/// Shape one durable request row for inspection. `liveness` is the
/// caller-classified state (`pending`/`launched`/`refused`); the
/// recorded binding supplies the executor identity and audit anchor.
/// Credential references are deliberately not carried — the endpoint
/// view is the public descriptor only.
fn request_view(
    request: &InvocationRequest,
    liveness: &str,
    invocation: Option<&Invocation>,
    refusal: Option<String>,
) -> RequestView {
    let (executor, endpoint, schema_version, digest) = match &request.placement {
        Some(binding) => (
            binding.executor.clone(),
            binding.endpoint.as_ref().map(|ep| RequestEndpointView {
                endpoint_id: ep.endpoint_id.clone(),
                kind: ep.kind.clone(),
                host: ep.host.clone(),
                port: ep.port,
                expected_identity_digest: ep.expected_identity_digest.clone(),
            }),
            Some(binding.schema_version),
            Some(binding.config_digest.clone()),
        ),
        None => ("local".into(), None, None, None),
    };
    RequestView {
        request_id: request.request_id.clone(),
        role: request.role.clone(),
        executor,
        endpoint,
        placement_schema_version: schema_version,
        placement_config_digest: digest,
        liveness: liveness.into(),
        refusal,
        invocation_id: invocation.map(|i| i.id.clone()),
        invocation_state: invocation.map(|i| i.state.clone()),
    }
}

pub fn work_detail(
    tx: &dyn ReadView,
    work_id: &WorkId,
    now_unix_seconds: i64,
) -> Result<QueryResult<WorkDetailView>, StoreError> {
    let Some(work) = tx.work(work_id)? else {
        return Ok(QueryResult::NotFound);
    };
    let pending = tx.pending_requests()?;
    let executions = tx
        .executions_for_work(work_id)?
        .into_iter()
        .map(|e| {
            let invs = tx.invocations_for_execution(&e.id).unwrap_or_default();
            let ids = invs.iter().map(|i| i.id.clone()).collect();
            // Every durable request row for the execution, classified
            // once: still-pending rows come from the pending set,
            // launched rows behind each Invocation, refused rows behind
            // the dispatch-refusal facts, and a consumed row with
            // neither is the gate-consumed case (`mark_consumed` without
            // a launch). Reading the request log once — never unioning
            // the three views — keeps one row from rendering twice for
            // one `request_id`, and the explicit consumed class keeps a
            // gate-consumed request visible.
            let pending_ids: std::collections::HashSet<&str> = pending
                .iter()
                .filter(|r| r.execution_id == e.id)
                .map(|r| r.request_id.as_str())
                .collect();
            let inv_by_request: std::collections::HashMap<&str, &Invocation> =
                invs.iter().map(|i| (i.request_id.as_str(), i)).collect();
            let mut refusals: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            if let Ok(obs) = tx.observations_for_execution(&e.id) {
                for fact in obs.iter().filter_map(|o| match &o.fact {
                    ObservationFact::DispatchRefused {
                        request_id, code, ..
                    } => Some((request_id.clone(), code.clone())),
                    _ => None,
                }) {
                    refusals.insert(fact.0, fact.1);
                }
            }
            let mut requests: Vec<RequestView> = tx
                .requests_for_execution(&e.id)
                .unwrap_or_default()
                .iter()
                .map(|r| {
                    if let Some(inv) = inv_by_request.get(r.request_id.as_str()).copied() {
                        request_view(r, "launched", Some(inv), None)
                    } else if let Some(code) = refusals.get(r.request_id.as_str()) {
                        request_view(r, "refused", None, Some(code.clone()))
                    } else if pending_ids.contains(r.request_id.as_str()) {
                        request_view(r, "pending", None, None)
                    } else {
                        request_view(r, "consumed", None, None)
                    }
                })
                .collect();
            requests.sort_by(|a, b| a.request_id.cmp(&b.request_id));
            ExecutionView {
                ownership: if e.state == ExecutionState::Uncertain {
                    "uncertain".into()
                } else {
                    "confirmed".into()
                },
                execution_id: e.id,
                state: e.state,
                invocation_ids: ids,
                requests,
                outcome: e.outcome,
            }
        })
        .collect::<Vec<_>>();
    let current = current_execution(tx, work_id)?;
    let mut attention = Vec::new();
    if let Some(e) = &current {
        attention = derive_attention(tx, work_id, e, now_unix_seconds)?;
    }
    let outcome = current.as_ref().and_then(|e| e.outcome.clone());

    let observations = tx.observations_for_work(work_id)?;
    // The opened change request (verified receipts only — the same gate
    // `current_target` applies) and its latest observed head.
    let change: Option<ChangeRequestRef> =
        crate::work::applied_opened_change(tx, work_id)?.map(|(c, _)| c);
    let current_change = change.map(|c| {
        let head = observations
            .iter()
            .enumerate()
            .filter_map(|(index, obs)| match &obs.fact {
                ObservationFact::ChangeHead { change: hc, head } if hc == &c => {
                    Some((obs.observed_unix_seconds, index, head.clone()))
                }
                _ => None,
            })
            .max_by_key(|(t, i, _)| (*t, *i))
            .map(|(_, _, h)| h);
        (c, head)
    });
    // Findings on the latest sealed round for the CURRENT diff target.
    // Rounds sealed for superseded targets are recorded evidence but
    // never mask or substitute the current surface — and a round that
    // does not bind the current target cannot be judged anyway
    // (`current_round_for` rejects it).
    let target = crate::work::current_target(tx, work_id)?;
    let mut findings = Vec::new();
    let mut judged = Vec::new();
    let mut current_round_id = None;
    let mut sources = vec![store_source()];
    let current_round = match &target {
        Some(t) => crate::work::latest_round_for_target(tx, work_id, t)?,
        None => None,
    };
    if let Some(round) = current_round {
        current_round_id = Some(round.id.clone());
        for o in tx.occurrences(&round.id)? {
            findings.push(FindingView {
                occurrence_id: o.id,
                source_key: o.source_key,
                summary: o.summary,
            });
        }
        if let Some(j) = tx.judgment_for_round(&round.id)? {
            for d in &j.decisions {
                judged.push(format!("{}:{:?}", d.occurrence_id, d.choice));
            }
        }
        sources.push(SourceStatus {
            source: format!("review:{}", round.source),
            availability: if round.completeness == "complete" {
                Availability::Known
            } else {
                Availability::Unavailable
            },
            observed_unix_seconds: None,
        });
    } else if current_change.is_some() {
        // A published change with no sealed round: the review evidence
        // has not been read — an empty finding list proves nothing.
        sources.push(SourceStatus {
            source: "review".into(),
            availability: Availability::Unavailable,
            observed_unix_seconds: None,
        });
    }

    let mut evidence: Vec<String> = Vec::new();
    for obs in &observations {
        match &obs.fact {
            ObservationFact::ProcessState { state, .. } => {
                evidence.push(format!("process:{state:?}"))
            }
            ObservationFact::Artifact {
                required_artifact,
                state,
                ..
            } => evidence.push(format!("artifact:{required_artifact}:{state:?}")),
            ObservationFact::HostActionResult {
                action_id, status, ..
            } => evidence.push(format!("host_action:{action_id}:{status:?}")),
            ObservationFact::Issue { issue, state, .. } => {
                evidence.push(format!("issue:{issue}:{state:?}"))
            }
            _ => {}
        }
    }

    Ok(QueryResult::Found {
        value: WorkDetailView {
            work_id: work.id.clone(),
            version: work.version,
            current_execution_id: current.map(|e| e.id),
            executions,
            current_change,
            findings,
            current_round_id,
            judged,
            outcome,
            attention,
            evidence,
        },
        as_of: tx.last_seq()?,
        sources,
    })
}

pub fn attention(
    tx: &dyn ReadView,
    work_id: Option<&WorkId>,
    now_unix_seconds: i64,
) -> Result<QueryResult<Vec<AttentionView>>, StoreError> {
    let mut out = Vec::new();
    let works: Vec<WorkId> = match work_id {
        Some(w) => vec![w.clone()],
        None => tx.works()?.into_iter().map(|w| w.id).collect(),
    };
    for w in works {
        if let Some(e) = current_execution(tx, &w)? {
            out.extend(derive_attention(tx, &w, &e, now_unix_seconds)?);
        }
    }
    Ok(QueryResult::Found {
        value: out,
        as_of: tx.last_seq()?,
        sources: vec![store_source()],
    })
}

/// Every slot holder, held reservation (with external-claim expiry), and
/// deferred Execution with its diagnostic — the stall explanation surface.
pub fn capacity(tx: &dyn ReadView) -> Result<QueryResult<CapacityView>, StoreError> {
    let mut holders = Vec::new();
    for (execution_id, since) in tx.slot_holders()? {
        let state = tx
            .execution(&execution_id)?
            .map(|e| e.state)
            .unwrap_or(ExecutionState::Uncertain);
        holders.push(SlotHolder {
            execution_id,
            state,
            since,
        });
    }
    let limit = tx.slot_limit()?;
    let reservations = tx.reservations()?;
    let deferred = tx.deferrals()?;
    Ok(QueryResult::Found {
        value: CapacityView {
            used: holders.len(),
            limit,
            holders,
            reservations,
            deferred,
        },
        as_of: tx.last_seq()?,
        sources: vec![store_source()],
    })
}

/// The selector protocol's exclusion bound: a request's scoped issue
/// set is at most this many issues (`docs/selector-protocol.md` bounds
/// table). The preflight reports an over-bound scope as an explicit
/// failure — never a truncated owner set that would let the adapter
/// re-select owned work.
pub const MAX_PREFLIGHT_SCOPE_ISSUES: usize = 4096;

/// Why an intake preflight was refused before the store was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakePreflightError {
    /// The scoped set exceeds [`MAX_PREFLIGHT_SCOPE_ISSUES`].
    ScopeTooLarge {
        issues: usize,
        limit: usize,
    },
    Store(StoreError),
}

impl std::fmt::Display for IntakePreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScopeTooLarge { issues, limit } => write!(
                f,
                "intake preflight scope of {issues} issues exceeds the \
                 protocol bound of {limit}"
            ),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IntakePreflightError {}

impl From<StoreError> for IntakePreflightError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// The advisory preflight view for one bounded selector request
/// (#213): free pending positions under the active settings revision
/// plus the scoped source owners, all from one consistent snapshot.
/// Read-only — it reserves nothing, and a later authoritative submit is
/// still decided inside its own transaction, so capacity lost after
/// preflight yields the ordinary capacity rejection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakePreflight {
    /// The active record's content revision; `None` when no settings
    /// record exists — the effective limit is then the documented
    /// default shared with the profile loader.
    pub settings_revision: Option<String>,
    /// The pending-queue limit new submits are decided against.
    pub pending_queue_limit: u32,
    /// Works whose active Execution is `queued` in this snapshot.
    pub pending_count: u64,
    /// `pending_queue_limit - pending_count`, floored at zero.
    pub free_positions: u64,
    /// Scoped issues already owned by live Work (`queued`, `running`,
    /// or `paused` active Execution), one row per `(issue, Work)`.
    /// Distinct from `free_positions`: source claims and queue
    /// positions are separate facts and never conflated.
    pub source_owners: Vec<IntakeSourceOwner>,
}

/// Read-only intake preflight (#213). Runs inside the caller's
/// transaction — `transact_deferred` for the advisory path — so every
/// field comes from one consistent snapshot. The scope is the
/// selector's exclusion candidate set: scoped issue identities the
/// adapter must not re-select. An over-bound scope fails explicitly;
/// the owner set is never silently truncated.
pub fn intake_preflight(
    tx: &dyn ReadView,
    scope: &[IssueRef],
) -> Result<IntakePreflight, IntakePreflightError> {
    if scope.len() > MAX_PREFLIGHT_SCOPE_ISSUES {
        return Err(IntakePreflightError::ScopeTooLarge {
            issues: scope.len(),
            limit: MAX_PREFLIGHT_SCOPE_ISSUES,
        });
    }
    let settings = tx.intake_settings()?;
    let pending_queue_limit = settings
        .as_ref()
        .map(|s| s.pending_queue_limit)
        .unwrap_or(DEFAULT_PENDING_QUEUE_LIMIT);
    let pending_count = tx.pending_queue_count()?;
    let source_owners = tx.intake_source_owners(scope)?;
    Ok(IntakePreflight {
        settings_revision: settings.map(|s| s.settings_revision),
        pending_queue_limit,
        pending_count,
        free_positions: u64::from(pending_queue_limit).saturating_sub(pending_count),
        source_owners,
    })
}

/// Per-status candidate counts for one run — every status is counted
/// separately so a partial batch can never read as wholly accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeCandidateCounts {
    pub pending: usize,
    pub accepted: usize,
    pub replayed: usize,
    pub rejected: usize,
    pub capacity_deferred: usize,
    pub unknown: usize,
    pub interrupted: usize,
    pub total: usize,
}

/// One intake run as the run-list query reports it: identity and
/// provenance fields plus candidate outcome counts. Read-only — the
/// query changes no run state and holds no schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeRunView {
    pub run_id: String,
    pub trigger: IntakeTrigger,
    pub selector_id: String,
    pub selector_version: u32,
    pub policy_revision: u64,
    /// The intake-settings revision the run was admitted under;
    /// audit provenance only.
    pub settings_revision: Option<String>,
    pub state: IntakeRunState,
    /// Bounded diagnostic for `failed`/`interrupted`/`skipped`.
    pub detail_code: Option<String>,
    pub created_unix_seconds: i64,
    pub finished_unix_seconds: Option<i64>,
    pub recorded_seq: EventSequence,
    pub candidates: IntakeCandidateCounts,
}

/// The bounded run list plus the durable status facts a caller can
/// report without the adapter: the live run, the newest completed
/// run (last success), and the newest failed-or-interrupted run
/// (last failure). `next_due` and reload diagnostics are
/// scheduler-process state and are never fabricated here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeRunsView {
    /// Newest-first, at most `limit` entries.
    pub items: Vec<IntakeRunView>,
    /// Whether older runs exist beyond the returned bound.
    pub truncated: bool,
    /// The single `open`/`admitting` run, when one exists.
    pub active_run: Option<String>,
    /// Newest `completed` run — the last successful intake.
    pub last_completed_run: Option<String>,
    /// Newest `failed` or `interrupted` run — the last failed intake.
    pub last_failed_run: Option<String>,
}

/// One candidate row inside a run detail, in returned ordinal order:
/// proposal and replay identity, payload digest, recorded status and
/// bounded reason, and the decision links (`command_id`, `work_id`,
/// `committed_seq`) where the status carries them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeCandidateView {
    pub ordinal: u32,
    pub proposal_key: String,
    pub replay_key: String,
    /// Digest of the canonical candidate payload — the audit anchor
    /// that survives after the adapter is gone.
    pub payload_digest: String,
    pub status: IntakeCandidateStatus,
    pub status_reason: Option<String>,
    pub command_id: Option<CommandId>,
    pub work_id: Option<WorkId>,
    pub committed_seq: Option<EventSequence>,
}

/// One run plus its candidates — the `intake-detail` query result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeRunDetailView {
    pub run: IntakeRunView,
    /// Digest of the canonical request payload — audit anchor; the
    /// payload bytes themselves stay in the store.
    pub request_digest: String,
    /// The lease owner identity the run was created under.
    pub owner_pid: i32,
    pub candidates: Vec<IntakeCandidateView>,
}

/// Upper bound on one `intake-runs` page — the query contract's
/// output bound.
pub const MAX_INTAKE_RUNS_LIMIT: usize = 100;

fn intake_run_view(run: &crate::store::IntakeRun, counts: IntakeCandidateCounts) -> IntakeRunView {
    IntakeRunView {
        run_id: run.run_id.clone(),
        trigger: run.trigger,
        selector_id: run.selector_id.as_str().to_string(),
        selector_version: run.selector_version,
        policy_revision: run.policy_revision,
        settings_revision: run.settings_revision.clone(),
        state: run.state,
        detail_code: run.detail_code.clone(),
        created_unix_seconds: run.created_unix_seconds,
        finished_unix_seconds: run.finished_unix_seconds,
        recorded_seq: run.recorded_seq,
        candidates: counts,
    }
}

fn candidate_counts(
    statuses: impl Iterator<Item = IntakeCandidateStatus>,
) -> IntakeCandidateCounts {
    let mut counts = IntakeCandidateCounts::default();
    for status in statuses {
        counts.total += 1;
        match status {
            IntakeCandidateStatus::Pending => counts.pending += 1,
            IntakeCandidateStatus::Accepted => counts.accepted += 1,
            IntakeCandidateStatus::Replayed => counts.replayed += 1,
            IntakeCandidateStatus::Rejected => counts.rejected += 1,
            IntakeCandidateStatus::CapacityDeferred => counts.capacity_deferred += 1,
            IntakeCandidateStatus::Unknown => counts.unknown += 1,
            IntakeCandidateStatus::Interrupted => counts.interrupted += 1,
        }
    }
    counts
}

/// The bounded intake run list, newest-first. `limit` is clamped to
/// [`MAX_INTAKE_RUNS_LIMIT`]; the view reports whether older runs were
/// cut. Read-only: the query changes no run state and consults no
/// adapter.
pub fn intake_runs(
    tx: &dyn ReadView,
    limit: usize,
) -> Result<QueryResult<IntakeRunsView>, StoreError> {
    let limit = limit.min(MAX_INTAKE_RUNS_LIMIT);
    let runs = tx.intake_runs()?;
    let active = runs
        .iter()
        .find(|r| matches!(r.state, IntakeRunState::Open | IntakeRunState::Admitting))
        .map(|r| r.run_id.clone());
    let last_completed = runs
        .iter()
        .rev()
        .find(|r| r.state == IntakeRunState::Completed)
        .map(|r| r.run_id.clone());
    let last_failed = runs
        .iter()
        .rev()
        .find(|r| {
            matches!(
                r.state,
                IntakeRunState::Failed | IntakeRunState::Interrupted
            )
        })
        .map(|r| r.run_id.clone());
    let truncated = runs.len() > limit;
    let mut items = Vec::with_capacity(limit.min(runs.len()));
    for run in runs.iter().rev().take(limit) {
        let candidates = tx.intake_candidates(&run.run_id)?;
        let counts = candidate_counts(candidates.iter().map(|c| c.status));
        items.push(intake_run_view(run, counts));
    }
    Ok(QueryResult::Found {
        value: IntakeRunsView {
            items,
            truncated,
            active_run: active,
            last_completed_run: last_completed,
            last_failed_run: last_failed,
        },
        as_of: tx.last_seq()?,
        sources: vec![store_source()],
    })
}

/// One run plus its candidates in original ordinal order. `NotFound`
/// for an unknown run id — never an empty detail.
pub fn intake_run_detail(
    tx: &dyn ReadView,
    run_id: &str,
) -> Result<QueryResult<IntakeRunDetailView>, StoreError> {
    let Some(run) = tx.intake_run(run_id)? else {
        return Ok(QueryResult::NotFound);
    };
    let candidates = tx.intake_candidates(run_id)?;
    let counts = candidate_counts(candidates.iter().map(|c| c.status));
    let views = candidates
        .iter()
        .map(|c| IntakeCandidateView {
            ordinal: c.ordinal,
            proposal_key: c.proposal_key.clone(),
            replay_key: c.replay_key.clone(),
            payload_digest: c.payload_digest.clone(),
            status: c.status,
            status_reason: c.status_reason.clone(),
            command_id: c.command_id.clone(),
            work_id: c.work_id.clone(),
            committed_seq: c.committed_seq,
        })
        .collect();
    Ok(QueryResult::Found {
        value: IntakeRunDetailView {
            run: intake_run_view(&run, counts),
            request_digest: run.request_digest.clone(),
            owner_pid: run.owner_pid,
            candidates: views,
        },
        as_of: tx.last_seq()?,
        sources: vec![store_source()],
    })
}

/// Recover a lost response: look up the durable decision by command id or
/// by the caller's replay key (origin + actor + idempotency key).
pub fn command_result(
    tx: &dyn ReadView,
    lookup: &CommandResultLookup,
) -> Result<QueryResult<CommandDecision>, StoreError> {
    let found = match lookup {
        CommandResultLookup::Id { command_id } => tx.command_decision(command_id)?,
        CommandResultLookup::ReplayKey {
            origin,
            actor,
            idempotency_key,
        } => tx
            .stored_decision(origin, actor, idempotency_key)?
            .map(|(_, decision)| decision),
    };
    match found {
        Some(value) => Ok(QueryResult::Found {
            value,
            as_of: tx.last_seq()?,
            sources: vec![store_source()],
        }),
        None => Ok(QueryResult::NotFound),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResultLookup {
    Id {
        command_id: CommandId,
    },
    ReplayKey {
        origin: Origin,
        actor: Actor,
        idempotency_key: String,
    },
}

/// Outstanding (unreconciled) outbox effects for a Work — `pending` and
/// `unknown` intents are effects the terminal states must account for.
pub fn outstanding_actions(tx: &dyn ReadView, work_id: &WorkId) -> Result<Vec<String>, StoreError> {
    Ok(tx
        .action_intents()?
        .into_iter()
        .filter(|i| {
            &i.work_id == work_id
                && matches!(
                    i.status,
                    HostActionStatus::Pending | HostActionStatus::Unknown { .. }
                )
        })
        .map(|i| i.action_id)
        .collect())
}

/// Outstanding effects that still justify deferring a terminal
/// outcome: `unknown` intents always — the write may already have
/// landed and only reconciliation settles it — and `pending` intents
/// only while the Work's latest Execution still grants `host_write`.
/// Under a denied grant the write drain can never attempt a pending
/// intent, so deferring on it stalls the terminal outcome forever
/// with no effect to wait for (the intent record itself stays
/// `pending` — a denied permission is a deferral, not a verdict).
pub fn settlement_blocking_actions(
    tx: &dyn ReadView,
    work_id: &WorkId,
) -> Result<Vec<String>, StoreError> {
    let host_write = tx
        .executions_for_work(work_id)?
        .into_iter()
        .max_by_key(|execution| execution.ordinal)
        .is_some_and(|execution| execution.allowed_effects.host_write);
    Ok(tx
        .action_intents()?
        .into_iter()
        .filter(|i| {
            &i.work_id == work_id
                && match i.status {
                    HostActionStatus::Pending => host_write,
                    HostActionStatus::Unknown { .. } => true,
                    _ => false,
                }
        })
        .map(|i| i.action_id)
        .collect())
}

/// Whether recorded evidence covers a clean exit: every required artifact
/// of the Invocation was reported `Present` with a digest matching any
/// declared expectation.
pub fn artifacts_complete(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
    invocation: &Invocation,
) -> Result<bool, StoreError> {
    let obs = tx.observations_for_execution(execution_id)?;
    Ok(invocation.required_artifacts.iter().all(|req| {
        obs.iter().any(|o| {
            matches!(
                &o.fact,
                ObservationFact::Artifact {
                    invocation_id,
                    required_artifact,
                    state: ArtifactFact::Present { digest },
                } if invocation_id == &invocation.id
                    && required_artifact == &req.kind
                    && !digest.is_empty()
                    && req.expected_digest.as_ref().is_none_or(|e| e == digest)
            )
        })
    }))
}

/// Latest process fact recorded for an Invocation.
pub fn latest_process_fact(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
    invocation_id: &InvocationId,
) -> Result<Option<ProcessStateFact>, StoreError> {
    Ok(tx
        .observations_for_execution(execution_id)?
        .iter()
        .rev()
        .find_map(|o| match &o.fact {
            ObservationFact::ProcessState {
                invocation_id: iid,
                state,
                ..
            } if iid == invocation_id => Some(state.clone()),
            _ => None,
        }))
}

/// The recorded writer's liveness, per the supervision contract:
/// `running` (observed live), `stopped` (a supervisor-observed exit is
/// on record — the verified stop), `unknown` (the liveness read failed
/// or the record is inconsistent). An unknown writer never reads as
/// stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterState {
    Running,
    Stopped,
    Unknown,
}

/// The supervision record Core derives for one Execution: the earliest
/// deadline supervision must act on, the write-resource reservations it
/// holds, the capacity slot it occupies, and the recorded writer's
/// liveness. Derived from saved facts only — nothing here inspects a
/// live process, and an unknown or inconsistent record never reports a
/// `stopped` writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisionRecord {
    pub execution_id: ExecutionId,
    /// The earliest bound supervision must act on, as a wall-clock
    /// instant: the Execution's runtime deadline once dispatched, and
    /// the Work-wide automatic-time deadline once the chain has a first
    /// dispatch (shifted by recorded human-decision waiting, which the
    /// ceiling excludes). `None` while neither bound is anchored.
    pub deadline_unix_seconds: Option<i64>,
    /// Resource keys of the reservations this Execution holds.
    pub reservation_ids: Vec<String>,
    /// The claimed capacity slot's sequence while the Execution holds it.
    pub capacity_slot: Option<EventSequence>,
    /// The recorded writer's liveness; `None` while no writer exists.
    pub writer_state: Option<WriterState>,
}

/// The supervision record for one Execution, derived from saved facts.
/// `reservation_ids` and `capacity_slot` stay set for as long as the
/// record says they must: stop-path and uncertain Executions keep both
/// until verified stop and required-effect reconciliation release them.
pub fn supervision(
    tx: &dyn ReadView,
    execution_id: &ExecutionId,
) -> Result<Option<SupervisionRecord>, StoreError> {
    let Some(execution) = tx.execution(execution_id)? else {
        return Ok(None);
    };
    let mut deadline = execution
        .started_unix_seconds
        .map(|s| s + execution.budget.runtime_limit_seconds as i64);
    if let (Some(work), Some(usage)) = (
        tx.work(&execution.work_id)?,
        tx.work_usage(&execution.work_id)?,
    ) && let Some(first) = usage.first_dispatch_unix_seconds
    {
        // First dispatch + the limit + excluded human waiting = the
        // wall-clock instant the automatic-time ceiling fires.
        let work_deadline = first
            + work.policy.automatic_time_limit_seconds as i64
            + usage.human_wait_seconds_total as i64;
        deadline = Some(deadline.map_or(work_deadline, |d| d.min(work_deadline)));
    }
    let holder = ReservationHolder::Execution {
        execution_id: execution.id.clone(),
    };
    let mut reservation_ids: Vec<String> = tx
        .reservations()?
        .into_iter()
        .filter(|r| r.holder == holder)
        .map(|r| r.resource_key)
        .collect();
    reservation_ids.sort();
    let capacity_slot = tx
        .slot_holders()?
        .into_iter()
        .find(|(holder, _)| holder == &execution.id)
        .map(|(_, since)| since);
    Ok(Some(SupervisionRecord {
        execution_id: execution.id.clone(),
        deadline_unix_seconds: deadline,
        reservation_ids,
        capacity_slot,
        writer_state: writer_state_of(tx, &execution)?,
    }))
}

/// The writer liveness the record reports. An unresolved writer reads
/// `running` only on a live observation; a failed or missing read and an
/// inconsistent record (an exit recorded while the writer is unresolved,
/// or a resolution with no verified exit on record) read `unknown` —
/// never a stronger state. `Abandoned` is proven unable to write, hence
/// `stopped`.
fn writer_state_of(
    tx: &dyn ReadView,
    execution: &Execution,
) -> Result<Option<WriterState>, StoreError> {
    let invocations = tx.invocations_for_execution(&execution.id)?;
    if invocations.is_empty() {
        return Ok(None);
    }
    let mut saw_unknown = false;
    let mut saw_running = false;
    for invocation in &invocations {
        let latest = latest_process_fact(tx, &execution.id, &invocation.id)?;
        let state = match invocation.state {
            InvocationState::Done => match latest {
                Some(ProcessStateFact::Exited) => WriterState::Stopped,
                // Resolved without a verified exit on record.
                _ => WriterState::Unknown,
            },
            // Never launched or prevented before writing: proven unable
            // to write.
            InvocationState::Abandoned => WriterState::Stopped,
            // Prepared, Running, and Unknown are unresolved writers: only
            // a live observation reads running.
            _ => match latest {
                Some(ProcessStateFact::Running) => WriterState::Running,
                _ => WriterState::Unknown,
            },
        };
        match state {
            WriterState::Unknown => saw_unknown = true,
            WriterState::Running => saw_running = true,
            WriterState::Stopped => {}
        }
    }
    Ok(Some(if saw_unknown {
        WriterState::Unknown
    } else if saw_running {
        WriterState::Running
    } else {
        WriterState::Stopped
    }))
}

/// The recorded state of one typed host read subject: the complete read
/// carrying the greatest `subject_version`. A delayed delivery of an
/// older read, or a partial/unavailable one, is retained as history —
/// neither can move the recorded revision backward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRead {
    pub revision: ReadRevision,
    pub subject_version: u64,
    /// The actor the host reported for this read.
    pub actor: HostActorRef,
}

/// The current recorded read for `subject`, or `None` when no complete
/// read is on record. Ordering is by `subject_version` — the remote's
/// version of that subject at read time — with the first delivery at a
/// version holding the recorded revision when same-version reads
/// conflict.
pub fn current_read(
    tx: &dyn ReadView,
    subject: &ReadSubject,
) -> Result<Option<RecordedRead>, StoreError> {
    let mut current: Option<RecordedRead> = None;
    for o in tx.all_observations()? {
        let ObservationFact::HostRead {
            subject: s,
            revision,
            subject_version,
            actor,
            completeness,
        } = &o.fact
        else {
            continue;
        };
        if s != subject || !completeness.is_complete() {
            continue;
        }
        let Some(revision) = revision else { continue };
        if current
            .as_ref()
            .is_none_or(|c| *subject_version > c.subject_version)
        {
            current = Some(RecordedRead {
                revision: revision.clone(),
                subject_version: *subject_version,
                actor: actor.clone(),
            });
        }
    }
    Ok(current)
}

/// Whether recorded evidence satisfies a decision bound to `required`:
/// only the subject's current complete read qualifies, and only when it
/// carries exactly that revision — a decision bound to revision r does
/// not fire while the recorded read is still at r−1, and a stale
/// delivery arriving afterward cannot revoke it.
pub fn read_satisfies(
    tx: &dyn ReadView,
    subject: &ReadSubject,
    required: &ReadRevision,
) -> Result<bool, StoreError> {
    Ok(current_read(tx, subject)?.is_some_and(|r| r.revision == *required))
}
