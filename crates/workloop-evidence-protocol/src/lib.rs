//! Immutable evidence wire contract. No store, CLI, or runtime exporter dependencies.
mod tokens;
mod validate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub use tokens::*;
pub use validate::*;

pub const PROTOCOL: &str = "rui-evidence/1";
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RECORDS: usize = 500;

macro_rules! vocabulary {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ProtocolVersion {
    #[serde(rename = "rui-evidence/1")]
    V1,
}
vocabulary!(RecordKind { Entity, Link });
vocabulary!(EntityType {
    Work,
    Execution,
    Invocation,
    ReviewRound,
    FindingOccurrence,
    RepairAttempt,
    Verification,
    Outcome,
    Requirement,
    Invariant
});
vocabulary!(State {
    Pending,
    Running,
    Passed,
    Failed,
    Cancelled,
    Unknown
});
vocabulary!(Completeness {
    Complete,
    Partial,
    Unknown
});
vocabulary!(Disposition {
    Open,
    Accepted,
    Dismissed,
    Resolved,
    Unknown
});
vocabulary!(Provenance {
    Dispatch,
    Annotation
});
vocabulary!(OutcomeScope {
    Execution,
    Review,
    Publication
});
vocabulary!(VerificationScope {
    Targeted,
    Workspace,
    Integration,
    Unknown
});
vocabulary!(Availability {
    Available,
    Unavailable,
    RetentionExpired
});
vocabulary!(GapCode {
    NotRecorded,
    NotYetObserved,
    RetentionExpired,
    SourceUnavailable
});
vocabulary!(EvidenceClass {
    Body,
    Relation,
    Artifact,
    History,
    Intent
});
vocabulary!(Relation {
    WorkHasExecution,
    ExecutionHasInvocation,
    ExecutionHasRound,
    RoundHasFinding,
    WorkHasRequirement,
    WorkHasInvariant,
    ExecutionHasRepair,
    FindingViolatesInvariant,
    RepairAddressesFinding,
    VerificationTestsRepair,
    ExecutionHasOutcome,
    RoundHasRepair
});
vocabulary!(ErrorCode {
    UnsupportedProtocol,
    InvalidRequest,
    InvalidCursor,
    SnapshotExpired,
    CheckpointExpired,
    InstallationMismatch,
    EpochMismatch,
    RecordTooLarge,
    ArtifactUnavailable,
    ArtifactTooLarge,
    DigestMismatch,
    Usage,
    Config,
    Store,
    Forbidden,
    StoreUnavailable
});

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct RevisionRef {
    pub source_id: String,
    pub revision: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactIdentity {
    pub installation_id: String,
    pub store_epoch: String,
    pub source_id: String,
    pub revision: String,
    pub revision_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactDescriptor {
    pub artifact_id: String,
    pub identity: ArtifactIdentity,
    pub sha256: String,
    pub byte_length: u64,
    pub media_type: String,
    pub availability: Availability,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Locator {
    JsonPointer { pointer: String },
    ByteRange { start: u64, end: u64 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceRef {
    pub artifact_id: String,
    pub sha256: String,
    pub locator: Locator,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    Observed,
    SourceClaim,
    HumanInference {
        author: String,
        tool: String,
        version: String,
        rationale: String,
    },
    ModelInterpretation {
        author: String,
        tool: String,
        version: String,
        rationale: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EntityPayload {
    Work {
        work_id: String,
        version: u64,
        intent: Option<String>,
        requirements: Vec<RevisionRef>,
        constraints: Vec<RevisionRef>,
        dispatch_source: Option<RevisionRef>,
    },
    Execution {
        execution_id: String,
        work: RevisionRef,
        state: State,
        outcome: Option<RevisionRef>,
        base_head: Option<String>,
        result_head: Option<String>,
        started_at: Option<String>,
        finished_at: Option<String>,
    },
    Invocation {
        invocation_id: String,
        execution: RevisionRef,
        process_attempt_id: String,
        state: State,
    },
    ReviewRound {
        round_id: String,
        execution: RevisionRef,
        change_id: String,
        reviewed_head: Option<String>,
        completeness: Completeness,
        observation_id: Option<String>,
    },
    FindingOccurrence {
        occurrence_id: String,
        source_finding_key: String,
        round: RevisionRef,
        reviewed_head: Option<String>,
        body: Option<String>,
        disposition: Disposition,
        category: Option<String>,
        invariants: Vec<RevisionRef>,
    },
    RepairAttempt {
        attempt_id: String,
        execution: RevisionRef,
        round: Option<RevisionRef>,
        target_head: Option<String>,
        result_head: Option<String>,
        state: State,
        sequence: Option<u64>,
    },
    Verification {
        check_id: String,
        name: String,
        tested_head: Option<String>,
        state: State,
        scope: VerificationScope,
        exit_code: Option<i32>,
    },
    Outcome {
        outcome_id: String,
        execution: RevisionRef,
        scope: OutcomeScope,
        state: State,
        head: Option<String>,
    },
    Requirement {
        requirement_id: String,
        text: Option<String>,
        category: Option<String>,
        provenance: Provenance,
        source_revision: Option<RevisionRef>,
    },
    Invariant {
        invariant_id: String,
        text: Option<String>,
        category: Option<String>,
        provenance: Provenance,
        source_revision: Option<RevisionRef>,
    },
}
impl EntityPayload {
    pub fn entity_type(&self) -> EntityType {
        match self {
            Self::Work { .. } => EntityType::Work,
            Self::Execution { .. } => EntityType::Execution,
            Self::Invocation { .. } => EntityType::Invocation,
            Self::ReviewRound { .. } => EntityType::ReviewRound,
            Self::FindingOccurrence { .. } => EntityType::FindingOccurrence,
            Self::RepairAttempt { .. } => EntityType::RepairAttempt,
            Self::Verification { .. } => EntityType::Verification,
            Self::Outcome { .. } => EntityType::Outcome,
            Self::Requirement { .. } => EntityType::Requirement,
            Self::Invariant { .. } => EntityType::Invariant,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RecordData {
    Entity {
        #[serde(flatten)]
        entity: EntityPayload,
    },
    Link {
        relation: Relation,
        from: RevisionRef,
        to: RevisionRef,
    },
    EntityTombstone {
        entity_type: EntityType,
        reason: String,
    },
    LinkTombstone {
        relation: Relation,
        from: RevisionRef,
        to: RevisionRef,
        reason: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecordBody {
    pub record_kind: RecordKind,
    pub source_id: String,
    pub revision: String,
    pub revision_seq: u64,
    pub work_id: String,
    pub assertion: Assertion,
    pub source_refs: Vec<SourceRef>,
    #[serde(flatten)]
    pub data: RecordData,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Record {
    pub source_id: String,
    pub revision: String,
    pub revision_seq: u64,
    pub change_seq: u64,
    pub body_sha256: String,
    pub body_json: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GapScope {
    Revision { record: RevisionRef },
    Artifact { artifact_id: String, sha256: String },
    History { before_seq: u64 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Gap {
    pub code: GapCode,
    pub scope: GapScope,
    pub evidence_class: EvidenceClass,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportRequest {
    pub protocol_version: ProtocolVersion,
    pub after: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportPage {
    pub protocol_version: ProtocolVersion,
    pub installation_id: String,
    pub store_epoch: String,
    pub snapshot_id: String,
    pub snapshot_seq: u64,
    pub retained_from_seq: u64,
    pub after_seq: Option<u64>,
    pub entities: Vec<Record>,
    pub links: Vec<Record>,
    pub artifacts: Vec<ArtifactDescriptor>,
    pub gaps: Vec<Gap>,
    pub next_cursor: Option<String>,
    pub checkpoint: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRequest {
    pub protocol_version: ProtocolVersion,
    pub artifact_id: String,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactResult {
    pub protocol_version: ProtocolVersion,
    pub descriptor: ArtifactDescriptor,
    pub bytes_written: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}
vocabulary!(QueryStatus { Accepted, Error });
vocabulary!(SourceAvailability {
    Known,
    Unknown,
    Unavailable
});
vocabulary!(CommandKind {
    EvidenceExport,
    EvidenceArtifact
});

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueryCommand {
    pub id: Option<String>,
    pub kind: CommandKind,
    pub actor: String,
    pub idempotency_key: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuerySource {
    pub source: String,
    pub availability: SourceAvailability,
    pub observed_unix_seconds: Option<i64>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub enum SchemaVersion {
    #[serde(rename = "1")]
    V1,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueryEnvelope<T> {
    pub schema_version: SchemaVersion,
    pub status: QueryStatus,
    pub command: Option<QueryCommand>,
    pub work_id: Option<String>,
    pub execution_id: Option<String>,
    pub result: Option<T>,
    pub reason: Option<ProtocolError>,
    pub evidence_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<QuerySource>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<u64>,
}

/// Schema roots include the parsed body contract even though transport retains its exact string bytes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WireContract {
    pub export_envelope: QueryEnvelope<ExportPage>,
    pub artifact_envelope: QueryEnvelope<ArtifactResult>,
    pub export_request: ExportRequest,
    pub export_page: ExportPage,
    pub record_body: RecordBody,
    pub artifact_request: ArtifactRequest,
    pub artifact_result: ArtifactResult,
    pub error: ProtocolError,
}
pub fn schema() -> schemars::Schema {
    schemars::schema_for!(WireContract)
}
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
