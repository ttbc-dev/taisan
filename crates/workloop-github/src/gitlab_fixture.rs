//! A third, independently structured fixture host for conformance
//! parameterization. `GitlabFixture` models a GitLab-shaped remote —
//! deliberately different mechanics from `FixtureGithub`, the same
//! outbox contract:
//!
//! - Change-request identity is a **per-project IID** (`!N`), not a
//!   globally unique provider ID: `!1` on two repositories names two
//!   different merge requests, so only the scoped (repository, IID)
//!   pair is a stable identity — matching the contract rule that
//!   display numbers and provider-local numbering are never Core
//!   equality keys.
//! - A merge request has no stored head: its head is *derived* from the
//!   source branch at read time, so pushing to the source branch moves
//!   the change-request head — the same contract-visible effect as a
//!   head edit, produced by a different mechanism.
//! - `open_change_request` requires the source branch to exist on the
//!   remote; an absent branch is a `failed` receipt, never an invented
//!   head.
//! - The adapter is bound to one host instance and drains only intents
//!   recorded for that instance; intents for another instance are left
//!   `pending` for their own adapter — an effect never migrates across
//!   hosts.
//!
//! Its remote model is a per-project namespace map, deliberately
//! different from `FixtureGithub`'s flat maps and `SparseHost`'s event
//! journal so shared-suite assertions cannot lean on one fixture's
//! internals.

use std::collections::BTreeMap;

use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, HostInstanceId, RepositoryRef,
};
use workloop_core::observation::{
    HostActionStatus, NormalizedEffectFact, Observation, ObservationFact, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

/// One project (repository) on this host instance: an independent
/// numbering, branch, and merge-request namespace.
#[derive(Debug, Default)]
struct Project {
    /// Per-project merge-request counter — the IID namespace. Two
    /// projects each have a `!1`; they are different change requests.
    next_iid: u64,
    /// `branch` → head object id.
    branches: BTreeMap<String, String>,
    /// `change_request` IID string → merge request. No stored head: the
    /// head always derives from the source branch.
    merge_requests: BTreeMap<String, MergeRequest>,
}

/// A merge request tracks its source branch; the reported head follows
/// the branch rather than a snapshot taken at open time.
#[derive(Debug)]
struct MergeRequest {
    /// The IID — both identity and display number on this host.
    iid: u64,
    source_branch: String,
    /// Per-MR note counter, a different comment-ID scheme again.
    next_note: u64,
}

/// A GitLab-shaped in-memory host bound to one host instance.
#[derive(Debug)]
pub struct GitlabFixture {
    /// The host instance this adapter serves. Intents recorded for a
    /// different instance are not this adapter's to drain.
    bound_host: HostInstanceId,
    /// `repository` (host-scoped identity string) → project state.
    projects: BTreeMap<String, Project>,
    /// The write history, for test assertions.
    applied: Vec<String>,
}

impl Default for GitlabFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl GitlabFixture {
    /// Bound to the host instance the shared suite uses.
    pub fn new() -> Self {
        Self::for_host(HostInstanceId::trusted("test-host"))
    }

    /// Bound to an explicit host instance — the adapter drains only
    /// intents recorded for `host`.
    pub fn for_host(bound_host: HostInstanceId) -> Self {
        Self {
            bound_host,
            projects: BTreeMap::new(),
            applied: Vec::new(),
        }
    }

    /// The capability set this fixture declares: the full baseline
    /// except `issue_read`, which it does not implement. A source-bound
    /// Work is therefore rejected `unsupported_capability` at admission
    /// — support differences stay explicit at the declared set.
    pub fn declared_capabilities() -> &'static [&'static str] {
        &[
            "repository_read",
            "branch_read",
            "push_ref",
            "change_request_create",
            "change_request_read",
            "change_request_update",
            "comment_create",
            "checks_read",
            "review_findings_read",
        ]
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// The head the remote records for `(repository, branch)` — a typed
    /// read independent of the write-history encoding.
    pub fn remote_head(&self, repository: &RepositoryRef, branch: &str) -> Option<String> {
        self.projects
            .get(&repository.to_string())
            .and_then(|p| p.branches.get(branch).cloned())
    }

    /// Test control: move the change-request head. On this host the
    /// head follows the source branch, so the head moves by moving the
    /// branch — a different mechanism than a direct head edit, the same
    /// contract-visible outcome for head-bound writes.
    pub fn set_head(&mut self, repository: &RepositoryRef, change_id: &str, head_object: &str) {
        if let Some(project) = self.projects.get_mut(&repository.to_string())
            && let Some(mr) = project.merge_requests.get(change_id)
        {
            let source = mr.source_branch.clone();
            project.branches.insert(source, head_object.to_string());
        }
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
    /// pending when each attempt starts. Intents recorded for another
    /// host instance are skipped — they stay `pending` for the adapter
    /// bound there.
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

    /// The current head of a merge request, derived from its source
    /// branch — on this host there is no separate stored head to go stale.
    fn mr_head(&self, repository: &RepositoryRef, change_id: &str) -> Option<String> {
        let project = self.projects.get(&repository.to_string())?;
        let mr = project.merge_requests.get(change_id)?;
        project.branches.get(&mr.source_branch).cloned()
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
        if intent.host != self.bound_host {
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
        let failed = |fixture: &GitlabFixture, action_id: String, reason: &str| {
            fixture.emit(
                store,
                config,
                obs(result(
                    action_id,
                    HostActionStatus::Failed {
                        reason: reason.to_string(),
                    },
                    vec![],
                )),
                now,
            )
        };

        match &intent.action {
            HostAction::PushRef { branch, produced } => {
                let project = self
                    .projects
                    .entry(intent.repository.to_string())
                    .or_default();
                project
                    .branches
                    .insert(branch.clone(), produced.object.clone());
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
                // A push to an MR's source branch moves the MR head on
                // this host — report the fresh head for every open MR
                // tracking the pushed branch.
                let mut moved: Vec<(ChangeRequestRef, String)> = Vec::new();
                if let Some(project) = self.projects.get(&intent.repository.to_string()) {
                    for (change_id, mr) in &project.merge_requests {
                        if &mr.source_branch == branch
                            && let Some(head) = project.branches.get(&mr.source_branch)
                        {
                            moved.push((
                                ChangeRequestRef {
                                    repository: intent.repository.clone(),
                                    change_request: ChangeRequestId::trusted(change_id.clone()),
                                    display_number: Some(mr.iid),
                                },
                                head.clone(),
                            ));
                        }
                    }
                }
                for (change, head) in moved {
                    self.emit(
                        store,
                        config,
                        obs(ObservationFact::ChangeHead {
                            change,
                            head: CommitRevision::new(intent.repository.clone(), head)
                                .map_err(|detail| StoreError::Internal { detail })?,
                        }),
                        now,
                    )?;
                }
            }
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                ..
            } => {
                // On this host a merge request opens only against a
                // branch that exists on the remote — an absent branch is
                // a failed receipt, never an invented head.
                let head_object = self
                    .projects
                    .get(&intent.repository.to_string())
                    .and_then(|p| p.branches.get(source_branch).cloned());
                let Some(head_object) = head_object else {
                    failed(self, intent.action_id.clone(), "source branch not found")?;
                    return Ok(());
                };
                let project = self
                    .projects
                    .entry(intent.repository.to_string())
                    .or_default();
                project.next_iid += 1;
                let iid = project.next_iid;
                // Identity and display are the same per-project number:
                // `!{iid}` repeats across projects and names different
                // change requests — only the scoped pair is stable.
                let change = ChangeRequestRef {
                    repository: intent.repository.clone(),
                    change_request: ChangeRequestId::trusted(format!("!{iid}")),
                    display_number: Some(iid),
                };
                project.merge_requests.insert(
                    change.change_request.as_str().to_string(),
                    MergeRequest {
                        iid,
                        source_branch: source_branch.clone(),
                        next_note: 0,
                    },
                );
                self.applied.push(format!(
                    "open_change:{}:{}:{}",
                    intent.repository, change.change_request, target_branch
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
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                ..
            } => {
                let actual = self.mr_head(&intent.repository, change.change_request.as_str());
                if actual.as_deref() == Some(expected_head.object.as_str()) {
                    self.applied.push(format!(
                        "update_change:{}:{}",
                        intent.repository, change.change_request
                    ));
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Applied,
                            vec![NormalizedEffectFact::MetadataUpdated {
                                change: change.clone(),
                                head: expected_head.clone(),
                                fields_digest: "gitlab-fixture".into(),
                            }],
                        )),
                        now,
                    )?;
                } else {
                    failed(self, intent.action_id.clone(), "expected head moved")?;
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
                ..
            } => {
                let actual = self.mr_head(&intent.repository, change.change_request.as_str());
                if actual.as_deref() == Some(expected_head.object.as_str()) {
                    let note_id = {
                        let project = self
                            .projects
                            .get_mut(&intent.repository.to_string())
                            .ok_or_else(|| StoreError::Internal {
                                detail: "comment on unknown project".into(),
                            })?;
                        let mr = project
                            .merge_requests
                            .get_mut(change.change_request.as_str())
                            .ok_or_else(|| StoreError::Internal {
                                detail: "comment on unknown merge request".into(),
                            })?;
                        mr.next_note += 1;
                        format!("note-{}-{}", change.change_request.as_str(), mr.next_note)
                    };
                    self.applied.push(format!(
                        "post_comment:{}:{}",
                        intent.repository, change.change_request
                    ));
                    self.emit(
                        store,
                        config,
                        obs(result(
                            intent.action_id.clone(),
                            HostActionStatus::Applied,
                            vec![NormalizedEffectFact::CommentPosted {
                                change: change.clone(),
                                comment_id: note_id,
                                actor: intent.actor.clone(),
                                marker: marker.clone(),
                                body_digest: "gitlab-fixture".into(),
                                target: None,
                            }],
                        )),
                        now,
                    )?;
                } else {
                    failed(self, intent.action_id.clone(), "expected head moved")?;
                }
            }
        }
        Ok(())
    }
}
