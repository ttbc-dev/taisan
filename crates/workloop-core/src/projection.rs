//! Read-only derived state over the store. Queries never refresh GitHub or
//! start a worker; every view is shaped from saved Work/Execution/Invocation
//! records and Observations — there is no separate card state.

use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId};
use crate::domain::*;
use crate::observation::{
    ArtifactFact, FindingRef, Observation, ObservationFact, Outcome, ProcessStateFact,
};
use crate::store::{Deferral, PrActionStatus, ReadView, StoreError};

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
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrRef {
    pub repository: String,
    pub number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDetailView {
    pub work_id: WorkId,
    pub version: u64,
    pub current_execution_id: Option<ExecutionId>,
    pub executions: Vec<ExecutionView>,
    pub current_pr: Option<(PrRef, Option<String>)>,
    /// Findings bound to the current head; older revisions stay in evidence.
    /// This is the exact set a `judge_findings` Command must decide on —
    /// the same derivation `work` validates against.
    pub findings: Vec<FindingRef>,
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
        .filter(|observation| observation.subject_execution_id.is_none())
        .count();
    let occurrence = format!(
        "{}:{}:{}",
        execution.id.0,
        execution.enqueued_seq,
        execution_evidence + work_evidence
    );
    let mut out = Vec::new();
    let mut push = |reason: &str, anchor: String, threshold: Option<u64>, action: &str| {
        out.push(AttentionView {
            attention_id: format!("att:{}:{}:{}:{}", work_id.0, reason, anchor, occurrence),
            work_id: work_id.clone(),
            reason: reason.to_string(),
            evidence: execution
                .outcome
                .as_ref()
                .map(|o| o.evidence_refs.clone())
                .unwrap_or_default(),
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
                );
            }
        }
        _ => {}
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

pub fn work_detail(
    tx: &dyn ReadView,
    work_id: &WorkId,
    now_unix_seconds: i64,
) -> Result<QueryResult<WorkDetailView>, StoreError> {
    let Some(work) = tx.work(work_id)? else {
        return Ok(QueryResult::NotFound);
    };
    let executions = tx
        .executions_for_work(work_id)?
        .into_iter()
        .map(|e| {
            let ids = tx
                .invocations_for_execution(&e.id)
                .map(|v| v.into_iter().map(|i| i.id).collect())
                .unwrap_or_default();
            ExecutionView {
                ownership: if e.state == ExecutionState::Uncertain {
                    "uncertain".into()
                } else {
                    "confirmed".into()
                },
                execution_id: e.id,
                state: e.state,
                invocation_ids: ids,
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
    let current_pr = observations
        .iter()
        .enumerate()
        .filter_map(|(index, obs)| match &obs.fact {
            ObservationFact::PrHead {
                repository,
                number,
                head_sha,
            } => Some((
                (obs.observed_unix_seconds, index),
                (
                    PrRef {
                        repository: repository.clone(),
                        number: *number,
                    },
                    Some(head_sha.clone()),
                ),
            )),
            _ => None,
        })
        .max_by_key(|(order, _)| *order)
        .map(|(_, pr)| pr);
    let findings = current_pr
        .as_ref()
        .and_then(|(_, head)| head.as_ref())
        .map(|head| judgeable_findings(&observations, head))
        .unwrap_or_default();
    let mut sources = vec![store_source()];
    if let Some((_, Some(head))) = &current_pr {
        let has_review_for_head = observations.iter().any(|obs| {
            matches!(
                &obs.fact,
                ObservationFact::PrReview { head_sha, .. } if head_sha == head
            )
        });
        sources.push(SourceStatus {
            source: "github:review".into(),
            availability: if has_review_for_head {
                Availability::Known
            } else {
                Availability::Unknown
            },
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
            ObservationFact::PrActionResult {
                action_id, result, ..
            } => evidence.push(format!("pr_action:{action_id}:{result:?}")),
            _ => {}
        }
    }

    Ok(QueryResult::Found {
        value: WorkDetailView {
            work_id: work.id.clone(),
            version: work.version,
            current_execution_id: current.map(|e| e.id),
            executions,
            current_pr,
            findings,
            outcome,
            attention,
            evidence,
        },
        as_of: tx.last_seq()?,
        sources,
    })
}

/// The findings a `judge_findings` Command may decide on for `head_sha`:
/// the union of itemized findings from every review recorded against that
/// head. A verdict-only review contributes one synthesized finding keyed
/// by reviewer and head, so the displayed set and the set `work`
/// validates are always the same derivation. Findings for other
/// revisions stay in evidence history and never appear here.
pub fn judgeable_findings(observations: &[Observation], head_sha: &str) -> Vec<FindingRef> {
    let mut findings = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |finding: FindingRef| {
        if seen.insert(finding.finding_id.clone()) {
            findings.push(finding);
        }
    };
    for obs in observations {
        let ObservationFact::PrReview {
            head_sha: observed_head,
            reviewer,
            verdict,
            findings: itemized,
            ..
        } = &obs.fact
        else {
            continue;
        };
        if observed_head != head_sha {
            continue;
        }
        if itemized.is_empty() {
            push(FindingRef {
                finding_id: format!("verdict:{reviewer}:{observed_head}"),
                reviewed_revision: observed_head.clone(),
                summary: format!("review by {reviewer} at {observed_head}: {verdict:?}"),
                location: None,
                evidence: vec![],
            });
        } else {
            for finding in itemized {
                let mut finding = finding.clone();
                // The Observation's head is authoritative for the revision
                // this finding was reported against.
                finding.reviewed_revision = observed_head.clone();
                push(finding);
            }
        }
    }
    findings
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
                && matches!(i.status, PrActionStatus::Pending | PrActionStatus::Unknown)
        })
        .map(|i| i.action_id)
        .collect())
}

/// Whether recorded evidence covers a clean exit: every required artifact
/// of the Invocation was reported `Present`.
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
                    state: ArtifactFact::Present,
                } if invocation_id == &invocation.id && required_artifact == req
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
