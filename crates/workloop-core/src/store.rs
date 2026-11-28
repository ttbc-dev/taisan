use serde::{Deserialize, Serialize};

use crate::command::{CommandDecision, CommandId};
use crate::domain::*;
use crate::identity::{
    ActorRole, AdapterId, ChangeRequestRef, CommitRevision, HostActorRef, HostInstanceId, IssueRef,
    RepositoryRef, StoreId,
};
use crate::observation::{HostActionStatus, NormalizedEffectFact, Observation};
use crate::worker::WorkerRequestExport;

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
    /// Bind a request to the exact Worker adapter selected before its
    /// first launch. First binding is authoritative; a bound request is
    /// never migrated to another adapter.
    BindRequestAdapter {
        request_id: String,
        binding: WorkerAdapterBinding,
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
    /// enforces the row exists; Core decides the transition. A retirement
    /// carries the evidence generation it was decided from: the store
    /// refuses an evidence generation older than the intent's scheduling
    /// generation or the Work's current generation (a stale decision
    /// cannot retire against newer evidence) and records it on the row.
    UpdateRecoveryIntent {
        intent_id: String,
        state: RecoveryIntentState,
        retirement_reason: Option<String>,
        evidence_generation: Option<u64>,
    },
    /// Records one intake invocation (`docs/intake-identity-storage.md`).
    /// At most one run may be `open`/`admitting` at a time; a second
    /// live run is a `ConstraintConflict`.
    InsertIntakeRun {
        run: IntakeRun,
    },
    /// Commit boundary B1 in one apply: persists a whole-response-
    /// validated candidate batch in original order *and* moves the run
    /// `open` → `admitting`. The store refuses the effect unless the run
    /// is still `open`, every candidate is `pending` with no decision
    /// links, and each `candidate.run_id` matches `run_id` — so a crash
    /// can never leave candidates without their run state, an `open`
    /// run holding candidates, or a run `admitting` with a partial
    /// batch.
    AdmitIntakeBatch {
        run_id: String,
        candidates: Vec<IntakeCandidate>,
    },
    /// Guarded run-state transition: only `state`, `detail_code`, and
    /// `finished_unix_seconds` may change, and only from
    /// `expected_state`.
    UpdateIntakeRun {
        run_id: String,
        state: IntakeRunState,
        detail_code: Option<String>,
        finished_unix_seconds: Option<i64>,
        expected_state: IntakeRunState,
    },
    /// Guarded candidate-status transition. Identity fields
    /// (`run_id`, `ordinal`, `proposal_key`, `replay_key`, `payload`,
    /// `payload_digest`, `recorded_seq`) are immutable; only the status
    /// and decision-link fields (`status`, `status_reason`,
    /// `command_id`, `work_id`, `committed_seq`) are written, and only
    /// from `expected_status`. The written status must also carry a
    /// link set the spec's Links column allows: unresolved statuses
    /// (`pending`, `capacity_deferred`, `unknown`, `interrupted`) keep
    /// all links NULL, `rejected` never carries `work_id`, and the
    /// decided statuses carry the command identity their outcome came
    /// from.
    UpdateIntakeCandidate {
        candidate: IntakeCandidate,
        expected_status: IntakeCandidateStatus,
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
    /// The durable WorkerRequest export for one Invocation. Written
    /// before any process is launched; a launch that fails or never
    /// returns leaves the export behind for reconciliation.
    InsertWorkerRequestExport {
        export: WorkerRequestExport,
    },
    /// Binds the recorded export's `request_id` to the spawned process
    /// identity, in the same transaction that records the running
    /// Invocation. Binding a missing export is a `ConstraintConflict` —
    /// the export is committed before the launch, so its absence here
    /// means the record and the launch disagree.
    BindWorkerRequestExport {
        request_id: String,
        process_identity: String,
    },
    /// The canonical executable request body for one durable dispatch
    /// request (`taisan.worker.request/2`, `docs/v0.3.md`). Written in
    /// the commit that binds the request before launch; a restart
    /// replays these bytes verbatim rather than reconstructing the
    /// request from mutable live configuration. A duplicate insert
    /// under the same `request_id` is a replay when the recorded bytes
    /// are identical and a `ConstraintConflict` when they differ —
    /// the store never silently replaces the body an already-launched
    /// worker was handed.
    InsertWorkerRequestBody {
        body: crate::worker_v2::WorkerRequestBody,
    },
    /// The immutable executor endpoint binding for one remote request
    /// (`docs/executor-transport-contract.md` §2). Written before the
    /// first remote operation is sent. A duplicate insert with
    /// identical content is a replay; different content is a
    /// `ConstraintConflict` — a bound request is never silently
    /// migrated to another endpoint, incarnation, payload, or launch
    /// configuration.
    InsertExecutorBinding {
        binding: crate::executor::ExecutorBinding,
    },
    /// The durable executor operation record (`docs/executor-transport-
    /// contract.md` §4): the command identity and canonical bytes are
    /// committed before the operation is sent. A duplicate insert with
    /// identical bytes is an idempotent replay; differing bytes under
    /// the same `(request_id, operation_id)` are a `ConstraintConflict`
    /// — the store's form of `conflicting_replay`. A second `start`
    /// for one request conflicts structurally.
    RecordExecutorCommand {
        operation: crate::executor::ExecutorOperation,
    },
    /// Attach a validated receipt to its previously recorded command.
    /// The receipt must parse and echo every identity field of the
    /// stored command; an identical re-record is a replay, differing
    /// bytes a `ConstraintConflict`, and a receipt for an unrecorded
    /// operation is refused.
    RecordExecutorReceipt {
        request_id: String,
        operation_id: String,
        receipt_body: Vec<u8>,
    },
    /// Full-row upsert of the durable remote-collection record
    /// (`docs/executor-transport-contract.md` §6, #376). One request
    /// admits one collection identity: a row naming a different
    /// `collection_id`, `invocation_id`, or `collection_digest` is a
    /// `ConstraintConflict` — the contract's "a different collection
    /// digest for one Invocation is a conflict". State moves are
    /// monotonic (`collecting → collected → acknowledged`, `invalid`
    /// terminal); a regression or a move off a terminal row conflicts.
    /// `expected_seq` is the `recorded_seq` the writer read (0 for a
    /// row believed absent).
    UpsertExecutorCollection {
        collection: crate::executor::ExecutorCollection,
        expected_seq: EventSequence,
    },
    /// Replaces the singleton intake-settings record, guarded by the
    /// caller-observed revision: `expected_revision` must equal the
    /// currently recorded `settings_revision`, and `None` requires the
    /// record to be absent. A stale or absent-row mismatch is a
    /// `ConstraintConflict` — a reload writer can never overwrite a
    /// newer active limit (`docs/execution-loop.md` queue admission).
    SetIntakeSettings {
        settings: IntakeSettingsRecord,
        expected_revision: Option<String>,
    },
}

/// The active installation intake-settings record: the content
/// revision the profile loader derived over the complete validated
/// `[intake]` section and the admission bound every submit transaction
/// enforces. Written only through the revision-guarded
/// [`Effect::SetIntakeSettings`]; an absent record resolves to
/// [`crate::work::DEFAULT_PENDING_QUEUE_LIMIT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeSettingsRecord {
    pub settings_revision: String,
    pub pending_queue_limit: u32,
    pub recorded_seq: EventSequence,
}

/// One scoped issue's live owner for the intake preflight: the Work
/// whose immutable issue membership covers the issue and whose active
/// Execution is non-terminal — `queued`, `ready`, `running`,
/// `waiting`, `stopping`, `paused`, or `uncertain`. Issue claims are a
/// separate record retained through holds, pauses, and uncertainty;
/// this view reports queue-relevant owners, not the claim ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeSourceOwner {
    pub issue: IssueRef,
    pub work_id: WorkId,
    pub execution_state: ExecutionState,
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
    /// Publish review feedback bound to the head it examined.
    PublishReviewFeedback {
        change: ChangeRequestRef,
        head: CommitRevision,
        body: String,
    },
    /// Post one marked reply on the identified thread, preserving its
    /// exact thread identity. The persisted tag is the contract
    /// operation name, matching `HostAction::name()`.
    #[serde(rename = "thread_reply")]
    ReplyToThread {
        change: ChangeRequestRef,
        thread_id: String,
        marker: String,
        body: String,
    },
    /// Resolve the identified thread, bound to the head its findings
    /// were examined at. A reply is not a resolution.
    ResolveThread {
        change: ChangeRequestRef,
        thread_id: String,
        head: CommitRevision,
    },
    /// Mark the change request ready (non-draft), bound to an exact
    /// head.
    MarkReady {
        change: ChangeRequestRef,
        expected_head: CommitRevision,
    },
}

impl HostAction {
    /// The operation name used for capability/eligibility bookkeeping
    /// and failure receipts — the contract operation names, stable
    /// across payload changes.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PushRef { .. } => "push_ref",
            Self::OpenChangeRequest { .. } => "open_change_request",
            Self::UpdateChangeRequest { .. } => "update_change_request",
            Self::PostComment { .. } => "post_comment",
            Self::PublishReviewFeedback { .. } => "publish_review_feedback",
            Self::ReplyToThread { .. } => "thread_reply",
            Self::ResolveThread { .. } => "resolve_thread",
            Self::MarkReady { .. } => "mark_ready",
        }
    }
}

/// Which path requested one intake invocation. Closed vocabulary —
/// persisted as snake_case tags and CHECK-constrained in SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeTrigger {
    /// One-shot CLI invocation.
    Manual,
    /// Automatic intake enabled at driver start.
    ScheduledStartup,
    /// A periodic due tick.
    ScheduledInterval,
    /// Disabled→enabled transition invoking immediately.
    ScheduledReenable,
}

/// Lifecycle of one durable intake run
/// (`docs/intake-identity-storage.md`). `Open`/`Admitting` are the only
/// nonterminal states; at most one run may hold either at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeRunState {
    /// Invocation owned; the adapter call is in flight or its response
    /// is being validated. No validated batch exists yet.
    Open,
    /// A whole-response-validated candidate batch committed; submits are
    /// in progress.
    Admitting,
    /// Every candidate reached a recorded terminal status.
    Completed,
    /// Invocation-level failure before a validated batch; `detail_code`
    /// carries the bounded failure code.
    Failed,
    /// The owning invocation cannot continue: recovery proved the
    /// owner gone, or the live owner itself aborted (a propagating
    /// store fault or unresolved candidates). `detail_code` carries
    /// which.
    Interrupted,
    /// Recorded without an adapter launch (e.g. zero free pending
    /// positions at preflight). Terminal at insert.
    Skipped,
}

/// Outcome recorded for one validated candidate. `Pending` and
/// `Unknown` are the only non-final states; the rest are terminal and
/// carry their decision links where one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeCandidateStatus {
    /// Validated, not yet decided in this run.
    Pending,
    /// This run's submit freshly accepted; new Work created.
    Accepted,
    /// Submit returned the saved decision for this replay identity.
    Replayed,
    /// Ordinary final rejection; `status_reason` carries the code.
    Rejected,
    /// Refused for capacity before any decision was recorded;
    /// deferrable, never a saved rejection.
    CapacityDeferred,
    /// A submit's outcome is undetermined; reconcile via the command
    /// ledger under `replay_key`.
    Unknown,
    /// The run was interrupted before this candidate resolved.
    Interrupted,
}

/// One durable intake invocation: who asked (trigger), which adapter
/// and revisions ran, what was requested (`request_payload` +
/// `request_digest`), who owned it, and where it ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeRun {
    pub run_id: String,
    pub trigger: IntakeTrigger,
    pub selector_id: AdapterId,
    pub selector_version: u32,
    pub policy_revision: u64,
    /// Opaque installation intake-settings revision; audit provenance
    /// only, never part of proposal or replay identity.
    pub settings_revision: Option<String>,
    pub state: IntakeRunState,
    /// Bounded diagnostic code for `failed`/`interrupted`/`skipped`.
    pub detail_code: Option<String>,
    /// Canonical JSON bytes of the complete request parameters,
    /// including the full exclusion snapshot.
    pub request_payload: Vec<u8>,
    pub request_digest: String,
    /// The lease owner identity this run was created under.
    pub owner_pid: i32,
    pub owner_started_at: String,
    pub owner_nonce: String,
    pub created_unix_seconds: i64,
    pub finished_unix_seconds: Option<i64>,
    pub recorded_seq: EventSequence,
}

/// One validated candidate inside an intake run, in the adapter's
/// returned order (`ordinal`). Identity fields are immutable after
/// insert; the status and decision-link fields are the only mutable
/// surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeCandidate {
    pub run_id: String,
    pub ordinal: u32,
    /// The proposal-v1 semantic digest; globally unique per semantic
    /// proposal.
    pub proposal_key: String,
    /// `proposal/<selector_id>:<proposal_key>` — the submit command's
    /// idempotency key.
    pub replay_key: String,
    /// Canonical proposal-v1 semantic JSON bytes.
    pub payload: Vec<u8>,
    pub payload_digest: String,
    pub status: IntakeCandidateStatus,
    /// Bounded reason: rejection code, deferral cause, or diagnostic.
    pub status_reason: Option<String>,
    /// The durable command decision this candidate resolved to, when
    /// one exists. NULL for pending/deferred/unknown/interrupted.
    pub command_id: Option<CommandId>,
    /// The Work the decision created or replayed, when accepted.
    pub work_id: Option<WorkId>,
    /// The deciding command's `committed_at` sequence.
    pub committed_seq: Option<EventSequence>,
    pub recorded_seq: EventSequence,
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
/// and the evidence generation when recorded evidence shows it can
/// never fire.
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
    /// The evidence generation a retirement was decided from; `None`
    /// while the intent is pending or consumed.
    pub retirement_generation: Option<u64>,
    pub scheduled_seq: EventSequence,
}

/// One durable command decision reduced to what a read-only census needs:
/// the Command kind and Work/Execution target parsed from the canonical
/// payload, whether the decision was accepted, and the committed
/// sequence. `kind` is `None` when the stored payload could not be
/// decoded — an unreadable record the caller must surface, never
/// silently drop; `work_id`/`execution_id` are `None` both for
/// undecodable payloads and for commands that legitimately carry no
/// such target. `accepted` is `false` for an undecodable decision: an
/// unreadable decision can never prove a release happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRecord {
    pub command_id: CommandId,
    pub kind: Option<String>,
    pub work_id: Option<WorkId>,
    /// The Execution the command targets: `execution_id` for
    /// resume/cancel, `expected_execution_id` for retry; `None` for
    /// Work-scoped commands.
    pub execution_id: Option<ExecutionId>,
    pub accepted: bool,
    /// The decision's `committed_at` — the sequence its effects were
    /// anchored at. For an accepted resume/release this equals the
    /// `enqueued_seq` of the Execution generation the command created,
    /// which is how a census scopes release evidence to the current
    /// wait. `None` when the stored decision could not be decoded.
    pub committed_at: Option<EventSequence>,
    pub seq: EventSequence,
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

    /// At most `limit` distinct idempotency keys carrying a saved
    /// decision for `(origin, actor)` that start with `prefix`, in key
    /// order. Intake uses this to size a selector request beyond the
    /// known replay identities: re-presenting a saved decision creates
    /// no new pending position, so a replay must not consume the
    /// budget that new proposals need.
    ///
    /// The prefix and limit keep the read bounded: the commands ledger
    /// is never pruned, so an unbounded read would grow with every
    /// admission. Only as many keys as the ask can use are returned.
    ///
    /// The default has no durable history to report; the SQLite store
    /// overrides it with the command ledger.
    fn decided_idempotency_keys(
        &self,
        _origin: &Origin,
        _actor: &Actor,
        _prefix: &str,
        _limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

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
    /// Every durable request row for `execution`, consumed or not, in
    /// committed order. Inspection needs the consumed-by-gate rows too:
    /// a request can be consumed with neither an Invocation nor a
    /// dispatch-refusal fact, so the pending set alone would hide it.
    ///
    /// The default has no durable request log to enumerate; the SQLite
    /// store overrides it with the invocation_requests table.
    fn requests_for_execution(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Vec<InvocationRequest>, StoreError> {
        Ok(Vec::new())
    }
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

    /// Resolved v2 Invocations whose required process observations are
    /// recorded but whose bounded Worker-report collection has not
    /// recorded an accepted fact yet — the durable retry set for a pass
    /// that committed the terminal observation and then crashed before
    /// ingestion. Stores that cannot enumerate them return none and
    /// rely on the supervisor's inline collection at finish.
    fn pending_report_invocations(&self) -> Result<Vec<Invocation>, StoreError> {
        Ok(Vec::new())
    }

    fn works(&self) -> Result<Vec<Work>, StoreError>;
    fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError>;

    fn stop_intent(&self, invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError>;

    /// The durable handoff record for one artifact id; `None` = absent.
    fn artifact_handoff(&self, artifact_id: &str) -> Result<Option<ArtifactHandoff>, StoreError>;
    /// The durable WorkerRequest export recorded for a deterministic
    /// request identity; `None` = absent. The supervisor writes this
    /// binding at launch; callers (tests today, lost-start reconciliation
    /// later) read it back to resolve the spawned writer. The running
    /// Invocation row remains the primary reconciliation key; this binding
    /// is the durable audit record of the launch.
    fn worker_request_export(
        &self,
        request_id: &str,
    ) -> Result<Option<WorkerRequestExport>, StoreError>;
    /// The canonical executable request body recorded for a durable
    /// dispatch request; `None` = absent (a legacy v1 dispatch or a
    /// request recorded before the v2 wire existed). Callers replay the
    /// bytes verbatim; they never reconstruct a body.
    fn worker_request_body(
        &self,
        request_id: &str,
    ) -> Result<Option<crate::worker_v2::WorkerRequestBody>, StoreError>;
    /// The immutable executor binding recorded for a remote request;
    /// `None` = absent (the request is local-bound or predates remote
    /// placement). A corrupt row is an error, never silently absent —
    /// a missing binding refuses dispatch, a forged one must too.
    fn executor_binding(
        &self,
        request_id: &str,
    ) -> Result<Option<crate::executor::ExecutorBinding>, StoreError>;
    /// The durable operation record for one command identity;
    /// `None` = absent. Replay and reconciliation read this before any
    /// endpoint contact.
    fn executor_operation(
        &self,
        request_id: &str,
        operation_id: &str,
    ) -> Result<Option<crate::executor::ExecutorOperation>, StoreError>;
    /// Every durable operation record for one request, ordered by
    /// `operation_id` — the deterministic reconciliation set a
    /// restart replays against.
    fn executor_operations_for_request(
        &self,
        request_id: &str,
    ) -> Result<Vec<crate::executor::ExecutorOperation>, StoreError>;
    /// The durable remote-collection record for one request; `None` =
    /// absent (nothing collected or declared). A corrupt row is an
    /// error, never silently absent — a collection that cannot prove
    /// its durable record is never `acknowledged` (contract §6, #376).
    /// Stores that cannot enumerate them return `None` and rely on the
    /// collection driver's explicit re-check.
    fn executor_collection(
        &self,
        _request_id: &str,
    ) -> Result<Option<crate::executor::ExecutorCollection>, StoreError> {
        Ok(None)
    }
    /// Collection rows the collection/acknowledgement sweep must
    /// advance: everything not yet `acknowledged` or `invalid`.
    /// Stores that cannot enumerate them return none.
    fn incomplete_executor_collections(
        &self,
    ) -> Result<Vec<crate::executor::ExecutorCollection>, StoreError> {
        Ok(Vec::new())
    }
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

    /// One intake run by id.
    fn intake_run(&self, run_id: &str) -> Result<Option<IntakeRun>, StoreError>;
    /// All intake runs in recorded order.
    fn intake_runs(&self) -> Result<Vec<IntakeRun>, StoreError>;
    /// The single nonterminal (`open`/`admitting`) intake run, if any.
    fn active_intake_run(&self) -> Result<Option<IntakeRun>, StoreError>;
    /// One run's candidates in original ordinal order.
    fn intake_candidates(&self, run_id: &str) -> Result<Vec<IntakeCandidate>, StoreError>;
    /// Every candidate row for one proposal identity, across runs.
    fn intake_candidates_for_proposal(
        &self,
        proposal_key: &str,
    ) -> Result<Vec<IntakeCandidate>, StoreError>;
    /// Every candidate that linked one command decision, oldest
    /// first — a replayed proposal links the same saved decision, so
    /// the original recorder is the first row. Recovers the original
    /// outcome after a lost response.
    fn intake_candidates_for_command(
        &self,
        command_id: &CommandId,
    ) -> Result<Vec<IntakeCandidate>, StoreError>;
    /// Candidates in any of the given statuses — the recovery sweep's
    /// work list (`pending`/`unknown`).
    fn intake_candidates_by_status(
        &self,
        statuses: &[IntakeCandidateStatus],
    ) -> Result<Vec<IntakeCandidate>, StoreError>;

    /// The active intake-settings record, if one was ever written.
    /// Absent resolves to the documented default pending-queue limit.
    fn intake_settings(&self) -> Result<Option<IntakeSettingsRecord>, StoreError>;

    /// Distinct Works whose active Execution is `queued` — the pending
    /// queue occupancy the install-wide admission limit counts.
    /// Executing and paused states hold no queue position.
    fn pending_queue_count(&self) -> Result<u64, StoreError>;

    /// Live owners of the scoped issues for the intake preflight: one
    /// row per `(issue, Work)` whose immutable membership covers the
    /// issue and whose active Execution is non-terminal, ordered by
    /// issue key then Work id.
    fn intake_source_owners(
        &self,
        issues: &[IssueRef],
    ) -> Result<Vec<IntakeSourceOwner>, StoreError>;

    /// Every scoped issue with a live owner — the same
    /// non-terminal owner set [`Self::intake_source_owners`]
    /// reports, unfiltered. The intake coordinator needs the complete
    /// owned set *before* it can scope a request's exclusions, so this
    /// read exists independently of any candidate issue list.
    fn intake_owned_issues(&self) -> Result<Vec<IssueRef>, StoreError>;

    /// Every durable command decision, oldest first, reduced to
    /// [`CommandRecord`]. Read-only evidence for queries that must know
    /// which Commands were ever accepted (e.g. whether a released wait
    /// ever saw its resume/retry).
    fn commands(&self) -> Result<Vec<CommandRecord>, StoreError>;

    fn last_seq(&self) -> Result<EventSequence, StoreError>;
}

/// Write side of a transaction.
pub trait TxView: ReadView {
    /// Monotonic durable position; also the queue order key.
    fn alloc_seq(&mut self) -> Result<EventSequence, StoreError>;
    /// Monotonic counter position for ordering records that must not
    /// mint an `event_log` row: draws from the same durable `id_counters`
    /// space as [`TxView::alloc_id`], bumped in place.
    fn next_counter(&mut self, name: &str) -> Result<i64, StoreError>;
    /// Overwrites only the merge-check service cursors on the Work's
    /// usage row — an in-place position update, deliberately not an
    /// audited [`Effect`]: one bounded cell per Work instead of one
    /// event row per pair per pass.
    fn write_merge_check_services(
        &mut self,
        work_id: &WorkId,
        services: &[MergeCheckService],
    ) -> Result<(), StoreError>;
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

/// A catalogue that resolves nothing: stores without retained-body
/// support hand this to [`Store::body_catalog`], so a v2 dispatch on
/// such a store refuses `request.snapshot_missing` — fail-closed —
/// rather than silently skipping body verification.
struct NoBodyCatalog;

static NO_BODY_CATALOG: NoBodyCatalog = NoBodyCatalog;

impl BodyCatalog for NoBodyCatalog {
    fn body_entry(&self, _body_id: &str) -> Result<Option<BodyCatalogEntry>, StoreError> {
        Ok(None)
    }
    fn body_entry_at(
        &self,
        _category: &str,
        _path: &str,
    ) -> Result<Option<BodyCatalogEntry>, StoreError> {
        Ok(None)
    }
    fn register_body(&self, _body: &NewCatalogBody) -> Result<(), StoreError> {
        Err(StoreError::Internal {
            detail: "this store has no retained-body catalogue".into(),
        })
    }
    fn set_body_pin(
        &self,
        _body_id: &str,
        _dependency: &str,
        _pinned: bool,
    ) -> Result<(), StoreError> {
        Err(StoreError::Internal {
            detail: "this store has no retained-body catalogue".into(),
        })
    }
}

/// The store port. `work` calls `transact` once per Command; the closure may
/// run more than once if the implementation retries serialization errors,
/// so it must be deterministic over the view it reads.
pub trait Store {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError>;

    /// The retained-body catalogue backing this store, when it has
    /// one. The v2 dispatch path resolves required input bodies
    /// through it; a store without catalogue support resolves every
    /// body as absent, which the dispatch builder turns into a typed
    /// refusal rather than a skipped check.
    fn body_catalog(&self) -> &dyn BodyCatalog {
        &NO_BODY_CATALOG
    }

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

/// One retained-body catalogue row as the dispatch path reads it:
/// durable metadata plus the availability state the retention sweep
/// owns. `path` is the body file's location relative to the configured
/// bodies root; `state` is the catalogue lifecycle
/// (`present`/`reclaiming`/`reclaimed`/`unavailable`) — anything but
/// `present` means the bytes must not be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyCatalogEntry {
    pub body_id: String,
    pub category: String,
    pub path: String,
    pub bytes: u64,
    pub state: String,
    pub pinned: bool,
}

/// A body a producer registers: the catalogue metadata plus the file
/// the producer already wrote under the bodies root. Registration is
/// durable metadata only — it never touches the filesystem, so callers
/// write the file first and register second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCatalogBody {
    pub body_id: String,
    pub category: String,
    pub path: String,
    pub bytes: u64,
    pub registered_unix_seconds: i64,
    pub eligible_after_unix_seconds: Option<i64>,
    pub pinned: bool,
}

/// The retained-body catalogue port (`docs/retention-policy.md` CAS
/// objects). Separate from [`Store`]: catalogue rows are producer-side
/// metadata that outlive any one transaction, so reads and writes go
/// through this port rather than `TxView`. Resolution is fail-closed —
/// an absent entry, a non-`present` state, or unreadable bytes all
/// refuse dispatch.
pub trait BodyCatalog {
    /// The catalogue row for `body_id`, or `None` when never
    /// registered.
    fn body_entry(&self, body_id: &str) -> Result<Option<BodyCatalogEntry>, StoreError>;
    /// The catalogue row for one `(category, path)` pair, or `None`.
    /// Retained bodies whose identity is a deterministic lookup path —
    /// issue snapshots resolve by the spec-bound identity, not by
    /// content address — are found through this read.
    fn body_entry_at(
        &self,
        category: &str,
        path: &str,
    ) -> Result<Option<BodyCatalogEntry>, StoreError>;
    /// Register a produced body. Re-registering identical metadata is
    /// a replay; conflicting metadata for the same `body_id` is a
    /// `ConstraintConflict` — a content address never changes.
    fn register_body(&self, body: &NewCatalogBody) -> Result<(), StoreError>;
    /// Add or remove one protective dependency on a body. Pin
    /// creation on a body that already left `present` is refused.
    fn set_body_pin(&self, body_id: &str, dependency: &str, pinned: bool)
    -> Result<(), StoreError>;
}
