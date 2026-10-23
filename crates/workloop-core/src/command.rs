use serde::{Deserialize, Serialize};

use crate::domain::{
    Actor, EventSequence, ExecutionId, GitHubIssueRef, Origin, WorkId, WriteResource,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandId(pub String);

pub type PrHeadSha = String;
pub type WorkVersion = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    pub actor: Actor,
    pub origin: Origin,
    pub idempotency_key: String,
    pub body: CommandBody,
}

/// Commands on existing Work carry the version the caller saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandBody {
    Submit {
        intent: String,
        issue_refs: Vec<GitHubIssueRef>,
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
        expected_pr_head: PrHeadSha,
        decisions: Vec<FindingDecision>,
    },
    ReleaseJudgment {
        work_id: WorkId,
        expected_work_version: WorkVersion,
        expected_pr_head: PrHeadSha,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingDecision {
    pub finding_id: String,
    pub choice: FindingChoice,
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
    StalePrHead,
    Ineligible,
    OwnershipUncertain,
    CapacityExhausted,
    SourceUnavailable,
    IdempotencyConflict,
    ResourceConflict,
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
}

/// Digest of the canonical command payload, stored with the decision so a
/// key replay with a changed body is `idempotency_conflict`. FNV-1a over a
/// deterministic serialization — change detection only, not a secret.
pub fn payload_digest(origin: &Origin, actor: &Actor, key: &str, body: &CommandBody) -> String {
    let canonical =
        serde_json::to_string(&(origin, actor, key, body)).expect("command body serializes");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}
