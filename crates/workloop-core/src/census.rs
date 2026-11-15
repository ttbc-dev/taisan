//! Bounded read-only anomaly census: the structured "stuck lane"
//! inventory that replaces a manual Phase-1 sweep. One invocation reads
//! the durable v0.1 store records and reports every detector's findings
//! as capped, self-describing sections — suitable as input to lane
//! diagnosis, never as a mutation plan.
//!
//! Read-only by construction: the census runs inside a deferred
//! transaction, performs no writes, launches nothing, and refreshes no
//! external source. Two ownership classes are report-only by contract —
//! `uncertain` and `foreign` orphaned findings must never reach a
//! write-capable path; only `proven` findings are actionable, and even
//! they are evidence for an operator decision, not an order.
//!
//! Detector → record mapping (issue #150 spec clarifications):
//!
//! | detector | durable records |
//! |---|---|
//! | `judgment_waits` | `review_rounds` sealed without `round_judgments` coverage of `set_digest`; `paused` Executions whose `paused_judgment` Attention has no `attention_acks` row; undecodable store records |
//! | `released_wait_blocked` | nonterminal `paused`/`waiting` Executions with a pause/waiting reason, Attention acknowledged, and no accepted `resume`/`retry` Command recorded |
//! | `orphaned_findings` | `finding_occurrences` on uncovered rounds with no unresolved writer Invocation; ownership `proven`/`foreign`/`uncertain` |
//! | `parked_selector_starvation` | `deferrals` aged past the event-sequence threshold plus `capacity_slots`/`resource_reservations` context |
//! | `stale_spec_halts` | `not_applicable_v0.1` — no spec-halt record exists in v0.1; reported explicitly, never dropped |
//! | `source_gate_holds` | `works.source_gate` ∈ {changed, needs_check}; Executions `waiting` with `waiting_reason = human_judgment` |
//!
//! Orphaned-finding ownership resolves the durable chain
//! occurrence → `review_rounds` → `works` → `works.binding`. `proven`
//! requires the bound repository host to be this installation's host
//! and at least one recorded Invocation; `foreign` is a binding whose
//! host differs from this installation's; `uncertain` covers an
//! unreadable source (`completeness` other than `complete`), a Work
//! with no recorded invocations, or missing binding evidence.
//! `round.source` is an unvalidated adapter string (a reviewer login),
//! not a binding crosswalk, so reviewer identity is not separately
//! decidable in v0.1 — a foreign-bound Work is the foreign signal.

use serde::Serialize;

use crate::domain::{
    EventSequence, Execution, ExecutionId, ExecutionState, FindingOccurrence, ReviewRound,
    RoundJudgment, WaitingReason, Work, WorkId,
};
use crate::identity::{HostInstanceId, RepositoryRef, SourceGate};
use crate::store::{CommandRecord, ReadView, StoreError};

/// Census bounds. Every detector is individually capped; `truncated`
/// on a section reports the cap honestly rather than silently
/// dropping the tail.
#[derive(Debug, Clone)]
pub struct CensusBounds {
    /// Maximum items reported per detector section.
    pub item_limit: usize,
    /// A deferral is `stale` once `last_seq - since_seq` exceeds this
    /// event-sequence distance — queue-order time, not wall clock.
    pub deferral_age_events: i64,
}

impl Default for CensusBounds {
    fn default() -> Self {
        Self {
            item_limit: 100,
            deferral_age_events: 256,
        }
    }
}

/// One detector's report: always present, explicitly bounded.
#[derive(Debug, Clone, Serialize)]
pub struct DetectorSection<T: Serialize> {
    /// `false` only where v0.1 has no record basis; the section is
    /// still emitted so nothing is silently dropped.
    pub applicable: bool,
    /// Why the detector is not applicable, or a section-level caveat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Anomaly items, capped at `limit`.
    pub items: Vec<T>,
    /// Items found before the cap.
    pub total: usize,
    /// `items.len() < total`: the cap bit.
    pub truncated: bool,
    /// The per-detector item cap applied.
    pub limit: usize,
    /// Detector-specific context (capacity state, gate census, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

fn section<T: Serialize>(
    applicable: bool,
    note: Option<String>,
    mut all: Vec<T>,
    limit: usize,
    context: Option<serde_json::Value>,
) -> DetectorSection<T> {
    let total = all.len();
    all.truncate(limit);
    DetectorSection {
        applicable,
        note,
        items: all,
        total,
        truncated: total > limit,
        limit,
        context,
    }
}

/// The full anomaly inventory. Every detector section is present on
/// every run; an empty `items` with `applicable: true` is a clean bill
/// for that detector, not missing coverage.
#[derive(Debug, Clone, Serialize)]
pub struct AnomalyCensus {
    /// The store sequence the census read — the snapshot `as_of`.
    pub as_of: EventSequence,
    /// Open judgment waits: uncovered sealed rounds, unacknowledged
    /// paused-judgment Attention, and unreadable records.
    pub judgment_waits: DetectorSection<JudgmentWaitItem>,
    /// Released waits whose parent Execution never saw its
    /// resume/retry.
    pub released_wait_blocked: DetectorSection<ReleasedWaitBlock>,
    /// Findings no judgment can ever reach, by ownership class.
    pub orphaned_findings: DetectorSection<OrphanedFinding>,
    /// Aged dispatch deferrals with the capacity context that explains
    /// them.
    pub parked_selector_starvation: DetectorSection<ParkedDeferral>,
    /// Stale spec-halts: `not_applicable_v0.1`, reported explicitly.
    pub stale_spec_halts: DetectorSection<serde_json::Value>,
    /// Source-gate holds and human-judgment waits (spec:ambiguous /
    /// needs-human sweeps).
    pub source_gate_holds: DetectorSection<SourceGateItem>,
}

/// One `judgment_waits` finding.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JudgmentWaitItem {
    /// A sealed review round no `round_judgments` row covers.
    UnjudgedRound {
        work_id: WorkId,
        round_id: String,
        completeness: String,
        /// `None` when the occurrence read itself was unreadable.
        occurrence_count: Option<usize>,
        set_digest: String,
        generation: u64,
        seq: EventSequence,
        /// A newer round exists for the same (work, target): this round
        /// can no longer receive a judgment — permanently uncovered,
        /// not awaiting a decision.
        superseded: bool,
    },
    /// A `paused` Execution whose `paused_judgment` Attention has no
    /// recorded acknowledgement — the wait is open and unseen.
    UnackedPausedJudgment {
        work_id: WorkId,
        execution_id: ExecutionId,
        pause_reason: Option<String>,
        paused_since_unix_seconds: Option<i64>,
    },
    /// A durable record the census could not decode; reported instead
    /// of guessing clean state. `record` names the table-level read
    /// (`executions`, `review_rounds`, `finding_occurrences`,
    /// `round_judgments`, `attention_acks`, `invocations`).
    UnreadableRecord {
        work_id: Option<WorkId>,
        record: String,
        detail: String,
    },
    /// A commands row whose canonical payload could not be decoded.
    UnreadableCommand { command_id: String, detail: String },
}

/// A recorded Command reduced to release-path evidence.
#[derive(Debug, Clone, Serialize)]
pub struct CommandRef {
    pub command_id: String,
    pub kind: String,
    pub accepted: bool,
    pub seq: EventSequence,
}

/// One `released_wait_blocked` finding: the wait was acknowledged but
/// the parent Execution still sits in `paused`/`waiting`.
#[derive(Debug, Clone, Serialize)]
pub struct ReleasedWaitBlock {
    pub work_id: WorkId,
    pub execution_id: ExecutionId,
    pub state: ExecutionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_since_unix_seconds: Option<i64>,
    pub enqueued_seq: EventSequence,
    /// Attention ids recorded as acknowledged for this Execution's
    /// wait (structural match — see [`ack_matches`]).
    pub acked_attention_ids: Vec<String>,
    /// Release-path Commands seen for this Work/Execution — accepted
    /// and rejected alike — so the operator sees what was tried.
    pub release_commands: Vec<CommandRef>,
}

/// Who a stranded finding belongs to. `Uncertain` and `Foreign` are
/// report-only: no write-capable path may consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingOwnership {
    /// Bound to a Work owned by this installation with recorded
    /// invocations — the only actionable class.
    Proven,
    /// Bound to a different host binding (another installation's
    /// host identity).
    Foreign,
    /// Round/Work binding missing, the source unreadable, or no
    /// recorded invocations prove this installation ran the Work.
    Uncertain,
}

/// One `orphaned_findings` item.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanedFinding {
    pub occurrence_id: String,
    pub round_id: String,
    pub work_id: WorkId,
    pub source_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    pub ownership: FindingOwnership,
    /// Why the ownership class was assigned.
    pub ownership_reason: String,
    /// The round is superseded by a newer round for the same target.
    pub superseded: bool,
}

/// One `parked_selector_starvation` item: a deferral that has aged
/// past the event-sequence threshold.
#[derive(Debug, Clone, Serialize)]
pub struct ParkedDeferral {
    pub execution_id: ExecutionId,
    /// The deferred Execution's Work, when its row reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<WorkId>,
    pub reason: String,
    pub diagnostic: String,
    pub since_seq: EventSequence,
    /// `last_seq - since_seq` at census time.
    pub age_events: i64,
    /// `age_events` exceeds the census threshold.
    pub stale: bool,
    /// The deferred Execution's current state, when its row reads —
    /// a deferral on a terminal or missing Execution is itself notable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_state: Option<ExecutionState>,
}

/// One `source_gate_holds` item.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceGateItem {
    /// `works.source_gate` ∈ {changed, needs_check}: dispatch is held
    /// until the recorded prerequisite is satisfied.
    SourceGateHold {
        work_id: WorkId,
        source_gate: SourceGate,
        work_version: u64,
        repository: RepositoryRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        execution_state: Option<ExecutionState>,
    },
    /// An Execution waiting on `human_judgment`.
    HumanJudgmentWait {
        work_id: WorkId,
        execution_id: ExecutionId,
        state: ExecutionState,
    },
}

/// The per-Work evidence one census pass collects. Every sub-read is
/// optional: `None` marks an unreadable record, which is reported in
/// `judgment_waits` rather than failing the whole scan.
struct WorkScan {
    work: Work,
    executions: Option<Vec<Execution>>,
    rounds: Option<Vec<RoundScan>>,
    acks: Option<Vec<String>>,
    /// Read lazily, and only for Works with an uncovered round
    /// carrying occurrences: whether any Invocation is an unresolved
    /// writer and whether any Invocation was ever recorded. `None`
    /// means not needed or unreadable.
    invocations: Option<InvocationScan>,
}

struct RoundScan {
    round: ReviewRound,
    occurrences: Option<Vec<FindingOccurrence>>,
    judgment: Option<RoundJudgment>,
    judgment_unreadable: bool,
}

struct InvocationScan {
    any_recorded: bool,
    any_unresolved: bool,
}

/// Attention-id structural match: `att:{work}:{reason}:{anchor}:{exec}:{seq}:{count}`.
/// The anchor is free text and may contain `:`, so the match is prefix +
/// execution segment, tolerant of anchor and evidence-count drift — an
/// ack recorded against an earlier version of the same wait still
/// counts as the wait having been seen.
fn ack_matches(ack: &str, work_id: &WorkId, reason: &str, execution: &Execution) -> bool {
    ack.starts_with(&format!("att:{}:{}:", work_id.0, reason))
        && ack.contains(&format!(":{}:", execution.id.0))
}

/// Any recorded ack naming this Execution under any Attention reason —
/// the "the wait was seen" signal for the released-wait detector.
fn any_ack_matches(ack: &str, work_id: &WorkId, execution: &Execution) -> bool {
    ack.starts_with(&format!("att:{}:", work_id.0))
        && ack.contains(&format!(":{}:", execution.id.0))
}

/// Judgment coverage: a `round_judgments` row exists for the round and
/// binds its exact `set_digest`. An unreadable judgment read cannot
/// prove coverage, so the round counts as uncovered — the unreadable
/// record is reported alongside.
fn round_covered(scan: &RoundScan) -> bool {
    !scan.judgment_unreadable
        && scan
            .judgment
            .as_ref()
            .is_some_and(|j| j.set_digest == scan.round.set_digest)
}

/// The newest sealed round per (work, target): only it can still
/// receive a judgment (`current_round_for` reads the latest per
/// target). Older rounds for the same target are `superseded`.
fn superseded(rounds: &[RoundScan], round: &ReviewRound) -> bool {
    rounds
        .iter()
        .filter(|r| r.round.target == round.target)
        .map(|r| r.round.seq)
        .max()
        .is_some_and(|newest| newest > round.seq)
}

/// Classify one orphaned finding's ownership from the durable chain.
/// Order matters: a foreign host binding is definitive; an unreadable
/// source or missing invocation proof can only ever be `uncertain`.
fn classify_ownership(
    work: &Work,
    round: &ReviewRound,
    invocations: &InvocationScan,
    installation_host: &HostInstanceId,
) -> (FindingOwnership, String) {
    if work.binding.repository.host != *installation_host {
        return (
            FindingOwnership::Foreign,
            format!(
                "work {} is bound to host {}, not this installation ({})",
                work.id.0, work.binding.repository.host, installation_host
            ),
        );
    }
    if round.completeness != "complete" {
        return (
            FindingOwnership::Uncertain,
            format!(
                "round {} source is unreadable (completeness: {})",
                round.id, round.completeness
            ),
        );
    }
    if !invocations.any_recorded {
        return (
            FindingOwnership::Uncertain,
            format!(
                "work {} has no recorded invocations; ownership unproven",
                work.id.0
            ),
        );
    }
    (
        FindingOwnership::Proven,
        "bound to this installation's host with recorded invocations".into(),
    )
}

/// Run the bounded read-only census inside the caller's transaction.
/// `installation_host` is this installation's host identity — the
/// ownership boundary for `proven`/`foreign` classification.
pub fn anomaly_census(
    tx: &dyn ReadView,
    bounds: &CensusBounds,
    installation_host: &HostInstanceId,
) -> Result<AnomalyCensus, StoreError> {
    let limit = bounds.item_limit;
    let as_of = tx.last_seq()?;
    let commands = tx.commands()?;

    // --- Per-Work scan -------------------------------------------------
    // One pass collects every record family the detectors need, so a
    // corrupt row is reported once and the rest of the census still
    // reads cleanly.
    let mut scans = Vec::new();
    let mut unreadable: Vec<JudgmentWaitItem> = Vec::new();
    for work in tx.works()? {
        let work_id = work.id.clone();
        let executions = match tx.executions_for_work(&work_id) {
            Ok(v) => Some(v),
            Err(e) => {
                unreadable.push(JudgmentWaitItem::UnreadableRecord {
                    work_id: Some(work_id.clone()),
                    record: "executions".into(),
                    detail: e.to_string(),
                });
                None
            }
        };
        let rounds = match tx.rounds_for_work(&work_id) {
            Ok(rounds) => Some(
                rounds
                    .into_iter()
                    .map(|round| {
                        let round_id = round.id.clone();
                        let occurrences = match tx.occurrences(&round_id) {
                            Ok(v) => Some(v),
                            Err(e) => {
                                unreadable.push(JudgmentWaitItem::UnreadableRecord {
                                    work_id: Some(work_id.clone()),
                                    record: "finding_occurrences".into(),
                                    detail: format!("round {round_id}: {e}"),
                                });
                                None
                            }
                        };
                        let (judgment, judgment_unreadable) = match tx.judgment_for_round(&round_id)
                        {
                            Ok(j) => (j, false),
                            Err(e) => {
                                unreadable.push(JudgmentWaitItem::UnreadableRecord {
                                    work_id: Some(work_id.clone()),
                                    record: "round_judgments".into(),
                                    detail: format!("round {round_id}: {e}"),
                                });
                                (None, true)
                            }
                        };
                        RoundScan {
                            round,
                            occurrences,
                            judgment,
                            judgment_unreadable,
                        }
                    })
                    .collect(),
            ),
            Err(e) => {
                unreadable.push(JudgmentWaitItem::UnreadableRecord {
                    work_id: Some(work_id.clone()),
                    record: "review_rounds".into(),
                    detail: e.to_string(),
                });
                None
            }
        };
        let acks = match tx.attention_acks(&work_id) {
            Ok(v) => Some(v),
            Err(e) => {
                unreadable.push(JudgmentWaitItem::UnreadableRecord {
                    work_id: Some(work_id.clone()),
                    record: "attention_acks".into(),
                    detail: e.to_string(),
                });
                None
            }
        };
        scans.push(WorkScan {
            work,
            executions,
            rounds,
            acks,
            invocations: None,
        });
    }

    // Commands whose payload could not be decoded are unreadable
    // records too — they can never prove a release happened.
    for c in &commands {
        if c.kind.is_none() {
            unreadable.push(JudgmentWaitItem::UnreadableCommand {
                command_id: c.command_id.0.clone(),
                detail: "canonical command payload undecodable".into(),
            });
        }
    }

    // --- Invocation scans (lazy, all-or-nothing per Work) --------------
    // Only Works with an uncovered round carrying occurrences can
    // produce orphans; a partially read invocation set cannot prove
    // "no non-terminal invocation", so one bad read disqualifies the
    // Work's orphan items and is reported as unreadable.
    for scan in &mut scans {
        let work_id = scan.work.id.clone();
        let needs = scan.rounds.as_ref().is_some_and(|rounds| {
            rounds.iter().any(|rs| {
                !round_covered(rs) && rs.occurrences.as_ref().is_some_and(|o| !o.is_empty())
            })
        });
        if !needs {
            continue;
        }
        let Some(executions) = &scan.executions else {
            continue; // executions unreadable: orphanhood unverifiable
        };
        let mut invocations = Vec::new();
        let mut ok = true;
        for e in executions {
            match tx.invocations_for_execution(&e.id) {
                Ok(v) => invocations.extend(v),
                Err(err) => {
                    unreadable.push(JudgmentWaitItem::UnreadableRecord {
                        work_id: Some(work_id.clone()),
                        record: "invocations".into(),
                        detail: format!("execution {}: {err}", e.id.0),
                    });
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            scan.invocations = Some(InvocationScan {
                any_recorded: !invocations.is_empty(),
                any_unresolved: invocations.iter().any(|i| i.state.is_unresolved_writer()),
            });
        }
    }

    // --- Detector 1: judgment waits ------------------------------------
    // Unreadable records lead the section: "we could not read this" is
    // the strongest anomaly signal and must survive the item cap.
    let mut judgment_items: Vec<JudgmentWaitItem> = unreadable;
    for scan in &scans {
        let work_id = &scan.work.id;
        // Sealed rounds without complete judgment coverage.
        if let Some(rounds) = &scan.rounds {
            for rs in rounds {
                if round_covered(rs) {
                    continue;
                }
                judgment_items.push(JudgmentWaitItem::UnjudgedRound {
                    work_id: work_id.clone(),
                    round_id: rs.round.id.clone(),
                    completeness: rs.round.completeness.clone(),
                    occurrence_count: rs.occurrences.as_ref().map(|o| o.len()),
                    set_digest: rs.round.set_digest.clone(),
                    generation: rs.round.generation,
                    seq: rs.round.seq,
                    superseded: superseded(rounds, &rs.round),
                });
            }
        }
        // Paused Executions whose paused_judgment Attention is
        // unacknowledged. An unreadable acks read cannot prove the
        // wait was seen, so the work contributes no item — the
        // unreadable record is the report.
        if let (Some(executions), Some(acks)) = (&scan.executions, &scan.acks) {
            for e in executions
                .iter()
                .filter(|e| !e.state.is_terminal() && e.state == ExecutionState::Paused)
            {
                let acked = acks
                    .iter()
                    .any(|a| ack_matches(a, work_id, "paused_judgment", e));
                if !acked {
                    judgment_items.push(JudgmentWaitItem::UnackedPausedJudgment {
                        work_id: work_id.clone(),
                        execution_id: e.id.clone(),
                        pause_reason: e.pause_reason.clone(),
                        paused_since_unix_seconds: e.paused_since_unix_seconds,
                    });
                }
            }
        }
    }
    let judgment_waits = section(
        true,
        None,
        judgment_items,
        limit,
        Some(serde_json::json!({
            "note": "unreadable records lead the section; detector items follow in store order",
        })),
    );

    // --- Detector 2: released-wait parent blocked ----------------------
    let mut released_items = Vec::new();
    for scan in &scans {
        let work_id = &scan.work.id;
        let (Some(executions), Some(acks)) = (&scan.executions, &scan.acks) else {
            continue;
        };
        let work_commands: Vec<&CommandRecord> = commands
            .iter()
            .filter(|c| c.work_id.as_ref() == Some(work_id))
            .collect();
        for e in executions.iter().filter(|e| {
            !e.state.is_terminal()
                && matches!(e.state, ExecutionState::Paused | ExecutionState::Waiting)
                && match e.state {
                    ExecutionState::Paused => e.pause_reason.is_some(),
                    ExecutionState::Waiting => e.waiting_reason.is_some(),
                    _ => false,
                }
        }) {
            let acked: Vec<String> = acks
                .iter()
                .filter(|a| any_ack_matches(a, work_id, e))
                .cloned()
                .collect();
            if acked.is_empty() {
                continue;
            }
            // A recorded release is an ACCEPTED resume/retry Command
            // targeting this Execution. Rejected attempts stay visible
            // in `release_commands` but cannot prove the wait was
            // released — the parent is still blocked.
            let release_commands: Vec<CommandRef> = work_commands
                .iter()
                .filter(|c| {
                    c.execution_id.as_ref() == Some(&e.id)
                        || (c.execution_id.is_none()
                            && matches!(
                                c.kind.as_deref(),
                                Some("release_judgment" | "judge_findings")
                            ))
                })
                .map(|c| CommandRef {
                    command_id: c.command_id.0.clone(),
                    kind: c.kind.clone().unwrap_or_else(|| "unreadable".into()),
                    accepted: c.accepted,
                    seq: c.seq,
                })
                .collect();
            let released = release_commands
                .iter()
                .any(|c| c.accepted && matches!(c.kind.as_str(), "resume" | "retry"));
            if released {
                continue;
            }
            released_items.push(ReleasedWaitBlock {
                work_id: work_id.clone(),
                execution_id: e.id.clone(),
                state: e.state.clone(),
                waiting_reason: e.waiting_reason.clone(),
                pause_reason: e.pause_reason.clone(),
                paused_since_unix_seconds: e.paused_since_unix_seconds,
                enqueued_seq: e.enqueued_seq,
                acked_attention_ids: acked.into_iter().take(8).collect(),
                release_commands: release_commands.into_iter().rev().take(8).collect(),
            });
        }
    }
    let released_wait_blocked = section(true, None, released_items, limit, None);

    // --- Detector 3: orphaned findings ---------------------------------
    let mut orphan_items = Vec::new();
    for scan in &scans {
        let work_id = &scan.work.id;
        let (Some(rounds), Some(inv)) = (&scan.rounds, &scan.invocations) else {
            continue;
        };
        if inv.any_unresolved {
            continue; // a live writer may still disposition the findings
        }
        for rs in rounds {
            if round_covered(rs) {
                continue;
            }
            let Some(occurrences) = &rs.occurrences else {
                continue; // occurrence read unreadable; already reported
            };
            if occurrences.is_empty() {
                continue;
            }
            let (ownership, reason) =
                classify_ownership(&scan.work, &rs.round, inv, installation_host);
            let is_superseded = superseded(rounds, &rs.round);
            for occ in occurrences {
                orphan_items.push(OrphanedFinding {
                    occurrence_id: occ.id.clone(),
                    round_id: rs.round.id.clone(),
                    work_id: work_id.clone(),
                    source_key: occ.source_key.clone(),
                    severity: occ.severity.clone(),
                    ownership,
                    ownership_reason: reason.clone(),
                    superseded: is_superseded,
                });
            }
        }
    }
    let orphaned_findings = section(true, None, orphan_items, limit, None);

    // --- Detector 4: parked selector starvation ------------------------
    let deferrals = tx.deferrals()?;
    let slot_holders = tx.slot_holders()?;
    let slot_limit = tx.slot_limit()?;
    let reservations = tx.reservations()?;
    // execution_id → (work_id, state) for deferral context.
    let mut exec_index: std::collections::HashMap<&str, (&WorkId, &ExecutionState)> =
        std::collections::HashMap::new();
    for scan in &scans {
        if let Some(executions) = &scan.executions {
            for e in executions {
                exec_index.insert(e.id.0.as_str(), (&scan.work.id, &e.state));
            }
        }
    }
    let mut deferral_items: Vec<ParkedDeferral> = deferrals
        .iter()
        .map(|d| {
            let age = as_of - d.since_seq;
            let (work_id, state) = exec_index
                .get(d.execution_id.0.as_str())
                .map(|(w, s)| (Some((*w).clone()), Some((*s).clone())))
                .unwrap_or((None, None));
            ParkedDeferral {
                execution_id: d.execution_id.clone(),
                work_id,
                reason: d.reason.clone(),
                diagnostic: d.diagnostic.clone(),
                since_seq: d.since_seq,
                age_events: age,
                stale: age > bounds.deferral_age_events,
                execution_state: state,
            }
        })
        .filter(|d| d.stale)
        .collect();
    deferral_items.sort_by(|a, b| a.execution_id.0.cmp(&b.execution_id.0));
    let capacity_context = serde_json::json!({
        "slots_used": slot_holders.len(),
        "slot_limit": slot_limit,
        "free_slots": slot_limit.saturating_sub(slot_holders.len() as u32),
        "reservations_held": reservations.len(),
        "external_reservations": reservations
            .iter()
            .filter(|r| matches!(r.holder, crate::domain::ReservationHolder::External { .. }))
            .count(),
        "deferrals_total": deferrals.len(),
        "deferral_age_threshold_events": bounds.deferral_age_events,
    });
    let parked_selector_starvation =
        section(true, None, deferral_items, limit, Some(capacity_context));

    // --- Detector 5: stale spec-halts ----------------------------------
    // v0.1 has no spec-halt card concept; the detector is reported
    // explicitly as not applicable rather than silently dropped.
    let stale_spec_halts = section::<serde_json::Value>(
        false,
        Some(
            "not_applicable_v0.1: the v0.1 store has no spec-halt record; \
             stale spec halts cannot be enumerated from durable state"
                .into(),
        ),
        vec![],
        limit,
        Some(serde_json::json!({
            "status": "not_applicable_v0.1",
        })),
    );

    // --- Detector 6: source-gate holds / human-judgment waits ----------
    let mut gate_items = Vec::new();
    for scan in &scans {
        let work = &scan.work;
        let exec_state = scan.executions.as_ref().and_then(|ex| {
            ex.iter()
                .find(|e| !e.state.is_terminal())
                .map(|e| e.state.clone())
        });
        if matches!(
            work.source_gate,
            SourceGate::Changed | SourceGate::NeedsCheck
        ) {
            gate_items.push(SourceGateItem::SourceGateHold {
                work_id: work.id.clone(),
                source_gate: work.source_gate,
                work_version: work.version,
                repository: work.binding.repository.clone(),
                execution_state: exec_state.clone(),
            });
        }
        if let Some(executions) = &scan.executions {
            for e in executions.iter().filter(|e| {
                !e.state.is_terminal()
                    && e.state == ExecutionState::Waiting
                    && e.waiting_reason == Some(WaitingReason::HumanJudgment)
            }) {
                gate_items.push(SourceGateItem::HumanJudgmentWait {
                    work_id: work.id.clone(),
                    execution_id: e.id.clone(),
                    state: e.state.clone(),
                });
            }
        }
    }
    let source_gate_holds = section(true, None, gate_items, limit, None);

    Ok(AnomalyCensus {
        as_of,
        judgment_waits,
        released_wait_blocked,
        orphaned_findings,
        parked_selector_starvation,
        stale_spec_halts,
        source_gate_holds,
    })
}
