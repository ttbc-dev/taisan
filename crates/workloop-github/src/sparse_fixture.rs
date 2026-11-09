//! A second, independently structured fixture host for conformance
//! parameterization. `SparseHost` declares only a subset of the baseline
//! operations and reports every unsupported intent as a failed receipt —
//! an unsupported operation stays explicit and is never counted as
//! applied. Its remote model is a scoped event journal (repository +
//! branch/change keys), deliberately different from `FixtureGithub`'s
//! flat maps so shared-suite assertions cannot lean on one fixture's
//! internals.

use std::collections::BTreeMap;

use workloop_core::identity::{ChangeRequestId, ChangeRequestRef, CommitRevision};
use workloop_core::observation::{
    HostActionStatus, NormalizedEffectFact, Observation, ObservationFact, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

/// Journal of every remote fact the fixture observed — the assertion
/// surface shared with `FixtureGithub` via the applied-write history.
#[derive(Debug, Default)]
pub struct SparseHost {
    /// `(repository, branch)` → head object id. Scoped keys: the same
    /// branch name on two repositories can never collide.
    branches: BTreeMap<(String, String), String>,
    /// `(repository, change_request_id)` → head object id.
    changes: BTreeMap<(String, String), String>,
    next_number: u64,
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
    pub fn set_head(
        &mut self,
        repository: &workloop_core::identity::RepositoryRef,
        change_id: &str,
        head_object: &str,
    ) {
        self.changes.insert(
            (repository.to_string(), change_id.to_string()),
            head_object.to_string(),
        );
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
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
    /// pending when each attempt starts. Unsupported operations receive a
    /// `failed` receipt naming the unsupported operation — explicit, and
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
                    HostActionStatus::Failed {
                        reason: format!("unsupported operation: {name}"),
                    },
                    vec![],
                )),
                now,
            )?;
            Ok(())
        };

        match &intent.action {
            HostAction::PushRef { branch, produced } => {
                self.branches.insert(
                    (intent.repository.to_string(), branch.clone()),
                    produced.object.clone(),
                );
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
                self.next_number += 1;
                let change = ChangeRequestRef {
                    repository: intent.repository.clone(),
                    change_request: ChangeRequestId::trusted(format!("cr-{}", self.next_number)),
                    display_number: Some(self.next_number),
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
            // This fixture implements no head-bound writes. Reporting
            // `failed(unsupported)` keeps the intent's fate explicit —
            // an unsupported capability is never an applied receipt.
            HostAction::UpdateChangeRequest { .. } => {
                unsupported(self, "update_change_request")?;
            }
            HostAction::PostComment { .. } => {
                unsupported(self, "post_comment")?;
            }
        }
        Ok(())
    }
}
