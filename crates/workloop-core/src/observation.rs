use serde::{Deserialize, Serialize};

use crate::domain::{EventSequence, ExecutionId, InvocationId, WorkId};
use crate::identity::{
    ChangeRequestRef, CommitRevision, DiffSnapshot, DiffTarget, EvidenceRef, HostActorRef,
    HostCapability, HostInstanceId, IssueRef, RepositoryRef,
};
use crate::worker_v2::ReportClassification;

/// Delivery identity for transports that assign one (webhooks, worker
/// reports). The scope disambiguates which authority issued the id, so a
/// host event id never deduplicates a local one and vice versa. An
/// observation without delivery identity never deduplicates. The store
/// dedupes per `(scope, source, event_id)` only — the subject is not
/// part of the key — so `event_id` must be unique per distinct subject
/// observation: two subjects reporting the same host state (for example
/// Works polling a shared target branch) need different ids, or the
/// second delivery is dropped as a duplicate and that subject never
/// records the fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryId {
    pub scope: DeliveryScope,
    /// The adapter source namespace (e.g. "github.webhook",
    /// "fixture.sync").
    pub source: String,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryScope {
    /// A host-side event id, scoped to the host instance.
    Host { host: HostInstanceId },
    /// A local id from this store's adapters (worker reports, probes).
    Local,
}

/// A fact reported by a worker, a process probe, a host adapter, or an
/// external system. Facts are inputs to `work`'s judgment, never state
/// themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Deduplication identity; uniqueness is enforced by the store per
    /// `(scope, source, event_id)`. `None` never deduplicates.
    #[serde(default)]
    pub delivery: Option<DeliveryId>,
    /// The Work or Execution this fact is about; None keeps it a system fact.
    pub subject_work_id: Option<WorkId>,
    pub subject_execution_id: Option<ExecutionId>,
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
        /// The proven exit status the supervisor collected when it
        /// reaped the writer's own exit — `Some(0)` is the only
        /// positive completion evidence a gate may count. `None`
        /// means the process resolved without a proven status
        /// (reparented, ended by a signal, or reported by a path that
        /// never held the child); it is never a pass.
        #[serde(default)]
        exit_code: Option<i32>,
    },
    Artifact {
        invocation_id: InvocationId,
        required_artifact: String,
        state: ArtifactFact,
    },
    /// The worker runtime definitely refused the launch before any
    /// process existed — distinct from `ProcessState::Unknown`.
    LaunchRefused {
        invocation_id: InvocationId,
        reason: String,
    },
    /// The launch's leader was spawned into this process group. The
    /// supervisor commits it the moment the spawn call returns —
    /// before the running commit — so an unidentified launch keeps a
    /// durable handle on its group even when the leader's identity
    /// commit never landed. The unidentified-launch absence proof
    /// reads it back and enumerates the group for descendants that
    /// escaped the signature and directory scans without calling
    /// `setsid`.
    LaunchGroup {
        invocation_id: InvocationId,
        pgid: i32,
        /// The leader's start as captured at report time: `pgid` is
        /// the leader's pid, so the group member at that slot is the
        /// owner identity the absence proof verifies the group
        /// against — a live process there with a different start means
        /// the number was recycled after the leader died and the whole
        /// group is a foreign family. `None` when the spawn could not
        /// capture it; the owner slot is then unverifiable and the
        /// member check stays conservative.
        #[serde(default)]
        leader_started_at: Option<String>,
    },
    /// The bounded escalation for an unidentified launch whose
    /// absence scans could not be completed a bounded number of
    /// consecutive times. The launch stays an unresolved writer — a
    /// live process may still exist, so the slot is never released —
    /// but the unprovable absence is surfaced to the operator as a
    /// distinct outcome instead of an indefinite `uncertain` hold.
    /// The escalation surfaces the outcome; it does not conclude it:
    /// the absence scans keep running underneath, so a later provable
    /// answer still resolves the writer.
    LaunchAbsenceUnprovable {
        invocation_id: InvocationId,
        reason: String,
    },
    /// The supervisor refused dispatch before any Invocation existed:
    /// a required input body was missing, stale, corrupt, oversized,
    /// or otherwise undeliverable. `code` is one of the closed
    /// `request.*` dispatch-refusal codes; no process ever ran for
    /// the request.
    DispatchRefused {
        request_id: String,
        code: String,
        detail: String,
    },
    /// The supervisor-collected `taisan.worker.report/2` evidence for
    /// one v2 Invocation (`docs/v0.3.md` §6): the classification the
    /// envelope earned against the exact durable request binding,
    /// plus references to the retained report/log bodies. Claims
    /// inside the report — `claimed_head`, `claimed_checks`,
    /// `findings`, `invariant_items` — stay claims: this fact is
    /// evidence for later gates, never authority over lifecycle,
    /// artifacts, or completion.
    WorkerReport {
        invocation_id: InvocationId,
        request_id: String,
        classification: ReportClassification,
        /// The retained report evidence: CAS body id of the exact
        /// bytes the supervisor read, the retained byte count, and
        /// whether the on-disk file exceeded the read bound (an
        /// oversize report keeps a bounded truncated prefix, never
        /// the tail it did not read). `None` whenever no bytes were
        /// retained — the `absent` class, and `invalid`
        /// classifications whose path was unsafe or unreadable.
        report: Option<WorkerReportEvidence>,
        /// The measured `runtime.log` retention under the same
        /// bounded-read rule; `None` when no regular log file existed
        /// or the path was unsafe or unreadable — the worker's own
        /// `log` descriptor stays a claim inside the report body.
        log: Option<WorkerReportEvidence>,
    },
    /// A different report-byte identity observed for an Invocation
    /// that already carries accepted `WorkerReport` evidence. The
    /// conflict is retained alongside — the accepted bytes are never
    /// overwritten and the new bytes are never silently adopted.
    WorkerReportConflict {
        invocation_id: InvocationId,
        request_id: String,
        /// The accepted report evidence identity (`None` = the
        /// accepted fact retained no report bytes — an `absent`
        /// report, or an invalid one whose path was unsafe or
        /// unreadable).
        accepted_report: Option<WorkerReportEvidence>,
        /// The conflicting identity now observed (`None` = no bytes
        /// were retained this collection — the report file is gone,
        /// or its path is unsafe or unreadable).
        observed_report: Option<WorkerReportEvidence>,
    },
    /// The supervisor's verdict on one retained Worker `claimed_head`
    /// checked against an acknowledged handoff's `produced` commit
    /// (`docs/v0.3.md` §6.3). The claim stays evidence: `agrees`
    /// records whether the claim named the commit the handoff
    /// actually promoted, and a `false` verdict never moves,
    /// replaces, or blocks the artifact — the acknowledged ref plus
    /// supervisor-run verification on that exact commit remain the
    /// only authority.
    ClaimedHeadCheck {
        invocation_id: InvocationId,
        /// The artifact whose `produced` commit was compared — one
        /// Invocation may hand off several required artifacts, each
        /// with its own promoted commit.
        artifact_id: String,
        /// The commit the Worker claimed (always present — a report
        /// carrying no `claimed_head` records no verdict at all).
        claimed: String,
        /// The commit the acknowledged handoff promoted.
        produced: String,
        /// `claimed == produced`.
        agrees: bool,
    },
    /// The artifact handoff became durable: `produced` is the commit
    /// the acknowledged handoff promoted. Recorded when the supervisor
    /// cannot also form a `ClaimedHeadCheck` verdict — no retained
    /// Worker report, an `absent`/`invalid` classification,
    /// unavailable report bytes, or a report without `claimed_head` —
    /// so the acknowledgement still re-drives publication for a
    /// waiting Execution.
    HandoffAcknowledged {
        artifact_id: String,
        produced: String,
    },
    /// An authoritative read of a source issue. Drives the source gate:
    /// compared against the approved spec snapshot, never trusted by
    /// position or timestamp alone.
    Issue {
        issue: IssueRef,
        /// The host's version token for this read.
        source_revision: String,
        content_digest: String,
        state: IssueState,
    },
    /// A host-reported capability report. `authorization` combines
    /// adapter support with credential rights; `denied` or `unknown`
    /// never authorizes an effect.
    CapabilityReport {
        host: HostInstanceId,
        capabilities: Vec<CapabilityEntry>,
    },
    /// The current head of a change request.
    ChangeHead {
        change: ChangeRequestRef,
        head: CommitRevision,
    },
    /// A branch's current revision — tracks target-branch movement so a
    /// diff target moves when only the target branch does.
    BranchState {
        repository: RepositoryRef,
        branch: String,
        state: BranchState,
    },
    /// One named check conclusion on an exact head.
    Check {
        change: ChangeRequestRef,
        head: CommitRevision,
        check_id: String,
        producer: HostActorRef,
        conclusion: CheckConclusion,
    },
    /// One review collection. On ingest, a `complete` review seals an
    /// immutable round for its exact diff target; an identical set on a
    /// re-delivery is idempotent, a different set installs a new round.
    Review {
        target: DiffTarget,
        /// The reviewing source identity (collector or reviewer id).
        source: String,
        source_revision: Option<String>,
        completeness: Completeness,
        occurrences: Vec<OccurrenceInput>,
    },
    /// One invariant-lane assessment for an exact diff target. A lane
    /// passes only on `complete` assessments whose every required item
    /// is `positive`; omissions and `inconclusive` items cannot pass.
    Assessment {
        target: DiffTarget,
        /// Digest over the assessed invariant id set.
        set_digest: String,
        complete: bool,
        items: Vec<AssessmentItem>,
    },
    /// One durable stage-pipeline record. The loop writes an entry record
    /// when it enters a stage and terminal records as evidence arrives;
    /// the fact itself never authorizes a transition. Core persists these
    /// records through its own transaction — `handle_observation` refuses
    /// a delivered record, since ingest carries no authority to write
    /// pipeline history.
    StageEvidence {
        evidence: crate::stage::StageEvidence,
    },
    /// The normalized result for a recorded `HostActionIntent`. An
    /// `applied` result must carry at least one normalized effect fact
    /// matching the intent's immutable payload — an unsupported claim
    /// never marks the intent applied.
    HostActionResult {
        action_id: String,
        /// The actor the host authenticated, when reported; recorded so
        /// a receipt that ran as the wrong actor can be flagged.
        #[serde(default)]
        authenticated_actor: Option<HostActorRef>,
        status: HostActionStatus,
        effects: Vec<NormalizedEffectFact>,
    },
    /// One typed host read — the `observe` half of the host port. Binds
    /// the subject, the exact revision it read, the actor the host
    /// reports, and the read's completeness. `subject_version` is the
    /// remote's version of that subject at read time: ordering is by
    /// recorded revision, never delivery order, so a delayed older read
    /// is retained as history and can never move the recorded revision
    /// backward.
    HostRead {
        subject: ReadSubject,
        /// `None` when the read could not produce its required revision
        /// form — paired with `Partial`/`Unavailable` completeness and
        /// never treated as current.
        revision: Option<ReadRevision>,
        subject_version: u64,
        actor: HostActorRef,
        completeness: Completeness,
    },
}

/// A typed host read subject: the variant names the subject kind, the
/// fields carry the scoped subject identity. Ordering and equality are
/// over the full identity — the same display number or branch name on
/// another repository is a different subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadSubject {
    /// The repository itself: its recorded default-branch head.
    Repository { repository: RepositoryRef },
    /// One allowlisted issue's authoritative content read.
    Issue { issue: IssueRef },
    /// A change request's head-bound read.
    Pr { change: ChangeRequestRef },
    /// A branch/ref head inside one repository.
    Head {
        repository: RepositoryRef,
        branch: String,
    },
    /// An immutable diff snapshot for one `(base, head)` pair — each
    /// distinct pair is a distinct subject, so a snapshot is never
    /// revised in place.
    Diff { snapshot: DiffSnapshot },
    /// One named check on one change request.
    Check {
        change: ChangeRequestRef,
        check: String,
    },
    /// A change request's review stream.
    Review { change: ChangeRequestRef },
}

impl ReadSubject {
    /// A deterministic scoped key for adapter-side bookkeeping (remote
    /// version maps, delivery identities). Repository-scoped identities
    /// stay scoped — display metadata takes no part in it.
    pub fn key(&self) -> String {
        match self {
            Self::Repository { repository } => format!("repo:{repository}"),
            Self::Issue { issue } => format!("issue:{issue}"),
            Self::Pr { change } => format!("pr:{change}"),
            Self::Head { repository, branch } => format!("head:{repository}:{branch}"),
            Self::Diff { snapshot } => format!("diff:{}", snapshot.snapshot_id()),
            Self::Check { change, check } => format!("check:{change}:{check}"),
            Self::Review { change } => format!("review:{change}"),
        }
    }
}

/// The per-kind revision a host read observed. The form is fixed by the
/// subject kind; a read that cannot produce its required form records a
/// partial completeness instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadRevision {
    /// `repository` (default-branch head), `head` (branch head) and
    /// `pr` (change head): the exact commit revision observed.
    Sha { revision: CommitRevision },
    /// `issue`: the host's content revision token plus content digest.
    Issue {
        source_revision: String,
        content_digest: String,
    },
    /// `check`: the check-run identity plus the exact head it examined.
    Check {
        check_run_id: String,
        head: CommitRevision,
    },
    /// `review`: the newest review identity the stream reports.
    Review { review_id: String },
    /// `diff`: the immutable snapshot identity of the `(base, head)`
    /// pair read.
    DiffSnapshot { snapshot_id: String },
}

/// One reported capability: support is adapter capability,
/// authorization is credential rights for the bound actor. Both must be
/// `supported`/`authorized` for Core to treat the capability as held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub capability: HostCapability,
    pub support: CapabilityState,
    pub authorization: CapabilityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Authorized,
    Denied,
    Unknown,
}

impl CapabilityEntry {
    /// Held only when the adapter reports support and the credential is
    /// authorized; `unknown` or `denied` in either field is not held.
    pub fn usable(&self) -> bool {
        matches!(self.support, CapabilityState::Supported)
            && matches!(self.authorization, CapabilityState::Authorized)
    }

    pub fn denied(&self) -> bool {
        matches!(self.support, CapabilityState::Denied)
            || matches!(self.authorization, CapabilityState::Denied)
    }
}

/// How complete a review delivery is. A `partial`/`unavailable` review
/// is recorded but can never satisfy a requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial { reason: String },
    Unavailable { reason: String },
}

impl Completeness {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One finding as delivered by the source. `source_key` is the
/// source-supplied stable key; Core assigns the install-scoped
/// occurrence id at seal time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceInput {
    pub source_key: String,
    #[serde(default)]
    pub content_digest: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// The human-readable finding text the claim carried — what the
    /// reviewer found, so operators and Fix workers read the sealed
    /// evidence rather than only its key. Absent on facts recorded
    /// before the field existed and on adapter-minted occurrences
    /// that carry no prose.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentItem {
    /// The required invariant id.
    pub id: String,
    pub verdict: AssessmentVerdict,
    #[serde(default)]
    pub refs: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentVerdict {
    Positive,
    Negative,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStateFact {
    Running,
    Exited,
    Unknown,
}

/// `Present` must carry a digest: the artifact's exact payload identity.
/// A `Corrupt` artifact is never `missing` — it is retained with its
/// reason and invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFact {
    Present { digest: String },
    Missing,
    Corrupt { reason: Option<String> },
}

/// One retained worker-output body referenced by a `WorkerReport`
/// fact: the CAS content id of the exact bytes the supervisor read,
/// how many bytes were retained, and whether the on-disk file
/// exceeded the read bound — a truncated retention is honest about
/// the tail it never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerReportEvidence {
    /// The CAS `content_id` (`retained-body-cas-v1` digest) of the
    /// retained bytes — resolves through the body catalogue.
    pub body_id: String,
    /// Retained byte count (may be below the file's true size when
    /// `truncated`).
    pub bytes: u64,
    /// The file exceeded the invocation's budget bound; only the
    /// bounded prefix was retained.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
    /// The read could not be completed; distinct from a missing issue.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    At { revision: CommitRevision },
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Pending,
}

/// The claimed result of a host action. `Applied` is only honored when
/// the accompanying normalized effect facts match the recorded intent.
/// `Pending` is the recorded-not-yet-attempted state of an outbox intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostActionStatus {
    Pending,
    Applied,
    /// A definite no-effect failure: safe to retry, counts transport.
    Failed {
        reason: String,
    },
    /// The host's capability map does not contain the operation:
    /// terminal and explicit, never a permission verdict, and never
    /// retried as a transport failure.
    Unsupported {
        reason: String,
    },
    /// The action's effect cannot be determined; never treated as
    /// success and never silently re-attempted.
    Unknown {
        reason: String,
    },
}

/// How the observed ref relates to the produced revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefRelation {
    Equal,
    /// The observed ref descends from the produced revision — the push
    /// succeeded and someone (possibly us) added commits on top.
    Ancestor,
    /// The observed ref does not contain the produced revision — the
    /// recorded produced revision was not landed.
    Diverged,
}

/// A normalized effect fact inside a host-action receipt. Each variant
/// names the exact subject so a receipt can be matched against the
/// recorded intent's immutable payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NormalizedEffectFact {
    RefPublished {
        repository: RepositoryRef,
        branch: String,
        produced: CommitRevision,
        observed: CommitRevision,
        relation: RefRelation,
    },
    ChangeOpened {
        change: ChangeRequestRef,
        head: CommitRevision,
        source_branch: String,
        target_branch: String,
    },
    MetadataUpdated {
        change: ChangeRequestRef,
        head: CommitRevision,
        fields_digest: String,
    },
    CommentPosted {
        change: ChangeRequestRef,
        comment_id: String,
        actor: HostActorRef,
        marker: String,
        body_digest: String,
        target: Option<DiffTarget>,
    },
    ReviewPublished {
        change: ChangeRequestRef,
        review_id: String,
        head: CommitRevision,
        body_digest: String,
    },
    ReplyPosted {
        change: ChangeRequestRef,
        thread_id: String,
        reply_id: String,
        marker: String,
        body_digest: String,
    },
    /// `target` echoes the diff target the write was bound to;
    /// `observed` is the head a fresh read reports after the write, and
    /// `relation` records how it relates to `target.head` — a racing
    /// actor may move the change across the write/read gap.
    ThreadResolved {
        change: ChangeRequestRef,
        thread_id: String,
        target: DiffTarget,
        observed: CommitRevision,
        relation: RefRelation,
    },
    /// `head` echoes the head the write was bound to; `observed` is the
    /// head a fresh read reports after the write, and `relation`
    /// records how it relates to `head`.
    ReadyObserved {
        change: ChangeRequestRef,
        head: CommitRevision,
        observed: CommitRevision,
        relation: RefRelation,
    },
}

/// An evidence-backed assessment. `succeeded` requires verified
/// current-target evidence and no unresolved writer, gate, or outgoing
/// host action. When the declared requirement set is nonempty,
/// `evidence_refs` must be nonempty typed references — never an empty
/// untyped list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub state: crate::domain::ExecutionState,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub evidence_refs: Vec<EvidenceRef>,
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
    /// No material progress beyond the policy window. A signal for
    /// human attention, never proof of termination.
    NoProgress,
}
