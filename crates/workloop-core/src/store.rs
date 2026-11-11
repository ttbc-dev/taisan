use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId};
use crate::domain::*;
use crate::identity::{
    ActorRole, ChangeRequestRef, CommitRevision, HostActorRef, HostInstanceId, IssueRef,
    RepositoryRef, StoreId,
};
use crate::observation::{HostActionStatus, NormalizedEffectFact, Observation};

/// A write `work` asks the store to apply inside the caller's transaction.
/// The store maps each variant to constrained SQL; it never interprets
/// lifecycle legality.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Atomically records the Work, its binding and immutable
    /// membership/spec children, the active issue claims, and the usage
    /// row. Issue-claim conflicts surface as `ConstraintConflict`.
    InsertWork {
        work: Work,
        spec: SpecSnapshot,
        issue_refs: Vec<WorkIssueRef>,
        claims: Vec<IssueRef>,
        usage: WorkUsage,
    },
    /// Full-row write guarded by the caller-observed version. Carries
    /// spec/source-gate updates atomically with the version bump.
    UpdateWork {
        work: Work,
        expected: u64,
    },
    /// Appends a new immutable spec revision.
    InsertSpec {
        spec: SpecSnapshot,
    },
    InsertExecution {
        execution: Execution,
    },
    /// Full-row write guarded by the state `work` read. The store enforces
    /// the guard, not the transition's legality.
    UpdateExecution {
        execution: Execution,
        expected_state: ExecutionState,
    },
    /// Reserve one of the fixed capacity slot rows.
    ClaimSlot {
        execution_id: ExecutionId,
    },
    ReleaseSlot {
        execution_id: ExecutionId,
    },
    InsertReservation {
        reservation: ResourceReservation,
    },
    /// Delete rows by key; `holder` scopes a release to the caller's own
    /// claims.
    DeleteReservations {
        resource_keys: Vec<String>,
        holder: ReservationHolder,
    },
    /// Refresh a held claim's TTL (same-actor renewal).
    RenewReservation {
        resource_key: String,
        holder: ReservationHolder,
        expires_at_unix_seconds: i64,
    },
    InsertRequest {
        request: InvocationRequest,
    },
    InsertInvocation {
        invocation: Invocation,
    },
    UpdateInvocation {
        invocation: Invocation,
        expected_state: InvocationState,
    },
    InsertObservation {
        observation: Observation,
    },
    InsertStopIntent {
        intent: StopIntent,
    },
    InsertHostActionIntent {
        intent: HostActionIntent,
    },
    /// Reconciled result of an outbox action; `unknown` stays outstanding.
    /// `effects` records the normalized receipt facts and the actor the
    /// host authenticated.
    UpdateHostActionIntent {
        action_id: String,
        status: HostActionStatus,
        authenticated_actor: Option<HostActorRef>,
        effects: Vec<NormalizedEffectFact>,
    },
    /// Seals an immutable review round and its occurrence set atomically.
    InsertReviewRound {
        round: ReviewRound,
        occurrences: Vec<FindingOccurrence>,
    },
    /// Records an immutable exact-set judgment over a sealed round.
    InsertRoundJudgment {
        judgment: RoundJudgment,
    },
    /// Takes active issue claims for a Work (submit/retry). Conflicts
    /// surface as `ConstraintConflict`.
    InsertIssueClaims {
        work_id: WorkId,
        issues: Vec<IssueRef>,
    },
    /// Releases the Work's active claims on terminal settlement.
    ReleaseIssueClaims {
        work_id: WorkId,
    },
    /// Overwrites the Work's usage row with the caller's updated value.
    UpdateWorkUsage {
        usage: WorkUsage,
    },
    /// Appends an immutable failure/recovery decision record.
    InsertRecoveryDecision {
        decision: RecoveryDecision,
    },
    /// Schedules a durable recovery intent in the deciding transaction.
    InsertRecoveryIntent {
        intent: RecoveryIntent,
    },
    /// Moves one recovery intent through its closed lifecycle. The store
    /// enforces the row exists; Core decides the transition.
    UpdateRecoveryIntent {
        intent_id: String,
        state: RecoveryIntentState,
        retirement_reason: Option<String>,
    },
    /// Why a queued Execution was deferred at dispatch; surfaced by the
    /// `capacity` Query.
    RecordDeferral {
        execution_id: ExecutionId,
        reason: String,
        diagnostic: String,
    },
    ClearDeferral {
        execution_id: ExecutionId,
    },
    InsertAttentionAck {
        work_id: WorkId,
        attention_id: String,
        actor_id: String,
    },
    /// `execution` consumed a durable request; the store is the handoff, so
    /// consumption is recorded rather than deleted.
    MarkRequestConsumed {
        request_id: String,
    },
    /// Full-row upsert of the durable artifact-handoff record. Each
    /// stage transition commits inside the caller's transaction, so a
    /// crash leaves the last committed stage — never a torn row.
    /// `expected_seq` is the `recorded_seq` the writer read (0 for a
    /// row believed absent): a row concurrently advanced past it
    /// makes the write a `ConstraintConflict`, never a silent
    /// regression.
    UpsertHandoff {
        handoff: ArtifactHandoff,
        expected_seq: EventSequence,
    },
    /// Immutable result of one declared verification argv run against
    /// the handed-off commit. Unique per `(artifact_id, requirement)`.
    InsertVerificationResult {
        result: VerificationResult,
    },
}

/// Reconciliation state of a recorded outbox action.
pub type HostIntentStatus = HostActionStatus;

/// A recorded intent to perform a host-side write, bound to the exact
/// host, repository, role, and actor the binding authorized. Reconciled
/// by `action_id`; the payload digest pins the immutable intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostActionIntent {
    pub action_id: String,
    pub work_id: WorkId,
    pub host: HostInstanceId,
    pub repository: RepositoryRef,
    pub role: ActorRole,
    pub actor: HostActorRef,
    pub action: HostAction,
    /// SHA-256 over the canonical action payload.
    pub payload_digest: String,
    #[serde(default = "pending_status")]
    pub status: HostActionStatus,
    /// The actor the host authenticated on the receipt, when reported.
    #[serde(default)]
    pub authenticated_actor: Option<HostActorRef>,
    /// The normalized effect facts recorded with the receipt.
    #[serde(default)]
    pub effects: Vec<NormalizedEffectFact>,
    pub created_seq: EventSequence,
}

fn pending_status() -> HostActionStatus {
    HostActionStatus::Pending
}

/// The typed host actions a binding can authorize. Each variant carries
/// its exact immutable payload; receipts must carry a matching
/// normalized effect fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HostAction {
    /// Publish `produced` on `branch` guarded by `expected_prior` —
    /// the remote ref position recorded at intent time; `None` requires
    /// the ref to be absent. The write is refused when the remote no
    /// longer matches it.
    PushRef {
        branch: String,
        produced: CommitRevision,
        /// The remote ref position the intent was recorded against;
        /// `None` requires an absent ref (create).
        #[serde(default)]
        expected_prior: Option<CommitRevision>,
    },
    /// Open a change request from `source_branch` into `target_branch`.
    OpenChangeRequest {
        source_branch: String,
        target_branch: String,
        title: String,
        body: String,
    },
    /// Update metadata; a head read immediately before writing must
    /// still equal `expected_head` or the write aborts.
    UpdateChangeRequest {
        change: ChangeRequestRef,
        expected_head: CommitRevision,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
    /// Post a comment bound to an exact head and marker.
    PostComment {
        change: ChangeRequestRef,
        expected_head: CommitRevision,
        marker: String,
        body: String,
    },
}

impl HostAction {
    /// The operation name used for capability/eligibility bookkeeping
    /// and failure receipts — stable across payload changes.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PushRef { .. } => "push_ref",
            Self::OpenChangeRequest { .. } => "open_change_request",
            Self::UpdateChangeRequest { .. } => "update_change_request",
            Self::PostComment { .. } => "post_comment",
        }
    }
}

/// A dispatch deferral: why a queued Execution was not admitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deferral {
    pub execution_id: ExecutionId,
    pub reason: String,
    pub diagnostic: String,
    pub since_seq: EventSequence,
}

/// Closed lifecycle of a durable recovery intent. A restart cannot
/// resurrect a `retired` intent or re-fire a `consumed` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryIntentState {
    Pending,
    Consumed,
    Retired,
}

/// A durable recovery intent persisted in the deciding transaction:
/// `(kind, due_at, generation, evidence_basis)` under one Work, fired at
/// most once by the recovery-scheduling phase, retired with a reason
/// when terminal evidence invalidates it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryIntent {
    pub intent_id: String,
    pub work_id: WorkId,
    pub execution_id: Option<ExecutionId>,
    pub kind: String,
    pub due_at_unix_seconds: i64,
    pub generation: u64,
    pub evidence_basis: String,
    pub state: RecoveryIntentState,
    pub retirement_reason: Option<String>,
    pub scheduled_seq: EventSequence,
}

/// Why the store refused an effect; `work` maps it to a rejection or
/// re-reads and re-decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A uniqueness or check constraint fired (reservation, request ID,
    /// nonterminal Execution, unresolved writer, slot, dedupe, claim).
    ConstraintConflict {
        detail: String,
    },
    /// `UPDATE ... WHERE version = expected` matched no row.
    VersionConflict {
        work_id: WorkId,
        current_version: u64,
    },
    /// `UPDATE ... WHERE state = expected` matched no row.
    StateConflict {
        execution_id: ExecutionId,
        current_state: ExecutionState,
    },
    NoFreeSlot,
    /// The store file is not a recognized v1 database: an unversioned
    /// slice-0 database is archive-only, and an unknown future version
    /// is refused.
    UnsupportedFormat {
        detail: String,
    },
    Internal {
        detail: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConstraintConflict { detail } => write!(f, "constraint conflict: {detail}"),
            Self::VersionConflict {
                work_id,
                current_version,
            } => write!(
                f,
                "version conflict on {}: now {current_version}",
                work_id.0
            ),
            Self::StateConflict {
                execution_id,
                current_state,
            } => write!(
                f,
                "state conflict on {}: now {current_state:?}",
                execution_id.0
            ),
            Self::NoFreeSlot => write!(f, "no free capacity slot"),
            Self::UnsupportedFormat { detail } => write!(f, "unsupported store format: {detail}"),
            Self::Internal { detail } => write!(f, "store error: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Read side of a transaction. `work` decides from this snapshot; the store
/// guarantees it is consistent with the writes the same tx applies.
pub trait ReadView {
    /// The installation authority of this store file.
    fn store_id(&self) -> Result<StoreId, StoreError>;

    fn stored_decision(
        &self,
        origin: &Origin,
        actor: &Actor,
        idempotency_key: &str,
    ) -> Result<Option<(String, CommandDecision)>, StoreError>;

    fn command_decision(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<CommandDecision>, StoreError>;

    fn work(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError>;
    /// The spec snapshot for `revision`, or the Work's current revision
    /// when `None`.
    fn spec(
        &self,
        work_id: &WorkId,
        revision: Option<SpecRevision>,
    ) -> Result<Option<SpecSnapshot>, StoreError>;
    fn execution(&self, execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError>;
    /// The Work's nonterminal Execution, if one exists (unique by constraint).
    fn active_execution(&self, work_id: &WorkId) -> Result<Option<Execution>, StoreError>;
    fn executions_for_work(&self, work_id: &WorkId) -> Result<Vec<Execution>, StoreError>;
    /// Queued Executions in committed enqueue order — dispatch scans this.
    fn queued_executions(&self) -> Result<Vec<Execution>, StoreError>;

    /// Immutable source membership recorded at acceptance.
    fn issue_refs(&self, work_id: &WorkId) -> Result<Vec<IssueRef>, StoreError>;
    /// The Work holding an active claim on this issue, if any.
    fn issue_claim(&self, issue: &IssueRef) -> Result<Option<WorkId>, StoreError>;

    /// The Work's durable usage accounting row.
    fn work_usage(&self, work_id: &WorkId) -> Result<Option<WorkUsage>, StoreError>;

    fn reservation(&self, resource_key: &str) -> Result<Option<ResourceReservation>, StoreError>;
    fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError>;

    fn slot_limit(&self) -> Result<u32, StoreError>;
    fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError>;

    /// Approved requests not yet consumed by `execution`.
    fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError>;
    fn request(&self, request_id: &str) -> Result<Option<InvocationRequest>, StoreError>;
    fn invocations_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<Invocation>, StoreError>;
    /// The Invocation recorded for a durable request, if any — the
    /// at-most-once reconciliation key.
    fn invocation_for_request(&self, request_id: &str) -> Result<Option<Invocation>, StoreError>;
    /// Invocations that may still write (prepared/running/unknown).
    fn unresolved_invocations(&self) -> Result<Vec<Invocation>, StoreError>;

    fn works(&self) -> Result<Vec<Work>, StoreError>;
    fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError>;

    fn stop_intent(&self, invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError>;

    /// The durable handoff record for one artifact id; `None` = absent.
    fn artifact_handoff(&self, artifact_id: &str) -> Result<Option<ArtifactHandoff>, StoreError>;
    /// The first recorded handoff row for an Invocation; an Invocation
    /// may carry one handoff per required artifact.
    fn handoff_for_invocation(
        &self,
        invocation_id: &InvocationId,
    ) -> Result<Option<ArtifactHandoff>, StoreError>;
    /// Rows the recovery sweep must advance: everything not yet
    /// `acknowledged` or `invalid`.
    fn incomplete_handoffs(&self) -> Result<Vec<ArtifactHandoff>, StoreError>;

    /// Recorded verification results for one handed-off artifact.
    fn verification_results(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<VerificationResult>, StoreError>;

    /// Recorded dispatch deferrals, keyed by Execution — the `capacity`
    /// Query's explanation of queue stalls.
    fn deferrals(&self) -> Result<Vec<Deferral>, StoreError>;

    /// Attention IDs already acknowledged for a Work.
    fn attention_acks(&self, work_id: &WorkId) -> Result<Vec<String>, StoreError>;

    /// Observations recorded against an Execution (process, artifact,
    /// host facts) for evidence views and reconciliation.
    fn observations_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<Observation>, StoreError>;
    fn observations_for_work(&self, work_id: &WorkId) -> Result<Vec<Observation>, StoreError>;
    /// System-scope facts not bound to one Work (capability reports).
    fn all_observations(&self) -> Result<Vec<Observation>, StoreError>;

    /// All outbox rows; `pending`/`unknown` are outstanding effects.
    fn action_intents(&self) -> Result<Vec<HostActionIntent>, StoreError>;

    /// The most recently sealed review round for a Work, if any.
    fn latest_round(&self, work_id: &WorkId) -> Result<Option<ReviewRound>, StoreError>;
    /// All sealed review rounds for a Work, oldest first.
    fn rounds_for_work(&self, work_id: &WorkId) -> Result<Vec<ReviewRound>, StoreError>;
    /// The occurrence set of one sealed round.
    fn occurrences(&self, round_id: &str) -> Result<Vec<FindingOccurrence>, StoreError>;
    /// The judgment recorded for one sealed round, if any.
    fn judgment_for_round(&self, round_id: &str) -> Result<Option<RoundJudgment>, StoreError>;
    /// The Work's most recent recorded round judgment.
    fn latest_judgment(&self, work_id: &WorkId) -> Result<Option<RoundJudgment>, StoreError>;

    /// Failure/recovery decision records for a Work.
    fn recovery_decisions(&self, work_id: &WorkId) -> Result<Vec<RecoveryDecision>, StoreError>;

    /// All recorded recovery intents; Core filters by state/due time.
    fn recovery_intents(&self) -> Result<Vec<RecoveryIntent>, StoreError>;

    fn last_seq(&self) -> Result<EventSequence, StoreError>;
}

/// Write side of a transaction.
pub trait TxView: ReadView {
    /// Monotonic durable position; also the queue order key.
    fn alloc_seq(&mut self) -> Result<EventSequence, StoreError>;
    /// Installation-scoped unique IDs (fresh UUID-based).
    fn alloc_id(&mut self, prefix: &str) -> Result<String, StoreError>;

    fn apply(&mut self, effect: &Effect) -> Result<(), StoreError>;

    /// Savepoint around one decide+apply attempt: a conflict midway must not
    /// leave earlier effects applied when `work` re-decides.
    fn begin_attempt(&mut self) -> Result<(), StoreError>;
    /// Roll back to the savepoint so a re-decision sees the pre-attempt view.
    fn revert_attempt(&mut self) -> Result<(), StoreError>;
    /// Release the savepoint after a successful attempt.
    fn commit_attempt(&mut self) -> Result<(), StoreError>;

    /// Persist the decision for idempotent replay; the unique
    /// `(origin, actor, idempotency_key)` constraint enforces one decision.
    /// `canonical_payload` is stored verbatim so replay compares bytes.
    fn save_decision(
        &mut self,
        origin: &Origin,
        actor: &Actor,
        idempotency_key: &str,
        payload_digest: &str,
        canonical_payload: &[u8],
        decision: &CommandDecision,
    ) -> Result<(), StoreError>;
}

/// The store port. `work` calls `transact` once per Command; the closure may
/// run more than once if the implementation retries serialization errors,
/// so it must be deterministic over the view it reads.
pub trait Store {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError>;

    /// A transaction that starts deferred: while the closure only reads,
    /// it holds no writer lock, so a Query or `preview` neither blocks a
    /// writer nor busy-waits behind one. A write the closure performs
    /// upgrades the transaction for the remainder of the call — preview
    /// relies on that to run effect application inside its rolled-back
    /// savepoint. Defaults to `transact`; stores that can begin a
    /// transaction deferred override it.
    fn transact_deferred<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.transact(f)
    }
}
