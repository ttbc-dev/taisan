//! Fixture GitHub adapter: records `PrActionIntent` outbox rows and emits
//! synthetic `pr_head`/`pr_checks`/`pr_review`/`pr_action_result`
//! Observations under test control.
//!
//! The adapter never mutates Execution state itself — every fact goes
//! through `work::handle_observation` so the outbox row, the head
//! recheck, and the lifecycle assessment land in the same durable
//! pipeline a real adapter would use.

use std::collections::BTreeMap;

use workloop_core::observation::{ActionResult, Observation, ObservationFact};
use workloop_core::store::{PrAction, PrActionStatus, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

/// An in-memory GitHub the tests steer. `apply_outbox` drains recorded
/// intents in outbox order; each write re-reads the synthetic remote head
/// immediately before applying, so an expected-head mismatch is detected
/// exactly where a real adapter would detect it.
#[derive(Debug, Default)]
pub struct FixtureGithub {
    /// `(repository, number)` → current head SHA.
    prs: BTreeMap<(String, u64), String>,
    next_number: u64,
    /// The write history, for test assertions.
    applied: Vec<String>,
}

impl FixtureGithub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test control: what the synthetic remote believes the PR head is.
    pub fn set_head(&mut self, repository: &str, number: u64, head_sha: &str) {
        self.prs
            .insert((repository.to_string(), number), head_sha.to_string());
        self.next_number = self.next_number.max(number);
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// Feed a synthetic Observation (pr_head, pr_checks, pr_review, an
    /// externally-supplied pr_action_result…) through the same ingest
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
    /// recorded as a `pr_action_result` Observation keyed by action ID;
    /// `unknown` stays outstanding until reconciled.
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
        let mut attempted = std::collections::HashSet::new();
        loop {
            let next = store.transact(|tx| {
                Ok(tx.action_intents()?.into_iter().find(|i| {
                    i.status == PrActionStatus::Pending && !attempted.contains(&i.action_id)
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
        intent: workloop_core::store::PrActionIntent,
        now: i64,
    ) -> Result<(), StoreError> {
        let work_id = intent.work_id.clone();
        let execution_id = store.transact(|tx| Ok(tx.active_execution(&work_id)?.map(|e| e.id)))?;
        let obs = |fact: ObservationFact| Observation {
            source_event_id: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.clone(),
            fact,
            observed_unix_seconds: now,
        };

        match &intent.action {
            PrAction::PublishPr { branch, .. } => {
                self.next_number += 1;
                let number = self.next_number;
                let head = format!("{}-head", branch);
                self.prs
                    .insert((intent.repository.clone(), number), head.clone());
                self.applied
                    .push(format!("publish_pr:{}:{}", intent.repository, number));
                // The write produced a fresh remote fact: the PR head.
                self.emit(
                    store,
                    config,
                    obs(ObservationFact::PrHead {
                        repository: intent.repository.clone(),
                        number,
                        head_sha: head,
                    }),
                    now,
                )?;
                self.emit(
                    store,
                    config,
                    obs(ObservationFact::PrActionResult {
                        action_id: intent.action_id,
                        action_kind: "publish_pr".into(),
                        result: ActionResult::Applied,
                    }),
                    now,
                )?;
            }
            PrAction::UpdatePr {
                number,
                expected_head,
                ..
            }
            | PrAction::PostPrComment {
                number,
                expected_head,
                ..
            } => {
                let kind = match &intent.action {
                    PrAction::UpdatePr { .. } => "update_pr",
                    _ => "post_pr_comment",
                };
                let actual = self.prs.get(&(intent.repository.clone(), *number)).cloned();
                if actual.as_deref() == Some(expected_head.as_str()) {
                    self.applied
                        .push(format!("{kind}:{}:{number}", intent.repository));
                    self.emit(
                        store,
                        config,
                        obs(ObservationFact::PrActionResult {
                            action_id: intent.action_id.clone(),
                            action_kind: kind.into(),
                            result: ActionResult::Applied,
                        }),
                        now,
                    )?;
                } else {
                    // Head moved under the recorded intent: abort, record
                    // the failure, and surface the fresh head — the caller
                    // decides whether to re-issue.
                    self.emit(
                        store,
                        config,
                        obs(ObservationFact::PrActionResult {
                            action_id: intent.action_id.clone(),
                            action_kind: kind.into(),
                            result: ActionResult::Failed,
                        }),
                        now,
                    )?;
                    if let Some(head) = actual {
                        self.emit(
                            store,
                            config,
                            obs(ObservationFact::PrHead {
                                repository: intent.repository.clone(),
                                number: *number,
                                head_sha: head,
                            }),
                            now,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}
