use serde::{Deserialize, Serialize};

use crate::identity::{HostBinding, IssueRef, RepositoryRef, SourceGate};
use crate::observation::Outcome;

/// Monotonic position in the installation's event log. Orders the queue,
/// marks snapshot `as_of`, and anchors resume positions.
pub type EventSequence = i64;

/// A spec revision: the immutable snapshot sequence within one Work.
/// `spec = 1` is recorded at acceptance; every approved change appends a
/// new revision. Existing spec rows are never mutated.
pub type SpecRevision = u64;

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

/// What a worker is allowed to change. Host mutation is never granted
/// here — it goes through a recorded `HostActionIntent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedEffects {
    pub workspace_write: bool,
    pub host_write: bool,
    pub network: bool,
}

/// Fresh UUID-based record identity. Prefixes are debug-readable hints
/// only; no executable meaning may be parsed from them.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// A SHA-256 digest over tagged canonical bytes, hex-encoded.
pub fn sha256_hex(tag: &str, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update(b":");
    h.update(bytes);
    use std::fmt::Write;
    h.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Canonical JSON encoding: object keys sorted recursively, UTF-8, no
/// insignificant whitespace. Every externally supplied payload is hashed
/// and compared over this encoding.
pub fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    fn write(v: &serde_json::Value, out: &mut Vec<u8>) {
        match v {
            serde_json::Value::Null => out.extend_from_slice(b"null"),
            serde_json::Value::Bool(b) => {
                out.extend_from_slice(if *b { b"true" } else { b"false" })
            }
            serde_json::Value::Number(_) | serde_json::Value::String(_) => {
                out.extend_from_slice(serde_json::to_string(v).expect("primitive json").as_bytes());
            }
            serde_json::Value::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write(item, out);
                }
                out.push(b']');
            }
            serde_json::Value::Object(map) => {
                out.push(b'{');
                // Sort keys explicitly — serde_json's Map only orders
                // when `preserve_order` is off, and a transitive feature
                // flip must never change the canonical bytes.
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(serde_json::to_string(k).expect("key json").as_bytes());
                    out.push(b':');
                    write(map.get(*k).expect("key from map.keys()"), out);
                }
                out.push(b'}');
            }
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
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
    HostWrite,
    Reconciliation,
}

/// The action an `uncertain`/`stopping` Execution wants once the writer is
/// proven stopped or prevented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredAction {
    Continue,
    /// Stop the writer, then park the Execution `paused` for repair —
    /// the recorded `repair_prerequisites` on the Execution carry the
    /// continuation.
    Pause,
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
    /// A bounded correction pass; counts against the Work-wide fix
    /// account, not just the Execution budget.
    Fix,
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

/// The immutable Work-wide recovery policy recorded at acceptance.
/// Successor Executions and configuration changes cannot reset these
/// bounds; limits compose — reaching any applicable limit prohibits
/// another automatic attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPolicy {
    /// Bounded host-corrective dispatch: fix Invocations across the
    /// Work's whole Execution chain.
    pub max_fix_invocations: u32,
    /// Consecutive equivalent failed validations before human action is
    /// required.
    pub equivalent_failure_limit: u32,
    /// Transport retries for a failed (no-effect) host action.
    pub transport_max_retries: u32,
    /// Unknown effect outcomes are reconciled for at most this long
    /// before the stage cannot proceed.
    pub effect_reconcile_seconds: u64,
    /// No material progress for this long raises Attention — a signal,
    /// never proof of termination.
    pub no_progress_seconds: u64,
    /// Work-wide automatic-execution ceiling from first dispatch,
    /// excluding recorded human-decision waiting.
    pub automatic_time_limit_seconds: u64,
}

impl Default for WorkPolicy {
    fn default() -> Self {
        Self {
            max_fix_invocations: 3,
            equivalent_failure_limit: 2,
            transport_max_retries: 3,
            effect_reconcile_seconds: 600,
            no_progress_seconds: 1800,
            automatic_time_limit_seconds: 4 * 3600,
        }
    }
}

/// Durable Work-wide usage accounting. Survives restarts and successor
/// Executions; never decremented by progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkUsage {
    pub work_id: WorkId,
    pub fix_invocations_used: u32,
    pub transport_retries_used: u32,
    /// Consecutive equivalent failed validations. A material-progress
    /// event resets the streak; it never returns spent attempts.
    pub equivalent_failure_streak: u32,
    /// The normalized signature of the last counted failure.
    pub equivalent_signature: Option<String>,
    /// Cumulative automatic seconds excluding human-decision waiting.
    pub automatic_seconds_used: u64,
    /// The first dispatch across the whole Execution chain; anchors the
    /// automatic-time accounting.
    pub first_dispatch_unix_seconds: Option<i64>,
    /// Wall clock of the last material-progress event.
    pub last_progress_unix_seconds: Option<i64>,
    /// Total recorded human-decision waiting, subtracted from elapsed
    /// wall time when computing automatic usage.
    pub human_wait_seconds_total: u64,
}

impl WorkUsage {
    pub fn new(work_id: &WorkId) -> Self {
        Self {
            work_id: work_id.clone(),
            fix_invocations_used: 0,
            transport_retries_used: 0,
            equivalent_failure_streak: 0,
            equivalent_signature: None,
            automatic_seconds_used: 0,
            first_dispatch_unix_seconds: None,
            last_progress_unix_seconds: None,
            human_wait_seconds_total: 0,
        }
    }
}

/// A typed repair prerequisite recorded when an Execution holds for a
/// repairable condition. `resume` rechecks checkable prerequisites
/// against store state newer than the pause; a prerequisite that is not
/// satisfied keeps the Execution held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairPrerequisite {
    /// A fresh authoritative source read matching the approved spec
    /// must exist after the pause.
    SourceRead,
    /// A capability report authorizing this capability must exist.
    Capability {
        capability: crate::identity::HostCapability,
    },
    /// A valid artifact fact for this name must exist.
    Artifact { name: String },
    /// The host must report an effect resolution for this intent.
    EffectResolution { action_id: String },
    /// The pause itself is the human decision; any resume Command
    /// satisfies it.
    OperatorDecision,
    /// A new approved spec revision is required; `resume` alone can
    /// never satisfy it.
    SpecReapproval,
}

/// Evidence lanes declared by the spec snapshot. A disabled lane is an
/// explicit policy fact and never counts as positive evidence; an
/// enabled lane needs its exact evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanePolicy {
    pub invariant_check: bool,
    pub code_review: bool,
}

impl Default for LanePolicy {
    fn default() -> Self {
        Self {
            invariant_check: true,
            code_review: true,
        }
    }
}

/// The immutable spec snapshot payload. Source snapshots carry the
/// authoritative `source_revision`/`content_digest` read the boundary
/// resolver performed before submission; they are compared against
/// fresh host observations, never assumed current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecPayload {
    pub intent: String,
    pub sources: Vec<crate::identity::SourceSnapshot>,
    pub constraints: Vec<String>,
    /// Human-configured verification commands for the work's change.
    pub verification_argv: Vec<Vec<String>>,
    /// Named host checks that must conclude `success` on the current
    /// target before completion.
    pub required_checks: Vec<String>,
    pub lanes: LanePolicy,
}

/// A stored spec snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecSnapshot {
    pub work_id: WorkId,
    pub revision: SpecRevision,
    pub payload: SpecPayload,
    /// SHA-256 over the canonical payload bytes.
    pub digest: String,
    pub approved_by: Actor,
    pub recorded_seq: EventSequence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: WorkId,
    /// Optimistic-concurrency version; precondition for Commands.
    pub version: u64,
    /// The immutable host binding recorded at acceptance.
    pub binding: HostBinding,
    /// The current spec revision. Work only runs on `spec ==
    /// approved_spec`.
    pub spec: SpecRevision,
    /// The last explicitly approved spec revision.
    pub approved_spec: SpecRevision,
    pub source_gate: SourceGate,
    /// The immutable recovery policy recorded at acceptance.
    pub policy: WorkPolicy,
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
    /// Typed repair prerequisites a `paused`-for-repair Execution must
    /// satisfy before readmission.
    pub repair_prerequisites: Vec<RepairPrerequisite>,
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
    /// Set when entering `paused`; elapsed time here counts as
    /// human-decision waiting, not automatic usage.
    pub paused_since_unix_seconds: Option<i64>,
    /// The role the next admitted Invocation runs as. `None` means
    /// `Coding`; a judgment containing `fix` dispositions sets `Fix` so
    /// the bounded fix account is charged on the next dispatch.
    pub next_role: Option<InvocationRole>,
}

/// Which budget bound a next Invocation would exceed. Exhaustion is a
/// terminal decision for the current Execution, recorded as its
/// `terminal_code`, never a caller-visible rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLimit {
    Invocations,
    ReviewRounds,
}

impl BudgetLimit {
    pub fn terminal_code(&self) -> &'static str {
        match self {
            Self::Invocations => "invocation_budget_exhausted",
            Self::ReviewRounds => "review_budget_exhausted",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Invocations => "invocation budget exhausted".into(),
            Self::ReviewRounds => "review round budget exhausted".into(),
        }
    }
}

impl Execution {
    /// The budget bound the next Invocation for `role` would exceed, if any.
    /// Checked before any counter increment or request emission, so
    /// `invocations_used`/`review_rounds_used` can never grow past their
    /// documented bounds.
    pub fn budget_exhaustion(&self, role: &InvocationRole) -> Option<BudgetLimit> {
        if self.invocations_used >= self.budget.max_invocations {
            return Some(BudgetLimit::Invocations);
        }
        if matches!(role, InvocationRole::Review)
            && self.review_rounds_used >= self.budget.max_review_rounds
        {
            return Some(BudgetLimit::ReviewRounds);
        }
        None
    }

    /// Whether this Execution may receive a new Invocation request for
    /// `role`. `queued` is deliberate: admission is the `queued → ready`
    /// request-creation point this predicate gates. `paused`/`stopping`
    /// require a Command to requeue first, and `uncertain` is excluded: a
    /// new writer is never granted while the previous one is unproven.
    pub fn accepts_invocation(&self, role: &InvocationRole) -> bool {
        matches!(
            self.state,
            ExecutionState::Queued
                | ExecutionState::Ready
                | ExecutionState::Running
                | ExecutionState::Waiting
        ) && self.budget_exhaustion(role).is_none()
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
    pub required_artifacts: Vec<RequiredArtifact>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    /// Opaque verifier — e.g. the `ps lstart` string — stored verbatim and
    /// re-verified before any signal, never parsed.
    pub started_at: String,
    /// The process group this identity belongs to. `None` for descendants
    /// that escaped the group (e.g. `setsid`), which is exactly why the
    /// snapshot covers them individually.
    #[serde(default)]
    pub pgid: Option<i32>,
}

/// The durable request `execution` consumes; survives a crash between the
/// `ready` commit and the spawn. The bound spec revision makes the
/// request replayable against immutable spec history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRequest {
    /// Derived: `<execution_id>:inv:<sequence>` — the at-most-once key.
    pub request_id: String,
    pub work_id: WorkId,
    pub execution_id: ExecutionId,
    pub role: InvocationRole,
    /// The spec revision this request was recorded against.
    pub spec_revision: SpecRevision,
    pub payload: serde_json::Value,
}

/// A resource a writer may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteResource {
    Workspace {
        workspace_id: String,
    },
    Branch {
        repository: RepositoryRef,
        branch: String,
    },
}

impl WriteResource {
    /// Normalized reservation key in one shared key space: a versioned
    /// array encoding so values containing separators can never
    /// collide. Never parse this string for executable meaning.
    pub fn key(&self) -> String {
        match self {
            Self::Workspace { workspace_id } => {
                serde_json::json!(["v1", "workspace", workspace_id]).to_string()
            }
            Self::Branch { repository, branch } => serde_json::json!([
                "v1",
                "branch",
                repository.host.as_str(),
                repository.repository.as_str(),
                branch
            ])
            .to_string(),
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
/// checkpoints, and the worker-runtime artifact store. `expected_digest`
/// pins the artifact to an exact payload when the producer declared one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredArtifact {
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub expected_digest: Option<String>,
}

/// A host-side source issue this Work is bound to: immutable membership
/// recorded at acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIssueRef {
    pub work_id: WorkId,
    pub issue: IssueRef,
}

/// The revision recorded on a durable request or receipt so it can be
/// replayed against immutable spec history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundRevision {
    pub spec: SpecRevision,
    pub digest: String,
}

/// A durable, immutable record of a classified failure or recovery
/// decision. Append-only; carries the usage snapshot after the decision
/// so Work-wide accounting is auditable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryDecision {
    pub id: String,
    pub work_id: WorkId,
    pub execution_id: Option<ExecutionId>,
    /// e.g. `correction_required`, `transport`, `equivalent_failure`,
    /// `capability_repair`, `source_hold`, `budget`, `human_hold`,
    /// `automatic_time_limit`.
    pub classification: String,
    pub signature: Option<String>,
    pub prerequisites: Vec<RepairPrerequisite>,
    pub detail: String,
    /// JSON snapshot of `WorkUsage` after the decision.
    pub usage_after: serde_json::Value,
    pub seq: EventSequence,
}

/// One immutable review round for an exact diff target. Rounds are
/// never mutated; a changed occurrence set installs a new round and
/// bumps the evidence generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRound {
    pub id: String,
    pub work_id: WorkId,
    pub target: crate::identity::DiffTarget,
    /// The review source identity (e.g. reviewer or collector).
    pub source: String,
    pub source_revision: Option<String>,
    /// `complete`, `partial:<reason>`, or `unavailable:<reason>`.
    pub completeness: String,
    /// Digest over the sorted occurrence identity set.
    pub set_digest: String,
    pub generation: u64,
    pub seq: EventSequence,
}

/// One finding inside a sealed round. `id` is the install-scoped stable
/// occurrence id; `source_key` is the source-supplied key. Identity is
/// never text similarity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingOccurrence {
    pub id: String,
    pub round_id: String,
    pub source_key: String,
    pub content_digest: Option<String>,
    pub severity: Option<String>,
    pub position: Option<String>,
    pub thread_id: Option<String>,
}

/// An immutable, exact-set judgment over one sealed round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundJudgment {
    pub id: String,
    pub work_id: WorkId,
    pub round_id: String,
    pub set_digest: String,
    pub decisions: Vec<crate::command::FindingDecision>,
    pub actor_id: String,
    pub seq: EventSequence,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonicalization must not depend on serde_json's map ordering:
    /// two maps built in different insertion orders canonicalize
    /// identically even if a transitive `preserve_order` feature flip
    /// changes iteration order.
    #[test]
    fn canonical_json_is_order_independent() {
        let forward = serde_json::json!({"a": 1, "b": {"x": 2, "y": 3}, "c": [4, 5]});
        let backward = serde_json::json!({"c": [4, 5], "b": {"y": 3, "x": 2}, "a": 1});
        assert_eq!(canonical_json(&forward), canonical_json(&backward));
    }
}
