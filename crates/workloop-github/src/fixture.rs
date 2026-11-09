//! Fixture host adapter: records `HostActionIntent` outbox rows and emits
//! synthetic `change_head`/`host_action_result` Observations under test
//! control.
//!
//! The adapter never mutates Execution state itself — every fact goes
//! through `work::handle_observation` so the outbox row, the head
//! recheck, and the lifecycle assessment land in the same durable
//! pipeline a real adapter would use.

use std::collections::BTreeMap;

use workloop_core::domain::WorkId;
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
            report.attempted += 1;
            self.attempt(store, config, intent, now)?;
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

    /// Phase 1 (read-only source refresh): re-read every change head
    /// this fixture knows for in-scope Works and ingest a `change_head`
    /// Observation only when the synthetic remote head differs from the
    /// store's latest recorded head for that change. The read-compare
    /// makes the pass idempotent — a restart re-entering phase order
    /// observes no movement and ingests nothing — and a deadline stops
    /// the read before it starts the next source.
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
