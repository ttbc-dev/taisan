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

use workloop_core::domain::{ExecutionId, WorkId};
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, HostActorRef, IssueRef,
    RepositoryRef, SourceSnapshot,
};
use workloop_core::observation::{
    BranchState, Completeness, DeliveryId, DeliveryScope, HostActionStatus, IssueState,
    NormalizedEffectFact, Observation, ObservationFact, ReadRevision, ReadSubject, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

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
    /// `change_request_id` → the full scoped ref, learned at open or
    /// registration time so raw-id mutators can bump the scoped
    /// subject version.
    change_refs: BTreeMap<String, ChangeRequestRef>,
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
    /// `change_request_id`s the remote currently reports ready
    /// (non-draft).
    ready_changes: std::collections::BTreeSet<String>,
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
    /// The write history, for test assertions.
    applied: Vec<String>,
    /// When installed, every pending intent's recorded credential
    /// binding is resolved and authenticated before its write — the
    /// adapter-boundary credential gate. Absent a book the fixture is
    /// permissive, preserving the pre-credential contract tests.
    credentials: Option<CredentialBook>,
}

/// What one pending intent's pass did.
enum AttemptOutcome {
    /// No host attempt ran: the intent names another host instance, the
    /// Execution is missing, or its `host_write` grant is denied.
    Skipped,
    /// A credential failure blocked the write before any host call;
    /// the intent stays `pending` and the pass reports the class.
    CredentialBlocked(&'static str),
    /// The adapter attempted the write at the host.
    Attempted,
}

/// What the synthetic remote reports for one source issue read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRead {
    pub source_revision: String,
    pub content_digest: String,
    pub state: IssueState,
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
}

/// One comment the remote stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    /// The change the comment hangs on.
    pub change_id: String,
    /// The remote-issued comment identity — the read-back object id.
    pub comment_id: String,
    pub actor: HostActorRef,
    pub body: String,
}

/// The remote's record of one review thread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteThread {
    /// The change the thread hangs on.
    pub change_id: String,
    pub resolved: bool,
    /// Posted replies, in order: `(reply_id, actor, body)`.
    pub replies: Vec<(String, HostActorRef, String)>,
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
/// already matching the recorded facts, and sources left for the next
/// pass because the tick/source deadline stopped the read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceRefreshReport {
    pub unchanged: usize,
    pub ingested: usize,
    pub deferred: Vec<String>,
}

/// What one host-outbox pass attempted, with the untouched remainder.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OutboxReport {
    pub attempted: usize,
    pub applied: usize,
    pub failed: usize,
    pub unknown: usize,
    /// Pending in-scope intents not attempted because the per-tick
    /// bound stopped the drain; they stay pending for the next pass.
    pub deferred: Vec<String>,
    /// Intents whose credential binding failed resolution or actor
    /// verification during the pass; each stays `pending` with its
    /// `credential.*` class recorded for the repair path.
    pub credential_blocked: Vec<CredentialBlock>,
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
            },
        );
        self.bump(&ReadSubject::Issue {
            issue: issue.clone(),
        });
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
        self.versions.get(&subject.key()).copied().unwrap_or(0)
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

    /// Test control: append one review to a change's review stream.
    pub fn add_review(&mut self, change: &ChangeRequestRef, review_id: &str, actor: &HostActorRef) {
        self.reviews
            .entry(change.change_request.as_str().to_string())
            .or_default()
            .push(ReviewRecord {
                review_id: review_id.to_string(),
                actor: actor.clone(),
                body: String::new(),
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

    /// The remote's thread record, for assertions on the post-effect
    /// state.
    pub fn thread(&self, thread_id: &str) -> Option<&RemoteThread> {
        self.threads.get(thread_id)
    }

    /// Whether the remote reports the change as ready (non-draft).
    pub fn is_ready(&self, change_id: &str) -> bool {
        self.ready_changes.contains(change_id)
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
                AttemptOutcome::Skipped => {}
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
                // Denied or unchanged: counted as attempted, still pending.
                _ => {}
            }
        }
        Ok(report)
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
        let mut report = SourceRefreshReport::default();
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
            let (revision, digest, state) = match self.issues.get(&key) {
                Some(read) => (
                    read.source_revision.clone(),
                    read.content_digest.clone(),
                    read.state.clone(),
                ),
                None => (String::new(), String::new(), IssueState::Unavailable),
            };
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
            if let HostAction::UpdateChangeRequest { change, .. } = &intent.action {
                note(change);
            }
            for effect in &intent.effects {
                match effect {
                    NormalizedEffectFact::ChangeOpened { change, .. }
                    | NormalizedEffectFact::MetadataUpdated { change, .. }
                    | NormalizedEffectFact::CommentPosted { change, .. } => note(change),
                    _ => {}
                }
            }
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
            return Ok(AttemptOutcome::Skipped);
        }
        let work_id = intent.work_id.clone();
        let execution = store.transact(|tx| {
            Ok(tx
                .executions_for_work(&work_id)?
                .into_iter()
                .max_by_key(|execution| execution.ordinal))
        })?;
        let Some(execution) = execution else {
            return Ok(AttemptOutcome::Skipped);
        };
        if !execution.allowed_effects.host_write {
            return Ok(AttemptOutcome::Skipped);
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
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "open_change:{}:{}",
                    intent.repository, change.change_request
                ));
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
                    self.bump_change(change.change_request.as_str());
                    self.applied.push(format!(
                        "update_change:{}:{}",
                        intent.repository, change.change_request
                    ));
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
                        body: body.clone(),
                    });
                    self.bump(&ReadSubject::Pr {
                        change: change.clone(),
                    });
                    self.applied.push(format!(
                        "post_comment:{}:{}",
                        intent.repository, change.change_request
                    ));
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
                    .push((reply_id.clone(), authenticated.clone(), body.clone()));
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied
                    .push(format!("thread_reply:{}:{}", intent.repository, thread_id));
                // Read-back: the receipt cites the remote-issued reply
                // identity on the exact thread.
                self.apply_queued_mutations();
                let posted = self
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.replies.iter().find(|(id, _, _)| id == &reply_id))
                    .expect("the just-written reply");
                let body_digest = workloop_core::domain::sha256_hex(
                    "reply-v1",
                    &workloop_core::domain::canonical_json(&serde_json::json!({
                        "body": posted.2,
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
                self.threads.get_mut(thread_id).expect("checked").resolved = true;
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "resolve_thread:{}:{}",
                    intent.repository, thread_id
                ));
                // Read-back: the receipt reports the thread state a
                // fresh read observes.
                self.apply_queued_mutations();
                let resolved = self.threads.get(thread_id).is_some_and(|t| t.resolved);
                let observed_head = self.changes.get(change.change_request.as_str()).cloned();
                if resolved && let Some(object) = observed_head {
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
                                    head: CommitRevision::new(intent.repository.clone(), object)
                                        .map_err(|detail| StoreError::Internal { detail })?,
                                    base: None,
                                    target_revision: None,
                                },
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
                self.ready_changes
                    .insert(change.change_request.as_str().to_string());
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "mark_ready:{}:{}",
                    intent.repository, change.change_request
                ));
                // Read-back: the ready state and head a fresh read
                // observes — not the attempted values.
                self.apply_queued_mutations();
                let ready = self.is_ready(change.change_request.as_str());
                let observed_head = self.changes.get(change.change_request.as_str()).cloned();
                match (ready, observed_head) {
                    (true, Some(object)) => {
                        self.emit(
                            store,
                            config,
                            obs(result(
                                intent.action_id.clone(),
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::ReadyObserved {
                                    change: change.clone(),
                                    head: CommitRevision::new(intent.repository.clone(), object)
                                        .map_err(|detail| StoreError::Internal { detail })?,
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
fn declared_sources(
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

/// The latest recorded `issue` read for one declared source, ordered by
/// observed time then row order — the same "latest wins" rule the
/// source gate uses.
fn latest_recorded_issue(
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
fn latest_recorded_head(
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
