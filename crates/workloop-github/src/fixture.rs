//! Fixture host adapter: records `HostActionIntent` outbox rows and emits
//! synthetic `issue`/`change_head`/`host_action_result` Observations
//! under test control.
//!
//! The adapter never mutates Execution state itself — every fact goes
//! through `work::handle_observation` so the outbox row, the head
//! recheck, and the lifecycle assessment land in the same durable
//! pipeline a real adapter would use.

use std::collections::BTreeMap;
use std::path::Path;

use workloop_core::domain::{ExecutionId, WorkId, new_id};
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef, DiffTarget, HostActorRef,
    IssueRef, RepositoryRef, SourceSnapshot,
};
use workloop_core::mergeability::{
    MAX_CONFLICT_RECHECKS_PER_PASS, MAX_MERGE_CHECKS_PER_PASS, MAX_MERGE_RECHECKS_PER_PASS,
    MergeabilityState,
};
use workloop_core::observation::{
    BranchState, Completeness, DeliveryId, DeliveryScope, HostActionStatus, IssueState,
    NormalizedEffectFact, Observation, ObservationFact, ReadRevision, ReadSubject, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};
use workloop_selector::collect::{CollectedIssue, IssuePage, SourceReadError, SourceReader};

use crate::credential::{
    CredentialError, CredentialProviders, ResolvedCredential, bind_credential,
    read_credential_file, run_credential_command,
};

/// What the fixture's synthetic host reports when a resolved credential
/// authenticates — test control for the "who does this credential act
/// as" step a real adapter performs.
#[derive(Debug, Clone)]
pub enum CredentialSeat {
    /// The host recognizes the value and reports this actor identity.
    Actor(HostActorRef),
    /// The host reports the credential as expired.
    Expired,
    /// The host denies the credential outright.
    Denied,
}

/// The fixture's credential environment: the slots `credential_ref`s
/// resolve through, plus the authentication outcome each resolved value
/// maps to. Seated `env:` values live in memory only — the process
/// environment is never mutated, and no seat value is ever logged,
/// persisted, or placed on argv.
#[derive(Debug, Default)]
pub struct CredentialBook {
    env: BTreeMap<String, String>,
    seats: BTreeMap<String, CredentialSeat>,
}

impl CredentialBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seat or rotate one `env:` slot's value.
    pub fn seat_env(&mut self, name: &str, value: &str) {
        self.env.insert(name.to_string(), value.to_string());
    }

    /// The host recognizes `value` and reports `actor` for it.
    pub fn seat_actor(&mut self, value: &str, actor: HostActorRef) {
        self.seats
            .insert(value.to_string(), CredentialSeat::Actor(actor));
    }

    /// The host reports `value` as an expired credential.
    pub fn seat_expired(&mut self, value: &str) {
        self.seats
            .insert(value.to_string(), CredentialSeat::Expired);
    }

    /// The host denies `value` outright.
    pub fn seat_denied(&mut self, value: &str) {
        self.seats.insert(value.to_string(), CredentialSeat::Denied);
    }

    /// The adapter's authentication step: which actor this credential
    /// acts as. An unrecognized value is unresolvable — never an
    /// anonymous fallback.
    pub fn authenticate(
        &self,
        credential: &ResolvedCredential,
    ) -> Result<HostActorRef, CredentialError> {
        match self.seats.get(credential.expose()) {
            Some(CredentialSeat::Actor(actor)) => Ok(actor.clone()),
            Some(CredentialSeat::Expired) => Err(CredentialError::expired(
                "the host reports the credential as expired",
            )),
            Some(CredentialSeat::Denied) => Err(CredentialError::unresolvable(
                "the host denies the credential",
            )),
            None => Err(CredentialError::unresolvable(
                "the host does not recognize the credential",
            )),
        }
    }
}

impl CredentialProviders for CredentialBook {
    fn env(&self, name: &str) -> Result<String, CredentialError> {
        self.env
            .get(name)
            .cloned()
            .ok_or_else(|| CredentialError::unresolvable(format!("env: variable {name} is unset")))
    }

    fn file(&self, path: &Path) -> Result<Vec<u8>, CredentialError> {
        read_credential_file(path)
    }

    fn command(&self, argv: &[String]) -> Result<String, CredentialError> {
        run_credential_command(argv)
    }
}

/// An in-memory host the tests steer. `apply_outbox` drains recorded
/// intents in outbox order; each write re-reads the synthetic remote head
/// immediately before applying, so an expected-head mismatch is detected
/// exactly where a real adapter would detect it.
#[derive(Debug, Default)]
pub struct FixtureGithub {
    /// When set, the adapter serves only this host instance: intents
    /// recorded for another instance are left `pending` for their own
    /// adapter — an effect never migrates across hosts. `None` drains
    /// every intent (legacy test/driver behavior).
    bound_host: Option<workloop_core::identity::HostInstanceId>,
    /// `change_request_id` → current head object id. Change-request ids
    /// are globally unique (the provider's stable identity), while
    /// display numbers are per-repository below.
    changes: BTreeMap<String, String>,
    /// `(repository, branch)` → current head object id — a real host's
    /// branch namespace is scoped to the repository, so identical branch
    /// names on two repositories never collide.
    branches: BTreeMap<(String, String), String>,
    /// `issue` (display form `repo/issue`) → the synthetic remote's
    /// current authoritative read. A declared source with no row is an
    /// unavailable read — the host cannot answer it.
    issues: BTreeMap<String, IssueRead>,
    /// `issue` → the actor the remote attributes its content to. A read
    /// of an issue with no attributed actor reports the reading actor.
    issue_actors: BTreeMap<String, HostActorRef>,
    /// `repository` → the recorded default-branch head and the actor
    /// the remote attributes it to.
    repositories: BTreeMap<String, RepoHead>,
    /// `(repository, branch)` → the actor that last moved the ref.
    branch_actors: BTreeMap<(String, String), HostActorRef>,
    /// `change_request_id` → the actor the remote attributes its head
    /// to (the opener or the last pusher).
    change_actors: BTreeMap<String, HostActorRef>,
    /// `change_request_id` → the actor that created the change request.
    /// Recorded when the change is registered or opened and never
    /// rewritten by a later head move — the stable attribution a
    /// lost-receipt create reconciles from, while [`Self::change_actors`]
    /// keeps tracking whoever moved the head last for reads.
    change_openers: BTreeMap<String, HostActorRef>,
    /// `change_request_id` → the full scoped ref, learned at open or
    /// registration time so raw-id mutators can bump the scoped
    /// subject version.
    change_refs: BTreeMap<String, ChangeRequestRef>,
    /// `change_request_id` → `(source_branch, target_branch)` recorded
    /// at open — the tuple a reconcile read matches on when the
    /// remote-issued id was never captured in a receipt.
    change_sources: BTreeMap<String, (String, String)>,
    /// `change_request_id` → the actor credited with the latest stored
    /// metadata write (the opener at registration, then each updater).
    meta_actors: BTreeMap<String, HostActorRef>,
    /// `(change_request_id, check name)` → the remote's check-run
    /// record.
    checks: BTreeMap<(String, String), CheckRun>,
    /// `change_request_id` → the review stream, oldest first.
    reviews: BTreeMap<String, Vec<ReviewRecord>>,
    /// The remote's stored comments, in post order: what a fresh read
    /// of the discussion returns — the receipt's read-back source.
    comments: Vec<PostedComment>,
    /// `thread_id` → the remote's thread record: resolved state and
    /// the posted reply stream.
    threads: BTreeMap<String, RemoteThread>,
    /// `change_request_id` → the actor credited with the ready
    /// (non-draft) transition. Absent = the remote reports draft.
    ready_actors: std::collections::BTreeMap<String, HostActorRef>,
    /// Operation names the host's capability map does not contain:
    /// attempts report `unsupported`, distinctly from any permission
    /// verdict.
    unsupported_ops: std::collections::BTreeSet<String>,
    /// `(operation, actor)` pairs the remote denies — the bound actor
    /// lacks the grant on this subject.
    denied_ops: std::collections::BTreeSet<(String, String)>,
    /// `(operation, actor)` pairs the remote marks ineligible — the
    /// actor class cannot perform the operation on this subject
    /// (e.g. the author reviewing their own change).
    ineligible_ops: std::collections::BTreeSet<(String, String)>,
    /// Object ids the remote knows. A read naming an unknown object is
    /// partial — the host never invents a revision.
    objects: std::collections::BTreeSet<String>,
    /// `object` → parent object: the remote's commit ancestry, used to
    /// classify the observed ref against the produced revision on a
    /// read-back (`Equal`/`Ancestor`/`Diverged`).
    parents: BTreeMap<String, String>,
    /// `change_request_id` → the remote's stored title/body: what a
    /// metadata update wrote and a fresh read returns.
    change_meta: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Change ids the remote reports merged.
    merged_changes: std::collections::BTreeSet<String>,
    /// `change_request_id` → the mergeability verdict the remote reports
    /// for that change's current head against its base.
    change_mergeability: BTreeMap<String, (MergeabilityState, Vec<String>)>,
    /// `action name` → failure reason: test-injected execution faults.
    /// A matching attempt consumes the entry — one-shot per arming.
    forced_failures: BTreeMap<String, String>,
    /// Remote mutations a racing actor lands after each successful
    /// adapter write, before the read-back — the write/read gap a real
    /// adapter cannot close.
    queued_mutations: std::collections::VecDeque<RemoteMutation>,
    /// `ReadSubject::key()` → the remote's version of that subject.
    /// Every mutation bumps the version of each subject it touched;
    /// reads carry the version they observed, so a delayed delivery of
    /// an older read can never outrank a newer one.
    versions: BTreeMap<String, u64>,
    next_id: u64,
    /// `repository` → next display number: numbering is a per-repository
    /// namespace, as on a real host.
    next_number: BTreeMap<String, u64>,
    /// `repository` → the synthetic remote's issue listing for source
    /// collection, keyed by scoped `IssueRef` so display-number
    /// differences cannot fork identity. Separate from `issues`: the
    /// collection read is a listing, the source-gate read is a
    /// single-issue authoritative read.
    source_issues: BTreeMap<RepositoryRef, BTreeMap<IssueRef, CollectedIssue>>,
    /// `repository` keys whose source-list reads the remote denies —
    /// the capability/authorization refusal class.
    source_denied: std::collections::BTreeSet<String>,
    /// `repository` keys whose source-list reads the remote cannot
    /// complete — the transport/host failure class.
    source_unavailable: std::collections::BTreeSet<String>,
    /// The write history, for test assertions.
    applied: Vec<String>,
    /// Test-armed kill switch: the next attempt that lands its remote
    /// mutation "dies" before any receipt is persisted — the durable
    /// record stays `pending` while the remote keeps the effect, and
    /// the drain stops as a dead worker's would. One-shot.
    crash_next_apply: bool,
    /// When installed, every pending intent's recorded credential
    /// binding is resolved and authenticated before its write — the
    /// adapter-boundary credential gate. Absent a book the fixture is
    /// permissive, preserving the pre-credential contract tests.
    credentials: Option<CredentialBook>,
}

/// What one pending intent's pass did. Shared by both host
/// implementations so the bounded drain reports the same outcome
/// classes regardless of which adapter performed the attempt.
pub(crate) enum AttemptOutcome {
    /// No host attempt ran, with why: the intent names another host
    /// instance, the Execution is missing, or its `host_write` grant is
    /// denied.
    Skipped(&'static str),
    /// A credential failure blocked the write before any host call;
    /// the intent stays `pending` and the pass reports the class.
    CredentialBlocked(&'static str),
    /// The adapter attempted the write at the host.
    Attempted,
    /// The remote mutation landed and the worker "died" before the
    /// receipt could persist — the test-armed crash point between
    /// apply and receipt-write.
    Crashed,
}

/// What the synthetic remote reports for one source issue read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRead {
    pub source_revision: String,
    pub content_digest: String,
    pub state: IssueState,
    /// The issue title exactly as the remote reports it — carried into
    /// the retained snapshot body a dispatch request binds.
    pub title: String,
    /// The issue body exactly as the remote reports it.
    pub body: String,
}

/// The remote's record of a repository's default-branch head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHead {
    pub head: String,
    pub actor: HostActorRef,
}

/// The remote's record of one check-run on a change request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub run_id: String,
    pub head: String,
    pub actor: HostActorRef,
}

/// One review in a change request's review stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub review_id: String,
    pub actor: HostActorRef,
    /// The body the remote stored — what a fresh read of the review
    /// returns for the receipt's digest.
    pub body: String,
    /// The object id the review was posted against: a review is bound
    /// to the revision it reviewed, so the remote can distinguish a
    /// review on an older head from the effect a later intent bound.
    pub head: String,
}

/// One comment the remote stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    /// The change the comment hangs on.
    pub change_id: String,
    /// The remote-issued comment identity — the read-back object id.
    pub comment_id: String,
    pub actor: HostActorRef,
    /// The intent marker the posting carried — the idempotence key a
    /// reconcile read matches on.
    pub marker: String,
    pub body: String,
}

/// The remote's record of one review thread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteThread {
    /// The change the thread hangs on.
    pub change_id: String,
    pub resolved: bool,
    /// The actor the remote credits with the resolution — `None` while
    /// the thread is open.
    pub resolved_by: Option<HostActorRef>,
    /// Posted replies, in order: `(reply_id, actor, marker, body)`.
    pub replies: Vec<(String, HostActorRef, String, String)>,
}

/// What one typed `read` resolved against the synthetic remote: the
/// observed revision in its per-kind form, the remote's version of the
/// subject at read time, the actor the remote reports, and the read's
/// completeness. A missing required field is `Partial` — the host never
/// invents a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRead {
    pub revision: Option<ReadRevision>,
    pub subject_version: u64,
    pub actor: HostActorRef,
    pub completeness: Completeness,
}

/// A remote mutation queued by test control to model a racing actor:
/// applied after each successful adapter write, before the read-back —
/// the write/read gap a real adapter cannot close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMutation {
    /// A racing actor moved a branch ref to `head` (descends from its
    /// previous position).
    MoveBranch {
        repository: RepositoryRef,
        branch: String,
        head: String,
        actor: HostActorRef,
    },
    /// A racing actor moved a change request's head.
    MoveChange {
        change: ChangeRequestRef,
        head: String,
        actor: HostActorRef,
    },
    /// A racing actor changed a change request's stored fields.
    SetChangeMeta {
        change: ChangeRequestRef,
        title: Option<String>,
        body: Option<String>,
    },
}

/// What one source-refresh pass read: head movements ingested, heads
/// already matching the recorded facts, sources left for the next
/// pass because the tick/source deadline stopped the read, and the
/// full content of every issue source successfully read — carried out
/// so the composition root can retain the immutable snapshot bodies a
/// dispatch request binds (`docs/v0.3.md` §4).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceRefreshReport {
    pub unchanged: usize,
    pub ingested: usize,
    pub deferred: Vec<String>,
    /// Sibling changes re-checked because a managed merge was recorded
    /// in this pass.
    pub merge_rechecks: usize,
    /// Sibling changes observed conflicted against the new base.
    pub sibling_conflicts: usize,
    /// Merge-detection checks issued this pass for recorded changes
    /// that have no recorded merge — bounded by
    /// `MAX_MERGE_CHECKS_PER_PASS` over the whole pass. On the
    /// production adapter a check is also how an unrecorded merge is
    /// discovered, so a detection whose read returns merged counts
    /// here too; the fixture learns merges from its declared state,
    /// so only checks of unmerged changes appear in this count.
    pub merge_detections: usize,
    /// Every issue source whose authoritative read completed this
    /// pass, whether or not the recorded observation changed — the
    /// retained body must exist even when the gate already matched.
    pub snapshots: Vec<crate::api::IssueSnapshotRead>,
    /// The pass's own clock reading at refresh start — the timestamp
    /// retention stamps on the bodies this report's snapshots produce,
    /// so downstream work never needs a second clock call.
    pub now_unix_seconds: i64,
}

/// What one host-outbox pass attempted, with the untouched remainder.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OutboxReport {
    pub attempted: usize,
    pub applied: usize,
    pub failed: usize,
    pub unknown: usize,
    /// Terminal unsupported receipts — the host capability map lacks
    /// the operation, so the intent is decided rather than pending.
    /// Counted separately so a pass summary surfaces the outcome an
    /// operator must act on instead of hiding it inside `attempted`.
    pub unsupported: usize,
    /// Pending in-scope intents not attempted because the per-tick
    /// bound stopped the drain; they stay pending for the next pass.
    pub deferred: Vec<String>,
    /// Intents whose credential binding failed resolution or actor
    /// verification during the pass; each stays `pending` with its
    /// `credential.*` class recorded for the repair path.
    pub credential_blocked: Vec<CredentialBlock>,
    /// In-scope pending intents the pass could not act on at all, each
    /// with why — they stay `pending` and reappear every pass, so a
    /// permanently unroutable intent must be visible in reports.
    pub skipped: Vec<SkippedIntent>,
}

/// One intent a credential failure blocked before any host write. The
/// block names the failure class only — never the ref and never a
/// secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialBlock {
    pub action_id: String,
    /// The `credential.*` failure class.
    pub code: &'static str,
}

/// One in-scope intent the pass never attempted, with why: another
/// host instance owns the effect, the Execution is missing, or its
/// `host_write` grant is denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedIntent {
    pub action_id: String,
    /// `host_mismatch` | `missing_execution` | `host_write_denied`.
    pub reason: &'static str,
}

/// What one reconcile pass did against the outstanding intents:
/// per-pass counts only — a `still_unknown` intent is re-examined by
/// the next pass and counted there too.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Outstanding in-scope intents the pass read against the remote.
    /// Reading consumes the per-tick item budget the same way a write
    /// attempt does.
    pub examined: usize,
    /// Intents the remote evidence settled `applied` — the receipt
    /// records the remote's attribution and normalized facts.
    pub applied: usize,
    /// `unknown` intents a definitive-absence read settled `failed`.
    pub failed: usize,
    /// `unknown` intents the pass left `unknown` — the read was
    /// indeterminate or the evidence did not match the intent.
    pub still_unknown: usize,
    /// Outstanding in-scope intents not examined because the bound
    /// stopped the pass.
    pub deferred: Vec<String>,
    /// Intents whose recorded credential binding failed resolution or
    /// actor verification during the pass; each stays outstanding
    /// with its `credential.*` class recorded for the repair path.
    pub credential_blocked: Vec<CredentialBlock>,
}

/// What one read-only comparison of an intent's recorded effect
/// against the remote establishes — the tri-state every verdict in a
/// reconcile or pre-write pass reduces to.
pub(crate) enum ObservedEffect {
    /// The remote shows the effect the intent recorded, attributed to
    /// the actor the remote reports, with the normalized fact a fresh
    /// read derives.
    Present {
        actor: HostActorRef,
        fact: Box<NormalizedEffectFact>,
    },
    /// The remote definitively shows the effect did not land.
    Absent,
    /// The read establishes neither presence nor absence — the intent
    /// stays outstanding.
    Indeterminate { reason: String },
}

impl FixtureGithub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The capability set this fixture declares for its bound actors:
    /// the full baseline vocabulary the current fixture implements.
    /// Scenarios that require an absent capability are skipped for this
    /// fixture, never weakened.
    pub fn declared_capabilities() -> &'static [&'static str] {
        &[
            "repository_read",
            "branch_read",
            "push_ref",
            "change_request_create",
            "change_request_read",
            "change_request_update",
            "comment_create",
            "review_feedback",
            "thread_reply",
            "thread_resolve",
            "mark_ready",
            "checks_read",
            "review_findings_read",
            "issue_read",
        ]
    }

    /// Bound to one host instance — the adapter drains only intents
    /// recorded for `host`.
    pub fn for_host(host: workloop_core::identity::HostInstanceId) -> Self {
        Self {
            bound_host: Some(host),
            ..Self::default()
        }
    }

    /// Install the credential environment this adapter resolves and
    /// authenticates against before every write.
    pub fn with_credentials(mut self, credentials: CredentialBook) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Install or replace the credential environment on an existing
    /// adapter — the test-control counterpart of
    /// [`Self::with_credentials`].
    pub fn install_credentials(&mut self, credentials: CredentialBook) {
        self.credentials = Some(credentials);
    }

    /// Test control: mutate the seated credential environment between
    /// passes — e.g. rotating a slot's value mid-Work.
    pub fn credentials_mut(&mut self) -> Option<&mut CredentialBook> {
        self.credentials.as_mut()
    }

    /// Test control: what the synthetic remote believes the change head is.
    /// Moves the scoped `pr` subject's version when the change id is
    /// registered; an unregistered id keeps version 0.
    pub fn set_head(&mut self, change_id: &str, head_object: &str) {
        self.note_object(head_object);
        self.changes
            .insert(change_id.to_string(), head_object.to_string());
        self.bump_change(change_id);
    }

    /// Test control: the authoritative read the synthetic remote returns
    /// for one source issue. `IssueState::Unavailable` models a read the
    /// host cannot complete; removing the row models the same thing, so
    /// an unplanted declared source reads as unavailable.
    pub fn set_issue(
        &mut self,
        issue: &IssueRef,
        source_revision: &str,
        content_digest: &str,
        state: IssueState,
    ) {
        self.issues.insert(
            issue.to_string(),
            IssueRead {
                source_revision: source_revision.to_string(),
                content_digest: content_digest.to_string(),
                state,
                title: String::new(),
                body: String::new(),
            },
        );
        self.bump(&ReadSubject::Issue {
            issue: issue.clone(),
        });
    }

    /// Plant an issue read whose title/body are self-consistent: the
    /// recorded `content_digest` is the canonical
    /// `issue_content_digest(title, body)`, exactly what a real
    /// transport reports. Dispatch-path tests need this shape — the
    /// retained snapshot body verifies the digest against the content.
    pub fn set_issue_content(
        &mut self,
        issue: &IssueRef,
        source_revision: &str,
        title: &str,
        body: &str,
        state: IssueState,
    ) {
        let content_digest = workloop_selector::issue_content_digest(title, body);
        self.issues.insert(
            issue.to_string(),
            IssueRead {
                source_revision: source_revision.to_string(),
                content_digest,
                state,
                title: title.to_string(),
                body: body.to_string(),
            },
        );
        self.bump(&ReadSubject::Issue {
            issue: issue.clone(),
        });
    }

    /// The authoritative read this fixture reports for one issue source,
    /// keyed by the scoped issue identity (repository + issue ID).
    pub fn issue_read(&self, issue: &IssueRef) -> Option<IssueRead> {
        self.issues.get(&issue.to_string()).cloned()
    }

    /// Test control: plant one fact in the synthetic remote's issue
    /// listing for source collection (#202). The fact is stored
    /// verbatim — including deliberately malformed fields a contract
    /// test needs — keyed by scoped `IssueRef` so a renumbered plant
    /// cannot fork identity.
    pub fn plant_source_issue(&mut self, repository: &RepositoryRef, issue: CollectedIssue) {
        debug_assert_eq!(issue.issue.repository, *repository);
        self.source_issues
            .entry(repository.clone())
            .or_default()
            .insert(issue.issue.clone(), issue);
    }

    /// Test control: the remote denies source-list reads for
    /// `repository` — the capability/authorization refusal class.
    pub fn deny_source_reads(&mut self, repository: &RepositoryRef) {
        self.source_denied.insert(repository.to_string());
    }

    /// Test control: the remote cannot complete source-list reads for
    /// `repository` — the transport/host failure class.
    pub fn unavailable_source_reads(&mut self, repository: &RepositoryRef) {
        self.source_unavailable.insert(repository.to_string());
    }

    /// The adapter-owned source reader for collection (#202): resolves
    /// the configured source-read credential and authenticates it
    /// against the bound actor before any listing. Publication
    /// credentials and store access are never involved — the reader
    /// borrows the fixture and collection is read-only.
    pub fn source_reader(
        &self,
        credential_ref: Option<&CredentialRef>,
        actor: &HostActorRef,
    ) -> Result<FixtureSourceReader<'_>, CredentialError> {
        let book = self.credentials.as_ref().ok_or_else(|| {
            CredentialError::missing("no credential environment is installed for source reads")
        })?;
        bind_credential(credential_ref, actor, book, &|credential| {
            book.authenticate(credential)
        })?;
        Ok(FixtureSourceReader {
            fixture: self,
            actor: actor.clone(),
        })
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// The head the synthetic remote currently records for
    /// `(repository, branch)` — a typed read for scoped-identity
    /// assertions, independent of the write-history encoding.
    pub fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.branches
            .get(&(repository.to_string(), branch.to_string()))
            .cloned()
    }

    fn bump(&mut self, subject: &ReadSubject) {
        *self.versions.entry(subject.key()).or_insert(0) += 1;
    }

    fn subject_version(&self, subject: &ReadSubject) -> u64 {
        let version = self.versions.get(&subject.key()).copied().unwrap_or(0);
        match subject {
            // A diff read's remote state is its base/head objects: an
            // object arriving on the remote moves the subject's version
            // even though no mutator names the snapshot, so a partial
            // read and the later complete read of the same pair deliver
            // under distinct identities.
            ReadSubject::Diff { snapshot } => {
                version
                    + u64::from(self.objects.contains(snapshot.base().object.as_str()))
                    + u64::from(self.objects.contains(snapshot.head().object.as_str()))
            }
            _ => version,
        }
    }

    /// Bump the scoped `pr` subject for a change id; a change the remote
    /// never registered keeps version 0 — its reads stay consistent but
    /// unordered, which the ledger treats as the first recorded state.
    fn bump_change(&mut self, change_id: &str) {
        if let Some(change) = self.change_refs.get(change_id) {
            let subject = ReadSubject::Pr {
                change: change.clone(),
            };
            self.bump(&subject);
        }
    }

    fn note_object(&mut self, object: &str) {
        self.objects.insert(object.to_string());
    }

    /// Test control: objects the remote already knows — e.g. a fetched
    /// base/head pair a diff read can reference.
    pub fn seed_objects(&mut self, objects: &[&str]) {
        for object in objects {
            self.note_object(object);
        }
    }

    /// Test control: record `child`'s parent in the remote's commit
    /// ancestry — the fact a fast-forward push implies.
    pub fn set_parent(&mut self, child: &str, parent: &str) {
        self.note_object(child);
        self.parents.insert(child.to_string(), parent.to_string());
    }

    /// Whether `ancestor` is on `object`'s recorded parent chain.
    fn descends(&self, object: &str, ancestor: &str) -> bool {
        let mut current = object;
        for _ in 0..=self.parents.len() {
            if current == ancestor {
                return true;
            }
            let Some(parent) = self.parents.get(current) else {
                return false;
            };
            current = parent;
        }
        false
    }

    /// Test control: the next attempt of `action_name` faults at
    /// execution and reports `Failed{reason}` — the write never reaches
    /// the remote. One-shot; re-arm to repeat.
    pub fn force_failure(&mut self, action_name: &str, reason: &str) {
        self.forced_failures
            .insert(action_name.to_string(), reason.to_string());
    }

    /// Test control: queue a remote mutation a racing actor lands after
    /// the next successful adapter write, before its read-back.
    pub fn queue_remote_mutation(&mut self, mutation: RemoteMutation) {
        self.queued_mutations.push_back(mutation);
    }

    /// Apply every queued racing-actor mutation — the remote state the
    /// read-back will observe.
    fn apply_queued_mutations(&mut self) {
        while let Some(mutation) = self.queued_mutations.pop_front() {
            match mutation {
                RemoteMutation::MoveBranch {
                    repository,
                    branch,
                    head,
                    actor,
                } => {
                    let key = (repository.to_string(), branch.clone());
                    if let Some(old) = self.branches.get(&key).cloned() {
                        self.parents.insert(head.clone(), old);
                    }
                    self.note_object(&head);
                    self.branches.insert(key.clone(), head);
                    self.branch_actors.insert(key, actor);
                    self.bump(&ReadSubject::Head { repository, branch });
                }
                RemoteMutation::MoveChange {
                    change,
                    head,
                    actor,
                } => {
                    let id = change.change_request.as_str().to_string();
                    if let Some(old) = self.changes.get(&id).cloned() {
                        self.parents.insert(head.clone(), old);
                    }
                    self.note_object(&head);
                    self.changes.insert(id.clone(), head);
                    self.change_actors.insert(id.clone(), actor);
                    self.bump(&ReadSubject::Pr { change });
                }
                RemoteMutation::SetChangeMeta {
                    change,
                    title,
                    body,
                } => {
                    let meta = self
                        .change_meta
                        .entry(change.change_request.as_str().to_string())
                        .or_default();
                    if title.is_some() {
                        meta.0 = title;
                    }
                    if body.is_some() {
                        meta.1 = body;
                    }
                    self.bump(&ReadSubject::Pr { change });
                }
            }
        }
    }

    /// The remote's stored fields for a change — the fresh read a
    /// metadata receipt reports.
    fn change_fields(&self, change_id: &str) -> (Option<String>, Option<String>) {
        self.change_meta.get(change_id).cloned().unwrap_or_default()
    }

    /// Test control: mark a change merged on the remote.
    pub fn set_merged(&mut self, change: &ChangeRequestRef) {
        self.merged_changes
            .insert(change.change_request.as_str().to_string());
        self.bump_change(change.change_request.as_str());
    }

    /// Alias for [`Self::set_merged`].
    pub fn mark_merged(&mut self, change: &ChangeRequestRef) {
        self.set_merged(change);
    }

    /// Alias for [`Self::set_change_mergeability`].
    pub fn set_mergeability(
        &mut self,
        change: &ChangeRequestRef,
        state: MergeabilityState,
        paths: &[&str],
    ) {
        self.set_change_mergeability(change, state, paths);
    }

    /// Test control: the mergeability verdict the remote reports for a
    /// change's current head against its base.
    pub fn set_change_mergeability(
        &mut self,
        change: &ChangeRequestRef,
        state: MergeabilityState,
        paths: &[&str],
    ) {
        self.change_mergeability.insert(
            change.change_request.as_str().to_string(),
            (state, paths.iter().map(|p| (*p).to_string()).collect()),
        );
        self.bump_change(change.change_request.as_str());
    }

    /// Test control: the remote's record of a repository's
    /// default-branch head and the actor attributed to it.
    pub fn set_repository(&mut self, repository: &RepositoryRef, head: &str, actor: &HostActorRef) {
        self.note_object(head);
        self.repositories.insert(
            repository.to_string(),
            RepoHead {
                head: head.to_string(),
                actor: actor.clone(),
            },
        );
        self.bump(&ReadSubject::Repository {
            repository: repository.clone(),
        });
    }

    /// Test control: the actor the remote attributes an issue's content
    /// to; reads of an issue without one report the reading actor.
    pub fn set_issue_actor(&mut self, issue: &IssueRef, actor: &HostActorRef) {
        self.issue_actors.insert(issue.to_string(), actor.clone());
    }

    /// Test control: move a branch head as the given actor — the
    /// typed, versioned counterpart of a remote ref update.
    pub fn set_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        self.note_object(head);
        self.branches.insert(
            (repository.to_string(), branch.to_string()),
            head.to_string(),
        );
        self.branch_actors
            .insert((repository.to_string(), branch.to_string()), actor.clone());
        self.bump(&ReadSubject::Head {
            repository: repository.clone(),
            branch: branch.to_string(),
        });
    }

    /// Test control: register a change's scoped ref so raw-id mutators
    /// can bump its subject version, then move its head as `actor`.
    pub fn set_change_head(&mut self, change: &ChangeRequestRef, head: &str, actor: &HostActorRef) {
        self.note_object(head);
        self.change_refs
            .insert(change.change_request.as_str().to_string(), change.clone());
        self.changes
            .insert(change.change_request.as_str().to_string(), head.to_string());
        self.change_actors
            .insert(change.change_request.as_str().to_string(), actor.clone());
        self.bump(&ReadSubject::Pr {
            change: change.clone(),
        });
    }

    /// Test control: the remote's check-run record for
    /// `(change, check)` — run id, examined head, and producing actor.
    pub fn set_check(
        &mut self,
        change: &ChangeRequestRef,
        check: &str,
        run_id: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        if !head.is_empty() {
            self.note_object(head);
        }
        self.checks.insert(
            (
                change.change_request.as_str().to_string(),
                check.to_string(),
            ),
            CheckRun {
                run_id: run_id.to_string(),
                head: head.to_string(),
                actor: actor.clone(),
            },
        );
        self.bump(&ReadSubject::Check {
            change: change.clone(),
            check: check.to_string(),
        });
    }

    /// Test control: append one review to a change's review stream,
    /// bound to the change's current head.
    pub fn add_review(&mut self, change: &ChangeRequestRef, review_id: &str, actor: &HostActorRef) {
        let head = self
            .changes
            .get(change.change_request.as_str())
            .cloned()
            .unwrap_or_default();
        self.reviews
            .entry(change.change_request.as_str().to_string())
            .or_default()
            .push(ReviewRecord {
                review_id: review_id.to_string(),
                actor: actor.clone(),
                body: String::new(),
                head,
            });
        self.bump(&ReadSubject::Review {
            change: change.clone(),
        });
    }

    /// Test control: append one review carrying `body`, attributed to
    /// `actor` and bound to revision `head` — the remote record a
    /// reconcile read adopts when the review's receipt was lost.
    pub fn seed_review(
        &mut self,
        change: &ChangeRequestRef,
        review_id: &str,
        actor: &HostActorRef,
        body: &str,
        head: &str,
    ) {
        self.reviews
            .entry(change.change_request.as_str().to_string())
            .or_default()
            .push(ReviewRecord {
                review_id: review_id.to_string(),
                actor: actor.clone(),
                body: body.to_string(),
                head: head.to_string(),
            });
        self.bump(&ReadSubject::Review {
            change: change.clone(),
        });
    }

    /// Test control: register an open thread on a change — the remote
    /// thread a reply or resolve effect targets.
    pub fn add_thread(&mut self, change: &ChangeRequestRef, thread_id: &str) {
        self.threads.insert(
            thread_id.to_string(),
            RemoteThread {
                change_id: change.change_request.as_str().to_string(),
                ..RemoteThread::default()
            },
        );
    }

    /// Test control: mark a thread resolved, credited to `actor` — the
    /// remote post-state a reconcile read settles from when the
    /// resolve's receipt was lost.
    pub fn set_thread_resolved(&mut self, thread_id: &str, actor: &HostActorRef) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.resolved = true;
            thread.resolved_by = Some(actor.clone());
        }
    }

    /// Test control: seed a comment the remote attributes to `actor`
    /// carrying `marker` — the remote record a reconcile read adopts
    /// when the comment's receipt was lost.
    pub fn seed_comment(
        &mut self,
        change: &ChangeRequestRef,
        comment_id: &str,
        actor: &HostActorRef,
        marker: &str,
        body: &str,
    ) {
        self.comments.push(PostedComment {
            change_id: change.change_request.as_str().to_string(),
            comment_id: comment_id.to_string(),
            actor: actor.clone(),
            marker: marker.to_string(),
            body: body.to_string(),
        });
    }

    /// Test control: register a change the remote attributes to
    /// `actor`, opened from `source_branch` onto `target_branch` —
    /// the remote record a reconcile read adopts when the create's
    /// receipt was lost.
    pub fn register_change(
        &mut self,
        change: &ChangeRequestRef,
        source_branch: &str,
        target_branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        self.set_change_head(change, head, actor);
        self.change_sources.insert(
            change.change_request.as_str().to_string(),
            (source_branch.to_string(), target_branch.to_string()),
        );
        self.change_openers
            .insert(change.change_request.as_str().to_string(), actor.clone());
        self.meta_actors
            .insert(change.change_request.as_str().to_string(), actor.clone());
    }

    /// Test control: store a change's metadata fields and the actor
    /// the remote credits with writing them — the remote post-state a
    /// reconcile read of an update intent settles from.
    pub fn set_change_meta(
        &mut self,
        change: &ChangeRequestRef,
        title: Option<&str>,
        body: Option<&str>,
        actor: &HostActorRef,
    ) {
        let id = change.change_request.as_str().to_string();
        let meta = self.change_meta.entry(id.clone()).or_default();
        if title.is_some() {
            meta.0 = title.map(str::to_string);
        }
        if body.is_some() {
            meta.1 = body.map(str::to_string);
        }
        self.meta_actors.insert(id, actor.clone());
        self.bump(&ReadSubject::Pr {
            change: change.clone(),
        });
    }

    /// Test control: arm the kill switch — the next attempt that lands
    /// its remote mutation returns [`AttemptOutcome::Crashed`] before
    /// any receipt is persisted. Models a worker killed between the
    /// host write and the durable receipt-write: the intent stays
    /// `pending`, the remote keeps the effect, and the drain stops.
    pub fn set_crash_after_apply(&mut self) {
        self.crash_next_apply = true;
    }

    /// Test control: mark a change ready (non-draft), credited to
    /// `actor` — the remote post-state a reconcile read settles from.
    pub fn set_ready(&mut self, change: &ChangeRequestRef, actor: &HostActorRef) {
        self.ready_actors
            .insert(change.change_request.as_str().to_string(), actor.clone());
        self.bump(&ReadSubject::Pr {
            change: change.clone(),
        });
    }

    /// The remote's thread record, for assertions on the post-effect
    /// state.
    pub fn thread(&self, thread_id: &str) -> Option<&RemoteThread> {
        self.threads.get(thread_id)
    }

    /// Whether the remote reports the change as ready (non-draft).
    pub fn is_ready(&self, change_id: &str) -> bool {
        self.ready_actors.contains_key(change_id)
    }

    /// Test control: drop `operation` from the host's capability map —
    /// attempts report `unsupported`, distinctly from a denied grant.
    pub fn remove_support(&mut self, operation: &str) {
        self.unsupported_ops.insert(operation.to_string());
    }

    /// Test control: `actor` lacks the grant for `operation` on the
    /// subject — attempts record `Failed{reason: denied}`.
    pub fn deny(&mut self, operation: &str, actor: &HostActorRef) {
        self.denied_ops
            .insert((operation.to_string(), actor.actor.as_str().to_string()));
    }

    /// Test control: `actor`'s class cannot perform `operation` on the
    /// subject — attempts record `Failed{reason: ineligible}`.
    pub fn mark_ineligible(&mut self, operation: &str, actor: &HostActorRef) {
        self.ineligible_ops
            .insert((operation.to_string(), actor.actor.as_str().to_string()));
    }

    /// Resolve one typed read target against the remote: the observed
    /// revision in its per-kind form, the remote's version of the
    /// subject at read time, the actor the remote reports, and the
    /// read's completeness. An absent subject or a missing required
    /// field is a `Partial` read — the host never invents a revision.
    pub fn read(&self, subject: &ReadSubject, reader: &HostActorRef) -> RemoteRead {
        let version = self.subject_version(subject);
        let partial = |reason: &str| RemoteRead {
            revision: None,
            subject_version: version,
            actor: reader.clone(),
            completeness: Completeness::Partial {
                reason: reason.to_string(),
            },
        };
        let complete = |revision: ReadRevision, actor: HostActorRef| RemoteRead {
            revision: Some(revision),
            subject_version: version,
            actor,
            completeness: Completeness::Complete,
        };
        let sha = |repository: &RepositoryRef, object: &str| {
            CommitRevision::new(repository.clone(), object)
                .map(|revision| ReadRevision::Sha { revision })
        };
        match subject {
            ReadSubject::Repository { repository } => {
                match self.repositories.get(&repository.to_string()) {
                    Some(state) => match sha(repository, &state.head) {
                        Ok(revision) => complete(revision, state.actor.clone()),
                        Err(_) => partial("repository head is not a valid object id"),
                    },
                    None => partial("repository is not on the remote"),
                }
            }
            ReadSubject::Issue { issue } => match self.issues.get(&issue.to_string()) {
                Some(read) => {
                    let actor = self
                        .issue_actors
                        .get(&issue.to_string())
                        .cloned()
                        .unwrap_or_else(|| reader.clone());
                    if read.state == IssueState::Unavailable {
                        return RemoteRead {
                            completeness: Completeness::Unavailable {
                                reason: "issue read is unavailable".into(),
                            },
                            ..partial("issue read is unavailable")
                        };
                    }
                    if read.source_revision.is_empty() || read.content_digest.is_empty() {
                        return partial("issue read is missing its revision or digest");
                    }
                    complete(
                        ReadRevision::Issue {
                            source_revision: read.source_revision.clone(),
                            content_digest: read.content_digest.clone(),
                        },
                        actor,
                    )
                }
                None => partial("issue is not on the remote"),
            },
            ReadSubject::Pr { change } => match self.changes.get(change.change_request.as_str()) {
                Some(head) => {
                    let actor = self
                        .change_actors
                        .get(change.change_request.as_str())
                        .cloned()
                        .unwrap_or_else(|| reader.clone());
                    match sha(&change.repository, head) {
                        Ok(revision) => complete(revision, actor),
                        Err(_) => partial("change head is not a valid object id"),
                    }
                }
                None => partial("change request is not on the remote"),
            },
            ReadSubject::Head { repository, branch } => {
                match self.branches.get(&(repository.to_string(), branch.clone())) {
                    Some(head) => {
                        let actor = self
                            .branch_actors
                            .get(&(repository.to_string(), branch.clone()))
                            .cloned()
                            .unwrap_or_else(|| reader.clone());
                        match sha(repository, head) {
                            Ok(revision) => complete(revision, actor),
                            Err(_) => partial("branch head is not a valid object id"),
                        }
                    }
                    None => partial("branch is not on the remote"),
                }
            }
            ReadSubject::Diff { snapshot } => {
                let known = self.objects.contains(snapshot.base().object.as_str())
                    && self.objects.contains(snapshot.head().object.as_str());
                if known {
                    complete(
                        ReadRevision::DiffSnapshot {
                            snapshot_id: snapshot.snapshot_id(),
                        },
                        reader.clone(),
                    )
                } else {
                    partial("diff base or head is not on the remote")
                }
            }
            ReadSubject::Check { change, check } => {
                match self
                    .checks
                    .get(&(change.change_request.as_str().to_string(), check.clone()))
                {
                    Some(run) => {
                        if run.run_id.is_empty() {
                            return partial("check read is missing its check-run id");
                        }
                        match CommitRevision::new(change.repository.clone(), &run.head) {
                            Ok(head) => complete(
                                ReadRevision::Check {
                                    check_run_id: run.run_id.clone(),
                                    head,
                                },
                                run.actor.clone(),
                            ),
                            Err(_) => partial("check head is not a valid object id"),
                        }
                    }
                    None => partial("check is not on the remote"),
                }
            }
            ReadSubject::Review { change } => {
                match self
                    .reviews
                    .get(change.change_request.as_str())
                    .and_then(|rs| rs.last())
                {
                    Some(review) if !review.review_id.is_empty() => complete(
                        ReadRevision::Review {
                            review_id: review.review_id.clone(),
                        },
                        review.actor.clone(),
                    ),
                    _ => partial("change request has no review on the remote"),
                }
            }
        }
    }

    /// Poll one typed read target and deliver the Observation through
    /// the shared ingest pipeline — the same path a webhook delivery
    /// would use. The delivery identity is the subject at the observed
    /// version, so a re-poll of an unchanged subject deduplicates and a
    /// delayed delivery of an older read is recorded as history only.
    pub fn observe(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        work_id: Option<&WorkId>,
        subject: &ReadSubject,
        reader: &HostActorRef,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        let read = self.read(subject, reader);
        let observation = Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "fixture.poll".into(),
                event_id: format!("{}@{}", subject.key(), read.subject_version),
            }),
            subject_work_id: work_id.cloned(),
            subject_execution_id: None,
            fact: ObservationFact::HostRead {
                subject: subject.clone(),
                revision: read.revision,
                subject_version: read.subject_version,
                actor: read.actor,
                completeness: read.completeness,
            },
            observed_unix_seconds: now_unix_seconds,
        };
        self.emit(store, config, observation, now_unix_seconds)
    }

    /// Feed a synthetic Observation (change_head, check, review, an
    /// externally-supplied host_action_result…) through the same ingest
    /// pipeline a webhook delivery would use.
    pub fn emit(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        handle_observation(store, config, &observation, now_unix_seconds)
    }

    /// Attempt every outstanding (`pending`) outbox intent, in order.
    /// Each action's result — `applied`, `failed`, or `unknown` — is
    /// recorded as a `host_action_result` Observation keyed by action ID;
    /// `unknown` stays outstanding until reconciled.
    ///
    /// The Execution must grant `host_write`. Denied intents stay
    /// `pending`. This fixture performs no network I/O.
    pub fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        self.apply_outbox_bounded(
            store,
            config,
            &mut || now_unix_seconds,
            None,
            None,
            usize::MAX,
        )
        .map(|_| ())
    }

    /// The bounded, optionally Work-scoped drain the loop's host-outbox
    /// phase calls: at most `max_items` attempts per tick (the approved
    /// ≤ 8/tick), only intents belonging to `scope` when one is given,
    /// and the untouched remainder reported rather than silently left.
    /// Like every other item loop, it checks `deadline` before starting
    /// each new item — a drain that runs out of tick budget stops and
    /// reports the rest for the next pass instead of writing past the
    /// deadline.
    pub fn apply_outbox_bounded(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError> {
        // Attempts are tracked so an intent that cannot make progress is
        // skipped for the rest of this pass — it neither loops the pass
        // forever nor blocks later independent intents (a retried stuck
        // intent may resolve on the next pass).
        // The installation-level grant is the outermost control: a
        // revoked `host_write` stops the drain even for executions that
        // were admitted under a more permissive configuration.
        let mut report = OutboxReport::default();
        if !config.allowed_effects.host_write {
            return Ok(report);
        }
        let now = clock();
        let in_scope = |intent: &HostActionIntent| scope.is_none_or(|w| &intent.work_id == w);
        let mut attempted = std::collections::HashSet::new();
        loop {
            // Both bounds stop the drain before the next item starts:
            // the per-tick item budget and the tick deadline. Whichever
            // trips, the untouched remainder is reported, never dropped.
            // Only a real host attempt consumes the item budget — an
            // intent that cannot run yet (its Execution is missing or
            // denies `host_write`) is skipped without charging the
            // budget, so a permanently pending intent never starves the
            // ready intents behind it.
            if report.attempted >= max_items || deadline.is_some_and(|d| clock() >= d) {
                let pending: Vec<String> = store.transact(|tx| {
                    Ok(tx
                        .action_intents()?
                        .into_iter()
                        .filter(|i| {
                            i.status == HostActionStatus::Pending
                                && !attempted.contains(&i.action_id)
                                && in_scope(i)
                        })
                        .map(|i| i.action_id)
                        .collect())
                })?;
                report.deferred = pending;
                break;
            }
            let next = store.transact(|tx| {
                Ok(tx.action_intents()?.into_iter().find(|i| {
                    i.status == HostActionStatus::Pending
                        && !attempted.contains(&i.action_id)
                        && in_scope(i)
                }))
            })?;
            let Some(intent) = next else { break };
            let action_id = intent.action_id.clone();
            attempted.insert(action_id.clone());
            match self.attempt(store, config, intent, now)? {
                AttemptOutcome::Attempted => report.attempted += 1,
                AttemptOutcome::CredentialBlocked(code) => {
                    report.credential_blocked.push(CredentialBlock {
                        action_id: action_id.clone(),
                        code,
                    });
                }
                AttemptOutcome::Skipped(reason) => {
                    report.skipped.push(SkippedIntent {
                        action_id: action_id.clone(),
                        reason,
                    });
                }
                // The test-armed crash point: the remote mutation
                // landed but the worker "died" before the receipt —
                // the intent stays `pending` and the drain stops the
                // way a dead worker's would, the untouched remainder
                // reported like a bound-exhausted remainder.
                AttemptOutcome::Crashed => {
                    report.attempted += 1;
                    report.deferred = store.transact(|tx| {
                        Ok(tx
                            .action_intents()?
                            .into_iter()
                            .filter(|i| {
                                i.status == HostActionStatus::Pending
                                    && !attempted.contains(&i.action_id)
                                    && in_scope(i)
                            })
                            .map(|i| i.action_id)
                            .collect())
                    })?;
                    break;
                }
            }
            let status = store.transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| i.action_id == action_id)
                    .map(|i| i.status))
            })?;
            match status {
                Some(HostActionStatus::Applied) => report.applied += 1,
                Some(HostActionStatus::Failed { .. }) => report.failed += 1,
                Some(HostActionStatus::Unknown { .. }) => report.unknown += 1,
                Some(HostActionStatus::Unsupported { .. }) => report.unsupported += 1,
                // Denied or unchanged: counted as attempted, still pending.
                _ => {}
            }
        }
        Ok(report)
    }

    /// Read-only reconciliation of every outstanding (`pending` or
    /// `unknown`) outbox intent, in order. See
    /// [`Self::reconcile_outbox_bounded`].
    pub fn reconcile_outbox(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<ReconcileReport, StoreError> {
        self.reconcile_outbox_bounded(
            store,
            config,
            &mut || now_unix_seconds,
            None,
            None,
            usize::MAX,
        )
    }

    /// The bounded, optionally Work-scoped reconcile pass: compare each
    /// outstanding intent's recorded effect against the remote's
    /// current state and feed the verdict through the same
    /// `host_action_result` receipt path a write attempt uses.
    ///
    /// The pass performs host reads only — remote state is mutated
    /// solely inside [`Self::attempt`]. An observed effect settles
    /// `applied` with the remote's own attribution and normalized
    /// facts (never on this adapter's say-so), a definitive-absence
    /// read settles an `unknown` intent `failed` — a `pending` intent
    /// may simply not have been attempted yet, so absence leaves it
    /// pending — and an indeterminate read changes nothing: the
    /// intent stays outstanding for a later read. Reconciliation is
    /// never a substitute for a write grant: the `host_write` check
    /// stays in [`Self::attempt`] alone. Intents bound to another
    /// host instance are left for that adapter.
    pub fn reconcile_outbox_bounded(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<ReconcileReport, StoreError> {
        let mut report = ReconcileReport::default();
        let now = clock();
        let in_scope = |intent: &HostActionIntent| scope.is_none_or(|w| &intent.work_id == w);
        let outstanding = |i: &HostActionIntent| {
            matches!(
                i.status,
                HostActionStatus::Pending | HostActionStatus::Unknown { .. }
            )
        };
        let mut examined = std::collections::HashSet::new();
        loop {
            if report.examined >= max_items || deadline.is_some_and(|d| clock() >= d) {
                report.deferred = store.transact(|tx| {
                    Ok(tx
                        .action_intents()?
                        .into_iter()
                        .filter(|i| {
                            outstanding(i) && !examined.contains(&i.action_id) && in_scope(i)
                        })
                        .map(|i| i.action_id)
                        .collect())
                })?;
                break;
            }
            let next = store.transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| outstanding(i) && !examined.contains(&i.action_id) && in_scope(i)))
            })?;
            let Some(intent) = next else { break };
            let action_id = intent.action_id.clone();
            examined.insert(action_id.clone());
            // Not this instance's intent: leave it outstanding for the
            // adapter bound to the recorded host — reconciliation never
            // migrates an effect across host instances.
            if self
                .bound_host
                .as_ref()
                .is_some_and(|bound| *bound != intent.host)
            {
                continue;
            }
            // The same adapter-boundary credential gate apply uses: the
            // intent's recorded role binding resolves and authenticates
            // under the actor authorized at intent time — a rotated
            // credential that authenticates as another actor cannot
            // rebind or settle the outstanding action.
            let mut verified_actor: Option<HostActorRef> = None;
            if let Some(book) = &self.credentials {
                let reference = store.transact(|tx| {
                    Ok(tx.work(&intent.work_id)?.and_then(|work| {
                        work.binding
                            .actors
                            .iter()
                            .find(|binding| binding.role == intent.role)
                            .map(|binding| binding.credential_ref.clone())
                    }))
                })?;
                match bind_credential(reference.as_ref(), &intent.actor, book, &|credential| {
                    book.authenticate(credential)
                }) {
                    Ok(actor) => verified_actor = Some(actor),
                    Err(error) => {
                        report.credential_blocked.push(CredentialBlock {
                            action_id,
                            code: error.code(),
                        });
                        continue;
                    }
                }
            }
            report.examined += 1;
            let reader = verified_actor.unwrap_or_else(|| intent.actor.clone());
            let work_id = intent.work_id.clone();
            let execution_id = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(&work_id)?
                    .into_iter()
                    .max_by_key(|execution| execution.ordinal)
                    .map(|execution| execution.id))
            })?;
            let receipt = |actor: Option<HostActorRef>,
                           status: HostActionStatus,
                           effects: Vec<NormalizedEffectFact>| {
                Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: execution_id.clone(),
                    fact: ObservationFact::HostActionResult {
                        action_id: action_id.clone(),
                        authenticated_actor: actor,
                        status,
                        effects,
                    },
                    observed_unix_seconds: now,
                }
            };
            match self.observe_effect(&intent) {
                ObservedEffect::Present { actor, fact } => {
                    // The remote's attribution authenticates the
                    // receipt: an effect the remote credits to another
                    // actor is recorded `unknown{actor_mismatch}` by
                    // the durable ingest path, never `applied`. That
                    // verdict is non-terminal, so the intent stays
                    // outstanding — re-observing the identical
                    // attribution and fact already on the record adds
                    // nothing, and a persistently foreign-attributed
                    // effect must not append a duplicate receipt on
                    // every pass (reconcile receipts carry no delivery
                    // identity, so the dedupe index never sees them).
                    let already_recorded = matches!(
                        &intent.status,
                        HostActionStatus::Unknown { reason } if reason == "actor_mismatch"
                    ) && intent.authenticated_actor.as_ref() == Some(&actor)
                        && intent.effects.len() == 1
                        && intent.effects[0] == *fact;
                    if !already_recorded {
                        self.emit(
                            store,
                            config,
                            receipt(Some(actor), HostActionStatus::Applied, vec![*fact]),
                            now,
                        )?;
                    }
                }
                ObservedEffect::Absent => {
                    // Absence settles only an intent that already
                    // carries a verdict: `unknown` becomes `failed` — a
                    // definite no-effect outcome. A `pending` intent is
                    // possibly never attempted, so absence is not
                    // evidence of failure.
                    if let HostActionStatus::Unknown { .. } = intent.status {
                        self.emit(
                            store,
                            config,
                            receipt(
                                Some(reader.clone()),
                                HostActionStatus::Failed {
                                    reason: "reconcile: effect not observed".into(),
                                },
                                vec![],
                            ),
                            now,
                        )?;
                    }
                }
                ObservedEffect::Indeterminate { reason } => {
                    // Update the record when the read's explanation
                    // moved (e.g. the remote now attributes the
                    // candidate differently); an unchanged reason needs
                    // no new receipt.
                    if let HostActionStatus::Unknown { reason: recorded } = &intent.status
                        && recorded != &reason
                    {
                        self.emit(
                            store,
                            config,
                            receipt(
                                Some(reader.clone()),
                                HostActionStatus::Unknown { reason },
                                vec![],
                            ),
                            now,
                        )?;
                    }
                }
            }
            match store.transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| i.action_id == action_id)
                    .map(|i| i.status))
            })? {
                Some(HostActionStatus::Applied) => report.applied += 1,
                Some(HostActionStatus::Failed { .. }) => report.failed += 1,
                Some(HostActionStatus::Unknown { .. }) => report.still_unknown += 1,
                _ => {}
            }
        }
        Ok(report)
    }

    /// The read-only half of reconciliation: compare `intent`'s
    /// recorded effect against the synthetic remote's current state
    /// and report presence, definitive absence, or indeterminacy.
    /// Performs no mutation; every `Present` carries the actor the
    /// remote attributes the effect to and the normalized fact a
    /// fresh read derives.
    fn observe_effect(&self, intent: &HostActionIntent) -> ObservedEffect {
        let unattributable = || ObservedEffect::Indeterminate {
            reason: "effect producer is not attributable".into(),
        };
        match &intent.action {
            HostAction::PushRef {
                branch, produced, ..
            } => {
                let key = (intent.repository.to_string(), branch.clone());
                let Some(object) = self.branches.get(&key).cloned() else {
                    return ObservedEffect::Absent;
                };
                let relation = if object == produced.object {
                    RefRelation::Equal
                } else if self.descends(&object, &produced.object) {
                    RefRelation::Ancestor
                } else {
                    return ObservedEffect::Absent;
                };
                // The ref-mover record attributes the head, not the
                // push that introduced `produced`: on an exact match
                // the last mover is the pusher, but once a racing
                // actor moved the branch on top of the landed commit,
                // ancestry alone proves the push reached the branch
                // and the mover says nothing about who produced it.
                // The ancestor case therefore binds the intent's own
                // actor — the same attribution the write path's
                // read-back reports for identical remote state.
                let actor = if relation == RefRelation::Equal {
                    match self.branch_actors.get(&key).cloned() {
                        Some(actor) => actor,
                        None => return unattributable(),
                    }
                } else {
                    intent.actor.clone()
                };
                let observed = if relation == RefRelation::Equal {
                    produced.clone()
                } else {
                    match CommitRevision::new(intent.repository.clone(), object) {
                        Ok(revision) => revision,
                        Err(_) => {
                            return ObservedEffect::Indeterminate {
                                reason: "observed head is not a valid object id".into(),
                            };
                        }
                    }
                };
                ObservedEffect::Present {
                    actor,
                    fact: Box::new(NormalizedEffectFact::RefPublished {
                        repository: intent.repository.clone(),
                        branch: branch.clone(),
                        produced: produced.clone(),
                        observed,
                        relation,
                    }),
                }
            }
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                ..
            } => {
                // The remote-issued id was never captured when the
                // receipt was lost: match the change by its immutable
                // (repository, source, target) identity instead.
                let candidates: Vec<&ChangeRequestRef> = self
                    .change_sources
                    .iter()
                    .filter(|(_, (source, target))| {
                        source == source_branch && target == target_branch
                    })
                    .filter_map(|(id, _)| self.change_refs.get(id))
                    .filter(|change| change.repository == intent.repository)
                    .collect();
                match candidates.as_slice() {
                    [] => ObservedEffect::Absent,
                    [change] => {
                        let id = change.change_request.as_str();
                        let Some(object) = self.changes.get(id) else {
                            return ObservedEffect::Indeterminate {
                                reason: "matching change request has no head on the remote".into(),
                            };
                        };
                        let Some(actor) = self.change_openers.get(id).cloned() else {
                            return unattributable();
                        };
                        match CommitRevision::new(intent.repository.clone(), object.clone()) {
                            Ok(head) => ObservedEffect::Present {
                                actor,
                                fact: Box::new(NormalizedEffectFact::ChangeOpened {
                                    change: (*change).clone(),
                                    head,
                                    source_branch: source_branch.clone(),
                                    target_branch: target_branch.clone(),
                                }),
                            },
                            Err(_) => ObservedEffect::Indeterminate {
                                reason: "observed head is not a valid object id".into(),
                            },
                        }
                    }
                    _ => ObservedEffect::Indeterminate {
                        reason: "multiple change requests match the recorded source/target".into(),
                    },
                }
            }
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                title,
                body,
            } => {
                let id = change.change_request.as_str();
                // Metadata is overwriteable state: the effect is only
                // verifiable at the head the intent bound. A moved
                // head cannot distinguish "our update landed, then the
                // head raced" from "never landed" — and coincidentally
                // matching fields are not evidence enough to adopt.
                // An intent that already recorded a verdict (`unknown`
                // means an attempt was made and its receipt was lost)
                // therefore stays outstanding: absence here is not
                // definitive and presence cannot be proven. A `pending`
                // intent has no recorded attempt, so the guarded write
                // arbitrates instead — its stale-head refusal is the
                // contract's verdict for a bound head that can no
                // longer hold.
                if self.changes.get(id).map(String::as_str) != Some(expected_head.object.as_str()) {
                    return if matches!(intent.status, HostActionStatus::Unknown { .. }) {
                        ObservedEffect::Indeterminate {
                            reason: "head moved after a recorded attempt; \
                                     landed-then-raced cannot be told from never-landed"
                                .into(),
                        }
                    } else {
                        ObservedEffect::Absent
                    };
                }
                // The update's post-condition: every field the intent
                // carried is what the remote now stores. Omitted fields
                // are unconstrained — a PATCH never rewrites them.
                let (stored_title, stored_body) = self.change_fields(id);
                let applied = title
                    .as_ref()
                    .is_none_or(|t| stored_title.as_ref() == Some(t))
                    && body
                        .as_ref()
                        .is_none_or(|b| stored_body.as_ref() == Some(b));
                if !applied {
                    return ObservedEffect::Absent;
                }
                let Some(actor) = self.meta_actors.get(id).cloned() else {
                    return unattributable();
                };
                ObservedEffect::Present {
                    actor,
                    fact: Box::new(NormalizedEffectFact::MetadataUpdated {
                        change: change.clone(),
                        head: expected_head.clone(),
                        fields_digest: workloop_core::domain::sha256_hex(
                            "change-meta-v1",
                            &workloop_core::domain::canonical_json(&serde_json::json!({
                                "title": stored_title,
                                "body": stored_body,
                            })),
                        ),
                    }),
                }
            }
            HostAction::PostComment {
                change,
                marker,
                body,
                ..
            } => {
                let marked: Vec<&PostedComment> = self
                    .comments
                    .iter()
                    .filter(|comment| {
                        comment.change_id == change.change_request.as_str()
                            && &comment.marker == marker
                    })
                    .collect();
                match marked.as_slice() {
                    [] => ObservedEffect::Absent,
                    [posted] if &posted.body == body => {
                        let body_digest = workloop_core::domain::sha256_hex(
                            "comment-v1",
                            &workloop_core::domain::canonical_json(&serde_json::json!({
                                "body": posted.body,
                            })),
                        );
                        ObservedEffect::Present {
                            actor: posted.actor.clone(),
                            fact: Box::new(NormalizedEffectFact::CommentPosted {
                                change: change.clone(),
                                comment_id: posted.comment_id.clone(),
                                actor: posted.actor.clone(),
                                marker: marker.clone(),
                                body_digest,
                                target: None,
                            }),
                        }
                    }
                    [_] => ObservedEffect::Indeterminate {
                        reason: "the recorded marker carries a different stored body".into(),
                    },
                    _ => ObservedEffect::Indeterminate {
                        reason: "multiple comments carry the recorded marker".into(),
                    },
                }
            }
            HostAction::PublishReviewFeedback { change, head, body } => {
                // A review is bound to the revision it was posted
                // against: a body-identical review on an older head is
                // not this intent's effect — only a record the remote
                // binds to the intent's own head can settle it.
                let candidates: Vec<&ReviewRecord> = self
                    .reviews
                    .get(change.change_request.as_str())
                    .map(|records| {
                        records
                            .iter()
                            .filter(|r| &r.body == body && r.head == head.object)
                            .collect()
                    })
                    .unwrap_or_default();
                match candidates.as_slice() {
                    [] => ObservedEffect::Absent,
                    [review] => {
                        let body_digest = workloop_core::domain::sha256_hex(
                            "review-v1",
                            &workloop_core::domain::canonical_json(&serde_json::json!({
                                "body": review.body,
                            })),
                        );
                        match CommitRevision::new(intent.repository.clone(), review.head.clone()) {
                            Ok(reviewed) => ObservedEffect::Present {
                                actor: review.actor.clone(),
                                fact: Box::new(NormalizedEffectFact::ReviewPublished {
                                    change: change.clone(),
                                    review_id: review.review_id.clone(),
                                    head: reviewed,
                                    body_digest,
                                }),
                            },
                            Err(_) => ObservedEffect::Indeterminate {
                                reason: "the review's recorded revision is not a valid object id"
                                    .into(),
                            },
                        }
                    }
                    _ => ObservedEffect::Indeterminate {
                        reason: "multiple reviews carry the recorded body on the bound head".into(),
                    },
                }
            }
            HostAction::ReplyToThread {
                change,
                thread_id,
                marker,
                body,
            } => {
                let belongs = self
                    .threads
                    .get(thread_id)
                    .is_some_and(|t| t.change_id == change.change_request.as_str());
                let Some(thread) = self.threads.get(thread_id).filter(|_| belongs) else {
                    return ObservedEffect::Absent;
                };
                let marked: Vec<&(String, HostActorRef, String, String)> = thread
                    .replies
                    .iter()
                    .filter(|(_, _, m, _)| m == marker)
                    .collect();
                match marked.as_slice() {
                    [] => ObservedEffect::Absent,
                    [(reply_id, actor, _, stored_body)] if stored_body == body => {
                        let body_digest = workloop_core::domain::sha256_hex(
                            "reply-v1",
                            &workloop_core::domain::canonical_json(&serde_json::json!({
                                "body": stored_body,
                            })),
                        );
                        ObservedEffect::Present {
                            actor: actor.clone(),
                            fact: Box::new(NormalizedEffectFact::ReplyPosted {
                                change: change.clone(),
                                thread_id: thread_id.clone(),
                                marker: marker.clone(),
                                reply_id: reply_id.clone(),
                                body_digest,
                            }),
                        }
                    }
                    [_] => ObservedEffect::Indeterminate {
                        reason: "the recorded marker carries a different stored body".into(),
                    },
                    _ => ObservedEffect::Indeterminate {
                        reason: "multiple replies carry the recorded marker".into(),
                    },
                }
            }
            HostAction::ResolveThread {
                change,
                thread_id,
                head,
            } => {
                let Some(thread) = self.threads.get(thread_id) else {
                    return ObservedEffect::Absent;
                };
                if thread.change_id != change.change_request.as_str() || !thread.resolved {
                    return ObservedEffect::Absent;
                }
                let Some(actor) = thread.resolved_by.clone() else {
                    return unattributable();
                };
                let Some(object) = self.changes.get(change.change_request.as_str()).cloned() else {
                    return ObservedEffect::Indeterminate {
                        reason: "thread resolved but the change head is not readable".into(),
                    };
                };
                let relation = if object == head.object {
                    RefRelation::Equal
                } else if self.descends(&object, &head.object) {
                    RefRelation::Ancestor
                } else {
                    return ObservedEffect::Indeterminate {
                        reason:
                            "thread resolved but the bound head is not on the observed ancestry"
                                .into(),
                    };
                };
                match CommitRevision::new(intent.repository.clone(), object) {
                    Ok(observed) => ObservedEffect::Present {
                        actor,
                        fact: Box::new(NormalizedEffectFact::ThreadResolved {
                            change: change.clone(),
                            thread_id: thread_id.clone(),
                            target: DiffTarget {
                                change: change.clone(),
                                head: head.clone(),
                                base: None,
                                target_revision: None,
                            },
                            observed,
                            relation,
                        }),
                    },
                    Err(_) => ObservedEffect::Indeterminate {
                        reason: "observed head is not a valid object id".into(),
                    },
                }
            }
            HostAction::MarkReady {
                change,
                expected_head,
            } => {
                let id = change.change_request.as_str();
                let Some(actor) = self.ready_actors.get(id).cloned() else {
                    return ObservedEffect::Absent;
                };
                let Some(object) = self.changes.get(id).cloned() else {
                    return ObservedEffect::Indeterminate {
                        reason: "ready but the change head is not readable".into(),
                    };
                };
                let relation = if object == expected_head.object {
                    RefRelation::Equal
                } else if self.descends(&object, &expected_head.object) {
                    RefRelation::Ancestor
                } else {
                    return ObservedEffect::Indeterminate {
                        reason: "ready but the bound head is not on the observed ancestry".into(),
                    };
                };
                match CommitRevision::new(intent.repository.clone(), object) {
                    Ok(observed) => ObservedEffect::Present {
                        actor,
                        fact: Box::new(NormalizedEffectFact::ReadyObserved {
                            change: change.clone(),
                            head: expected_head.clone(),
                            observed,
                            relation,
                        }),
                    },
                    Err(_) => ObservedEffect::Indeterminate {
                        reason: "observed head is not a valid object id".into(),
                    },
                }
            }
        }
    }

    /// Phase 1 (read-only source refresh): re-read every declared issue
    /// source for in-scope Works and every change head this fixture
    /// knows, ingesting an Observation only when the fresh read differs
    /// from the store's latest recorded fact. The read-compare makes the
    /// pass idempotent — a restart re-entering phase order observes no
    /// movement and ingests nothing — and a deadline stops the read
    /// before it starts the next source. Reads never mutate the
    /// synthetic remote and never touch a source no Work declared.
    pub fn refresh_sources(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
    ) -> Result<SourceRefreshReport, StoreError> {
        let now = clock();
        let mut report = SourceRefreshReport {
            now_unix_seconds: now,
            ..SourceRefreshReport::default()
        };
        // Authoritative reads of the declared issue sources feed the
        // source gate: a matching read unblocks admission, a changed or
        // closed source and an unreadable one both hold dispatch. Only
        // declared sources are ever read — broader remote activity is
        // never ingested as a substitute.
        for (work_id, execution_id, source) in declared_sources(store, scope)? {
            let key = source.issue.to_string();
            if let Some(d) = deadline {
                let tick_now = clock();
                if tick_now >= d {
                    report.deferred.push(key);
                    continue;
                }
            }
            let read = self.issues.get(&key);
            let (revision, digest, state) = match read {
                Some(read) => (
                    read.source_revision.clone(),
                    read.content_digest.clone(),
                    read.state.clone(),
                ),
                None => (String::new(), String::new(), IssueState::Unavailable),
            };
            // Every completed authoritative read is carried out for
            // retention — even when the recorded observation is
            // unchanged — so the composition root can (re)retain the
            // immutable snapshot body a bound request resolves.
            if let Some(read) = read {
                report.snapshots.push(crate::api::IssueSnapshotRead {
                    issue: source.issue.clone(),
                    source_revision: read.source_revision.clone(),
                    content_digest: read.content_digest.clone(),
                    title: read.title.clone(),
                    body: read.body.clone(),
                });
            }
            let latest = latest_recorded_issue(store, &work_id, &source.issue)?;
            if latest.as_ref() == Some(&(revision.clone(), digest.clone(), state.clone())) {
                report.unchanged += 1;
                continue;
            }
            self.emit(
                store,
                config,
                Observation {
                    delivery: None,
                    subject_work_id: Some(work_id),
                    subject_execution_id: Some(execution_id),
                    fact: ObservationFact::Issue {
                        issue: source.issue,
                        source_revision: revision,
                        content_digest: digest,
                        state,
                    },
                    observed_unix_seconds: now,
                },
                now,
            )?;
            report.ingested += 1;
        }
        // Attribute each known change to the Work that recorded an
        // intent carrying it; an unattributable change is never read
        // into this installation's facts.
        let mut owners: BTreeMap<String, (WorkId, ChangeRequestRef)> = BTreeMap::new();
        for intent in store.transact(|tx| tx.action_intents())? {
            if let Some(work_id) = scope
                && intent.work_id != *work_id
            {
                continue;
            }
            let mut note = |change: &ChangeRequestRef| {
                owners.insert(
                    change.change_request.as_str().to_string(),
                    (intent.work_id.clone(), change.clone()),
                );
            };
            match &intent.action {
                HostAction::UpdateChangeRequest { change, .. }
                | HostAction::PostComment { change, .. }
                | HostAction::PublishReviewFeedback { change, .. }
                | HostAction::ReplyToThread { change, .. }
                | HostAction::ResolveThread { change, .. }
                | HostAction::MarkReady { change, .. } => note(change),
                _ => {}
            }
            for effect in &intent.effects {
                match effect {
                    NormalizedEffectFact::ChangeOpened { change, .. }
                    | NormalizedEffectFact::MetadataUpdated { change, .. }
                    | NormalizedEffectFact::CommentPosted { change, .. }
                    | NormalizedEffectFact::ReviewPublished { change, .. }
                    | NormalizedEffectFact::ReplyPosted { change, .. }
                    | NormalizedEffectFact::ThreadResolved { change, .. }
                    | NormalizedEffectFact::ReadyObserved { change, .. } => note(change),
                    _ => {}
                }
            }
        }
        // A Work whose change head was already observed is managed even
        // if its opening intent has aged out of the retained outbox:
        // record the pair so merge detection and sibling re-checks still
        // see it.
        for (work_id, change) in recorded_change_pairs(store, scope)? {
            owners
                .entry(change.change_request.as_str().to_string())
                .or_insert((work_id, change));
        }
        for (change_id, head) in &self.changes {
            let Some((work_id, change)) = owners.get(change_id) else {
                continue;
            };
            if let Some(d) = deadline {
                let tick_now = clock();
                if tick_now >= d {
                    report.deferred.push(change_id.clone());
                    continue;
                }
            }
            let latest = latest_recorded_head(store, work_id, change)?;
            if latest.as_deref() == Some(head.as_str()) {
                report.unchanged += 1;
                continue;
            }
            let execution = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(work_id)?
                    .into_iter()
                    .max_by_key(|e| e.ordinal))
            })?;
            let head_revision = CommitRevision::new(change.repository.clone(), head.clone())
                .map_err(|detail| StoreError::Internal { detail })?;
            self.emit(
                store,
                config,
                Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: execution.map(|e| e.id),
                    fact: ObservationFact::ChangeHead {
                        change: change.clone(),
                        head: head_revision,
                    },
                    observed_unix_seconds: now,
                },
                now,
            )?;
            report.ingested += 1;
        }
        // A recorded managed merge re-checks the remaining open managed
        // changes on the same target. Every historical merge on one
        // branch re-reads the same fresh base and the same open
        // siblings, so the scan is collapsed to one per target per pass
        // and bounded over the whole pass. The stable observation
        // identity makes a steady conflicted pair one trigger, not one
        // per pass.
        let mut scanned_targets: BTreeMap<String, CommitRevision> = BTreeMap::new();
        let mut merge_scans = 0usize;
        // The sibling budget is pass-wide, not per merge: accumulated
        // merge history cannot multiply the remote verdict reads.
        let mut rechecked = 0usize;
        let check_epoch = new_id("merge-check");
        // The same bound the production adapter applies to merge
        // detection and merge scans: merged and unmerged recorded
        // changes share one least-recently-serviced ordering — a
        // consumed detection read and a consumed scan slot both leave
        // the same durable `MergeCheck` record, so the deferred tail
        // is reached on a later pass instead of the same leading set
        // re-scanning or re-reading every pass. Scans and detection
        // reads interleave in key order the way the production
        // candidate list does, so a mid-pass deadline defers the same
        // tail; the observation index breaks the same-second tie so
        // the rotation keeps moving once every pair is serviced.
        let mut merge_services: Vec<((i64, usize), WorkId, ChangeRequestRef, bool)> = Vec::new();
        for (owner_work, change) in owners.values() {
            let change_id = change.change_request.as_str();
            let checked =
                latest_recorded_merge_check(store, owner_work, change)?.unwrap_or((i64::MIN, 0));
            merge_services.push((
                checked,
                owner_work.clone(),
                change.clone(),
                self.merged_changes.contains(change_id),
            ));
        }
        merge_services.sort_by_key(|(checked, ..)| *checked);
        let mut detections = 0usize;
        for (_, owner_work, change, merged) in merge_services {
            let change_id = change.change_request.as_str();
            if !merged {
                // The bounded merge-detection mirror: the check runs in
                // the shared key order, and the durable record it leaves
                // is the ordering key the next pass reads.
                if let Some(d) = deadline
                    && clock() >= d
                {
                    report.deferred.push(format!("merge-check:{change_id}"));
                    continue;
                }
                if detections >= MAX_MERGE_CHECKS_PER_PASS {
                    report.deferred.push(format!("merge-check:{change_id}"));
                    continue;
                }
                detections += 1;
                report.merge_detections += 1;
                // The same fallback the production adapter applies: a head
                // that cannot build a revision still consumed the check,
                // so the ordering record binds the last recorded head —
                // only a pair with no usable head at all records nothing.
                let reported = self.changes.get(change_id).and_then(|object| {
                    CommitRevision::new(change.repository.clone(), object.clone()).ok()
                });
                let head = match reported {
                    Some(head) => Some(head),
                    None => latest_recorded_head(store, &owner_work, &change)?.and_then(|object| {
                        CommitRevision::new(change.repository.clone(), object).ok()
                    }),
                };
                let Some(head) = head else { continue };
                self.emit(
                    store,
                    config,
                    Observation {
                        delivery: Some(DeliveryId {
                            scope: DeliveryScope::Host {
                                host: change.repository.host.clone(),
                            },
                            source: "fixture.merge".into(),
                            event_id: format!(
                                "merge-check:{}:{}@{}#{check_epoch}",
                                owner_work.0, change_id, head.object
                            ),
                        }),
                        subject_work_id: Some(owner_work.clone()),
                        subject_execution_id: None,
                        fact: ObservationFact::MergeCheck {
                            change: change.clone(),
                            head: head.clone(),
                        },
                        observed_unix_seconds: now,
                    },
                    now,
                )?;
                continue;
            }
            // A recorded managed merge re-reads the fresh target base and
            // the open siblings on it; the recorded merge itself is the
            // fallback for a head or base the fixture no longer serves,
            // mirroring the production base-hint fallback.
            let recorded = latest_recorded_merge(store, &owner_work, &change)?;
            let target_branch = recorded
                .as_ref()
                .map(|(.., target)| target.clone())
                .or_else(|| {
                    self.change_sources
                        .get(change_id)
                        .map(|(_, target)| target.clone())
                })
                .unwrap_or_else(|| "main".to_string());
            if scanned_targets.contains_key(&target_branch) {
                // This pass already scanned the target; another merge on
                // the same branch adds no new base or sibling set.
                continue;
            }
            if let Some(d) = deadline
                && clock() >= d
            {
                report.deferred.push(format!("merge:{change_id}"));
                continue;
            }
            if merge_scans >= MAX_MERGE_RECHECKS_PER_PASS {
                report.deferred.push(format!("merge:{change_id}"));
                continue;
            }
            let Some(head_object) = recorded
                .as_ref()
                .map(|(head, ..)| head.object.clone())
                .or_else(|| self.changes.get(change_id).cloned())
            else {
                continue;
            };
            let Some(base_object) = self
                .branches
                .get(&(change.repository.to_string(), target_branch.clone()))
                .cloned()
                .or_else(|| recorded.as_ref().map(|(_, base, _)| base.object.clone()))
            else {
                continue;
            };
            let Ok(head) = CommitRevision::new(change.repository.clone(), head_object) else {
                continue;
            };
            let Ok(base) = CommitRevision::new(change.repository.clone(), base_object) else {
                continue;
            };
            let execution_id = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(&owner_work)?
                    .into_iter()
                    .max_by_key(|e| e.ordinal)
                    .map(|e| e.id))
            })?;
            self.emit(
                store,
                config,
                Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Host {
                            host: change.repository.host.clone(),
                        },
                        source: "fixture.merge".into(),
                        event_id: format!(
                            "merge:{}:{}@{}:{}",
                            owner_work.0, change_id, head.object, base.object
                        ),
                    }),
                    subject_work_id: Some(owner_work.clone()),
                    subject_execution_id: execution_id,
                    fact: ObservationFact::ChangeMerged {
                        change: change.clone(),
                        head: head.clone(),
                        base: base.clone(),
                        target_branch: target_branch.clone(),
                    },
                    observed_unix_seconds: now,
                },
                now,
            )?;
            report.merge_rechecks += 1;
            merge_scans += 1;
            scanned_targets.insert(target_branch.clone(), base.clone());
            // A consumed scan slot leaves the same durable ordering
            // record a detection read does: the pair sorts behind the
            // never-serviced remainder next pass instead of leading
            // the queue again and starving the deferred tail.
            self.emit(
                store,
                config,
                Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Host {
                            host: change.repository.host.clone(),
                        },
                        source: "fixture.merge".into(),
                        event_id: format!(
                            "merge-check:{}:{}@{}#{check_epoch}",
                            owner_work.0, change_id, head.object
                        ),
                    }),
                    subject_work_id: Some(owner_work.clone()),
                    subject_execution_id: None,
                    fact: ObservationFact::MergeCheck {
                        change: change.clone(),
                        head: head.clone(),
                    },
                    observed_unix_seconds: now,
                },
                now,
            )?;
            // The same candidate ordering the production adapter
            // applies: a settled recorded verdict for the current
            // `(head, base)` pair is final for the episode and never
            // re-read; unrecorded pairs and provisional `Unknown`
            // verdicts are checked first, least recently recorded
            // first, so the deferred tail is reached on a later pass.
            let mut candidates: Vec<(i64, &String, &WorkId, &ChangeRequestRef)> = Vec::new();
            for (sibling_id, (sibling_work, sibling)) in &owners {
                if sibling_id == change_id {
                    continue;
                }
                // A merged sibling is no longer open: like the remote's
                // open-change list, it drops out before any verdict read.
                if self.merged_changes.contains(sibling_id) {
                    continue;
                }
                let sibling_target = self
                    .change_sources
                    .get(sibling_id)
                    .map(|(_, target)| target.clone())
                    .unwrap_or_else(|| "main".to_string());
                if sibling_target != target_branch {
                    continue;
                }
                if !self.change_mergeability.contains_key(sibling_id) {
                    continue;
                }
                let Some(sibling_head_object) = self.changes.get(sibling_id) else {
                    continue;
                };
                let recorded = latest_recorded_mergeability(
                    store,
                    sibling_work,
                    sibling,
                    sibling_head_object,
                    &base,
                )?;
                let key = match recorded {
                    Some((MergeabilityState::Unknown, observed)) => observed,
                    Some(_) => continue,
                    None => i64::MIN,
                };
                candidates.push((key, sibling_id, sibling_work, sibling));
            }
            candidates.sort_by_key(|candidate| candidate.0);
            for (_, sibling_id, sibling_work, sibling) in candidates {
                if rechecked >= MAX_CONFLICT_RECHECKS_PER_PASS {
                    report.deferred.push(format!("merge-siblings:{change_id}"));
                    break;
                }
                if let Some(d) = deadline
                    && clock() >= d
                {
                    report.deferred.push(format!("merge-siblings:{change_id}"));
                    break;
                }
                let Some((state, paths)) = self.change_mergeability.get(sibling_id) else {
                    continue;
                };
                // Every verdict read consumes the pass-wide sibling
                // bound — re-reading a provisional `Unknown` pair is
                // still a re-check.
                rechecked += 1;
                let Some(sibling_head_object) = self.changes.get(sibling_id).cloned() else {
                    continue;
                };
                let Ok(sibling_head) =
                    CommitRevision::new(sibling.repository.clone(), sibling_head_object)
                else {
                    continue;
                };
                let execution_id = store.transact(|tx| {
                    Ok(tx
                        .executions_for_work(sibling_work)?
                        .into_iter()
                        .max_by_key(|e| e.ordinal)
                        .map(|e| e.id))
                })?;
                let token = workloop_core::domain::sha256_hex(
                    "fixture-mergeability-v1",
                    format!("{state:?}:{paths:?}").as_bytes(),
                );
                self.emit(
                    store,
                    config,
                    Observation {
                        delivery: Some(DeliveryId {
                            scope: DeliveryScope::Host {
                                host: sibling.repository.host.clone(),
                            },
                            source: "fixture.mergeability".into(),
                            event_id: format!(
                                "mergeability:{}:{}@{}:{}#{token}",
                                sibling_work.0, sibling_id, sibling_head.object, base.object
                            ),
                        }),
                        subject_work_id: Some(sibling_work.clone()),
                        subject_execution_id: execution_id,
                        fact: ObservationFact::Mergeability {
                            target: DiffTarget {
                                change: sibling.clone(),
                                head: sibling_head,
                                base: Some(base.clone()),
                                target_revision: Some(base.clone()),
                            },
                            state: *state,
                            conflicting_paths: paths.clone(),
                            reason: (*state == MergeabilityState::Unknown)
                                .then(|| "fixture mergeability is unknown".to_string()),
                        },
                        observed_unix_seconds: now,
                    },
                    now,
                )?;
                report.merge_rechecks += 1;
                if *state == MergeabilityState::Conflicting {
                    report.sibling_conflicts += 1;
                }
            }
        }
        Ok(report)
    }

    fn attempt(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        intent: HostActionIntent,
        now: i64,
    ) -> Result<AttemptOutcome, StoreError> {
        // Not this instance's intent: leave it pending for the adapter
        // bound to the recorded host — an effect never migrates across
        // host instances. It is not a real attempt, so it does not
        // consume the per-tick item budget.
        if self
            .bound_host
            .as_ref()
            .is_some_and(|bound| *bound != intent.host)
        {
            return Ok(AttemptOutcome::Skipped("host_mismatch"));
        }
        let work_id = intent.work_id.clone();
        let execution = store.transact(|tx| {
            Ok(tx
                .executions_for_work(&work_id)?
                .into_iter()
                .max_by_key(|execution| execution.ordinal))
        })?;
        let Some(execution) = execution else {
            return Ok(AttemptOutcome::Skipped("missing_execution"));
        };
        if !execution.allowed_effects.host_write {
            return Ok(AttemptOutcome::Skipped("host_write_denied"));
        }
        // The adapter-boundary credential gate: with a book installed,
        // the intent's recorded role binding resolves its credential
        // slot and the resolved value is authenticated against the
        // intent's authorized actor before any write. A failure leaves
        // the intent pending — an actor mismatch never rebinds an
        // outstanding action, and a same-actor renewal resolves the new
        // secret under the original binding.
        let mut verified_actor: Option<HostActorRef> = None;
        if let Some(book) = &self.credentials {
            let reference = store.transact(|tx| {
                Ok(tx.work(&work_id)?.and_then(|work| {
                    work.binding
                        .actors
                        .iter()
                        .find(|binding| binding.role == intent.role)
                        .map(|binding| binding.credential_ref.clone())
                }))
            })?;
            match bind_credential(reference.as_ref(), &intent.actor, book, &|credential| {
                book.authenticate(credential)
            }) {
                Ok(actor) => verified_actor = Some(actor),
                Err(error) => {
                    return Ok(AttemptOutcome::CredentialBlocked(error.code()));
                }
            }
        }
        let execution_id = Some(execution.id);
        let obs = |fact: ObservationFact| Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.clone(),
            fact,
            observed_unix_seconds: now,
        };
        let authenticated = verified_actor.unwrap_or_else(|| intent.actor.clone());
        let result =
            |action_id: String, status: HostActionStatus, effects: Vec<NormalizedEffectFact>| {
                ObservationFact::HostActionResult {
                    action_id,
                    authenticated_actor: Some(authenticated.clone()),
                    status,
                    effects,
                }
            };

        // Support is validated distinctly from eligibility: an
        // operation absent from the host's capability map is an
        // explicit `unsupported` receipt — the write is never
        // attempted and it is never a permission verdict.
        if self.unsupported_ops.contains(intent.action.name()) {
            self.emit(
                store,
                config,
                obs(result(
                    intent.action_id,
                    HostActionStatus::Unsupported {
                        reason: format!("unsupported operation: {}", intent.action.name()),
                    },
                    vec![],
                )),
                now,
            )?;
            return Ok(AttemptOutcome::Attempted);
        }
        // Reconcile-before-write: an intent whose effect the remote
        // already shows settles from the read — the receipt a crash
        // dropped is recovered, and the write is never re-executed to
        // discover whether it happened. An indeterminate read keeps
        // the intent outstanding rather than authorizing a second
        // write; only a definitive absence proceeds to the guarded
        // write below.
        match self.observe_effect(&intent) {
            ObservedEffect::Present { actor, fact } => {
                self.emit(
                    store,
                    config,
                    obs(ObservationFact::HostActionResult {
                        action_id: intent.action_id,
                        authenticated_actor: Some(actor),
                        status: HostActionStatus::Applied,
                        effects: vec![*fact],
                    }),
                    now,
                )?;
                return Ok(AttemptOutcome::Attempted);
            }
            ObservedEffect::Indeterminate { .. } => {
                // Parity with the production adapter: an unreadable
                // pre-write read never fabricates an `unknown` verdict
                // on a `pending` intent — nothing was attempted, and
                // the next absent reconcile read would otherwise
                // settle the never-attempted write `failed`.
                return Ok(AttemptOutcome::Skipped("read_indeterminate"));
            }
            ObservedEffect::Absent => {}
        }
        // Actor eligibility on the subject is checked before the
        // write: `ineligible` (the actor's class cannot perform the
        // operation here) and `denied` (the actor lacks the grant) are
        // distinct explicit failure classes — both terminal, never
        // conflated with `unsupported`.
        let eligibility_key = || {
            (
                intent.action.name().to_string(),
                authenticated.actor.as_str().to_string(),
            )
        };
        let ineligible = self.ineligible_ops.contains(&eligibility_key());
        let denied = self.denied_ops.contains(&eligibility_key());
        if ineligible || denied {
            self.emit(
                store,
                config,
                obs(result(
                    intent.action_id,
                    HostActionStatus::Failed {
                        reason: if ineligible { "ineligible" } else { "denied" }.into(),
                    },
                    vec![],
                )),
                now,
            )?;
            return Ok(AttemptOutcome::Attempted);
        }

        // A test-armed execution fault: the write never reaches the
        // remote; the failed receipt is explicit and the intent keeps
        // its own record — a fault in one effect never rewrites the
        // receipts of the others.
        if let Some(reason) = self.forced_failures.remove(intent.action.name()) {
            self.emit(
                store,
                config,
                obs(result(
                    intent.action_id,
                    HostActionStatus::Failed { reason },
                    vec![],
                )),
                now,
            )?;
            return Ok(AttemptOutcome::Attempted);
        }

        match &intent.action {
            HostAction::PushRef {
                branch,
                produced,
                expected_prior,
            } => {
                // The guarded fast-forward: the remote ref must still
                // hold the position recorded at intent time (`None` =
                // absent). A moved ref refuses the write — no ref
                // update occurs, and the fresh position surfaces so
                // the caller can re-issue against it.
                let key = (intent.repository.to_string(), branch.clone());
                let actual = self.branches.get(&key).cloned();
                let expected = expected_prior.as_ref().map(|p| p.object.clone());
                if actual != expected {
                    // The fresh position is recorded before the refusal
                    // receipt — the same order the bridge reports it, so
                    // a receipt-driven re-issue derives its guard from
                    // the observed remote, never the pre-read position.
                    let state = match &actual {
                        Some(head) => BranchState::At {
                            revision: CommitRevision::new(intent.repository.clone(), head)
                                .map_err(|detail| StoreError::Internal { detail })?,
                        },
                        None => BranchState::Absent,
                    };
                    self.emit(
                        store,
                        config,
                        obs(ObservationFact::BranchState {
                            repository: intent.repository.clone(),
                            branch: branch.clone(),
                            state,
                        }),
                        now,
                    )?;
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "stale_head".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                self.branches.insert(key.clone(), produced.object.clone());
                if let Some(prior) = &actual {
                    self.parents.insert(produced.object.clone(), prior.clone());
                }
                self.note_object(&produced.object);
                self.branch_actors.insert(key, authenticated.clone());
                self.bump(&ReadSubject::Head {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                });
                self.applied.push(format!(
                    "push_ref:{}:{}",
                    intent.repository, produced.object
                ));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the remote may have raced between write and
                // fresh read — the receipt reports what the read
                // observes, never the attempted value.
                self.apply_queued_mutations();
                let observed_object = self.remote_head(&intent.repository, branch);
                let (observed, relation) = match &observed_object {
                    Some(object) if object == &produced.object => {
                        (produced.clone(), RefRelation::Equal)
                    }
                    Some(object) => {
                        let relation = if self.descends(object, &produced.object) {
                            RefRelation::Ancestor
                        } else {
                            RefRelation::Diverged
                        };
                        (
                            CommitRevision::new(intent.repository.clone(), object)
                                .map_err(|detail| StoreError::Internal { detail })?,
                            relation,
                        )
                    }
                    None => {
                        // The ref is gone after our write: the outcome
                        // cannot be established as applied — report the
                        // produced revision as diverged evidence.
                        (produced.clone(), RefRelation::Diverged)
                    }
                };
                self.emit(
                    store,
                    config,
                    obs(result(
                        intent.action_id,
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::RefPublished {
                            repository: intent.repository.clone(),
                            branch: branch.clone(),
                            produced: produced.clone(),
                            observed,
                            relation,
                        }],
                    )),
                    now,
                )?;
            }
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                title,
                body,
            } => {
                self.next_id += 1;
                let display = {
                    let n = self
                        .next_number
                        .entry(intent.repository.to_string())
                        .or_insert(0);
                    *n += 1;
                    *n
                };
                let change = ChangeRequestRef {
                    repository: intent.repository.clone(),
                    change_request: ChangeRequestId::trusted(format!("cr-{}", self.next_id)),
                    display_number: Some(display),
                };
                let head_object = self
                    .branches
                    .get(&(intent.repository.to_string(), source_branch.clone()))
                    .cloned()
                    .unwrap_or_else(|| format!("{source_branch}-head"));
                self.changes.insert(
                    change.change_request.as_str().to_string(),
                    head_object.clone(),
                );
                self.change_meta.insert(
                    change.change_request.as_str().to_string(),
                    (Some(title.clone()), Some(body.clone())),
                );
                self.note_object(&head_object);
                self.change_refs
                    .insert(change.change_request.as_str().to_string(), change.clone());
                self.change_actors.insert(
                    change.change_request.as_str().to_string(),
                    authenticated.clone(),
                );
                self.change_openers.insert(
                    change.change_request.as_str().to_string(),
                    authenticated.clone(),
                );
                self.change_sources.insert(
                    change.change_request.as_str().to_string(),
                    (source_branch.clone(), target_branch.clone()),
                );
                self.meta_actors.insert(
                    change.change_request.as_str().to_string(),
                    authenticated.clone(),
                );
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "open_change:{}:{}",
                    intent.repository, change.change_request
                ));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the receipt reports the head a fresh
                // change-request lookup observes, not the value the
                // write attempted.
                self.apply_queued_mutations();
                let observed_head = self
                    .changes
                    .get(change.change_request.as_str())
                    .cloned()
                    .unwrap_or(head_object);
                let head = CommitRevision::new(intent.repository.clone(), observed_head)
                    .map_err(|detail| StoreError::Internal { detail })?;
                self.emit(
                    store,
                    config,
                    obs(ObservationFact::ChangeHead {
                        change: change.clone(),
                        head: head.clone(),
                    }),
                    now,
                )?;
                self.emit(
                    store,
                    config,
                    obs(result(
                        intent.action_id,
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::ChangeOpened {
                            change,
                            head,
                            source_branch: source_branch.clone(),
                            target_branch: target_branch.clone(),
                        }],
                    )),
                    now,
                )?;
            }
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                title,
                body,
            } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() == Some(expected_head.object.as_str()) {
                    // The remote stores only the fields the request
                    // carried — a PATCH never rewrites omitted fields.
                    {
                        let meta = self
                            .change_meta
                            .entry(change.change_request.as_str().to_string())
                            .or_default();
                        if title.is_some() {
                            meta.0 = title.clone();
                        }
                        if body.is_some() {
                            meta.1 = body.clone();
                        }
                    }
                    self.meta_actors.insert(
                        change.change_request.as_str().to_string(),
                        authenticated.clone(),
                    );
                    self.bump_change(change.change_request.as_str());
                    self.applied.push(format!(
                        "update_change:{}:{}",
                        intent.repository, change.change_request
                    ));
                    if std::mem::take(&mut self.crash_next_apply) {
                        return Ok(AttemptOutcome::Crashed);
                    }
                    // Read-back: the digest covers the remote's stored
                    // fields after any racing update — never the
                    // attempted payload.
                    self.apply_queued_mutations();
                    let (stored_title, stored_body) =
                        self.change_fields(change.change_request.as_str());
                    let fields_digest = workloop_core::domain::sha256_hex(
                        "change-meta-v1",
                        &workloop_core::domain::canonical_json(&serde_json::json!({
                            "title": stored_title,
                            "body": stored_body,
                        })),
                    );
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Applied,
                            vec![NormalizedEffectFact::MetadataUpdated {
                                change: change.clone(),
                                head: expected_head.clone(),
                                fields_digest,
                            }],
                        )),
                        now,
                    )?;
                } else {
                    // Head moved under the recorded intent: abort, record
                    // the failure, and surface the fresh head — the caller
                    // decides whether to re-issue.
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    if let Some(head) = actual {
                        self.emit(
                            store,
                            config,
                            obs(ObservationFact::ChangeHead {
                                change: change.clone(),
                                head: CommitRevision::new(intent.repository.clone(), head)
                                    .map_err(|detail| StoreError::Internal { detail })?,
                            }),
                            now,
                        )?;
                    }
                }
            }
            HostAction::PostComment {
                change,
                expected_head,
                marker,
                body,
            } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() == Some(expected_head.object.as_str()) {
                    let comment_id = format!("c-{}", intent.action_id);
                    self.comments.push(PostedComment {
                        change_id: change.change_request.as_str().to_string(),
                        comment_id: comment_id.clone(),
                        actor: authenticated.clone(),
                        marker: marker.clone(),
                        body: body.clone(),
                    });
                    self.bump(&ReadSubject::Pr {
                        change: change.clone(),
                    });
                    self.applied.push(format!(
                        "post_comment:{}:{}",
                        intent.repository, change.change_request
                    ));
                    if std::mem::take(&mut self.crash_next_apply) {
                        return Ok(AttemptOutcome::Crashed);
                    }
                    // Read-back: the receipt cites the remote-issued
                    // comment identity, read from the remote's stored
                    // discussion — not the attempted payload.
                    self.apply_queued_mutations();
                    let posted = self
                        .comments
                        .iter()
                        .find(|c| c.comment_id == comment_id)
                        .expect("the just-written comment");
                    let body_digest = workloop_core::domain::sha256_hex(
                        "comment-v1",
                        &workloop_core::domain::canonical_json(&serde_json::json!({
                            "body": posted.body,
                        })),
                    );
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Applied,
                            vec![NormalizedEffectFact::CommentPosted {
                                change: change.clone(),
                                comment_id: posted.comment_id.clone(),
                                actor: posted.actor.clone(),
                                marker: marker.clone(),
                                body_digest,
                                target: None,
                            }],
                        )),
                        now,
                    )?;
                } else {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                }
            }
            HostAction::PublishReviewFeedback { change, head, body } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() != Some(head.object.as_str()) {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                let review_id = format!("rv-{}", intent.action_id);
                self.reviews
                    .entry(change.change_request.as_str().to_string())
                    .or_default()
                    .push(ReviewRecord {
                        review_id: review_id.clone(),
                        actor: authenticated.clone(),
                        body: body.clone(),
                        // The head guard above already established
                        // `actual == head.object`: the remote binds the
                        // review to the revision it was posted against.
                        head: head.object.clone(),
                    });
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.bump(&ReadSubject::Review {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "publish_review_feedback:{}:{}",
                    intent.repository, change.change_request
                ));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the receipt cites the remote-issued review
                // id and the stored body digest.
                self.apply_queued_mutations();
                let posted = self
                    .reviews
                    .get(change.change_request.as_str())
                    .and_then(|rs| rs.iter().find(|r| r.review_id == review_id))
                    .expect("the just-written review");
                let body_digest = workloop_core::domain::sha256_hex(
                    "review-v1",
                    &workloop_core::domain::canonical_json(&serde_json::json!({
                        "body": posted.body,
                    })),
                );
                self.emit(
                    store,
                    config,
                    obs(result(
                        intent.action_id.clone(),
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::ReviewPublished {
                            change: change.clone(),
                            review_id: posted.review_id.clone(),
                            head: head.clone(),
                            body_digest,
                        }],
                    )),
                    now,
                )?;
            }
            HostAction::ReplyToThread {
                change,
                thread_id,
                marker,
                body,
            } => {
                let belongs = self
                    .threads
                    .get(thread_id)
                    .is_some_and(|t| t.change_id == change.change_request.as_str());
                if !belongs {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "unknown thread".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                let reply_id = format!("rp-{}", intent.action_id);
                self.threads
                    .get_mut(thread_id)
                    .expect("checked")
                    .replies
                    .push((
                        reply_id.clone(),
                        authenticated.clone(),
                        marker.clone(),
                        body.clone(),
                    ));
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied
                    .push(format!("thread_reply:{}:{}", intent.repository, thread_id));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the receipt cites the remote-issued reply
                // identity on the exact thread.
                self.apply_queued_mutations();
                let posted = self
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.replies.iter().find(|(id, ..)| id == &reply_id))
                    .expect("the just-written reply");
                let body_digest = workloop_core::domain::sha256_hex(
                    "reply-v1",
                    &workloop_core::domain::canonical_json(&serde_json::json!({
                        "body": posted.3,
                    })),
                );
                self.emit(
                    store,
                    config,
                    obs(result(
                        intent.action_id.clone(),
                        HostActionStatus::Applied,
                        vec![NormalizedEffectFact::ReplyPosted {
                            change: change.clone(),
                            thread_id: thread_id.clone(),
                            marker: marker.clone(),
                            reply_id: posted.0.clone(),
                            body_digest,
                        }],
                    )),
                    now,
                )?;
            }
            HostAction::ResolveThread {
                change,
                thread_id,
                head,
            } => {
                let belongs = self
                    .threads
                    .get(thread_id)
                    .is_some_and(|t| t.change_id == change.change_request.as_str());
                if !belongs {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "unknown thread".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() != Some(head.object.as_str()) {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                {
                    let thread = self.threads.get_mut(thread_id).expect("checked");
                    thread.resolved = true;
                    thread.resolved_by = Some(authenticated.clone());
                }
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "resolve_thread:{}:{}",
                    intent.repository, thread_id
                ));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the receipt reports the thread state a
                // fresh read observes.
                self.apply_queued_mutations();
                let resolved = self.threads.get(thread_id).is_some_and(|t| t.resolved);
                let observed_head = self.changes.get(change.change_request.as_str()).cloned();
                if resolved && let Some(object) = observed_head {
                    let observed = CommitRevision::new(intent.repository.clone(), object.clone())
                        .map_err(|detail| StoreError::Internal { detail })?;
                    // The receipt echoes the bound head and reports the
                    // observed head's relation to it — a racing actor
                    // may move the change across the write/read gap.
                    let relation = if object == head.object {
                        RefRelation::Equal
                    } else if self.descends(&object, &head.object) {
                        RefRelation::Ancestor
                    } else {
                        RefRelation::Diverged
                    };
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Applied,
                            vec![NormalizedEffectFact::ThreadResolved {
                                change: change.clone(),
                                thread_id: thread_id.clone(),
                                target: DiffTarget {
                                    change: change.clone(),
                                    head: head.clone(),
                                    base: None,
                                    target_revision: None,
                                },
                                observed,
                                relation,
                            }],
                        )),
                        now,
                    )?;
                } else {
                    // The write may have been undone by a racing actor:
                    // the outcome cannot be established — report
                    // unknown, never a fabricated resolve.
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Unknown {
                                reason: "thread not resolved at read-back".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                }
            }
            HostAction::MarkReady {
                change,
                expected_head,
            } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() != Some(expected_head.object.as_str()) {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                self.ready_actors.insert(
                    change.change_request.as_str().to_string(),
                    authenticated.clone(),
                );
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "mark_ready:{}:{}",
                    intent.repository, change.change_request
                ));
                if std::mem::take(&mut self.crash_next_apply) {
                    return Ok(AttemptOutcome::Crashed);
                }
                // Read-back: the ready state and head a fresh read
                // observes — not the attempted values.
                self.apply_queued_mutations();
                let ready = self.is_ready(change.change_request.as_str());
                let observed_head = self.changes.get(change.change_request.as_str()).cloned();
                match (ready, observed_head) {
                    (true, Some(object)) => {
                        let observed =
                            CommitRevision::new(intent.repository.clone(), object.clone())
                                .map_err(|detail| StoreError::Internal { detail })?;
                        // Same bound-echo/observed split as the resolve
                        // receipt: the remote may have raced the
                        // write/read gap.
                        let relation = if object == expected_head.object {
                            RefRelation::Equal
                        } else if self.descends(&object, &expected_head.object) {
                            RefRelation::Ancestor
                        } else {
                            RefRelation::Diverged
                        };
                        self.emit(
                            store,
                            config,
                            obs(result(
                                intent.action_id.clone(),
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::ReadyObserved {
                                    change: change.clone(),
                                    head: expected_head.clone(),
                                    observed,
                                    relation,
                                }],
                            )),
                            now,
                        )?;
                    }
                    _ => {
                        self.emit(
                            store,
                            config,
                            obs(result(
                                intent.action_id.clone(),
                                HostActionStatus::Unknown {
                                    reason: "ready state not observed at read-back".into(),
                                },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                }
            }
        }
        Ok(AttemptOutcome::Attempted)
    }
}

/// The spec-declared issue sources of in-scope Works that still have an
/// active Execution — the exact set phase 1 owes an authoritative read,
/// paired with the Execution the read's Observation binds to.
pub(crate) fn declared_sources(
    store: &impl Store,
    scope: Option<&WorkId>,
) -> Result<Vec<(WorkId, ExecutionId, SourceSnapshot)>, StoreError> {
    store.transact(|tx| {
        let mut out = Vec::new();
        for work in tx.works()? {
            if let Some(w) = scope
                && work.id != *w
            {
                continue;
            }
            let Some(execution) = tx.active_execution(&work.id)? else {
                continue;
            };
            let Some(spec) = tx.spec(&work.id, Some(work.approved_spec))? else {
                continue;
            };
            for source in &spec.payload.sources {
                out.push((work.id.clone(), execution.id.clone(), source.clone()));
            }
        }
        Ok(out)
    })
}

/// Every `(Work, change)` pair this store has observed, including Work
/// whose Execution has already reached a terminal state. Merge
/// detection and bounded sibling re-checks must still see a completed
/// Work's open change; only its ordinary head polling is suppressed.
pub(crate) fn recorded_change_pairs(
    store: &impl Store,
    scope: Option<&WorkId>,
) -> Result<Vec<(WorkId, ChangeRequestRef)>, StoreError> {
    store.transact(|tx| {
        let mut out = Vec::new();
        for work in tx.works()? {
            if let Some(w) = scope
                && work.id != *w
            {
                continue;
            }
            let latest = tx
                .observations_for_work(&work.id)?
                .into_iter()
                .filter_map(|o| match o.fact {
                    ObservationFact::ChangeHead { change, .. } => Some(change),
                    _ => None,
                })
                .next_back();
            if let Some(change) = latest {
                out.push((work.id.clone(), change));
            }
        }
        Ok(out)
    })
}

/// The latest recorded `ChangeMerged` observation for one change: the
/// durable proof the pull already merged, plus the target branch and
/// base observed at that time. A later scan re-reads only the fresh
/// target base, never the pull again — re-reading every recorded merge
/// each pass would make remote reads grow with the merge history.
pub(crate) fn latest_recorded_merge(
    store: &impl Store,
    work_id: &WorkId,
    change: &ChangeRequestRef,
) -> Result<Option<(CommitRevision, CommitRevision, String)>, StoreError> {
    let observations = store.transact(|tx| tx.observations_for_work(work_id))?;
    Ok(observations
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::ChangeMerged {
                change: merged,
                head,
                base,
                target_branch,
            } if merged == *change => Some((head, base, target_branch)),
            _ => None,
        })
        .next_back())
}

/// The position of the latest `MergeCheck` observation for one change
/// — the "least recently serviced" key the bounded merge-detection and
/// merge-scan sets order by, so the deferred tail is reached. The key
/// is `(observed time, observation order)`: the time is
/// second-granular, so the durable append order ties it out and the
/// servicing rotation keeps moving instead of freezing on a
/// same-second leading set. Ordered by observed time then row order —
/// the same "latest wins" rule the other recorded-fact lookups use.
pub(crate) fn latest_recorded_merge_check(
    store: &impl Store,
    work_id: &WorkId,
    change: &ChangeRequestRef,
) -> Result<Option<(i64, usize)>, StoreError> {
    let observations = store.transact(|tx| tx.observations_for_work(work_id))?;
    Ok(observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            matches!(&o.fact, ObservationFact::MergeCheck { change: c, .. } if c == change)
                .then_some((o.observed_unix_seconds, index))
        })
        .max_by_key(|(observed, index)| (*observed, *index)))
}

/// The latest recorded `Mergeability` verdict for one exact
/// `(change, head, base)` pair — its state and the time it was
/// observed. A settled verdict (mergeable or conflicting) is final for
/// the pair: the episode model binds both revisions, so any real change
/// mints a new pair. `Unknown` is a provisional answer a later pass
/// must resolve. Ordered by observed time then row order — the same
/// "latest wins" rule the other recorded-fact lookups use.
pub(crate) fn latest_recorded_mergeability(
    store: &impl Store,
    work_id: &WorkId,
    change: &ChangeRequestRef,
    head_object: &str,
    base: &CommitRevision,
) -> Result<Option<(MergeabilityState, i64)>, StoreError> {
    let observations = store.transact(|tx| tx.observations_for_work(work_id))?;
    Ok(observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            if let ObservationFact::Mergeability { target, state, .. } = &o.fact {
                let recorded_base = target
                    .base
                    .clone()
                    .or_else(|| target.target_revision.clone());
                if target.change == *change
                    && target.head.object == head_object
                    && recorded_base.as_ref() == Some(base)
                {
                    return Some((o.observed_unix_seconds, index, *state));
                }
            }
            None
        })
        .max_by_key(|(observed, index, _)| (*observed, *index))
        .map(|(observed, _, state)| (state, observed)))
}

/// The latest recorded `issue` read for one declared source, ordered by
/// observed time then row order — the same "latest wins" rule the
/// source gate uses.
pub(crate) fn latest_recorded_issue(
    store: &impl Store,
    work_id: &WorkId,
    issue: &IssueRef,
) -> Result<Option<(String, String, IssueState)>, StoreError> {
    let observations = store.transact(|tx| tx.observations_for_work(work_id))?;
    Ok(observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            if let ObservationFact::Issue {
                issue: i,
                source_revision,
                content_digest,
                state,
            } = &o.fact
                && i == issue
            {
                Some((
                    o.observed_unix_seconds,
                    index,
                    (
                        source_revision.clone(),
                        content_digest.clone(),
                        state.clone(),
                    ),
                ))
            } else {
                None
            }
        })
        .max_by_key(|(t, i, _)| (*t, *i))
        .map(|(_, _, v)| v))
}

/// The latest recorded `change_head` for one change, ordered by
/// observed time then row order — the same "latest wins" rule the
/// assessment paths use.
pub(crate) fn latest_recorded_head(
    store: &impl Store,
    work_id: &WorkId,
    change: &ChangeRequestRef,
) -> Result<Option<String>, StoreError> {
    let observations = store.transact(|tx| tx.observations_for_work(work_id))?;
    Ok(observations
        .iter()
        .enumerate()
        .filter_map(|(index, o)| {
            if let ObservationFact::ChangeHead { change: c, head } = &o.fact
                && c == change
            {
                Some((o.observed_unix_seconds, index, head.object.clone()))
            } else {
                None
            }
        })
        .max_by_key(|(t, i, _)| (*t, *i))
        .map(|(_, _, h)| h))
}

/// The slice-0 fixture on the shared integration port: inbound facts
/// through `ingest`, outbound writes through the recorded-intent
/// outbox. The real adapter implements the same trait, so one
/// contract test drives both.
impl crate::port::IntegrationPort for FixtureGithub {
    fn ingest(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        self.emit(store, config, observation, now_unix_seconds)
    }

    fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        // Routed through the bounded drain (its unbounded form) so the
        // trait method never shadows the inherent one.
        self.apply_outbox_bounded(
            store,
            config,
            &mut || now_unix_seconds,
            None,
            None,
            usize::MAX,
        )
        .map(|_| ())
    }
}

/// The fixture's [`SourceReader`] (#202): a paged issue listing over
/// the planted `source_issues` dataset. `cursor` is the decimal offset
/// into the repository's `IssueRef`-sorted issue set — opaque to the
/// collector, exactly as a host's page token would be. An
/// unrecognized cursor or a non-positive `page_size` is a `Malformed`
/// read, matching a real transport's unusable-page response. A
/// repository with no planted issues and no failure knob is a
/// complete empty listing.
#[derive(Debug)]
pub struct FixtureSourceReader<'a> {
    fixture: &'a FixtureGithub,
    /// The actor the resolved credential authenticated as — the
    /// reader's identity for any attributed read.
    #[allow(dead_code)]
    actor: HostActorRef,
}

impl SourceReader for FixtureSourceReader<'_> {
    fn list_issues(
        &mut self,
        repository: &RepositoryRef,
        cursor: Option<&str>,
        page_size: usize,
    ) -> Result<IssuePage, SourceReadError> {
        let key = repository.to_string();
        if page_size == 0 {
            return Err(SourceReadError::Malformed {
                reason: format!("non-positive page size for {key}"),
            });
        }
        if self.fixture.source_denied.contains(&key) {
            return Err(SourceReadError::Denied {
                reason: format!("source read denied for {key}"),
            });
        }
        if self.fixture.source_unavailable.contains(&key) {
            return Err(SourceReadError::Unavailable {
                reason: format!("source read unavailable for {key}"),
            });
        }
        let offset = match cursor {
            None => 0usize,
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| SourceReadError::Malformed {
                    reason: format!("unrecognized page cursor for {key}"),
                })?,
        };
        let issues: Vec<CollectedIssue> = self
            .fixture
            .source_issues
            .get(repository)
            .map(|issues| issues.values().cloned().collect())
            .unwrap_or_default();
        let page: Vec<CollectedIssue> = issues
            .iter()
            .skip(offset)
            .take(page_size)
            .cloned()
            .collect();
        let next_cursor = if !page.is_empty() && offset + page.len() < issues.len() {
            Some((offset + page.len()).to_string())
        } else {
            None
        };
        Ok(IssuePage {
            issues: page,
            next_cursor,
        })
    }
}
