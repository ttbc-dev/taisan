use serde::{Deserialize, Serialize};

use crate::domain::{EventSequence, InvocationId, WorkId};

/// A fact reported by a worker, a process probe, GitHub, or an external
/// system. Facts are inputs to `work`'s judgment, never state themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Deduplication identity for transports that assign one (webhooks,
    /// worker reports). Uniqueness is enforced by the store.
    pub source_event_id: Option<String>,
    /// The Work or Execution this fact is about; None keeps it a system fact.
    pub subject_work_id: Option<WorkId>,
    pub subject_execution_id: Option<crate::domain::ExecutionId>,
    pub fact: ObservationFact,
    pub observed_unix_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationFact {
    ProcessState {
        invocation_id: InvocationId,
        process_identity: String,
        state: ProcessStateFact,
    },
    Artifact {
        invocation_id: InvocationId,
        required_artifact: String,
        state: ArtifactFact,
    },
    PrHead {
        repository: String,
        number: u64,
        head_sha: String,
    },
    PrChecks {
        repository: String,
        head_sha: String,
        conclusion: CheckConclusion,
    },
    PrReview {
        repository: String,
        head_sha: String,
        reviewer: String,
        verdict: ReviewVerdict,
    },
    PrActionResult {
        action_id: String,
        action_kind: String,
        result: ActionResult,
    },
    IssueComment {
        issue: u64,
        comment_id: String,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStateFact {
    Running,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFact {
    Present,
    Missing,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    ChangesRequested,
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionResult {
    Applied,
    Failed,
    Unknown,
}

/// An evidence-backed assessment. `succeeded` requires verified current-head
/// PR evidence and no unresolved writer, gate, or outgoing GitHub action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub state: crate::domain::ExecutionState,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub evidence_refs: Vec<String>,
    pub recorded_seq: EventSequence,
}

/// Derived human-decision surface. A version identifies the actionable
/// condition; acknowledgement is scoped to that version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attention {
    pub work_id: WorkId,
    /// Bumps when the actionable condition changes — new evidence, an
    /// escalation, or a different required decision. Never timer-driven.
    pub version: AttentionVersion,
    pub reason: AttentionReason,
    /// The threshold that produced this Attention (e.g. uncertain duration).
    pub threshold: Option<String>,
    pub next_actor: String,
    pub next_action: String,
    pub since_seq: EventSequence,
}

pub type AttentionVersion = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    PausedJudgment,
    ConfirmedFailure,
    UncertainBeyondThreshold,
}
