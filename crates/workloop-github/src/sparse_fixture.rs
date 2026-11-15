//! A second, independently structured fixture host for conformance
//! parameterization. `SparseHost` declares only a subset of the baseline
//! operations and reports every unsupported intent as an `unsupported`
//! receipt — an unsupported operation stays explicit and is never
//! counted as applied. Its remote model is a scoped event journal (repository +
//! branch/change keys), deliberately different from `FixtureGithub`'s
//! flat maps so shared-suite assertions cannot lean on one fixture's
//! internals.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use workloop_core::domain::WorkId;
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, HostActorRef, HostInstanceId, RepositoryRef,
};
use workloop_core::observation::{
    BranchState, Completeness, DeliveryId, DeliveryScope, HostActionStatus, NormalizedEffectFact,
    Observation, ObservationFact, ReadRevision, ReadSubject, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

use crate::credential::bind_credential;
use crate::fixture::{
    CredentialBlock, CredentialBook, ObservedEffect, ReconcileReport, RemoteRead,
};

/// Journal of every remote fact the fixture observed — the assertion
/// surface shared with `FixtureGithub` via the applied-write history.
#[derive(Debug, Default)]
pub struct SparseHost {
    /// `(repository, branch)` → head object id. Scoped keys: the same
    /// branch name on two repositories can never collide.
    branches: BTreeMap<(String, String), String>,
    /// `(repository, change_request_id)` → head object id.
    changes: BTreeMap<(String, String), String>,
    /// `(repository, change_request_id)` → the scoped ref the remote
    /// issued — the identity a reconcile read cites when a create's
    /// receipt was lost.
    change_refs: BTreeMap<(String, String), ChangeRequestRef>,
    /// `(repository, change_request_id)` → `(source_branch,
    /// target_branch)` — the immutable identity a lost create
    /// reconciles against.
    change_sources: BTreeMap<(String, String), (String, String)>,
    /// `(repository, change_request_id)` → the actor the remote
    /// attributes the change's head to — the opener, or none for a
    /// head planted by test control.
    change_actors: BTreeMap<(String, String), HostActorRef>,
    /// `(repository, branch)` → the actor that last moved the ref.
    branch_actors: BTreeMap<(String, String), HostActorRef>,
    /// `ReadSubject::key()` → the remote's version of that subject.
    /// Every mutation bumps the version of each subject it touched;
    /// reads carry the version they observed, so a delayed delivery of
    /// an older read can never outrank a newer one.
    versions: BTreeMap<String, u64>,
    next_id: u64,
    /// `repository` → next display number: a per-repository namespace.
    next_number: BTreeMap<String, u64>,
    /// `object` → parent object: the remote's recorded commit ancestry,
    /// used to classify an observed ref against the produced revision
    /// on a read-back (`Equal`/`Ancestor`/`Diverged`).
    parents: BTreeMap<String, String>,
    /// Remote mutations a racing actor lands after each successful
    /// adapter write, before the read-back — the write/read gap a real
    /// adapter cannot close.
    queued_mutations: VecDeque<SparseMutation>,
    /// `action name` → failure reason: test-injected execution faults.
    /// A matching attempt consumes the entry — one-shot per arming.
    forced_failures: BTreeMap<String, String>,
    /// `(operation, actor)` pairs the remote denies — an authorization
    /// verdict on the grant axis, never the support axis.
    denied_ops: BTreeSet<(String, String)>,
    /// The credential environment this adapter resolves and
    /// authenticates against before every write and reconcile read.
    credentials: Option<CredentialBook>,
    /// When set, this instance serves only intents recorded for this
    /// host instance — the binding a removed adapter can no longer
    /// answer for.
    bound_host: Option<HostInstanceId>,
    /// The write history, for test assertions.
    applied: Vec<String>,
}

/// A remote mutation queued by test control to model a racing actor:
/// applied after each successful adapter write, before the read-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseMutation {
    /// A racing actor moved a branch ref to `head` (descends from its
    /// previous position).
    MoveBranch {
        repository: RepositoryRef,
        branch: String,
        head: String,
        actor: HostActorRef,
    },
}

impl SparseHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// The capability set this fixture declares: the baseline read/write
    /// set plus `push_ref` and `change_request_create` only. Head-bound
    /// metadata and comment writes are unsupported and reported as such.
    /// This is the normalized-port vocabulary the U3.1 conformance
    /// suite gates on — facts delivered through `emit`/`attempt`. The
    /// typed-read surface is separate: this remote journals only
    /// change-request and branch heads, so a typed read of any other
    /// subject — including kinds this vocabulary declares — answers
    /// `Partial` rather than an invented revision.
    pub fn declared_capabilities() -> &'static [&'static str] {
        &[
            "repository_read",
            "branch_read",
            "push_ref",
            "change_request_create",
            "change_request_read",
            "checks_read",
            "review_findings_read",
        ]
    }

    /// Bound to one host instance — the adapter drains and reconciles
    /// only intents recorded for `host`.
    pub fn for_host(host: HostInstanceId) -> Self {
        Self {
            bound_host: Some(host),
            ..Self::default()
        }
    }

    /// Install the credential environment this adapter resolves and
    /// authenticates against before every write and reconcile read.
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

    /// Test control: `actor` lacks the grant for `operation` on the
    /// subject — attempts record `Failed{reason: denied}`, an
    /// authorization verdict distinct from `unsupported`.
    pub fn deny(&mut self, operation: &str, actor: &HostActorRef) {
        self.denied_ops
            .insert((operation.to_string(), actor.actor.as_str().to_string()));
    }

    /// Test control: move a branch head as the given actor — the
    /// remote record a reconcile read settles a lost push receipt
    /// from, and the position a guarded write compares against.
    pub fn set_branch(
        &mut self,
        repository: &RepositoryRef,
        branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
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

    /// Test control: register a change the remote attributes to
    /// `actor`, opened from `source_branch` onto `target_branch` —
    /// the remote record a reconcile read adopts when the create's
    /// receipt was lost.
    pub fn plant_change(
        &mut self,
        change: &ChangeRequestRef,
        source_branch: &str,
        target_branch: &str,
        head: &str,
        actor: &HostActorRef,
    ) {
        let key = (
            change.repository.to_string(),
            change.change_request.as_str().to_string(),
        );
        self.changes.insert(key.clone(), head.to_string());
        self.change_refs.insert(key.clone(), change.clone());
        self.change_sources.insert(
            key.clone(),
            (source_branch.to_string(), target_branch.to_string()),
        );
        self.change_actors.insert(key, actor.clone());
        self.bump(&ReadSubject::Pr {
            change: change.clone(),
        });
    }

    /// Test control: what the remote believes the change head is.
    /// Moves the scoped `pr` subject's version — the moved head is a
    /// different remote state its reads must order after.
    pub fn set_head(&mut self, repository: &RepositoryRef, change_id: &str, head_object: &str) {
        self.changes.insert(
            (repository.to_string(), change_id.to_string()),
            head_object.to_string(),
        );
        self.bump(&ReadSubject::Pr {
            change: ChangeRequestRef {
                repository: repository.clone(),
                change_request: ChangeRequestId::trusted(change_id),
                display_number: None,
            },
        });
    }

    fn bump(&mut self, subject: &ReadSubject) {
        *self.versions.entry(subject.key()).or_insert(0) += 1;
    }

    fn subject_version(&self, subject: &ReadSubject) -> u64 {
        self.versions.get(&subject.key()).copied().unwrap_or(0)
    }

    /// Resolve one typed read target against the remote: the observed
    /// revision in its per-kind form, the remote's version of the
    /// subject at read time, the actor the remote reports, and the
    /// read's completeness. This remote journals only change-request
    /// and branch heads — a subject with no journal entry, or one the
    /// adapter's declared read set does not cover, is a `Partial`
    /// read; the host never invents a revision. The declared
    /// capability vocabulary covers the normalized port — check,
    /// review and repository facts exercise it through `emit`
    /// deliveries — so their typed reads here are honest `Partial`
    /// answers for a subject the sparse remote does not journal, not a
    /// failure to serve the declared vocabulary.
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
        match subject {
            ReadSubject::Pr { change } => match self.changes.get(&(
                change.repository.to_string(),
                change.change_request.as_str().to_string(),
            )) {
                Some(head) => {
                    let actor = self
                        .change_actors
                        .get(&(
                            change.repository.to_string(),
                            change.change_request.as_str().to_string(),
                        ))
                        .cloned()
                        .unwrap_or_else(|| reader.clone());
                    match CommitRevision::new(change.repository.clone(), head) {
                        Ok(revision) => complete(ReadRevision::Sha { revision }, actor),
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
                        match CommitRevision::new(repository.clone(), head) {
                            Ok(revision) => complete(ReadRevision::Sha { revision }, actor),
                            Err(_) => partial("branch head is not a valid object id"),
                        }
                    }
                    None => partial("branch is not on the remote"),
                }
            }
            ReadSubject::Repository { .. } => {
                partial("no repository default-branch record on the remote")
            }
            ReadSubject::Check { .. } => partial("no check record on the remote"),
            ReadSubject::Review { .. } => partial("no review record on the remote"),
            // `issue_read` and diff snapshots are outside this
            // adapter's declared read set — a read it cannot perform
            // is partial, never an invented revision.
            ReadSubject::Issue { .. } | ReadSubject::Diff { .. } => {
                partial("subject kind outside this adapter's declared read set")
            }
        }
    }

    /// Poll one typed read target and deliver the Observation through
    /// the shared ingest pipeline — the same path a webhook delivery
    /// would use. The delivery identity is the subject at the observed
    /// version under this adapter's own source namespace, so a re-poll
    /// of an unchanged subject deduplicates and a delayed delivery of
    /// an older read is recorded as history only.
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
                source: "sparse.poll".into(),
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

    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// The head the remote records for `(repository, branch)` — a typed
    /// read independent of the write-history encoding.
    pub fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.branches
            .get(&(repository.to_string(), branch.to_string()))
            .cloned()
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
    pub fn queue_remote_mutation(&mut self, mutation: SparseMutation) {
        self.queued_mutations.push_back(mutation);
    }

    /// Apply every queued racing-actor mutation — the remote state the
    /// read-back will observe.
    fn apply_queued_mutations(&mut self) {
        while let Some(mutation) = self.queued_mutations.pop_front() {
            match mutation {
                SparseMutation::MoveBranch {
                    repository,
                    branch,
                    head,
                    actor,
                } => {
                    let key = (repository.to_string(), branch.clone());
                    if let Some(old) = self.branches.get(&key).cloned() {
                        self.parents.insert(head.clone(), old);
                    }
                    self.branches.insert(key.clone(), head);
                    self.branch_actors.insert(key, actor);
                    self.bump(&ReadSubject::Head { repository, branch });
                }
            }
        }
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

    /// Feed a synthetic Observation through the same ingest pipeline a
    /// real delivery would use.
    pub fn emit(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        handle_observation(store, config, &observation, now_unix_seconds)
    }

    /// Read-only reconciliation of every outstanding (`pending` or
    /// `unknown`) outbox intent, in order — the same contract
    /// `FixtureGithub::reconcile_outbox` implements: compare each
    /// intent's recorded effect against the remote's current state and
    /// feed the verdict through the normal receipt path.
    ///
    /// The pass performs host reads only — remote state is mutated
    /// solely inside [`Self::attempt`]. An observed effect settles
    /// `applied` with the remote's own attribution and normalized
    /// facts, a definitive-absence read settles an `unknown` intent
    /// `failed` (a `pending` intent may simply not have been attempted
    /// yet, so absence leaves it pending), and an indeterminate read
    /// changes nothing. Reconciliation is never a substitute for a
    /// write grant: the `host_write` check stays in [`Self::attempt`]
    /// alone, and a credential that authenticates as another actor
    /// blocks the intent exactly as it does on the write path. Intents
    /// bound to another host instance are left for that adapter.
    pub fn reconcile_outbox(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<ReconcileReport, StoreError> {
        let mut report = ReconcileReport::default();
        let outstanding = |i: &HostActionIntent| {
            matches!(
                i.status,
                HostActionStatus::Pending | HostActionStatus::Unknown { .. }
            )
        };
        let mut examined = std::collections::HashSet::new();
        loop {
            let next = store.transact(|tx| {
                Ok(tx
                    .action_intents()?
                    .into_iter()
                    .find(|i| outstanding(i) && !examined.contains(&i.action_id)))
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
                    observed_unix_seconds: now_unix_seconds,
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
                    // nothing.
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
                            now_unix_seconds,
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
                            now_unix_seconds,
                        )?;
                    }
                }
                ObservedEffect::Indeterminate { reason } => {
                    // Update the record when the read's explanation
                    // moved; an unchanged reason needs no new receipt.
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
                            now_unix_seconds,
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
    /// recorded effect against this remote's current state and report
    /// presence, definitive absence, or indeterminacy. Performs no
    /// mutation; every `Present` carries the actor the remote
    /// attributes the effect to and the normalized fact a fresh read
    /// derives. Operations this remote does not implement journal no
    /// effect, so their read is definitively absent.
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
                // Same attribution rule as `FixtureGithub`: an exact
                // head match credits the ref's last mover; once a
                // racing actor moved the branch on, ancestry proves
                // the push reached the branch and the intent's own
                // actor is the honest attribution.
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
                    .filter_map(|(key, _)| self.change_refs.get(key))
                    .filter(|change| change.repository == intent.repository)
                    .collect();
                match candidates.as_slice() {
                    [] => ObservedEffect::Absent,
                    [change] => {
                        let key = (
                            change.repository.to_string(),
                            change.change_request.as_str().to_string(),
                        );
                        let Some(object) = self.changes.get(&key) else {
                            return ObservedEffect::Indeterminate {
                                reason: "matching change request has no head on the remote".into(),
                            };
                        };
                        let Some(actor) = self.change_actors.get(&key).cloned() else {
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
            // This remote journals no effect for the operations it does
            // not implement — absence is the honest read.
            _ => ObservedEffect::Absent,
        }
    }

    /// Drain outstanding intents in outbox order, bounded to the intents
    /// pending when each attempt starts. Unsupported operations receive
    /// an `unsupported` receipt naming the operation — explicit, and
    /// never an `applied` claim.
    pub fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        if !config.allowed_effects.host_write {
            return Ok(());
        }
        let mut attempted = std::collections::HashSet::new();
        loop {
            let next = store.transact(|tx| {
                Ok(tx.action_intents()?.into_iter().find(|i| {
                    i.status == HostActionStatus::Pending && !attempted.contains(&i.action_id)
                }))
            })?;
            let Some(intent) = next else { return Ok(()) };
            attempted.insert(intent.action_id.clone());
            self.attempt(store, config, intent, now_unix_seconds)?;
        }
    }

    fn attempt(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        intent: HostActionIntent,
        now: i64,
    ) -> Result<(), StoreError> {
        // Not this instance's intent: leave it pending for the adapter
        // bound to the recorded host — an effect never migrates across
        // host instances.
        if self
            .bound_host
            .as_ref()
            .is_some_and(|bound| *bound != intent.host)
        {
            return Ok(());
        }
        let work_id = intent.work_id.clone();
        let execution = store.transact(|tx| {
            Ok(tx
                .executions_for_work(&work_id)?
                .into_iter()
                .max_by_key(|execution| execution.ordinal))
        })?;
        let Some(execution) = execution else {
            return Ok(());
        };
        if !execution.allowed_effects.host_write {
            return Ok(());
        }
        // The adapter-boundary credential gate, identical to
        // `FixtureGithub`: with a book installed, the intent's recorded
        // role binding resolves its credential slot and the resolved
        // value is authenticated against the intent's authorized actor
        // before any write. A failure leaves the intent pending — an
        // actor mismatch never rebinds an outstanding action, and a
        // same-actor renewal resolves the new secret under the
        // original binding.
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
                Err(_) => return Ok(()),
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
        let unsupported = |fixture: &SparseHost, name: &str| -> Result<(), StoreError> {
            fixture.emit(
                store,
                config,
                obs(result(
                    intent.action_id.clone(),
                    HostActionStatus::Unsupported {
                        reason: format!("unsupported operation: {name}"),
                    },
                    vec![],
                )),
                now,
            )?;
            Ok(())
        };

        // The operations this remote implements get the same
        // pre-write sequence `FixtureGithub` runs: reconcile-before-
        // write adopts an effect the remote already shows (the receipt
        // a crash dropped is recovered, the write is never re-executed
        // to discover whether it happened), an indeterminate read
        // keeps the intent outstanding, then the grant check records
        // `denied` — an authorization verdict on the grant axis,
        // distinct from `unsupported` on the support axis — and only
        // a definitive absence reaches the guarded write.
        if matches!(
            intent.action,
            HostAction::PushRef { .. } | HostAction::OpenChangeRequest { .. }
        ) {
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
                    return Ok(());
                }
                ObservedEffect::Indeterminate { reason } => {
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id,
                            HostActionStatus::Unknown { reason },
                            vec![],
                        )),
                        now,
                    )?;
                    return Ok(());
                }
                ObservedEffect::Absent => {}
            }
            let eligibility_key = || {
                (
                    intent.action.name().to_string(),
                    authenticated.actor.as_str().to_string(),
                )
            };
            if self.denied_ops.contains(&eligibility_key()) {
                self.emit(
                    store,
                    config,
                    obs(result(
                        intent.action_id,
                        HostActionStatus::Failed {
                            reason: "denied".into(),
                        },
                        vec![],
                    )),
                    now,
                )?;
                return Ok(());
            }
            // A test-armed execution fault: the write never reaches the
            // remote; the failed receipt is explicit and the intent
            // keeps its own record — a fault in one effect never
            // rewrites the receipts of the others.
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
                return Ok(());
            }
        }

        match &intent.action {
            HostAction::PushRef {
                branch,
                produced,
                expected_prior,
            } => {
                // The same fast-forward guard the main fixture models:
                // a moved remote ref refuses the write.
                let key = (intent.repository.to_string(), branch.clone());
                let actual = self.branches.get(&key).cloned();
                if actual != expected_prior.as_ref().map(|p| p.object.clone()) {
                    // The fresh position is recorded before the refusal
                    // receipt — the same order `FixtureGithub` reports it,
                    // so a receipt-driven re-issue derives its guard from
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
                    return Ok(());
                }
                self.branches.insert(
                    (intent.repository.to_string(), branch.clone()),
                    produced.object.clone(),
                );
                self.branch_actors.insert(
                    (intent.repository.to_string(), branch.clone()),
                    authenticated.clone(),
                );
                if let Some(prior) = &actual {
                    self.parents.insert(produced.object.clone(), prior.clone());
                }
                self.bump(&ReadSubject::Head {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                });
                self.applied.push(format!(
                    "push_ref:{}:{}",
                    intent.repository, produced.object
                ));
                // Racing actors land their writes before the read-back:
                // the receipt must report the observed post-state, not
                // the attempted value.
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
                        let revision = CommitRevision::new(intent.repository.clone(), object)
                            .map_err(|detail| StoreError::Internal { detail })?;
                        (revision, relation)
                    }
                    None => (produced.clone(), RefRelation::Diverged),
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
                ..
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
                let change_key = (
                    intent.repository.to_string(),
                    change.change_request.as_str().to_string(),
                );
                self.changes.insert(change_key.clone(), head_object.clone());
                self.change_refs.insert(change_key.clone(), change.clone());
                self.change_sources.insert(
                    change_key.clone(),
                    (source_branch.clone(), target_branch.clone()),
                );
                self.change_actors.insert(change_key, authenticated.clone());
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "open_change:{}:{}",
                    intent.repository, change.change_request
                ));
                // Racing actors land their writes before the read-back:
                // the receipt reports the change head the remote holds
                // at read-back time, never the attempted value.
                self.apply_queued_mutations();
                let observed_head = self
                    .changes
                    .get(&(
                        intent.repository.to_string(),
                        change.change_request.as_str().to_string(),
                    ))
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
            // This fixture implements no head-bound or discussion
            // writes. An explicit `unsupported` receipt keeps the
            // intent's fate honest — an unsupported capability is
            // never an applied receipt and never a permission verdict.
            HostAction::UpdateChangeRequest { .. } => {
                unsupported(self, "update_change_request")?;
            }
            HostAction::PostComment { .. } => {
                unsupported(self, "post_comment")?;
            }
            HostAction::PublishReviewFeedback { .. } => {
                unsupported(self, "publish_review_feedback")?;
            }
            HostAction::ReplyToThread { .. } => {
                unsupported(self, "thread_reply")?;
            }
            HostAction::ResolveThread { .. } => {
                unsupported(self, "resolve_thread")?;
            }
            HostAction::MarkReady { .. } => {
                unsupported(self, "mark_ready")?;
            }
        }
        Ok(())
    }
}
