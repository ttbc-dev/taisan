//! Stage contracts: what a stage *means*, independent of the executor
//! that runs it. An executor cell — local, remote, or a future provider —
//! selects where a stage runs, never what it may skip. The rules below
//! were distilled from two production failure modes in the source
//! workflow (tracked here as issue #21): a converge stage that could
//! report success while its fix commits never reached the PR, and a
//! reviewer axis silently dropped for bot-authored PRs. The incident
//! analysis lives in `docs/source-workflow-audit.md`; the regression
//! scenarios are pinned by `tests/stage_contract.rs`.

use serde::{Deserialize, Serialize};

use crate::domain::{ExecutionId, LanePolicy, WorkId};
use crate::identity::{CommitRevision, EvidenceRef};
use crate::observation::{Observation, ObservationFact};

/// The canonical stage pipeline: code → verify → invariant-check? →
/// publication → review? → fix → reassessment. The two `?` lanes are the
/// only optional stages; an Invocation config enables or disables them,
/// and a disabled lane is recorded explicitly instead of disappearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Code,
    Verify,
    InvariantCheck,
    Publication,
    Review,
    Fix,
    Reassessment,
}

impl Stage {
    /// The only legal order. A stage may open only after every earlier
    /// stage has closed with `Complete` or an explicit `Disabled` record.
    pub const ORDER: [Self; 7] = [
        Self::Code,
        Self::Verify,
        Self::InvariantCheck,
        Self::Publication,
        Self::Review,
        Self::Fix,
        Self::Reassessment,
    ];

    /// Only these lanes are controlled by Invocation config today.
    pub fn is_optional(self) -> bool {
        matches!(self, Self::InvariantCheck | Self::Review)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Verify => "verify",
            Self::InvariantCheck => "invariant_check",
            Self::Publication => "publication",
            Self::Review => "review",
            Self::Fix => "fix",
            Self::Reassessment => "reassessment",
        }
    }
}

/// The reason recorded for a config-disabled lane. A lane is never
/// silently skipped; the reason is part of the durable stage record.
pub const LANE_DISABLED_REASON: &str = "lane disabled by Invocation config";

/// The status of one durable stage record. `Entered` proves the loop
/// entered the stage; only `Complete` can satisfy the next stage's
/// precondition. `Partial`/`Failed` retain the attempt but close nothing,
/// and `Disabled` closes an optional lane without pretending it passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StageStatus {
    Entered,
    Complete,
    Partial { reason: String },
    Failed { reason: String },
    Disabled { reason: String },
}

impl StageStatus {
    /// A closed lane is either truly complete or explicitly disabled.
    /// Entered/partial/failed records are durable history, never closure.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Complete | Self::Disabled { .. })
    }

    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One durable per-stage evidence record. The loop persists an `Entered`
/// record when it enters a stage and a terminal record when it observes
/// the outcome. Every record carries the same four fields, so history can
/// be replayed without inferring a stage that left no evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageEvidence {
    pub stage: Stage,
    pub status: StageStatus,
    pub revision: CommitRevision,
    pub facts: Vec<EvidenceRef>,
}

impl StageEvidence {
    pub fn entered(stage: Stage, revision: CommitRevision) -> Self {
        Self {
            stage,
            status: StageStatus::Entered,
            revision,
            facts: Vec::new(),
        }
    }

    /// A complete record must carry at least one typed fact. Claiming
    /// completion without evidence is refused before it is persisted.
    pub fn complete(
        stage: Stage,
        revision: CommitRevision,
        facts: Vec<EvidenceRef>,
    ) -> Result<Self, StageError> {
        if facts.is_empty() {
            return Err(StageError::MissingFacts { stage });
        }
        Ok(Self {
            stage,
            status: StageStatus::Complete,
            revision,
            facts,
        })
    }

    pub fn partial(
        stage: Stage,
        revision: CommitRevision,
        facts: Vec<EvidenceRef>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            status: StageStatus::Partial {
                reason: reason.into(),
            },
            revision,
            facts,
        }
    }

    pub fn failed(
        stage: Stage,
        revision: CommitRevision,
        facts: Vec<EvidenceRef>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            status: StageStatus::Failed {
                reason: reason.into(),
            },
            revision,
            facts,
        }
    }

    pub fn disabled(stage: Stage, revision: CommitRevision, reason: impl Into<String>) -> Self {
        Self {
            stage,
            status: StageStatus::Disabled {
                reason: reason.into(),
            },
            revision,
            facts: Vec::new(),
        }
    }

    /// Persist this record as an append-only Observation through a Core
    /// transaction (`Effect::InsertObservation`) — never through
    /// `handle_observation`, whose ingest path refuses delivered stage
    /// records. Stage evidence uses the existing durable observation log
    /// rather than a mutable stage pointer: a crash can lose an
    /// unrecorded attempt, never rewrite or erase evidence already
    /// committed.
    pub fn into_observation(
        self,
        work_id: WorkId,
        execution_id: ExecutionId,
        observed_unix_seconds: i64,
    ) -> Observation {
        Observation {
            delivery: None,
            subject_work_id: Some(work_id),
            subject_execution_id: Some(execution_id),
            fact: ObservationFact::StageEvidence { evidence: self },
            observed_unix_seconds,
        }
    }
}

/// Why Core refused a stage record. Each variant names the stage that
/// would have been opened or closed without the required evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageError {
    MissingFacts {
        stage: Stage,
    },
    LaneNotOpen {
        stage: Stage,
        blocked_by: Stage,
    },
    LaneClosed {
        stage: Stage,
    },
    LaneDisabled {
        stage: Stage,
    },
    LaneEnabled {
        stage: Stage,
    },
    /// The record's revision does not equal the recorded current head —
    /// evidence against a stale head or a foreign repository can never
    /// satisfy a lane.
    StaleRevision {
        stage: Stage,
    },
    /// The policy offered at pipeline construction differs from the
    /// Work's frozen `spec.payload.lanes`; the pipeline refuses it
    /// instead of silently adopting a drifted configuration.
    LanePolicyMismatch,
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFacts { stage } => {
                write!(f, "{} stage cannot complete without evidence", stage.name())
            }
            Self::LaneNotOpen { stage, blocked_by } => write!(
                f,
                "{} lane cannot open before {} closes",
                stage.name(),
                blocked_by.name()
            ),
            Self::LaneClosed { stage } => {
                write!(f, "{} lane is already closed", stage.name())
            }
            Self::LaneDisabled { stage } => {
                write!(f, "{} lane is disabled and cannot pass", stage.name())
            }
            Self::LaneEnabled { stage } => {
                write!(f, "{} lane is enabled and cannot be disabled", stage.name())
            }
            Self::StaleRevision { stage } => write!(
                f,
                "{} evidence does not bind to the recorded current head",
                stage.name()
            ),
            Self::LanePolicyMismatch => write!(
                f,
                "the lane policy differs from the Work's frozen spec lanes"
            ),
        }
    }
}

impl std::error::Error for StageError {}

/// The next move the loop may take over the recorded pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStage {
    Open(Stage),
    /// The named stage recorded a non-closing outcome; only more evidence
    /// for this same stage can advance the pipeline.
    Blocked {
        stage: Stage,
        status: StageStatus,
    },
    /// An optional lane is disabled by Invocation config and still owes its
    /// explicit `disabled{reason}` record before later stages can open.
    Disabled {
        stage: Stage,
        reason: String,
    },
    Complete,
}

/// Whether an observed head was adopted into the pipeline, already
/// matched, or was stale history that changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadObservation {
    Recorded,
    Unchanged,
    Stale,
}

/// The Core-side stage machine. It consumes only supplied facts and
/// recorded evidence: the loop persists records, the supervisor and host
/// adapter supply the facts, and no code path infers a transition from a
/// missing record. An old-head observation is retained in history and can
/// never change the recorded current head or satisfy a stage.
#[derive(Debug, Clone)]
pub struct StagePipeline {
    lanes: LanePolicy,
    records: Vec<StageEvidence>,
    current_head: Option<CommitRevision>,
    head_history: Vec<CommitRevision>,
    stale_heads: Vec<CommitRevision>,
}

impl StagePipeline {
    pub fn new(lanes: LanePolicy) -> Self {
        Self {
            lanes,
            records: Vec::new(),
            current_head: None,
            head_history: Vec::new(),
            stale_heads: Vec::new(),
        }
    }

    /// Bind the pipeline to the lane policy frozen into the Work's
    /// `spec.payload.lanes` at request creation. "Invocation config" is
    /// that snapshot, so a policy that differs from the frozen value —
    /// a live installation config that drifted after the snapshot was
    /// stamped — is refused, never silently adopted.
    pub fn bind(frozen: &LanePolicy, policy: &LanePolicy) -> Result<Self, StageError> {
        if frozen != policy {
            return Err(StageError::LanePolicyMismatch);
        }
        Ok(Self::new(frozen.clone()))
    }

    /// The replay form of `bind`: the pipeline is judged under the
    /// frozen spec lanes, so rebuilding under a differing policy is
    /// refused rather than reinterpreted.
    pub fn bind_from_observations(
        frozen: &LanePolicy,
        policy: &LanePolicy,
        observations: &[Observation],
    ) -> Result<Self, StageError> {
        if frozen != policy {
            return Err(StageError::LanePolicyMismatch);
        }
        Ok(Self::from_observations(frozen.clone(), observations))
    }

    /// Rebuild the stage view from durable observations. Recorded head
    /// observations replay through `observe_head` and each recorded
    /// evidence re-runs the same guards it was first accepted under —
    /// so a stale-head observation can never promote itself to the
    /// recorded head after a restart. A durably recorded evidence also
    /// pin-points the head it bound to: when it differs from the
    /// replayed head, an authoritative move (`record_current_head`)
    /// must have preceded it, and that move is restored before the
    /// record replays. Invalid records are ignored rather than silently
    /// repaired; the pipeline never promotes evidence that did not
    /// satisfy the same order guard when it was first recorded.
    pub fn from_observations(lanes: LanePolicy, observations: &[Observation]) -> Self {
        let mut pipeline = Self::new(lanes);
        for observation in observations {
            match &observation.fact {
                ObservationFact::ChangeHead { head, .. } => {
                    pipeline.observe_head(head);
                }
                ObservationFact::StageEvidence { evidence } => {
                    if pipeline
                        .current_head()
                        .is_some_and(|current| *current != evidence.revision)
                    {
                        pipeline.record_current_head(evidence.revision.clone());
                    }
                    let _ = pipeline.record(evidence.clone());
                }
                _ => {}
            }
        }
        pipeline
    }

    pub fn lanes(&self) -> &LanePolicy {
        &self.lanes
    }

    pub fn records(&self) -> &[StageEvidence] {
        &self.records
    }

    pub fn current_head(&self) -> Option<&CommitRevision> {
        self.current_head.as_ref()
    }

    pub fn head_history(&self) -> &[CommitRevision] {
        &self.head_history
    }

    pub fn stale_heads(&self) -> &[CommitRevision] {
        &self.stale_heads
    }

    pub fn latest(&self, stage: Stage) -> Option<&StageEvidence> {
        self.records
            .iter()
            .rev()
            .find(|record| record.stage == stage)
    }

    pub fn lane_enabled(&self, stage: Stage) -> bool {
        match stage {
            Stage::InvariantCheck => self.lanes.invariant_check,
            Stage::Review => self.lanes.code_review,
            _ => true,
        }
    }

    /// Adopt a head only when none is recorded. Any later differing head
    /// is an observation, not an authoritative transition: it stays in
    /// history and cannot move the recorded head or any stage state.
    pub fn observe_head(&mut self, observed: &CommitRevision) -> HeadObservation {
        match &self.current_head {
            None => {
                self.current_head = Some(observed.clone());
                self.head_history.push(observed.clone());
                HeadObservation::Recorded
            }
            Some(current) if current == observed => HeadObservation::Unchanged,
            Some(_) => {
                self.head_history.push(observed.clone());
                self.stale_heads.push(observed.clone());
                HeadObservation::Stale
            }
        }
    }

    /// Record an authoritative head transition decided by Core (for
    /// example after adopting a verified publication receipt). This is the
    /// only path that moves the current head after one is recorded.
    pub fn record_current_head(&mut self, head: CommitRevision) {
        if self.current_head.as_ref() != Some(&head) {
            self.current_head = Some(head.clone());
            self.head_history.push(head);
        }
    }

    /// The earliest stage that is not closed. Because the scan is ordered,
    /// a later stage is never offered while an earlier enabled lane is
    /// incomplete; a disabled optional lane is offered as an explicit
    /// record to write before publication can open.
    pub fn next_stage(&self) -> NextStage {
        for stage in Stage::ORDER {
            match self.latest(stage) {
                Some(record) if record.status.is_closed() => {}
                Some(record) => {
                    return NextStage::Blocked {
                        stage,
                        status: record.status.clone(),
                    };
                }
                None if stage.is_optional() && !self.lane_enabled(stage) => {
                    return NextStage::Disabled {
                        stage,
                        reason: LANE_DISABLED_REASON.to_string(),
                    };
                }
                None => return NextStage::Open(stage),
            }
        }
        NextStage::Complete
    }

    /// Whether this lane may open for work: its config lane is enabled
    /// and every earlier stage has closed. A disabled lane never opens —
    /// it is owed an explicit `disabled{reason}` record (`next_stage`
    /// reports it). This is the direct order predicate a transition
    /// consumes; it never looks at unrecorded attempts.
    pub fn lane_opens(&self, stage: Stage) -> bool {
        if !self.lane_enabled(stage) {
            return false;
        }
        for earlier in Stage::ORDER {
            if earlier == stage {
                return true;
            }
            let closed = self
                .latest(earlier)
                .is_some_and(|record| record.status.is_closed());
            if !closed {
                return false;
            }
        }
        false
    }

    /// Append one durable stage record. The record binds to the recorded
    /// current head — evidence against a stale head or a foreign
    /// repository is refused before it can satisfy a lane — and the
    /// order and lane guards run before the record is accepted, so
    /// neither a later lane nor a contradictory config record can enter
    /// history as if it were valid.
    pub fn record(&mut self, evidence: StageEvidence) -> Result<(), StageError> {
        let stage = evidence.stage;
        if self.current_head.is_none() {
            // The first record also records the head it binds to — the
            // same first-observation rule `observe_head` applies.
            self.observe_head(&evidence.revision);
        }
        if self.current_head.as_ref() != Some(&evidence.revision) {
            return Err(StageError::StaleRevision { stage });
        }
        let enabled = self.lane_enabled(stage);
        let disabled = matches!(evidence.status, StageStatus::Disabled { .. });
        if !enabled && !disabled {
            return Err(StageError::LaneDisabled { stage });
        }
        if enabled && disabled {
            return Err(StageError::LaneEnabled { stage });
        }
        if matches!(evidence.status, StageStatus::Complete) && evidence.facts.is_empty() {
            return Err(StageError::MissingFacts { stage });
        }
        match self.next_stage() {
            NextStage::Open(open) if open == stage => {}
            NextStage::Blocked { stage: blocked, .. } if blocked == stage => {}
            NextStage::Disabled {
                stage: disabled, ..
            } if disabled == stage => {}
            NextStage::Open(blocked)
            | NextStage::Blocked { stage: blocked, .. }
            | NextStage::Disabled { stage: blocked, .. } => {
                return Err(StageError::LaneNotOpen {
                    stage,
                    blocked_by: blocked,
                });
            }
            NextStage::Complete => return Err(StageError::LaneClosed { stage }),
        }
        self.records.push(evidence);
        Ok(())
    }

    /// Write the explicit `disabled{reason}` records the enabled/disabled
    /// configuration owes, in canonical order, and stop as soon as the
    /// next lane needs real evidence. Returns the stages recorded.
    pub fn materialize_disabled_lanes(
        &mut self,
        revision: &CommitRevision,
    ) -> Result<Vec<Stage>, StageError> {
        let mut recorded = Vec::new();
        while let NextStage::Disabled { stage, reason } = self.next_stage() {
            self.record(StageEvidence::disabled(stage, revision.clone(), reason))?;
            recorded.push(stage);
        }
        Ok(recorded)
    }
}

/// The structured result of a `converge` stage run. The executor returns
/// this record instead of a free-text verdict; the coordinator verifies
/// `pushed_sha` against the live PR head before adopting the result.
/// Publication is part of the stage's postcondition, exactly as
/// `propose` already models it for ship: bounded fix commits published to
/// the PR head, marked replies posted, draft→ready advanced exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvergeRecord {
    /// The PR head at stage entry.
    pub head_before: String,
    /// The PR head the executor claims to have produced. It may exist
    /// only in the executor's workspace — a claim, not publication.
    pub head_after: String,
    /// The head the executor pushed toward the PR, if any. `None` means
    /// nothing was published: a fix claim without publication evidence.
    pub pushed_sha: Option<String>,
    /// Marked reply comment IDs the run posted on the PR threads.
    pub replies_posted: Vec<String>,
    /// Review thread IDs the run claims to have addressed.
    pub threads_addressed: Vec<String>,
}

/// How a `converge` record classifies against the live PR head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConvergeClassification {
    /// Publication verified: the pushed head is the produced head
    /// (`head_after`) and is the live PR head or an ancestor of it. The
    /// only classification that may count as `review-fixed`.
    Fixed { pushed_sha: String },
    /// Nothing was produced or claimed: the head did not move and
    /// nothing was pushed.
    NothingToFix,
    /// A fix was claimed but publication cannot be shown or does not
    /// verify against the live head. A failure that preserves the whole
    /// record as evidence — never a success.
    UnpublishedFailure {
        live_head: String,
        record: ConvergeRecord,
    },
}

impl ConvergeRecord {
    /// Classify against the live PR head. `pushed_is_ancestor_of_live`
    /// must come from the coordinator's own verification (GitHub compare
    /// or an ancestry read) of `self.pushed_sha`, never from the
    /// executor's claim. A result counts as fixed only when the produced
    /// head was the one published (`pushed_sha == head_after`) and that
    /// pushed head verifies against the live PR head — equal to it or an
    /// ancestor of it. Any other fix claim, including a push that
    /// carried a different head than the fix produced, is an
    /// `UnpublishedFailure` carrying the full record as evidence.
    pub fn classify(
        &self,
        live_head: &str,
        pushed_is_ancestor_of_live: bool,
    ) -> ConvergeClassification {
        // Nothing produced: the head did not move. A no-op push of that
        // unchanged head is still nothing to fix, not a published fix.
        if self.head_after == self.head_before {
            return match self.pushed_sha.as_deref() {
                None => ConvergeClassification::NothingToFix,
                Some(pushed) if pushed == self.head_after => ConvergeClassification::NothingToFix,
                Some(_) => ConvergeClassification::UnpublishedFailure {
                    live_head: live_head.to_string(),
                    record: self.clone(),
                },
            };
        }
        // A produced fix must be the head that was actually published.
        // Reporting a pushed SHA that is not the produced head means the
        // produced commits never reached the PR — even when that SHA
        // verifies against the live head (observed failure: the push
        // carried the old head, the fix stayed in the workspace).
        match self.pushed_sha.as_deref() {
            Some(pushed)
                if pushed == self.head_after
                    && (pushed == live_head || pushed_is_ancestor_of_live) =>
            {
                ConvergeClassification::Fixed {
                    pushed_sha: pushed.to_string(),
                }
            }
            // A new head claimed with no pushed SHA, a mismatched SHA,
            // or a push that does not verify against the live head: a
            // failure that preserves the whole record, never a success.
            // An ancestry flag about a push that does not carry the
            // produced head cannot manufacture one.
            _ => ConvergeClassification::UnpublishedFailure {
                live_head: live_head.to_string(),
                record: self.clone(),
            },
        }
    }
}

/// Where a stage runs. Choosing an executor never changes a stage's
/// postconditions — only its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    Local,
    Remote,
}

/// The reviewer axis of the executor × reviewer matrix. Orthogonal to
/// `Executor`: an executor cell must never silently narrow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    Devin,
    Shadow,
    Both,
}

/// Why a configuration-time matrix check refused a cell. Refusal is the
/// only allowed outcome for an unsupported combination — never a silent
/// degradation of the reviewer axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatrixConfigError {
    UnsupportedExecutor {
        value: String,
    },
    UnsupportedReviewer {
        value: String,
    },
    UnsupportedCombination {
        executor: Executor,
        reviewer: Reviewer,
    },
}

/// The matrix cells configuration validation accepts today. Support is
/// declared here: a cell absent from this table is refused at
/// configuration-validation time rather than run with a degraded
/// reviewer axis. All six executor × reviewer cells are supported
/// because the adapter event-filter contract exempts lane-produced
/// bot-authored PRs and heads — a filter that drops them starves the
/// lane's own review loop (the observed silent-reviewer failure).
const SUPPORTED_MATRIX_CELLS: [(Executor, Reviewer); 6] = [
    (Executor::Local, Reviewer::Devin),
    (Executor::Local, Reviewer::Shadow),
    (Executor::Local, Reviewer::Both),
    (Executor::Remote, Reviewer::Devin),
    (Executor::Remote, Reviewer::Shadow),
    (Executor::Remote, Reviewer::Both),
];

/// The configuration-validation chokepoint for the executor × reviewer
/// matrix. It parses both axes and refuses any combination not declared
/// in `SUPPORTED_MATRIX_CELLS`. On success it returns the reviewer axis
/// exactly as configured: no executor cell may rewrite or default it,
/// and an unknown value is a configuration error rather than a fallback
/// (a remote executor must never silently downgrade shadow review).
pub fn validate_review_matrix(
    executor: &str,
    reviewer: &str,
) -> Result<(Executor, Reviewer), MatrixConfigError> {
    let executor = match executor {
        "local" => Executor::Local,
        "remote" => Executor::Remote,
        value => {
            return Err(MatrixConfigError::UnsupportedExecutor {
                value: value.to_string(),
            });
        }
    };
    let reviewer = match reviewer {
        "devin" => Reviewer::Devin,
        "shadow" => Reviewer::Shadow,
        "both" => Reviewer::Both,
        value => {
            return Err(MatrixConfigError::UnsupportedReviewer {
                value: value.to_string(),
            });
        }
    };
    if SUPPORTED_MATRIX_CELLS.contains(&(executor, reviewer)) {
        Ok((executor, reviewer))
    } else {
        Err(MatrixConfigError::UnsupportedCombination { executor, reviewer })
    }
}
