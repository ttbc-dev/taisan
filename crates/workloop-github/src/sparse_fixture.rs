//! A second, independently structured fixture host for conformance
//! parameterization. `SparseHost` declares only a subset of the baseline
//! operations and reports every unsupported intent as an `unsupported`
//! receipt — an unsupported operation stays explicit and is never
//! counted as applied. Its remote model is a scoped event journal (repository +
//! branch/change keys), deliberately different from `FixtureGithub`'s
//! flat maps so shared-suite assertions cannot lean on one fixture's
//! internals.

use std::collections::BTreeMap;

use workloop_core::domain::WorkId;
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, HostActorRef, RepositoryRef,
};
use workloop_core::observation::{
    BranchState, Completeness, DeliveryId, DeliveryScope, HostActionStatus, NormalizedEffectFact,
    Observation, ObservationFact, ReadRevision, ReadSubject, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

use crate::fixture::RemoteRead;

/// Journal of every remote fact the fixture observed — the assertion
/// surface shared with `FixtureGithub` via the applied-write history.
#[derive(Debug, Default)]
pub struct SparseHost {
    /// `(repository, branch)` → head object id. Scoped keys: the same
    /// branch name on two repositories can never collide.
    branches: BTreeMap<(String, String), String>,
    /// `(repository, change_request_id)` → head object id.
    changes: BTreeMap<(String, String), String>,
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
    /// The write history, for test assertions.
    applied: Vec<String>,
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
        let execution_id = Some(execution.id);
        let obs = |fact: ObservationFact| Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.clone(),
            fact,
            observed_unix_seconds: now,
        };
        let result =
            |action_id: String, status: HostActionStatus, effects: Vec<NormalizedEffectFact>| {
                ObservationFact::HostActionResult {
                    action_id,
                    authenticated_actor: Some(intent.actor.clone()),
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
                    intent.actor.clone(),
                );
                self.bump(&ReadSubject::Head {
                    repository: intent.repository.clone(),
                    branch: branch.clone(),
                });
                self.applied.push(format!(
                    "push_ref:{}:{}",
                    intent.repository, produced.object
                ));
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
                            observed: produced.clone(),
                            relation: RefRelation::Equal,
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
                self.changes.insert(
                    (
                        intent.repository.to_string(),
                        change.change_request.as_str().to_string(),
                    ),
                    head_object.clone(),
                );
                self.change_actors.insert(
                    (
                        intent.repository.to_string(),
                        change.change_request.as_str().to_string(),
                    ),
                    intent.actor.clone(),
                );
                self.bump(&ReadSubject::Pr {
                    change: change.clone(),
                });
                self.applied.push(format!(
                    "open_change:{}:{}",
                    intent.repository, change.change_request
                ));
                let head = CommitRevision::new(intent.repository.clone(), head_object)
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
