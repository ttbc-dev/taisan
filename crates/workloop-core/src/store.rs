use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId, PrHeadSha};
use crate::domain::*;
use crate::observation::Observation;

/// A write `work` asks the store to apply inside the caller's transaction.
/// The store maps each variant to constrained SQL; it never interprets
/// lifecycle legality.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    InsertWork {
        work: Work,
    },
    /// Optimistic version check inside the commit.
    BumpWorkVersion {
        work_id: WorkId,
        expected: u64,
        next: u64,
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
    InsertPrActionIntent {
        intent: PrActionIntent,
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
    InsertJudgment {
        work_id: WorkId,
        expected_pr_head: String,
        decisions: Vec<crate::command::FindingDecision>,
    },
}

/// A recorded intent to perform a GitHub write; reconciled by action ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrActionIntent {
    pub action_id: String,
    pub work_id: WorkId,
    pub repository: String,
    pub action: PrAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PrAction {
    PublishPr {
        branch: String,
        title: String,
        body: String,
    },
    UpdatePr {
        number: u64,
        /// Re-read immediately before writing; a mismatch aborts.
        expected_head: String,
        body: String,
    },
    PostPrComment {
        number: u64,
        expected_head: String,
        body: String,
    },
}

/// Why the store refused an effect; `work` maps it to a rejection or
/// re-reads and re-decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A uniqueness or check constraint fired (reservation, request ID,
    /// nonterminal Execution, unresolved writer, slot, dedupe).
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
            Self::Internal { detail } => write!(f, "store error: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Read side of a transaction. `work` decides from this snapshot; the store
/// guarantees it is consistent with the writes the same tx applies.
pub trait ReadView {
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
    fn execution(&self, execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError>;
    /// The Work's nonterminal Execution, if one exists (unique by constraint).
    fn active_execution(&self, work_id: &WorkId) -> Result<Option<Execution>, StoreError>;
    fn executions_for_work(&self, work_id: &WorkId) -> Result<Vec<Execution>, StoreError>;
    /// Queued Executions in committed enqueue order — dispatch scans this.
    fn queued_executions(&self) -> Result<Vec<Execution>, StoreError>;

    fn reservation(&self, resource_key: &str) -> Result<Option<ResourceReservation>, StoreError>;
    fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError>;

    fn slot_limit(&self) -> Result<u32, StoreError>;
    fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError>;

    /// Approved requests not yet consumed by `execution`.
    fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError>;
    fn invocations_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<Invocation>, StoreError>;

    /// The Work's most recent recorded judgment: its PR head and decisions.
    fn latest_judgment(
        &self,
        work_id: &WorkId,
    ) -> Result<Option<(PrHeadSha, Vec<crate::command::FindingDecision>)>, StoreError>;

    /// Latest PR head observed for a Work, ordered by observed time so a
    /// delayed delivery for an older head cannot move the head backward.
    /// Judge/release decisions read provenance from this persisted
    /// evidence, never from the caller's supplied head alone.
    fn latest_pr_head(&self, work_id: &WorkId) -> Result<Option<PrHeadSha>, StoreError>;

    fn last_seq(&self) -> Result<EventSequence, StoreError>;
}

/// Write side of a transaction.
pub trait TxView: ReadView {
    /// Monotonic durable position; also the queue order key.
    fn alloc_seq(&mut self) -> Result<EventSequence, StoreError>;
    /// Installation-scoped unique IDs (`w-`, `e-`, `inv-`, `req-`, `cmd-`…).
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
    fn save_decision(
        &mut self,
        origin: &Origin,
        actor: &Actor,
        idempotency_key: &str,
        payload_digest: &str,
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
}
