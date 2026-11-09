//! Fixture host adapter: records `HostActionIntent` outbox rows and emits
//! synthetic `change_head`/`host_action_result` Observations under test
//! control.
//!
//! The adapter never mutates Execution state itself — every fact goes
//! through `work::handle_observation` so the outbox row, the head
//! recheck, and the lifecycle assessment land in the same durable
//! pipeline a real adapter would use.

use std::collections::BTreeMap;

use workloop_core::identity::{ChangeRequestId, ChangeRequestRef, CommitRevision};
use workloop_core::observation::{
    HostActionStatus, NormalizedEffectFact, Observation, ObservationFact, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

/// An in-memory host the tests steer. `apply_outbox` drains recorded
/// intents in outbox order; each write re-reads the synthetic remote head
/// immediately before applying, so an expected-head mismatch is detected
/// exactly where a real adapter would detect it.
#[derive(Debug, Default)]
pub struct FixtureGithub {
    /// `change_request_id` → current head object id.
    changes: BTreeMap<String, String>,
    /// `branch` → current head object id.
    branches: BTreeMap<String, String>,
    next_number: u64,
    /// The write history, for test assertions.
    applied: Vec<String>,
}

impl FixtureGithub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test control: what the synthetic remote believes the change head is.
    pub fn set_head(&mut self, change_id: &str, head_object: &str) {
        self.changes
            .insert(change_id.to_string(), head_object.to_string());
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
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
        // Attempts are tracked so an intent that cannot make progress is
        // skipped for the rest of this pass — it neither loops the pass
        // forever nor blocks later independent intents (a retried stuck
        // intent may resolve on the next pass).
        // The installation-level grant is the outermost control: a
        // revoked `host_write` stops the drain even for executions that
        // were admitted under a more permissive configuration.
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

        match &intent.action {
            HostAction::PushRef { branch, produced } => {
                self.branches
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
                    .get(source_branch)
                    .cloned()
                    .unwrap_or_else(|| format!("{source_branch}-head"));
                self.changes.insert(
                    change.change_request.as_str().to_string(),
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
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                ..
            } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
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
                                fields_digest: "fixture".into(),
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
                ..
            } => {
                let actual = self.changes.get(change.change_request.as_str()).cloned();
                if actual.as_deref() == Some(expected_head.object.as_str()) {
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
                                comment_id: format!("c-{}", intent.action_id),
                                actor: intent.actor.clone(),
                                marker: marker.clone(),
                                body_digest: "fixture".into(),
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
        }
        Ok(())
    }
}
