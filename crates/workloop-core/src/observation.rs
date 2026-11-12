use serde::{Deserialize, Serialize};

use crate::domain::{EventSequence, ExecutionId, InvocationId, WorkId};
use crate::identity::{
    ChangeRequestRef, CommitRevision, DiffSnapshot, DiffTarget, EvidenceRef, HostActorRef,
    HostCapability, HostInstanceId, IssueRef, RepositoryRef,
};

/// Delivery identity for transports that assign one (webhooks, worker
/// reports). The scope disambiguates which authority issued the id, so a
/// host event id never deduplicates a local one and vice versa. An
/// observation without delivery identity never deduplicates.
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
