//! Fixture host adapter: records `HostActionIntent` outbox rows and emits
//! synthetic `issue`/`change_head`/`host_action_result` Observations
//! under test control.
//!
//! The adapter never mutates Execution state itself — every fact goes
//! through `work::handle_observation` so the outbox row, the head
//! recheck, and the lifecycle assessment land in the same durable
//! pipeline a real adapter would use.

use std::collections::BTreeMap;

use workloop_core::domain::{ExecutionId, WorkId};
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, IssueRef, RepositoryRef, SourceSnapshot,
};
use workloop_core::observation::{
    HostActionStatus, IssueState, NormalizedEffectFact, Observation, ObservationFact, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

/// An in-memory host the tests steer. `apply_outbox` drains recorded
/// intents in outbox order; each write re-reads the synthetic remote head
/// immediately before applying, so an expected-head mismatch is detected
/// exactly where a real adapter would detect it.
#[derive(Debug, Default)]
pub struct FixtureGithub {
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
    next_id: u64,
    /// `repository` → next display number: numbering is a per-repository
    /// namespace, as on a real host.
    next_number: BTreeMap<String, u64>,
    /// The write history, for test assertions.
    applied: Vec<String>,
}

/// What the synthetic remote reports for one source issue read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRead {
    pub source_revision: String,
    pub content_digest: String,
    pub state: IssueState,
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
            let host_attempt = self.attempt(store, config, intent, now)?;
            if host_attempt {
                report.attempted += 1;
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
    ) -> Result<bool, StoreError> {
        let work_id = intent.work_id.clone();
        let execution = store.transact(|tx| {
            Ok(tx
                .executions_for_work(&work_id)?
                .into_iter()
                .max_by_key(|execution| execution.ordinal))
        })?;
        let Some(execution) = execution else {
            return Ok(false);
        };
        if !execution.allowed_effects.host_write {
            return Ok(false);
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
        Ok(true)
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
