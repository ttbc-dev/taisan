use serde::{Deserialize, Serialize};

use crate::domain::{
    Actor, EventSequence, ExecutionId, Origin, WorkId, WriteResource, canonical_json, sha256_hex,
};
use crate::identity::{CommitRevision, SourceSnapshot};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandId(pub String);

pub type WorkVersion = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    pub actor: Actor,
    pub origin: Origin,
    pub idempotency_key: String,
    pub body: CommandBody,
}

/// Commands on existing Work carry the version the caller saw.
///
/// `Submit` receives an already-resolved submission: source issues carry
/// the authoritative `source_revision`/`content_digest` the boundary
/// resolver observed. Core never resolves display references itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandBody {
    Submit {
        intent: String,
        sources: Vec<SourceSnapshot>,
        constraints: Vec<String>,
        verification_argv: Vec<Vec<String>>,
    },
    ClaimResources {
        resources: Vec<WriteResource>,
        ttl_seconds: u64,
    },
    ReleaseResources {
        resources: Vec<WriteResource>,
    },
    Retry {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        expected_execution_id: ExecutionId,
    },
    Resume {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        execution_id: ExecutionId,
    },
    Cancel {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        execution_id: Option<ExecutionId>,
    },
    Acknowledge {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        attention_id: String,
    },
    JudgeFindings {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        expected_head: CommitRevision,
        decisions: Vec<FindingDecision>,
    },
    ReleaseJudgment {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        expected_head: CommitRevision,
    },
}

impl CommandBody {
    /// The Work a Command targets, if it names one.
    pub fn work_id(&self) -> Option<&WorkId> {
        match self {
            Self::Submit { .. } | Self::ClaimResources { .. } | Self::ReleaseResources { .. } => {
                None
            }
            Self::Retry { work_id, .. }
            | Self::Resume { work_id, .. }
            | Self::Cancel { work_id, .. }
            | Self::Acknowledge { work_id, .. }
            | Self::JudgeFindings { work_id, .. }
            | Self::ReleaseJudgment { work_id, .. } => Some(work_id),
        }
    }

    /// The caller's observed Work version, required for existing-Work Commands.
    pub fn expected_work_version(&self) -> Option<WorkVersion> {
        match self {
            Self::Submit { .. } | Self::ClaimResources { .. } | Self::ReleaseResources { .. } => {
                None
            }
            Self::Retry {
                expected_work_version,
                ..
            }
            | Self::Resume {
                expected_work_version,
                ..
            }
            | Self::Cancel {
                expected_work_version,
                ..
            }
            | Self::Acknowledge {
                expected_work_version,
                ..
            }
            | Self::JudgeFindings {
                expected_work_version,
                ..
            }
            | Self::ReleaseJudgment {
                expected_work_version,
                ..
            } => Some(*expected_work_version),
        }
    }
}

/// A per-occurrence disposition over one sealed review round. `decisions`
/// submitted to `JudgeFindings` must cover the round's occurrence set
/// exactly — omissions and unknown occurrences are `invalid_request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingDecision {
    /// The install-scoped occurrence id from the sealed round.
    pub occurrence_id: String,
    pub choice: FindingChoice,
    /// Required for `dismiss`/`accept`; recorded verbatim.
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingChoice {
    Fix,
    Dismiss,
    Accept,
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    InvalidRequest,
    Unauthorized,
    StaleWorkVersion,
    /// The caller's expected head is not the current sealed target head.
    StaleHead,
    Ineligible,
    OwnershipUncertain,
    CapacityExhausted,
    SourceUnavailable,
    IdempotencyConflict,
    ResourceConflict,
    /// The Work's required capabilities are not declared as supported by
    /// the installation, or a capability report denies them.
    UnsupportedCapability,
}

/// What an accepted Command changed; recorded with the decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandEffect {
    WorkCreated { execution_id: ExecutionId },
    ExecutionCreated { execution_id: ExecutionId },
    ResourcesClaimed { expires_at: Option<i64> },
    ResourcesReleased,
    StateChanged,
}

/// The durable, auditable result of a Command — replayed for a duplicate key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandDecision {
    Accepted {
        command_id: CommandId,
        work_id: Option<WorkId>,
        work_version: Option<WorkVersion>,
        effect: CommandEffect,
        committed_at: EventSequence,
    },
    Rejected {
        command_id: CommandId,
        reason: RejectionCode,
        current_work_id: Option<WorkId>,
        current_version: Option<WorkVersion>,
        evidence: Vec<String>,
        committed_at: EventSequence,
    },
}

impl CommandDecision {
    pub fn command_id(&self) -> &CommandId {
        match self {
            Self::Accepted { command_id, .. } | Self::Rejected { command_id, .. } => command_id,
        }
    }

    /// The sequence the decision's effects were anchored at. For an
    /// accepted resume/release this is the `enqueued_seq` the command
    /// assigned to the requeued Execution — the boundary between wait
    /// generations.
    pub fn committed_at(&self) -> EventSequence {
        match self {
            Self::Accepted { committed_at, .. } | Self::Rejected { committed_at, .. } => {
                *committed_at
            }
        }
    }
}

/// The canonical payload bytes stored with the decision so replay
/// compares bytes, not a lossy form.
pub fn canonical_payload(origin: &Origin, actor: &Actor, key: &str, body: &CommandBody) -> Vec<u8> {
    let value = serde_json::json!({
        "origin": origin,
        "actor": actor,
        "idempotency_key": key,
        "body": body,
    });
    canonical_json(&value)
}

/// Digest of the canonical command payload — SHA-256 over tagged canonical
/// bytes. A key replay with a changed body is `idempotency_conflict`.
pub fn payload_digest(origin: &Origin, actor: &Actor, key: &str, body: &CommandBody) -> String {
    sha256_hex("command-v1", &canonical_payload(origin, actor, key, body))
}
