//! Read-only derived state over the store. Queries never refresh the host
//! or start a worker; every view is shaped from saved Work/Execution/
//! Invocation records and Observations — there is no separate card state.

use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId};
use crate::domain::*;
use crate::identity::{ChangeRequestRef, CommitRevision, HostActorRef};
use crate::observation::{
    ArtifactFact, HostActionStatus, ObservationFact, Outcome, ProcessStateFact, ReadRevision,
    ReadSubject,
};
use crate::store::{Deferral, ReadView, StoreError};

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

/// One finding the detail pane displays: the occurrence id a
/// `JudgeFindings` decision names, plus the source-supplied key for
/// display. Identity is the occurrence id, never the display text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FindingView {
    pub occurrence_id: String,
    pub source_key: String,
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
                .map(|o| {
                    o.evidence_refs
                        .iter()
                        .map(|r| r.id.clone())
                        .collect::<Vec<_>>()
                })
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
