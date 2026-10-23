use serde::{Deserialize, Serialize};

use crate::observation::Outcome;

/// Monotonic position in the installation's event log. Orders the queue,
/// marks snapshot `as_of`, and anchors resume positions.
pub type EventSequence = i64;

/// A durable human intent; survives retries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InvocationId(pub String);

/// Which interface a request came through.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origin(pub String);

/// The acting identity, stable across adapter restarts (installation config).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub external_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Automation,
}

/// What a worker is allowed to change. GitHub mutation is never granted
/// here — it goes through a recorded `PrActionIntent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedEffects {
    pub workspace_write: bool,
    pub github_write: bool,
    pub network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Queued,
    Ready,
    Running,
    Waiting,
    Stopping,
    Paused,
    Uncertain,
    Succeeded,
    Failed,
    Cancelled,
}

impl ExecutionState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    ArtifactRecovery,
    Ci,
    HumanJudgment,
    GithubWrite,
    Reconciliation,
}

/// The action an `uncertain`/`stopping` Execution wants once the writer is
/// proven stopped or prevented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredAction {
    Continue,
    Cancel,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Prepared,
    Running,
    Done,
    Abandoned,
    Unknown,
}

impl InvocationState {
    /// A writer whose liveness/effects are not proven finished.
    pub fn is_unresolved_writer(&self) -> bool {
        matches!(self, Self::Prepared | Self::Running | Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationRole {
    Coding,
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationInput {
    pub input: serde_json::Value,
    pub resume: bool,
    pub next_execution_input: Option<serde_json::Value>,
    pub previous_execution_outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionBudget {
    /// All Invocation creation attempts count, including `not_started`.
    pub max_invocations: u32,
    /// Every review Invocation consumes one round, including head-driven re-reviews.
    pub max_review_rounds: u32,
    /// From first dispatch (ready), not acceptance.
    pub runtime_limit_seconds: u64,
    /// Bounded artifact recovery: attempts and seconds, separate from the
    /// invocation/runtime budgets.
    pub artifact_recovery_attempts: u32,
    pub artifact_recovery_seconds: u64,
    /// Continuously unresolved `uncertain` beyond this surfaces Attention.
    pub uncertain_threshold_seconds: u64,
}

impl Default for ExecutionBudget {
    fn default() -> Self {
        Self {
            max_invocations: 10,
            max_review_rounds: 5,
            runtime_limit_seconds: 6 * 3600,
            artifact_recovery_attempts: 3,
            artifact_recovery_seconds: 30 * 60,
            uncertain_threshold_seconds: 15 * 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubIssueRef {
    pub repository: String,
    pub number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: WorkId,
    /// Optimistic-concurrency version; precondition for Commands.
    pub version: u64,
    /// A human-editable snapshot; edits increment version.
    pub revision: u64,
    pub intent: String,
    /// Source links, never primary keys.
    pub issue_refs: Vec<GitHubIssueRef>,
    pub constraints: Vec<String>,
    /// Human-configured verification commands for the work's change.
    pub verification_argv: Vec<Vec<String>>,
    pub created_seq: EventSequence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub work_id: WorkId,
    /// Position within the Work's execution chain (E1 = 0).
    pub ordinal: u32,
    pub state: ExecutionState,
    pub waiting_reason: Option<WaitingReason>,
    /// For `stopping`/`uncertain`: what to do once the writer is proven
    /// stopped or prevented.
    pub desired_action: Option<DesiredAction>,
    /// Failure/cancellation detail recorded at the terminal transition.
    pub terminal_code: Option<String>,
    /// Why a `paused` Execution stopped, and how it may continue.
    pub pause_reason: Option<String>,
    /// The evidence-backed assessment recorded at a terminal transition.
    pub outcome: Option<crate::observation::Outcome>,
    /// Queue order is the committed sequence, not wall-clock time.
    pub enqueued_seq: EventSequence,
    pub allowed_effects: AllowedEffects,
    pub input: InvocationInput,
    pub budget: ExecutionBudget,
    pub invocations_used: u32,
    pub review_rounds_used: u32,
    /// Set at first dispatch; the runtime limit anchors here.
    pub started_seq: Option<EventSequence>,
    pub started_unix_seconds: Option<i64>,
    /// Bounded artifact recovery bookkeeping while waiting(artifact_recovery).
    pub recovery_attempts_used: u32,
    pub recovery_deadline_unix_seconds: Option<i64>,
    /// Set when entering `uncertain`; Attention uses continuous duration.
    pub uncertain_since_unix_seconds: Option<i64>,
}

impl Execution {
    /// Whether this Execution may receive a new Invocation request.
    /// `uncertain` is excluded: a new writer is never granted while the
    /// previous one is unproven.
    pub fn accepts_invocation(&self) -> bool {
        matches!(self.state, ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting)
            && self.invocations_used < self.budget.max_invocations
    }
}

/// A single worker-process start within an Execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub id: InvocationId,
    pub request_id: String,
    pub execution_id: ExecutionId,
    pub role: InvocationRole,
    pub state: InvocationState,
    /// Host-observable identity — pid plus a start-time verifier so a reused
    /// pid is never mistaken for this writer.
    pub process_identity: Option<ProcessIdentity>,
    pub required_artifacts: Vec<String>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    /// Opaque verifier — e.g. the `ps lstart` string — stored verbatim and
    /// re-verified before any signal, never parsed.
    pub started_at: String,
}

/// The durable request `execution` consumes; survives a crash between the
/// `ready` commit and the spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRequest {
    /// Derived: `<execution_id>:inv:<sequence>` — the at-most-once key.
    pub request_id: String,
    pub work_id: WorkId,
    pub execution_id: ExecutionId,
    pub role: InvocationRole,
    pub payload: serde_json::Value,
}

/// A resource a writer may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteResource {
    Workspace { workspace_id: String },
    Branch { repository: String, branch: String },
}

impl WriteResource {
    /// Normalized reservation key in one shared key space.
    pub fn key(&self) -> String {
        match self {
            Self::Workspace { workspace_id } => format!("workspace:{workspace_id}"),
            Self::Branch { repository, branch } => format!("branch:{repository}:{branch}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReservationHolder {
    Execution { execution_id: ExecutionId },
    External { actor_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReservation {
    pub resource_key: String,
    pub resource: WriteResource,
    pub holder: ReservationHolder,
    /// External claims carry a TTL; execution reservations have none.
    pub expires_at_unix_seconds: Option<i64>,
}

/// A durable stop intent. The descendant snapshot is recorded at intent time
/// — after the leader exits, orphans reparent and leave the process tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopIntent {
    pub invocation_id: InvocationId,
    pub identities: Vec<ProcessIdentity>,
    pub recorded_seq: EventSequence,
}

/// What a worker must export; recovered by `execution` from the workspace,
/// checkpoints, and the worker-runtime artifact store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequiredArtifact {
    pub kind: String,
    pub path: String,
}
