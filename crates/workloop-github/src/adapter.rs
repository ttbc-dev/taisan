//! The real GitHub integration adapter: the same port the slice-0
//! fixture implements ([`crate::port::IntegrationPort`]) — inbound
//! `ingest(Observation)`, outbound recorded `HostActionIntent`s —
//! with every GitHub call made through the [`crate::api::GitHubApi`]
//! transport seam.
//!
//! Contract points this adapter holds (docs/module-contracts.md,
//! docs/decisions.md, docs/github-adapter-mapping.md):
//!
//! - **Webhook deliveries** carry their transport GUID as the
//!   `source_event_id`; a redelivery deduplicates to one recorded
//!   effect. A delivery is always followed by a fresh read of the
//!   current head, so a delayed delivery for an old head leaves its
//!   own fact as evidence for *its* revision while the recorded
//!   current head never moves backward.
//! - **Check and review facts** carry the exact head they examined —
//!   the check run's SHA and the review's `commit_id` — never the
//!   change's current head, so a newer head inherits nothing.
//! - **Review findings come from the findings surface**: a complete
//!   review round is claimed only after `review_findings` answered,
//!   carrying every finding examined at the review's head plus one
//!   occurrence per change-requesting review. A webhook delivery for a
//!   review records partial evidence (the event carries no findings)
//!   while the fresh review read behind that delivery records the
//!   host's current review state; a failed findings read records the
//!   review `unavailable` — an empty clean round is never invented.
//! - **Head-bound writes** (`update_change_request`,
//!   `post_comment`) re-read the head immediately before writing; a
//!   mismatch against `expected_head` aborts before any write and
//!   records `failed` plus a fresh `change_head` Observation. v0.1
//!   never re-issues an action automatically — a terminal receipt is
//!   terminal.
//! - **An unknown GitHub result** is recorded `unknown`, stays
//!   outstanding, and is never retried by a later drain; only
//!   [`GitHubAdapter::reconcile_outstanding`] resolves it, by action
//!   ID, against the host's current state.
//! - **Merge is outside the adapter**: no `HostAction` and no
//!   [`crate::api::GitHubApi`] operation can merge; `succeeded` turns
//!   on evidence, never on a merge.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use workloop_core::domain::{ExecutionId, WorkId, canonical_json, new_id, sha256_hex};
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, HostActorId, HostActorRef,
    HostCapability, HostInstanceId, RepositoryRef, canonical_host_instance,
};
use workloop_core::observation::{
    BranchState, CapabilityEntry, CapabilityState, CheckConclusion, Completeness, DeliveryId,
    DeliveryScope, HostActionStatus, IssueState, NormalizedEffectFact, Observation,
    ObservationFact, OccurrenceInput, ReadSubject, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

use crate::api::{ApiError, GitHubApi, RemoteReview, RemoteReviewFinding, RepositoryPermissions};
use crate::credential::{CredentialError, CredentialProviders, resolve_credential};
use crate::fixture::{
    AttemptOutcome, CredentialBlock, OutboxReport, ReconcileReport, SkippedIntent,
    SourceRefreshReport, declared_sources, latest_recorded_head, latest_recorded_issue,
};

/// One delivered GitHub webhook: the transport GUID GitHub assigned,
/// the time it stamped on the event, and the fact the event asserts —
/// bound to the head the event was about, never re-labeled onto a
/// newer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookDelivery {
    /// The GitHub delivery GUID — the `source_event_id` the store
    /// deduplicates on.
    pub event_id: String,
    /// When GitHub emitted the event. Evidence is ordered by this
    /// time (clamped to the delivery time), so a delayed delivery is
    /// history for its own revision, never a rollback.
    pub emitted_at: i64,
    pub fact: WebhookFact,
}

/// What one webhook event asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookFact {
    /// `pull_request.synchronize` / `push`: the head the event was
    /// about.
    ChangeHead {
        change: ChangeRequestRef,
        head: CommitRevision,
    },
    /// A check run concluded — on the exact head it examined.
    Check {
        change: ChangeRequestRef,
        head: CommitRevision,
        check_id: String,
        producer: HostActorRef,
        conclusion: CheckConclusion,
    },
    /// A review was submitted — on the exact head it examined. The
    /// event itself carries no findings, so it records partial
    /// evidence; the fresh review read behind the delivery records the
    /// findings the host reports.
    Review {
        change: ChangeRequestRef,
        head: CommitRevision,
        review_id: String,
        source: String,
    },
}

/// What one [`GitHubAdapter`] session applied, in order — the write
/// history assertions drive. Encoding is adapter-internal, matching
/// the fixtures' so shared-suite assertions need no special case.
#[derive(Default)]
pub struct GitHubAdapter<C> {
    client: C,
    /// When set, only intents recorded for this host instance are
    /// drained; another instance's intents stay `pending` for their
    /// own adapter — an effect never migrates across hosts.
    bound_host: Option<HostInstanceId>,
    /// The credential providers the adapter resolves `credential_ref`s
    /// through at the adapter boundary. `None` keeps the transport's
    /// ambient identity (the construction tests use); production
    /// installs the OS providers so every intent binds its declared
    /// credential before any read or write.
    credentials: Option<Box<dyn CredentialProviders>>,
    applied: Vec<String>,
    poll_records: u64,
}

impl<C: GitHubApi> GitHubAdapter<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            bound_host: None,
            credentials: None,
            applied: Vec::new(),
            poll_records: 0,
        }
    }

    /// Bound to one host instance — the adapter drains only intents
    /// recorded for `host`.
    pub fn for_host(host: HostInstanceId, client: C) -> Self {
        Self {
            client,
            bound_host: Some(host),
            credentials: None,
            applied: Vec::new(),
            poll_records: 0,
        }
    }

    /// Install the credential providers `credential_ref`s resolve
    /// through — the adapter-boundary binding gate.
    pub fn with_credentials(mut self, providers: Box<dyn CredentialProviders>) -> Self {
        self.credentials = Some(providers);
        self
    }

    /// The adapter-boundary credential gate — the same binding the
    /// fixture's book performs: the intent's recorded role binding
    /// resolves its credential slot at use time and the resolved value
    /// authenticates as the intent's authorized actor before any read
    /// or write. On success the transport runs under the bound
    /// credential for the rest of this intent's calls. `Ok(None)` —
    /// no credential book installed — keeps the ambient identity; a
    /// `credential.*` failure never rebinds an outstanding action
    /// under another account.
    fn bind_intent_credential(
        &mut self,
        store: &impl Store,
        intent: &HostActionIntent,
    ) -> Result<Option<CredentialError>, StoreError> {
        let Some(providers) = &self.credentials else {
            return Ok(None);
        };
        let reference = store.transact(|tx| {
            Ok(tx.work(&intent.work_id)?.and_then(|work| {
                work.binding
                    .actors
                    .iter()
                    .find(|binding| binding.role == intent.role)
                    .map(|binding| binding.credential_ref.clone())
            }))
        })?;
        let bind = resolve_credential(reference.as_ref(), &**providers).and_then(|resolved| {
            let authenticated = self.client.authenticate(&resolved)?;
            if authenticated != intent.actor {
                return Err(CredentialError::ActorMismatch {
                    bound: intent.actor.clone(),
                    authenticated,
                });
            }
            Ok(resolved)
        });
        match bind {
            Ok(resolved) => {
                self.client.set_credential(Some(resolved));
                Ok(None)
            }
            Err(error) => Ok(Some(error)),
        }
    }

    /// The capability set this adapter declares. Every name is backed
    /// by a concrete [`GitHubApi`] operation — support is never
    /// declared without the surface that provides it.
    pub fn declared_capabilities() -> &'static [&'static str] {
        &[
            "repository_read",
            "branch_read",
            "push_ref",
            "change_request_list",
            "change_request_create",
            "change_request_read",
            "change_request_update",
            "comment_create",
            "checks_read",
            "review_findings_read",
            "issue_read",
        ]
    }

    /// The contract operation an action needs; the declared set gates
    /// each intent before any host call.
    fn capability_for(action: &HostAction) -> &'static str {
        match action {
            HostAction::PushRef { .. } => "push_ref",
            HostAction::OpenChangeRequest { .. } => "change_request_create",
            HostAction::UpdateChangeRequest { .. } => "change_request_update",
            HostAction::PostComment { .. } => "comment_create",
            HostAction::PublishReviewFeedback { .. } => "review_feedback",
            HostAction::ReplyToThread { .. } => "thread_reply",
            HostAction::ResolveThread { .. } => "thread_resolve",
            HostAction::MarkReady { .. } => "mark_ready",
        }
    }

    fn supports(action: &HostAction) -> bool {
        Self::declared_capabilities().contains(&Self::capability_for(action))
    }

    /// Whether the capability-report `support` axis can honestly claim
    /// `capability`: backed by a concrete [`GitHubApi`] operation —
    /// the same declared surface `attempt` gates on.
    fn implements(capability: HostCapability) -> bool {
        Self::declared_capabilities()
            .iter()
            .any(|name| HostCapability::from_name(name) == Some(capability))
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    pub fn applied(&self) -> &[String] {
        &self.applied
    }

    /// The inbound half of the port: one Observation through the
    /// shared ingest pipeline (dedup, reconciliation, assessment).
    pub fn ingest(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        handle_observation(store, config, &observation, now_unix_seconds)
    }

    /// The outbound half of the port: attempt every outstanding
    /// (`pending`) intent in order. `unknown`, `failed`, and
    /// `unsupported` receipts are terminal — a later drain never
    /// replays them; only `pending` intents are attempted.
    pub fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        // The installation-level grant is the outermost control: a
        // revoked `host_write` stops the drain even for executions
        // admitted under a more permissive configuration.
        if !config.allowed_effects.host_write {
            return Ok(());
        }
        let mut attempted: HashSet<String> = HashSet::new();
        loop {
            let next = store.transact(|tx| {
                Ok(tx.action_intents()?.into_iter().find(|i| {
                    i.status == HostActionStatus::Pending && !attempted.contains(&i.action_id)
                }))
            })?;
            let Some(intent) = next else { break };
            attempted.insert(intent.action_id.clone());
            self.attempt(store, config, intent, now_unix_seconds)?;
        }
        Ok(())
    }

    /// The bounded write drain the normal driver runs each tick:
    /// attempt every outstanding (`pending`) in-scope intent in
    /// order, bounded by the per-tick item budget and the tick
    /// deadline. Only a real host attempt consumes the item budget —
    /// an intent that cannot run yet (its host binding, missing
    /// Execution, or denied `host_write` grant) is reported `skipped`
    /// without charging the budget, so a permanently unroutable
    /// intent never starves the ready intents behind it. The
    /// untouched remainder is reported `deferred`, never dropped.
    ///
    /// Authentication is the transport's ambient credential (`gh`'s
    /// own login or `GH_TOKEN`): this adapter performs no
    /// per-credential binding, so `credential_blocked` is never
    /// populated — the scenario-book credential gate belongs to the
    /// fixture, not to this driver.
    pub fn apply_outbox_bounded(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
        scope: Option<&WorkId>,
        max_items: usize,
    ) -> Result<OutboxReport, StoreError> {
        // Attempts are tracked so an intent that cannot make progress
        // is skipped for the rest of this pass — it neither loops the
        // pass forever nor blocks later independent intents.
        let mut report = OutboxReport::default();
        if !config.allowed_effects.host_write {
            return Ok(report);
        }
        let now = clock();
        let in_scope = |intent: &HostActionIntent| scope.is_none_or(|w| &intent.work_id == w);
        let mut attempted = HashSet::new();
        loop {
            // Both bounds stop the drain before the next item starts:
            // the per-tick item budget and the tick deadline. Whichever
            // trips, the untouched remainder is reported, never dropped.
            if report.attempted >= max_items || deadline.is_some_and(|d| clock() >= d) {
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

    /// The bounded read-only reconcile pass the normal driver runs
    /// before the write drain each tick: compare every outstanding
    /// (`pending` or `unknown`) in-scope intent's recorded effect
    /// against the remote's current state and feed the verdict
    /// through the same `host_action_result` receipt path a write
    /// attempt uses.
    ///
    /// An observed effect settles `applied` with the normalized fact
    /// a fresh read derives — never on this adapter's say-so — a
    /// definitive-absence read settles an `unknown` intent `failed`
    /// (a `pending` intent may simply not have been attempted yet, so
    /// absence leaves it pending), and an indeterminate read changes
    /// nothing for `pending` and only refreshes the recorded reason
    /// for `unknown`. Reconciliation performs host reads only and is
    /// never a substitute for a write grant: the `host_write` check
    /// stays in [`Self::attempt`] alone. Intents bound to another
    /// host instance are left for that adapter.
    pub fn reconcile_outbox_bounded(
        &mut self,
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
        let mut examined = HashSet::new();
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
            // adapter bound to the recorded host — reconciliation
            // never migrates an effect across host instances.
            if self
                .bound_host
                .as_ref()
                .is_some_and(|bound| *bound != intent.host)
            {
                continue;
            }
            report.examined += 1;
            let work_id = intent.work_id.clone();
            let execution_id = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(&work_id)?
                    .into_iter()
                    .max_by_key(|execution| execution.ordinal)
                    .map(|execution| execution.id))
            })?;
            let obs = |fact: ObservationFact| Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact,
                observed_unix_seconds: now,
            };
            let receipt = |status: HostActionStatus, effects: Vec<NormalizedEffectFact>| {
                ObservationFact::HostActionResult {
                    action_id: action_id.clone(),
                    // The reconciliation read does not authenticate an
                    // actor; `None` records exactly that.
                    authenticated_actor: None,
                    status,
                    effects,
                }
            };
            match self.observe_effect(store, &intent)? {
                RemoteEffect::Present { fact } => {
                    self.ingest(
                        store,
                        config,
                        obs(receipt(HostActionStatus::Applied, vec![*fact])),
                        now,
                    )?;
                }
                RemoteEffect::Absent { reason } => {
                    // Absence settles only an intent that already
                    // carries a verdict: `unknown` becomes `failed` — a
                    // definite no-effect outcome. A `pending` intent is
                    // possibly never attempted, so absence is not
                    // evidence of failure and the write drain's guarded
                    // attempt arbitrates instead.
                    if matches!(intent.status, HostActionStatus::Unknown { .. }) {
                        self.ingest(
                            store,
                            config,
                            obs(receipt(HostActionStatus::Failed { reason }, vec![])),
                            now,
                        )?;
                    }
                }
                RemoteEffect::Indeterminate { reason } => {
                    // Update the record when the read's explanation
                    // moved; an unchanged reason needs no new receipt,
                    // and a `pending` intent stays untouched — the
                    // write drain's pre-write read records `unknown`.
                    if let HostActionStatus::Unknown { reason: recorded } = &intent.status
                        && recorded != &reason
                    {
                        self.ingest(
                            store,
                            config,
                            obs(receipt(HostActionStatus::Unknown { reason }, vec![])),
                            now,
                        )?;
                    }
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
                Some(HostActionStatus::Unknown { .. }) => report.still_unknown += 1,
                _ => {}
            }
        }
        Ok(report)
    }

    /// Deliver one webhook. The event's own fact is recorded under its
    /// transport GUID (a redelivery deduplicates), and every recorded
    /// delivery is followed by a fresh read of the change's current
    /// head — so a delayed event for an old head stays evidence for
    /// its own revision while the recorded current head is what GitHub
    /// reports now, never the event's stale head.
    pub fn deliver_webhook(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        delivery: &WebhookDelivery,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        let (change, fact) = match &delivery.fact {
            WebhookFact::ChangeHead { change, head } => (
                change,
                ObservationFact::ChangeHead {
                    change: change.clone(),
                    head: head.clone(),
                },
            ),
            WebhookFact::Check {
                change,
                head,
                check_id,
                producer,
                conclusion,
            } => (
                change,
                ObservationFact::Check {
                    change: change.clone(),
                    head: head.clone(),
                    check_id: check_id.clone(),
                    producer: producer.clone(),
                    conclusion: conclusion.clone(),
                },
            ),
            WebhookFact::Review {
                change,
                head,
                review_id,
                source,
            } => (
                change,
                ObservationFact::Review {
                    target: DiffTarget {
                        change: change.clone(),
                        head: head.clone(),
                        base: None,
                        target_revision: None,
                    },
                    source: source.clone(),
                    source_revision: Some(review_id.clone()),
                    // The delivery asserts only that a review was
                    // submitted: it carries no findings, so it is
                    // partial evidence — never a complete round that
                    // could pass the review lane on its own.
                    completeness: Completeness::Partial {
                        reason: "webhook delivery carries no review findings".into(),
                    },
                    occurrences: vec![],
                },
            ),
        };
        // Only this installation's changes are ingested: a delivery
        // naming an unattributable change is refused, never recorded
        // as an unbound fact.
        let Some((work_id, execution_id)) = attribute_change(store, change)? else {
            return Ok(IngestReceipt::Rejected(
                "change request is not attributed to this installation".into(),
            ));
        };
        // Evidence ranks by the time GitHub stamped on the event,
        // clamped to the delivery time — a delayed delivery cannot
        // post-date a read taken before it arrived.
        let observed_at = delivery.emitted_at.min(now_unix_seconds);
        let receipt = self.ingest(
            store,
            config,
            Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Host {
                        host: change.repository.host.clone(),
                    },
                    source: "github.webhook".into(),
                    event_id: delivery.event_id.clone(),
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact,
                observed_unix_seconds: observed_at,
            },
            observed_at,
        )?;
        if matches!(&receipt, IngestReceipt::Rejected(_)) {
            return Ok(receipt);
        }
        // A review delivery is always followed by a fresh read of the
        // review state (reviews plus findings) — the event asserted no
        // findings, so what GitHub reports *now* is the only source
        // that can record a complete round for this review.
        if matches!(&delivery.fact, WebhookFact::Review { .. }) {
            self.ingest_review_state(
                store,
                config,
                change,
                &work_id,
                &execution_id,
                now_unix_seconds,
                None,
                None,
                &delivery.event_id,
                false,
            )?;
        }
        // The fresh read: what GitHub reports *now* is the recorded
        // current head, ingested after (and therefore outranking) the
        // event's own observation under the same ordering rule.
        if let Ok(pull) = self
            .client
            .read_pull(&change.repository, &change.change_request)
        {
            let head = CommitRevision::new(change.repository.clone(), pull.head_sha.clone())
                .map_err(|detail| StoreError::Internal { detail })?;
            // The read is keyed by the delivery that triggered it, not
            // by the head it re-reports: a redelivery of the same
            // webhook deduplicates, while a different delivery that
            // observes a previously seen head (a force push back)
            // still records the current head. Including the head keeps
            // a redelivery after the host moved from reusing one
            // identity for different content.
            let event_id = format!("read:{change}:{}@{}", delivery.event_id, pull.head_sha);
            self.ingest(
                store,
                config,
                Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Local,
                        source: "github.poll".into(),
                        event_id,
                    }),
                    subject_work_id: Some(work_id),
                    subject_execution_id: execution_id,
                    fact: ObservationFact::ChangeHead {
                        change: change.clone(),
                        head,
                    },
                    observed_unix_seconds: now_unix_seconds,
                },
                now_unix_seconds,
            )?;
        }
        Ok(receipt)
    }

    /// Read one change from GitHub and ingest each fact bound to the
    /// exact revision it examined: the current head, the check runs
    /// GitHub reports *for that head* (a run recorded for a
    /// superseded head is never re-labeled onto the new one), and
    /// every review with the `commit_id` it examined — so a newer
    /// head inherits no check and no review conclusion. Reads never
    /// mutate the remote; an outage invents nothing.
    pub fn refresh_change(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        change: &ChangeRequestRef,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let Some((work_id, execution_id)) = attribute_change(store, change)? else {
            return Ok(());
        };
        self.refresh_change_for_work(
            store,
            config,
            change,
            &work_id,
            &execution_id,
            now_unix_seconds,
        )
    }

    pub fn refresh_change_for_work(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        change: &ChangeRequestRef,
        work_id: &WorkId,
        execution_id: &Option<ExecutionId>,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let bound = store.transact(|tx| {
            Ok(tx.work(work_id)?.map(|work| {
                (
                    work.binding.repository,
                    work.binding.adapter,
                    work.binding.contract_version,
                )
            }))
        })?;
        let Some((bound_repository, bound_adapter, bound_contract)) = bound else {
            return Err(StoreError::ConstraintConflict {
                detail: "work binding missing".into(),
            });
        };
        if bound_adapter != config.host.adapter
            || bound_contract != config.host.contract_version
            || bound_repository != config.host.repository
            || bound_repository != change.repository
        {
            return Err(StoreError::ConstraintConflict {
                detail: format!(
                    "work {work} binding does not cover change repository {repo}",
                    work = work_id.0.as_str(),
                    repo = change.repository,
                ),
            });
        }
        if self
            .bound_host
            .as_ref()
            .is_some_and(|bound| *bound != change.repository.host)
        {
            return Err(StoreError::ConstraintConflict {
                detail: format!(
                    "adapter host binding does not cover {host}",
                    host = change.repository.host,
                ),
            });
        }
        if let Some(eid) = execution_id {
            let belongs = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(work_id)?
                    .iter()
                    .any(|execution| &execution.id == eid))
            })?;
            if !belongs {
                return Err(StoreError::ConstraintConflict {
                    detail: "execution does not belong to the supplied work".into(),
                });
            }
        }
        // Distinguishes successive local head reads. A read is evidence
        // of what the host reports *now*, so re-observing a head after a
        // force push must not deduplicate against an earlier read of the
        // same revision.
        let epoch = new_id("poll");
        let mut target_revision: Option<CommitRevision> = None;
        // The repository read gates every subject read: an unreachable
        // repository invents no facts and the next pass catches up.
        if self.client.read_repository(&change.repository).is_err() {
            return self.record_unavailable_review(
                store,
                config,
                change,
                work_id,
                execution_id,
                None,
                "repository read failed",
                None,
                &epoch,
                true,
                now_unix_seconds,
            );
        }
        let pull = match self
            .client
            .read_pull(&change.repository, &change.change_request)
        {
            Ok(pull) => pull,
            // Outage or unknown change: no facts invented, nothing
            // recorded — the next successful read catches up.
            Err(_) => {
                return self.record_unavailable_review(
                    store,
                    config,
                    change,
                    work_id,
                    execution_id,
                    None,
                    "change read failed",
                    None,
                    &epoch,
                    true,
                    now_unix_seconds,
                );
            }
        };
        let head = CommitRevision::new(change.repository.clone(), pull.head_sha)
            .map_err(|detail| StoreError::Internal { detail })?;
        let work_label = work_id.0.as_str();
        let exec_label = execution_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| "none".into());
        let host_obs = |fact: ObservationFact, event_id: String, at: i64| -> Observation {
            Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Host {
                        host: change.repository.host.clone(),
                    },
                    source: "github.poll".into(),
                    event_id,
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact,
                observed_unix_seconds: at,
            }
        };
        let read_id = format!(
            "read:{work_label}:{exec_label}:{change}@{}#{epoch}",
            head.object
        );
        self.poll_ingest(
            store,
            config,
            Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Local,
                    source: "github.poll".into(),
                    event_id: read_id,
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact: ObservationFact::ChangeHead {
                    change: change.clone(),
                    head: head.clone(),
                },
                observed_unix_seconds: now_unix_seconds,
            },
            now_unix_seconds,
        )?;
        // Check runs come from the exact head's endpoint; each fact
        // carries the run's own SHA.
        let runs = match self.client.check_runs(&change.repository, &head.object) {
            Ok(runs) => runs,
            Err(_) => {
                return self.record_unavailable_review(
                    store,
                    config,
                    change,
                    work_id,
                    execution_id,
                    Some(&head),
                    "check runs read failed",
                    target_revision.clone(),
                    &epoch,
                    true,
                    now_unix_seconds,
                );
            }
        };
        for run in runs {
            let run_head = CommitRevision::new(change.repository.clone(), run.head_sha.clone())
                .map_err(|detail| StoreError::Internal { detail })?;
            self.poll_ingest(
                store,
                config,
                host_obs(
                    ObservationFact::Check {
                        change: change.clone(),
                        head: run_head,
                        check_id: run.name.clone(),
                        producer: HostActorRef {
                            host: change.repository.host.clone(),
                            actor: HostActorId::trusted(run.producer.clone()),
                        },
                        conclusion: run.conclusion.clone(),
                    },
                    format!(
                        "check:{work_label}:{exec_label}:{change}:{}@{:?}#{epoch}",
                        run.check_run_id, run.conclusion
                    ),
                    now_unix_seconds,
                ),
                now_unix_seconds,
            )?;
        }
        // Reviews keep the commit they examined — a review of the
        // superseded head stays bound to it, and the round it seals
        // carries the findings read from the host. The empty stream
        // binds this read's own head, the revision just recorded as
        // current.
        match self
            .client
            .read_branch(&change.repository, &pull.target_branch)
        {
            Ok(remote_head) => {
                let state = match remote_head {
                    Some(object) => {
                        let revision = CommitRevision::new(change.repository.clone(), object)
                            .map_err(|detail| StoreError::Internal { detail })?;
                        target_revision = Some(revision.clone());
                        BranchState::At { revision }
                    }
                    None => BranchState::Absent,
                };
                self.poll_ingest(
                    store,
                    config,
                    host_obs(
                        ObservationFact::BranchState {
                            repository: change.repository.clone(),
                            branch: pull.target_branch.clone(),
                            state,
                        },
                        format!(
                            "branch:{work_label}:{exec_label}:{}:{}#{epoch}",
                            change.repository, pull.target_branch
                        ),
                        now_unix_seconds,
                    ),
                    now_unix_seconds,
                )?;
            }
            Err(_) => {
                self.poll_ingest(
                    store,
                    config,
                    host_obs(
                        ObservationFact::HostRead {
                            subject: ReadSubject::Head {
                                repository: change.repository.clone(),
                                branch: pull.target_branch.clone(),
                            },
                            revision: None,
                            subject_version: 0,
                            actor: HostActorRef {
                                host: change.repository.host.clone(),
                                actor: HostActorId::trusted("github.poll"),
                            },
                            completeness: Completeness::Unavailable {
                                reason: "target branch read failed".into(),
                            },
                        },
                        format!(
                            "branch-read:{work_label}:{exec_label}:{}:{}#{epoch}",
                            change.repository, pull.target_branch
                        ),
                        now_unix_seconds,
                    ),
                    now_unix_seconds,
                )?;
                let known_target = latest_recorded_target_revision(
                    store,
                    work_id,
                    &change.repository,
                    &pull.target_branch,
                )?;
                return self.record_unavailable_review(
                    store,
                    config,
                    change,
                    work_id,
                    execution_id,
                    Some(&head),
                    "target branch read failed",
                    known_target,
                    &epoch,
                    true,
                    now_unix_seconds,
                );
            }
        }
        // Reviews keep the commit they examined — a review of the
        // superseded head stays bound to it, and the round it seals
        // carries the findings read from the host. The empty stream
        // binds this read's own head, the revision just recorded as
        // current.
        self.ingest_review_state(
            store,
            config,
            change,
            work_id,
            execution_id,
            now_unix_seconds,
            Some(&head),
            target_revision,
            &epoch,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_unavailable_review(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        change: &ChangeRequestRef,
        work_id: &WorkId,
        execution_id: &Option<ExecutionId>,
        head: Option<&CommitRevision>,
        reason: &str,
        target_revision: Option<CommitRevision>,
        epoch: &str,
        dedup_by_content: bool,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let head = match head {
            Some(head) => Some(head.clone()),
            None => latest_recorded_head(store, work_id, change)?
                .and_then(|object| CommitRevision::new(change.repository.clone(), object).ok()),
        };
        let Some(head) = head else { return Ok(()) };
        let work_label = work_id.0.as_str();
        let exec_label = execution_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| "none".into());
        let token = sha256_hex("poll-unavailable-v1", reason.as_bytes());
        let observation = Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Host {
                    host: change.repository.host.clone(),
                },
                source: "github.poll".into(),
                event_id: format!(
                    "review-unavailable:{work_label}:{exec_label}:{change}@{}#{token}#{epoch}",
                    head.object
                ),
            }),
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.clone(),
            fact: ObservationFact::Review {
                target: DiffTarget {
                    change: change.clone(),
                    head,
                    base: None,
                    target_revision,
                },
                source: "github.reviews".into(),
                source_revision: None,
                completeness: Completeness::Unavailable {
                    reason: reason.into(),
                },
                occurrences: vec![],
            },
            observed_unix_seconds: now_unix_seconds,
        };
        if dedup_by_content {
            self.poll_ingest(store, config, observation, now_unix_seconds)?;
        } else {
            self.ingest(store, config, observation, now_unix_seconds)?;
        }
        Ok(())
    }

    fn poll_ingest(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let Some(work_id) = observation.subject_work_id.clone() else {
            let receipt = self.ingest(store, config, observation, now_unix_seconds)?;
            if matches!(receipt, IngestReceipt::Recorded) {
                self.poll_records = self.poll_records.wrapping_add(1);
            }
            return Ok(());
        };
        let execution_id = observation.subject_execution_id.clone();
        if !poll_fact_is_new(store, &work_id, &execution_id, &observation.fact)? {
            return Ok(());
        }
        let receipt = self.ingest(store, config, observation, now_unix_seconds)?;
        if matches!(receipt, IngestReceipt::Recorded) {
            self.poll_records = self.poll_records.wrapping_add(1);
        }
        Ok(())
    }

    /// The capability report this adapter can honestly emit, refreshed
    /// once per source pass. `support` is the adapter's own declared
    /// operation surface — the same names `attempt` gates on, mapped
    /// into the capability vocabulary — never a wider promise;
    /// `authorization` is the credential's real permission read, and a
    /// read that fails records `unknown` for every entry: unconfirmed
    /// is never granted. The report is re-emitted only when its content
    /// changes, so an unchanged credential costs no durable rows.
    fn report_capabilities(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let host = config.host.repository.host.clone();
        let permissions = self.client.read_permissions(&config.host.repository).ok();
        let mut capabilities: Vec<CapabilityEntry> = config
            .declared_capabilities
            .iter()
            .map(|capability| CapabilityEntry {
                capability: *capability,
                support: if Self::implements(*capability) {
                    CapabilityState::Supported
                } else {
                    CapabilityState::Denied
                },
                authorization: capability_authorization(*capability, permissions),
            })
            .collect();
        capabilities.sort_by_key(|entry| entry.capability);
        let latest = latest_capability_entries(store, &host)?;
        if latest
            .as_ref()
            .is_some_and(|entries| same_capability_set(entries, &capabilities))
        {
            return Ok(());
        }
        self.ingest(
            store,
            config,
            Observation {
                delivery: None,
                subject_work_id: None,
                subject_execution_id: None,
                fact: ObservationFact::CapabilityReport { host, capabilities },
                observed_unix_seconds: now_unix_seconds,
            },
            now_unix_seconds,
        )?;
        Ok(())
    }

    pub fn refresh_sources(
        &mut self,
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
        if let Some(d) = deadline {
            let remaining = d.saturating_sub(now).max(0) as u64;
            self.client
                .set_read_deadline(Some(Instant::now() + Duration::from_secs(remaining)));
        }
        self.report_capabilities(store, config, now)?;
        for (work_id, execution_id, source) in declared_sources(store, scope)? {
            if !work_bound_to_config(store, config, self.bound_host.as_ref(), &work_id)? {
                continue;
            }
            let key = source.issue.to_string();
            if let Some(d) = deadline
                && clock() >= d
            {
                report.deferred.push(key);
                continue;
            }
            // The transport serves exactly one host instance: a
            // declared source on any other host can never be read
            // through it, so it records `unavailable` — the source
            // gate holds — rather than reading a same-slug issue on
            // the served host and binding the foreign identity to the
            // wrong remote's state.
            let served = canonical_host_instance(source.issue.repository.host.as_str())
                == canonical_host_instance(config.host.repository.host.as_str());
            let remote = if !served {
                None
            } else {
                self.client.read_issue(&source.issue).ok()
            };
            let (revision, digest, state) = match &remote {
                Some(remote) => (
                    remote.source_revision.clone(),
                    remote.content_digest.clone(),
                    remote.state.clone(),
                ),
                None => (String::new(), String::new(), IssueState::Unavailable),
            };
            // Every completed authoritative read is carried out for
            // retention — even when the recorded observation is
            // unchanged — so the composition root can (re)retain the
            // immutable snapshot body a bound request resolves.
            if let Some(remote) = &remote {
                report.snapshots.push(crate::api::IssueSnapshotRead {
                    issue: source.issue.clone(),
                    source_revision: remote.source_revision.clone(),
                    content_digest: remote.content_digest.clone(),
                    title: remote.title.clone(),
                    body: remote.body.clone(),
                });
            }
            let latest = latest_recorded_issue(store, &work_id, &source.issue)?;
            if latest.as_ref() == Some(&(revision.clone(), digest.clone(), state.clone())) {
                report.unchanged += 1;
                continue;
            }
            self.ingest(
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
        for (work_id, change) in attributed_change_pairs(store, scope)? {
            let key = format!("{}:{change}", work_id.0);
            if let Some(d) = deadline
                && clock() >= d
            {
                report.deferred.push(key);
                continue;
            }
            if change.repository != config.host.repository
                || !work_bound_to_config(store, config, self.bound_host.as_ref(), &work_id)?
            {
                continue;
            }
            // Only an active Execution owes the change a poll — the
            // same scope `declared_sources` applies to issue reads. A
            // finished Work's change is history: re-reading it could
            // only ingest late facts onto a terminal Execution.
            let Some(execution_id) = store
                .transact(|tx| Ok(tx.active_execution(&work_id)?.map(|execution| execution.id)))?
            else {
                continue;
            };
            let before = self.poll_records;
            self.refresh_change_for_work(
                store,
                config,
                &change,
                &work_id,
                &Some(execution_id),
                now,
            )?;
            if self.poll_records != before {
                report.ingested += 1;
            } else {
                report.unchanged += 1;
            }
        }
        Ok(report)
    }

    /// Read one change's review state — the review stream plus the
    /// findings surface — and ingest each review bound to the exact
    /// revision it examined. Every review fact for one head carries
    /// that head's full occurrence set (all findings examined there,
    /// plus one occurrence per change-requesting review), so sealing is
    /// order-independent: a later clean review never masks an earlier
    /// reviewer's findings. A findings read that fails records the
    /// review `unavailable` — the empty clean round is never claimed
    /// without the findings source answering. An empty stream is an
    /// answer too: one fact for the stream itself, bound to `poll_head`
    /// (or a fresh pull read when the caller has none), so a published
    /// change with zero submitted reviews still seals the
    /// complete-empty round the enabled review lane needs instead of
    /// showing the review source unavailable with no round at all.
    #[allow(clippy::too_many_arguments)]
    fn ingest_review_state(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        change: &ChangeRequestRef,
        work_id: &WorkId,
        execution_id: &Option<ExecutionId>,
        now_unix_seconds: i64,
        poll_head: Option<&CommitRevision>,
        mut target_revision: Option<CommitRevision>,
        epoch: &str,
        dedup_by_content: bool,
    ) -> Result<(), StoreError> {
        let reviews = match self
            .client
            .reviews(&change.repository, &change.change_request)
        {
            Ok(reviews) => reviews,
            // Outage or unknown change: no facts invented — the next
            // successful read catches up.
            Err(_) => {
                return self.record_unavailable_review(
                    store,
                    config,
                    change,
                    work_id,
                    execution_id,
                    poll_head,
                    "reviews read failed",
                    target_revision,
                    epoch,
                    dedup_by_content,
                    now_unix_seconds,
                );
            }
        };
        let findings = self
            .client
            .review_findings(&change.repository, &change.change_request);
        if reviews.is_empty() {
            // The stream answered with no reviews: one fact for the
            // stream itself, bound to the head GitHub reports now —
            // the caller's fresh pull read when it has one, otherwise
            // a pull read of our own. With no head the empty state
            // binds to nothing and records nothing.
            let head = match poll_head {
                Some(head) => head.clone(),
                None => {
                    let Ok(pull) = self
                        .client
                        .read_pull(&change.repository, &change.change_request)
                    else {
                        return self.record_unavailable_review(
                            store,
                            config,
                            change,
                            work_id,
                            execution_id,
                            None,
                            "change read failed",
                            None,
                            epoch,
                            dedup_by_content,
                            now_unix_seconds,
                        );
                    };
                    target_revision = latest_recorded_target_revision(
                        store,
                        work_id,
                        &change.repository,
                        &pull.target_branch,
                    )?;
                    CommitRevision::new(change.repository.clone(), pull.head_sha)
                        .map_err(|detail| StoreError::Internal { detail })?
                }
            };
            let (completeness, occurrences) = match &findings {
                Ok(findings) => (
                    Completeness::Complete,
                    review_occurrences(&reviews, findings, &head.object),
                ),
                Err(_) => (
                    // The stream arrived but the findings surface did
                    // not: recorded `unavailable` evidence that can
                    // satisfy nothing — never an empty clean round.
                    Completeness::Unavailable {
                        reason: "review findings read failed".into(),
                    },
                    vec![],
                ),
            };
            // The head joins the identity: the empty stream observed
            // at a superseded head is different evidence from the
            // empty stream at the current one, and an identical re-read
            // still deduplicates.
            let token = review_fact_token(&completeness, &occurrences);
            let exec_label = execution_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "none".into());
            let observation = Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Host {
                        host: change.repository.host.clone(),
                    },
                    source: "github.poll".into(),
                    event_id: format!(
                        "review-empty:{}:{exec_label}:{change}@{}#{token}#{epoch}",
                        work_id.0.as_str(),
                        head.object
                    ),
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact: ObservationFact::Review {
                    target: DiffTarget {
                        change: change.clone(),
                        head,
                        base: None,
                        target_revision: target_revision.clone(),
                    },
                    // No reviewer: the stream itself is the
                    // source of this fact.
                    source: "github.reviews".into(),
                    source_revision: None,
                    completeness,
                    occurrences,
                },
                observed_unix_seconds: now_unix_seconds,
            };
            if dedup_by_content {
                self.poll_ingest(store, config, observation, now_unix_seconds)?;
            } else {
                self.ingest(store, config, observation, now_unix_seconds)?;
            }
            return Ok(());
        }
        for review in &reviews {
            let review_head =
                CommitRevision::new(change.repository.clone(), review.commit_sha.clone())
                    .map_err(|detail| StoreError::Internal { detail })?;
            let (completeness, occurrences) = match &findings {
                Ok(findings) => (
                    Completeness::Complete,
                    review_occurrences(&reviews, findings, &review_head.object),
                ),
                Err(_) => (
                    // The reviews arrived but the findings surface did
                    // not: source availability is part of the fact, so
                    // this review is recorded `unavailable` evidence
                    // that can satisfy nothing.
                    Completeness::Unavailable {
                        reason: "review findings read failed".into(),
                    },
                    vec![],
                ),
            };
            // The content-derived identity: a re-read that observes
            // changed findings records the change (a new identity),
            // while an identical re-read deduplicates — the same rule
            // the check facts use for a changed conclusion.
            let token = review_fact_token(&completeness, &occurrences);
            let exec_label = execution_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "none".into());
            let review_target = recorded_review_target(
                store,
                work_id,
                change,
                &review_head,
                &review.author,
                &review.review_id,
            )?
            .or_else(|| target_revision.clone());
            let observation = Observation {
                delivery: Some(DeliveryId {
                    scope: DeliveryScope::Host {
                        host: change.repository.host.clone(),
                    },
                    source: "github.poll".into(),
                    event_id: format!(
                        "review:{}:{exec_label}:{change}@{}#{token}#{epoch}",
                        work_id.0.as_str(),
                        review.review_id
                    ),
                }),
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: execution_id.clone(),
                fact: ObservationFact::Review {
                    target: DiffTarget {
                        change: change.clone(),
                        head: review_head.clone(),
                        base: None,
                        target_revision: review_target,
                    },
                    source: review.author.clone(),
                    source_revision: Some(review.review_id.clone()),
                    completeness,
                    occurrences,
                },
                observed_unix_seconds: now_unix_seconds,
            };
            if dedup_by_content {
                self.poll_ingest(store, config, observation, now_unix_seconds)?;
            } else {
                self.ingest(store, config, observation, now_unix_seconds)?;
            }
        }
        Ok(())
    }

    /// Resolve every `unknown` intent against GitHub, keyed by action
    /// ID — never a blind retry. A decisive read records a fresh
    /// result Observation (which reconciles the row); an undecidable
    /// one leaves the intent `unknown` and outstanding. Returns the
    /// action IDs that reached a terminal result.
    pub fn reconcile_outstanding(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<Vec<String>, StoreError> {
        let unknown: Vec<HostActionIntent> = store
            .transact(|tx| tx.action_intents())?
            .into_iter()
            .filter(|i| matches!(i.status, HostActionStatus::Unknown { .. }))
            .filter(|i| {
                self.bound_host
                    .as_ref()
                    .is_none_or(|bound| *bound == i.host)
            })
            .collect();
        let mut resolved = Vec::new();
        for intent in unknown {
            // The same adapter-boundary credential gate `attempt`
            // runs: the intent's recorded role binding resolves and
            // authenticates under the actor authorized at intent
            // time — a rotated credential that authenticates as
            // another actor never rebinds or settles the outstanding
            // action; a `credential.*` failure leaves it unknown and
            // outstanding for repair.
            if self.bind_intent_credential(store, &intent)?.is_some() {
                continue;
            }
            let execution = store.transact(|tx| {
                Ok(tx
                    .executions_for_work(&intent.work_id)?
                    .into_iter()
                    .max_by_key(|execution| execution.ordinal)
                    .map(|execution| execution.id))
            })?;
            let obs = |fact: ObservationFact| Observation {
                delivery: None,
                subject_work_id: Some(intent.work_id.clone()),
                subject_execution_id: execution.clone(),
                fact,
                observed_unix_seconds: now_unix_seconds,
            };
            let result = |status: HostActionStatus, effects: Vec<NormalizedEffectFact>| {
                ObservationFact::HostActionResult {
                    action_id: intent.action_id.clone(),
                    // The reconciliation read does not authenticate an
                    // actor; `None` records exactly that.
                    authenticated_actor: None,
                    status,
                    effects,
                }
            };
            let record = |adapter: &mut Self,
                          status: HostActionStatus,
                          effects: Vec<NormalizedEffectFact>|
             -> Result<bool, StoreError> {
                adapter.ingest(
                    store,
                    config,
                    obs(result(status, effects)),
                    now_unix_seconds,
                )?;
                Ok(true)
            };
            let mut terminal = false;
            match &intent.action {
                HostAction::PushRef {
                    branch, produced, ..
                } => {
                    if let Ok(head) = self.client.read_branch(&intent.repository, branch) {
                        match head {
                            Some(head) if head == produced.object => {
                                record(
                                    self,
                                    HostActionStatus::Applied,
                                    vec![NormalizedEffectFact::RefPublished {
                                        repository: intent.repository.clone(),
                                        branch: branch.clone(),
                                        produced: produced.clone(),
                                        observed: produced.clone(),
                                        relation: RefRelation::Equal,
                                    }],
                                )?;
                                terminal = true;
                            }
                            Some(head) => match self.client.contains_revision(
                                &intent.repository,
                                &produced.object,
                                &head,
                            ) {
                                Ok(true) => {
                                    let observed =
                                        CommitRevision::new(intent.repository.clone(), head)
                                            .map_err(|detail| StoreError::Internal { detail })?;
                                    record(
                                        self,
                                        HostActionStatus::Applied,
                                        vec![NormalizedEffectFact::RefPublished {
                                            repository: intent.repository.clone(),
                                            branch: branch.clone(),
                                            produced: produced.clone(),
                                            observed,
                                            relation: RefRelation::Ancestor,
                                        }],
                                    )?;
                                    terminal = true;
                                }
                                Ok(false) => {
                                    record(
                                        self,
                                        HostActionStatus::Failed {
                                            reason: "reconciled: the branch does not contain the \
                                                     produced revision"
                                                .into(),
                                        },
                                        vec![],
                                    )?;
                                    terminal = true;
                                }
                                // The ancestry read itself failed: the
                                // outcome stays undecidable — `unknown`
                                // and outstanding, never a guess.
                                Err(_) => {}
                            },
                            None => {
                                record(
                                    self,
                                    HostActionStatus::Failed {
                                        reason: "reconciled: the branch is absent".into(),
                                    },
                                    vec![],
                                )?;
                                terminal = true;
                            }
                        }
                    }
                }
                HostAction::OpenChangeRequest {
                    source_branch,
                    target_branch,
                    title,
                    body,
                } => {
                    // Identify the effect by what this Work recorded
                    // when it issued the open — the source revision of
                    // its own applied push (when one is recorded) plus
                    // the intent's own payload (target branch, title,
                    // body). Neither the branch nor the revision alone
                    // identifies the effect: the branch can carry a
                    // change request this Work did not open, and a
                    // pull request's head follows its source branch, so
                    // a foreign change moves onto the recorded revision
                    // after our push. With no recorded push the intent
                    // stays `unknown` — branch identity alone never
                    // adopts somebody else's change.
                    if let Some(source_revision) =
                        recorded_source_revision(store, &intent, source_branch)?
                        && let Ok(pulls) = self.client.list_pulls_on_branch(
                            &intent.repository,
                            source_branch,
                            &source_revision.object,
                        )
                    {
                        let found = pulls.iter().find(|pull| {
                            pull.target_branch == *target_branch
                                && pull.title == *title
                                && pull.body == *body
                        });
                        if let Some(pull) = found {
                            let change = ChangeRequestRef {
                                repository: intent.repository.clone(),
                                change_request: ChangeRequestId::trusted(pull.id.clone()),
                                display_number: Some(pull.display_number),
                            };
                            let head = CommitRevision::new(
                                intent.repository.clone(),
                                pull.head_sha.clone(),
                            )
                            .map_err(|detail| StoreError::Internal { detail })?;
                            terminal = record(
                                self,
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::ChangeOpened {
                                    change,
                                    head,
                                    source_branch: source_branch.clone(),
                                    target_branch: target_branch.clone(),
                                }],
                            )?;
                        }
                        // No change request matching the recorded
                        // revision and payload: a matching change may
                        // exist with a moved head, or only a foreign
                        // change sits at this revision — the result
                        // stays undecidable: `unknown` and outstanding,
                        // never a failed claim or an adoption of
                        // somebody else's change.
                    }
                }
                HostAction::PostComment {
                    change,
                    marker,
                    body,
                    ..
                } => {
                    if let Ok(found) =
                        self.client
                            .find_comment(&intent.repository, &change.change_request, marker)
                    {
                        match found {
                            Some(comment) if comment.body == *body => {
                                let body_digest = sha256_hex(
                                    "comment-v1",
                                    &canonical_json(&serde_json::json!({
                                        "body": comment.body,
                                    })),
                                );
                                terminal = record(
                                    self,
                                    HostActionStatus::Applied,
                                    vec![NormalizedEffectFact::CommentPosted {
                                        change: change.clone(),
                                        comment_id: comment.comment_id,
                                        actor: intent.actor.clone(),
                                        marker: marker.clone(),
                                        body_digest,
                                        target: None,
                                    }],
                                )?;
                            }
                            None => {
                                // A full listing that finds no marker is
                                // a definite no-effect read.
                                terminal = record(
                                    self,
                                    HostActionStatus::Failed {
                                        reason: "reconciled: no comment with the marker".into(),
                                    },
                                    vec![],
                                )?;
                            }
                            // A comment carrying our marker but a
                            // different stored body cannot be told
                            // from a foreign post: like a payload
                            // mismatch on open, the intent stays
                            // `unknown` and outstanding rather than
                            // adopting somebody else's comment.
                            Some(_) => {}
                        }
                    }
                }
                HostAction::UpdateChangeRequest {
                    change,
                    expected_head,
                    title,
                    body,
                } => {
                    if let Ok(pull) = self
                        .client
                        .read_pull(&intent.repository, &change.change_request)
                    {
                        let title_matches = title.as_ref().is_none_or(|t| &pull.title == t);
                        let body_matches = body.as_ref().is_none_or(|b| &pull.body == b);
                        if title_matches && body_matches {
                            let fields_digest = sha256_hex(
                                "change-meta-v1",
                                &canonical_json(&serde_json::json!({
                                    "title": pull.title,
                                    "body": pull.body,
                                })),
                            );
                            terminal = record(
                                self,
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::MetadataUpdated {
                                    change: change.clone(),
                                    head: expected_head.clone(),
                                    fields_digest,
                                }],
                            )?;
                        }
                        // Fields that do not match the requested payload
                        // are undecidable (our write landing versus a
                        // racing overwrite): the intent stays `unknown`
                        // and outstanding.
                    }
                }
                HostAction::PublishReviewFeedback { .. }
                | HostAction::ReplyToThread { .. }
                | HostAction::ResolveThread { .. }
                | HostAction::MarkReady { .. } => {}
            }
            if terminal {
                resolved.push(intent.action_id.clone());
            }
        }
        Ok(resolved)
    }

    /// The read-only half of effect reconciliation and the pre-write
    /// check: compare `intent`'s recorded effect against the remote's
    /// current state and report presence, definitive absence, or
    /// indeterminacy. Performs no mutation; every `Present` carries
    /// the normalized fact a fresh read derives.
    ///
    /// Absence is status-aware: a `pending` intent may simply never
    /// have been attempted, so "not observed" is not evidence of
    /// failure — for `open_change_request` a pull whose head moved
    /// past the recorded source revision cannot be told apart from
    /// never-created, and for `update_change_request` fields that do
    /// not match cannot be told apart from a racing overwrite. Both
    /// stay `Indeterminate` for an `unknown` intent rather than
    /// authorizing a blind second write; a `pending` intent proceeds
    /// to the guarded write, whose own preconditions arbitrate.
    fn observe_effect(
        &mut self,
        store: &impl Store,
        intent: &HostActionIntent,
    ) -> Result<RemoteEffect, StoreError> {
        let pending = intent.status == HostActionStatus::Pending;
        match &intent.action {
            HostAction::PushRef {
                branch, produced, ..
            } => {
                let head = match self.client.read_branch(&intent.repository, branch) {
                    Ok(head) => head,
                    Err(_) => {
                        return Ok(RemoteEffect::Indeterminate {
                            reason: "branch read failed".into(),
                        });
                    }
                };
                let Some(head) = head else {
                    return Ok(RemoteEffect::Absent {
                        reason: "reconciled: the branch is absent".into(),
                    });
                };
                if head == produced.object {
                    return Ok(RemoteEffect::Present {
                        fact: Box::new(NormalizedEffectFact::RefPublished {
                            repository: intent.repository.clone(),
                            branch: branch.clone(),
                            produced: produced.clone(),
                            observed: produced.clone(),
                            relation: RefRelation::Equal,
                        }),
                    });
                }
                match self
                    .client
                    .contains_revision(&intent.repository, &produced.object, &head)
                {
                    Ok(true) => {
                        let observed = CommitRevision::new(intent.repository.clone(), head)
                            .map_err(|detail| StoreError::Internal { detail })?;
                        Ok(RemoteEffect::Present {
                            fact: Box::new(NormalizedEffectFact::RefPublished {
                                repository: intent.repository.clone(),
                                branch: branch.clone(),
                                produced: produced.clone(),
                                observed,
                                relation: RefRelation::Ancestor,
                            }),
                        })
                    }
                    Ok(false) => Ok(RemoteEffect::Absent {
                        reason: "reconciled: the branch does not contain the produced revision"
                            .into(),
                    }),
                    // The ancestry read failed: the outcome cannot be
                    // established — `unknown` and outstanding, never a
                    // guess.
                    Err(_) => Ok(RemoteEffect::Indeterminate {
                        reason: "ancestry read failed".into(),
                    }),
                }
            }
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                title,
                body,
            } => {
                // Identify the effect by what this Work recorded when
                // it issued the open — the source revision of its own
                // applied push plus the intent's own payload. Neither
                // the branch nor the revision alone identifies the
                // effect: a foreign change can sit at the recorded
                // revision, and a pull's head follows its source
                // branch, so our own open can move past it. With no
                // recorded push the intent stays indeterminate —
                // branch identity alone never adopts somebody else's
                // change.
                let Some(source_revision) = recorded_source_revision(store, intent, source_branch)?
                else {
                    return Ok(RemoteEffect::Indeterminate {
                        reason: "no recorded push identifies the source revision".into(),
                    });
                };
                let pulls = match self.client.list_pulls_on_branch(
                    &intent.repository,
                    source_branch,
                    &source_revision.object,
                ) {
                    Ok(pulls) => pulls,
                    Err(_) => {
                        return Ok(RemoteEffect::Indeterminate {
                            reason: "change-request listing failed".into(),
                        });
                    }
                };
                match pulls.iter().find(|pull| {
                    pull.target_branch == *target_branch
                        && pull.title == *title
                        && pull.body == *body
                }) {
                    Some(pull) => {
                        let change = ChangeRequestRef {
                            repository: intent.repository.clone(),
                            change_request: ChangeRequestId::trusted(pull.id.clone()),
                            display_number: Some(pull.display_number),
                        };
                        let head =
                            CommitRevision::new(intent.repository.clone(), pull.head_sha.clone())
                                .map_err(|detail| StoreError::Internal { detail })?;
                        Ok(RemoteEffect::Present {
                            fact: Box::new(NormalizedEffectFact::ChangeOpened {
                                change,
                                head,
                                source_branch: source_branch.clone(),
                                target_branch: target_branch.clone(),
                            }),
                        })
                    }
                    None => Ok(if pending {
                        // No recorded attempt yet: the guarded create
                        // arbitrates — the host's own duplicate refusal
                        // covers the landed-then-moved-head residue.
                        RemoteEffect::Absent {
                            reason: "reconciled: no matching change request".into(),
                        }
                    } else {
                        // A matching change may exist with a moved
                        // head: `unknown` stays outstanding, never a
                        // failed claim or an adoption of somebody
                        // else's change.
                        RemoteEffect::Indeterminate {
                            reason: "no change request matches the recorded revision and payload"
                                .into(),
                        }
                    }),
                }
            }
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                title,
                body,
            } => {
                let pull = match self
                    .client
                    .read_pull(&intent.repository, &change.change_request)
                {
                    Ok(pull) => pull,
                    Err(_) => {
                        return Ok(RemoteEffect::Indeterminate {
                            reason: "change-request read failed".into(),
                        });
                    }
                };
                // The head guard is part of the effect's contract: a
                // moved head means the guarded write would abort, so
                // matching fields cannot confirm this intent's write —
                // a racing overwrite sets the same fields. A `pending`
                // intent proceeds to the guarded write, whose own
                // re-read records the honest `failed` verdict and the
                // fresh position; an `unknown` intent stays
                // outstanding rather than claim a success it cannot
                // prove.
                if pull.head_sha != expected_head.object {
                    return Ok(if pending {
                        RemoteEffect::Absent {
                            reason: "reconciled: the change head moved past the recorded guard"
                                .into(),
                        }
                    } else {
                        RemoteEffect::Indeterminate {
                            reason: "the change head moved; the guarded write's outcome cannot be confirmed"
                                .into(),
                        }
                    });
                }
                // The update's post-condition: every field the intent
                // carried is what the remote now stores. Omitted fields
                // are unconstrained — a PATCH never rewrites them.
                let applied = title.as_ref().is_none_or(|t| &pull.title == t)
                    && body.as_ref().is_none_or(|b| &pull.body == b);
                if !applied {
                    return Ok(if pending {
                        RemoteEffect::Absent {
                            reason: "reconciled: stored fields differ from the recorded payload"
                                .into(),
                        }
                    } else {
                        // Fields that do not match are undecidable (our
                        // write landing versus a racing overwrite): the
                        // intent stays `unknown` and outstanding.
                        RemoteEffect::Indeterminate {
                            reason: "stored fields differ from the recorded payload".into(),
                        }
                    });
                }
                let fields_digest = sha256_hex(
                    "change-meta-v1",
                    &canonical_json(&serde_json::json!({
                        "title": pull.title,
                        "body": pull.body,
                    })),
                );
                Ok(RemoteEffect::Present {
                    fact: Box::new(NormalizedEffectFact::MetadataUpdated {
                        change: change.clone(),
                        head: expected_head.clone(),
                        fields_digest,
                    }),
                })
            }
            HostAction::PostComment {
                change,
                marker,
                body,
                ..
            } => {
                match self
                    .client
                    .find_comment(&intent.repository, &change.change_request, marker)
                {
                    Err(_) => Ok(RemoteEffect::Indeterminate {
                        reason: "comment listing failed".into(),
                    }),
                    // The marker is the idempotence key; a stored body
                    // that differs from the recorded payload is the
                    // fixture's `Indeterminate` verdict — our write's
                    // landing cannot be told from a foreign comment
                    // carrying the marker.
                    Ok(Some(comment)) if comment.body != *body => Ok(RemoteEffect::Indeterminate {
                        reason: "the recorded marker carries a different stored body".into(),
                    }),
                    Ok(Some(comment)) => {
                        let body_digest = sha256_hex(
                            "comment-v1",
                            &canonical_json(&serde_json::json!({
                                "body": comment.body,
                            })),
                        );
                        Ok(RemoteEffect::Present {
                            fact: Box::new(NormalizedEffectFact::CommentPosted {
                                change: change.clone(),
                                comment_id: comment.comment_id,
                                actor: intent.actor.clone(),
                                marker: marker.clone(),
                                body_digest,
                                target: None,
                            }),
                        })
                    }
                    // A full listing that finds no marker is a
                    // definite no-effect read — for `unknown` it
                    // settles `failed`; for `pending` the guarded
                    // write proceeds.
                    Ok(None) => Ok(RemoteEffect::Absent {
                        reason: "reconciled: no comment with the marker".into(),
                    }),
                }
            }
            // Outside the declared capability set: never attempted,
            // so there is no remote effect to observe.
            HostAction::PublishReviewFeedback { .. }
            | HostAction::ReplyToThread { .. }
            | HostAction::ResolveThread { .. }
            | HostAction::MarkReady { .. } => Ok(RemoteEffect::Indeterminate {
                reason: "operation has no remote read-back".into(),
            }),
        }
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
        // host instances. It is not a real attempt, so it never
        // reaches the host.
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
        // The adapter-boundary credential gate — the declared
        // `credential_ref` for the intent's role resolves at use time
        // and authenticates as the intent's authorized actor before
        // any reconcile read or guarded write; a `credential.*`
        // failure blocks the intent pending repair rather than
        // writing under the ambient login.
        if let Some(error) = self.bind_intent_credential(store, &intent)? {
            return Ok(AttemptOutcome::CredentialBlocked(error.code()));
        }
        let execution_id = Some(execution.id);
        let obs = |fact: ObservationFact| Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: execution_id.clone(),
            fact,
            observed_unix_seconds: now,
        };
        // The write receipt is authenticated by the transport's proven
        // identity — `gh api user` — never the intent's configured
        // actor: a credential acting as another account records that
        // account so the durable actor-mismatch and drift-rearm checks
        // see the truth. A transport that cannot prove an identity
        // records an unauthenticated receipt, never a fabricated one.
        let authenticated_actor = self.client.viewer().ok().flatten().and_then(|login| {
            HostActorId::new(login).ok().map(|actor| HostActorRef {
                host: intent.host.clone(),
                actor,
            })
        });
        let result =
            |action_id: String, status: HostActionStatus, effects: Vec<NormalizedEffectFact>| {
                ObservationFact::HostActionResult {
                    action_id,
                    authenticated_actor: authenticated_actor.clone(),
                    status,
                    effects,
                }
            };

        // Support is validated distinctly from anything else: an
        // operation outside the declared set is an explicit
        // `unsupported` receipt, never a write and never a permission
        // verdict.
        if !Self::supports(&intent.action) {
            self.ingest(
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
        // write below. The read never authenticates an actor, so the
        // receipt carries `authenticated_actor: None` — the same
        // record `reconcile_outstanding` writes.
        match self.observe_effect(store, &intent)? {
            RemoteEffect::Present { fact } => {
                self.ingest(
                    store,
                    config,
                    obs(ObservationFact::HostActionResult {
                        action_id: intent.action_id.clone(),
                        authenticated_actor: None,
                        status: HostActionStatus::Applied,
                        effects: vec![*fact],
                    }),
                    now,
                )?;
                return Ok(AttemptOutcome::Attempted);
            }
            RemoteEffect::Indeterminate { reason } => {
                self.ingest(
                    store,
                    config,
                    obs(ObservationFact::HostActionResult {
                        action_id: intent.action_id.clone(),
                        authenticated_actor: None,
                        status: HostActionStatus::Unknown { reason },
                        effects: vec![],
                    }),
                    now,
                )?;
                return Ok(AttemptOutcome::Attempted);
            }
            RemoteEffect::Absent { .. } => {}
        }

        // The credential gate before any write: the transport's proven
        // identity must be the actor the intent binds. A `gh` session
        // acting as another account never pushes, opens, or comments —
        // the write is refused and the receipt records the proven
        // actor, so the durable `actor_mismatch` verdict is evidence of
        // a prevented effect, never a landed one. An unprovable
        // identity proceeds unauthenticated: the write's own outcome
        // settles the intent.
        if let Some(actor) = &authenticated_actor
            && *actor != intent.actor
        {
            self.ingest(
                store,
                config,
                obs(result(
                    intent.action_id,
                    HostActionStatus::Unknown {
                        reason: "actor_mismatch".into(),
                    },
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
                let expected = expected_prior.as_ref().map(|prior| prior.object.as_str());
                match self
                    .client
                    .write_ref(&intent.repository, branch, &produced.object, expected)
                {
                    Ok(_) => {
                        // Read-back: the receipt reports what a fresh
                        // read observes, never the attempted value.
                        let observed_object =
                            match self.client.read_branch(&intent.repository, branch) {
                                Ok(observed) => observed,
                                Err(_) => {
                                    // The write's response arrived but
                                    // the read-back did not: the
                                    // outcome cannot be established as
                                    // applied — `unknown`, reconciled
                                    // later by action ID.
                                    self.ingest(
                                        store,
                                        config,
                                        obs(result(
                                            intent.action_id,
                                            HostActionStatus::Unknown {
                                                reason: "read-back failed".into(),
                                            },
                                            vec![],
                                        )),
                                        now,
                                    )?;
                                    return Ok(AttemptOutcome::Attempted);
                                }
                            };
                        self.applied.push(format!(
                            "push_ref:{}:{}",
                            intent.repository, produced.object
                        ));
                        let (observed, relation) = match &observed_object {
                            Some(object) if object == &produced.object => {
                                (produced.clone(), RefRelation::Equal)
                            }
                            Some(object) => {
                                let relation = match self.client.contains_revision(
                                    &intent.repository,
                                    &produced.object,
                                    object,
                                ) {
                                    Ok(true) => RefRelation::Ancestor,
                                    Ok(false) => RefRelation::Diverged,
                                    // The ancestry read failed: the
                                    // outcome cannot be established —
                                    // `unknown`, reconciled by action ID.
                                    Err(_) => {
                                        self.ingest(
                                            store,
                                            config,
                                            obs(result(
                                                intent.action_id,
                                                HostActionStatus::Unknown {
                                                    reason: "ancestry read failed".into(),
                                                },
                                                vec![],
                                            )),
                                            now,
                                        )?;
                                        return Ok(AttemptOutcome::Attempted);
                                    }
                                };
                                (
                                    CommitRevision::new(intent.repository.clone(), object.clone())
                                        .map_err(|detail| StoreError::Internal { detail })?,
                                    relation,
                                )
                            }
                            // The ref vanished after our write: the
                            // outcome cannot be established as applied.
                            None => (produced.clone(), RefRelation::Diverged),
                        };
                        self.ingest(
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
                    Err(ApiError::Refused { reason }) if reason == "stale_head" => {
                        // The fresh position is recorded before the
                        // refusal receipt — a receipt-driven re-issue
                        // derives its guard from the observed remote.
                        if let Ok(actual) = self.client.read_branch(&intent.repository, branch) {
                            let state = match &actual {
                                Some(head) => BranchState::At {
                                    revision: CommitRevision::new(
                                        intent.repository.clone(),
                                        head.clone(),
                                    )
                                    .map_err(|detail| StoreError::Internal { detail })?,
                                },
                                None => BranchState::Absent,
                            };
                            self.ingest(
                                store,
                                config,
                                obs(ObservationFact::BranchState {
                                    repository: intent.repository.clone(),
                                    branch: branch.clone(),
                                    state,
                                }),
                                now,
                            )?;
                        }
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Refused { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Unknown { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Unknown { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                }
            }
            HostAction::OpenChangeRequest {
                source_branch,
                target_branch,
                title,
                body,
            } => match self.client.open_pull(
                &intent.repository,
                source_branch,
                target_branch,
                title,
                body,
            ) {
                Ok(pull) => {
                    let change = ChangeRequestRef {
                        repository: intent.repository.clone(),
                        change_request: ChangeRequestId::trusted(pull.id.clone()),
                        display_number: Some(pull.display_number),
                    };
                    let head = CommitRevision::new(intent.repository.clone(), pull.head_sha)
                        .map_err(|detail| StoreError::Internal { detail })?;
                    self.applied.push(format!(
                        "open_change:{}:{}",
                        intent.repository, change.change_request
                    ));
                    // The host's stored state for the opened change —
                    // a fresh read of what the write created.
                    self.ingest(
                        store,
                        config,
                        obs(ObservationFact::ChangeHead {
                            change: change.clone(),
                            head: head.clone(),
                        }),
                        now,
                    )?;
                    self.ingest(
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
                Err(ApiError::Refused { reason }) => {
                    self.ingest(
                        store,
                        config,
                        obs(result(
                            intent.action_id,
                            HostActionStatus::Failed { reason },
                            vec![],
                        )),
                        now,
                    )?;
                }
                Err(ApiError::Unknown { reason }) => {
                    self.ingest(
                        store,
                        config,
                        obs(result(
                            intent.action_id,
                            HostActionStatus::Unknown { reason },
                            vec![],
                        )),
                        now,
                    )?;
                }
            },
            HostAction::UpdateChangeRequest {
                change,
                expected_head,
                title,
                body,
            } => {
                // The head re-read immediately before writing: the
                // guard point is the API call, not a cached value.
                let fresh = match self
                    .client
                    .read_pull(&intent.repository, &change.change_request)
                {
                    Ok(pull) => pull,
                    Err(ApiError::Refused { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id.clone(),
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                        return Ok(AttemptOutcome::Attempted);
                    }
                    Err(ApiError::Unknown { .. }) => {
                        // The guard read itself is indeterminate: no
                        // write ran, so the intent stays `pending` for
                        // the next pass — never a fabricated result.
                        return Ok(AttemptOutcome::Attempted);
                    }
                };
                if fresh.head_sha != expected_head.object {
                    // Head moved under the recorded intent: abort
                    // before writing, record the failure, and surface
                    // the fresh head. v0.1 never re-issues.
                    self.ingest(
                        store,
                        config,
                        obs(result(
                            intent.action_id,
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    let head =
                        CommitRevision::new(intent.repository.clone(), fresh.head_sha.clone())
                            .map_err(|detail| StoreError::Internal { detail })?;
                    self.ingest(
                        store,
                        config,
                        obs(ObservationFact::ChangeHead {
                            change: change.clone(),
                            head,
                        }),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                match self.client.update_pull(
                    &intent.repository,
                    &change.change_request,
                    title.as_deref(),
                    body.as_deref(),
                ) {
                    Ok(updated) => {
                        self.applied.push(format!(
                            "update_change:{}:{}",
                            intent.repository, change.change_request
                        ));
                        // The digest covers the fields the host stores
                        // after the write — the read-back, never the
                        // attempted payload.
                        let fields_digest = sha256_hex(
                            "change-meta-v1",
                            &canonical_json(&serde_json::json!({
                                "title": updated.title,
                                "body": updated.body,
                            })),
                        );
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::MetadataUpdated {
                                    change: change.clone(),
                                    head: expected_head.clone(),
                                    fields_digest,
                                }],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Refused { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Unknown { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Unknown { reason },
                                vec![],
                            )),
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
                // Same immediate head re-read as the metadata write.
                let fresh = match self
                    .client
                    .read_pull(&intent.repository, &change.change_request)
                {
                    Ok(pull) => pull,
                    Err(ApiError::Refused { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id.clone(),
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                        return Ok(AttemptOutcome::Attempted);
                    }
                    Err(ApiError::Unknown { .. }) => return Ok(AttemptOutcome::Attempted),
                };
                if fresh.head_sha != expected_head.object {
                    // Abort before writing: failed receipt plus the
                    // fresh head, and no automatic re-issue.
                    self.ingest(
                        store,
                        config,
                        obs(result(
                            intent.action_id,
                            HostActionStatus::Failed {
                                reason: "expected head moved".into(),
                            },
                            vec![],
                        )),
                        now,
                    )?;
                    let head =
                        CommitRevision::new(intent.repository.clone(), fresh.head_sha.clone())
                            .map_err(|detail| StoreError::Internal { detail })?;
                    self.ingest(
                        store,
                        config,
                        obs(ObservationFact::ChangeHead {
                            change: change.clone(),
                            head,
                        }),
                        now,
                    )?;
                    return Ok(AttemptOutcome::Attempted);
                }
                match self.client.comment_on_pull(
                    &intent.repository,
                    &change.change_request,
                    marker,
                    body,
                ) {
                    Ok(comment) => {
                        self.applied.push(format!(
                            "post_comment:{}:{}",
                            intent.repository, change.change_request
                        ));
                        let body_digest = sha256_hex(
                            "comment-v1",
                            &canonical_json(&serde_json::json!({
                                "body": comment.body,
                            })),
                        );
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Applied,
                                vec![NormalizedEffectFact::CommentPosted {
                                    change: change.clone(),
                                    comment_id: comment.comment_id,
                                    actor: intent.actor.clone(),
                                    marker: marker.clone(),
                                    body_digest,
                                    target: None,
                                }],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Refused { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Failed { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                    Err(ApiError::Unknown { reason }) => {
                        self.ingest(
                            store,
                            config,
                            obs(result(
                                intent.action_id,
                                HostActionStatus::Unknown { reason },
                                vec![],
                            )),
                            now,
                        )?;
                    }
                }
            }
            // Gated by the declared-set check above; kept exhaustive
            // without a second, different claim about these
            // operations.
            HostAction::PublishReviewFeedback { .. }
            | HostAction::ReplyToThread { .. }
            | HostAction::ResolveThread { .. }
            | HostAction::MarkReady { .. } => {}
        }
        Ok(AttemptOutcome::Attempted)
    }
}

/// The occurrence set one target's review facts carry: every review
/// finding examined at `commit`, plus one synthetic occurrence for
/// each change-requesting review examined there. Every review fact for
/// the same head carries the same set — sorted, so the sealed round
/// and the fact's delivery identity never depend on transport order,
/// and a later clean review never masks an earlier reviewer's findings.
fn review_occurrences(
    reviews: &[RemoteReview],
    findings: &[RemoteReviewFinding],
    commit: &str,
) -> Vec<OccurrenceInput> {
    let mut occurrences: Vec<OccurrenceInput> = findings
        .iter()
        .filter(|finding| finding.commit_sha == commit)
        .map(|finding| OccurrenceInput {
            source_key: finding.finding_id.clone(),
            content_digest: Some(sha256_hex(
                "review-finding-v1",
                &canonical_json(&serde_json::json!({ "body": finding.body })),
            )),
            severity: None,
            position: None,
            thread_id: None,
            summary: Some(finding.body.clone()),
        })
        .chain(
            reviews
                .iter()
                .filter(|review| review.changes_requested && review.commit_sha == commit)
                .map(|review| OccurrenceInput {
                    source_key: format!("changes-requested:{}", review.review_id),
                    content_digest: Some(sha256_hex(
                        "review-change-request-v1",
                        &canonical_json(&serde_json::json!({ "body": review.body })),
                    )),
                    severity: None,
                    position: None,
                    thread_id: None,
                    summary: Some(review.body.clone()),
                }),
        )
        .collect();
    occurrences.sort_by(|a, b| {
        (a.source_key.as_str(), a.content_digest.as_deref())
            .cmp(&(b.source_key.as_str(), b.content_digest.as_deref()))
    });
    occurrences
}

/// The content identity of one review fact: completeness plus the
/// occurrence set, digested. Changed review state (a new finding, a
/// changed availability) records under a new delivery identity instead
/// of colliding with the recorded row, while an identical re-read
/// deduplicates — the same rule the check facts use for a changed
/// conclusion.
fn review_fact_token(completeness: &Completeness, occurrences: &[OccurrenceInput]) -> String {
    let completeness = match completeness {
        Completeness::Complete => "complete".to_string(),
        Completeness::Partial { reason } => format!("partial:{reason}"),
        Completeness::Unavailable { reason } => format!("unavailable:{reason}"),
    };
    let mut material: Vec<(String, String)> = occurrences
        .iter()
        .map(|o| {
            (
                o.source_key.clone(),
                o.content_digest.clone().unwrap_or_default(),
            )
        })
        .collect();
    material.sort();
    sha256_hex(
        "review-fact-v1",
        &canonical_json(&serde_json::json!({
            "completeness": completeness,
            "occurrences": material,
        })),
    )
}

/// What one read-only comparison of an intent's recorded effect
/// against the remote establishes — the tri-state every reconcile
/// verdict and every pre-write check reduces to. Unlike the
/// fixture's counterpart, the production read never authenticates
/// an actor: receipts built from it carry
/// `authenticated_actor: None`, exactly like
/// [`GitHubAdapter::reconcile_outstanding`].
enum RemoteEffect {
    /// The remote shows the effect the intent recorded, with the
    /// normalized fact a receipt reports.
    Present { fact: Box<NormalizedEffectFact> },
    /// The remote definitively shows the effect did not land;
    /// `reason` is the `failed` receipt text an `unknown` intent
    /// settles with. A `pending` intent treats absence as
    /// "possibly never attempted" and proceeds to the guarded
    /// write instead.
    Absent { reason: String },
    /// The read establishes neither presence nor absence — the
    /// intent stays outstanding rather than authorizing a blind
    /// second write.
    Indeterminate { reason: String },
}

/// The source revision an `open_change_request` was issued against:
/// the revision of the latest applied `push_ref` on the same branch for
/// the same Work and repository. Returns `None` when no applied push is
/// recorded; the caller then leaves the intent `unknown` — without a
/// recorded source revision, no read can tell this Work's open apart
/// from a change request somebody else opened on the same branch.
fn recorded_source_revision(
    store: &impl Store,
    intent: &HostActionIntent,
    source_branch: &str,
) -> Result<Option<CommitRevision>, StoreError> {
    store.transact(|tx| {
        Ok(tx
            .action_intents()?
            .into_iter()
            .filter(|candidate| {
                candidate.work_id == intent.work_id
                    && candidate.repository == intent.repository
                    && candidate.status == HostActionStatus::Applied
                    && matches!(
                        &candidate.action,
                        HostAction::PushRef { branch, .. } if branch == source_branch
                    )
            })
            .max_by_key(|candidate| candidate.created_seq)
            .and_then(|candidate| match candidate.action {
                HostAction::PushRef { produced, .. } => Some(produced),
                _ => None,
            }))
    })
}

/// The Work (and its latest Execution) a change request belongs to in
/// this installation: recorded intents first — the change ids they
/// carry or the `ChangeOpened` receipts they produced — then an
/// already-attributed `change_head` Observation. An unattributable
/// change is never read into this installation's facts.
fn attribute_change(
    store: &impl Store,
    change: &ChangeRequestRef,
) -> Result<Option<(WorkId, Option<ExecutionId>)>, StoreError> {
    store.transact(|tx| {
        for intent in tx.action_intents()? {
            let carried = matches!(
                &intent.action,
                HostAction::UpdateChangeRequest { change: c, .. }
                    | HostAction::PostComment { change: c, .. }
                    | HostAction::PublishReviewFeedback { change: c, .. }
                    | HostAction::ReplyToThread { change: c, .. }
                    | HostAction::ResolveThread { change: c, .. }
                    | HostAction::MarkReady { change: c, .. }
                    if c == change
            );
            let receipt = intent.effects.iter().any(|effect| match effect {
                NormalizedEffectFact::ChangeOpened { change: c, .. }
                | NormalizedEffectFact::MetadataUpdated { change: c, .. }
                | NormalizedEffectFact::CommentPosted { change: c, .. }
                | NormalizedEffectFact::ReviewPublished { change: c, .. }
                | NormalizedEffectFact::ReplyPosted { change: c, .. }
                | NormalizedEffectFact::ThreadResolved { change: c, .. }
                | NormalizedEffectFact::ReadyObserved { change: c, .. } => c == change,
                NormalizedEffectFact::RefPublished { .. } => false,
            });
            if carried || receipt {
                let execution = tx
                    .executions_for_work(&intent.work_id)?
                    .into_iter()
                    .max_by_key(|execution| execution.ordinal)
                    .map(|execution| execution.id);
                return Ok(Some((intent.work_id.clone(), execution)));
            }
        }
        for observation in tx.all_observations()? {
            if let ObservationFact::ChangeHead { change: c, .. } = &observation.fact
                && c == change
                && let Some(work_id) = observation.subject_work_id
            {
                let execution = observation.subject_execution_id.or_else(|| {
                    tx.executions_for_work(&work_id)
                        .ok()?
                        .into_iter()
                        .max_by_key(|execution| execution.ordinal)
                        .map(|execution| execution.id)
                });
                return Ok(Some((work_id, execution)));
            }
        }
        Ok(None)
    })
}

/// The permission bit a capability's `authorization` reads: the
/// write-class capabilities need `push`, the read-class need `pull` —
/// the repository permission model's coarse split.
fn capability_needs_write(capability: &HostCapability) -> bool {
    matches!(
        capability,
        HostCapability::PushRef
            | HostCapability::ChangeRequestCreate
            | HostCapability::ChangeRequestUpdate
            | HostCapability::CommentWrite
            | HostCapability::ChecksWrite
            | HostCapability::InlineComment
            | HostCapability::ReviewFeedback
            | HostCapability::Approve
            | HostCapability::RequestChanges
            | HostCapability::ThreadReply
            | HostCapability::ThreadResolve
            | HostCapability::MarkReady
    )
}

/// One entry's authorization state: `unknown` when the permission
/// read could not confirm the bits — unconfirmed is never granted.
fn capability_authorization(
    capability: HostCapability,
    permissions: Option<RepositoryPermissions>,
) -> CapabilityState {
    let Some(permissions) = permissions else {
        return CapabilityState::Unknown;
    };
    let granted = if capability_needs_write(&capability) {
        permissions.push
    } else {
        permissions.pull
    };
    if granted {
        CapabilityState::Authorized
    } else {
        CapabilityState::Denied
    }
}

/// The latest capability report recorded for `host`, on canonical
/// host-instance identity — the same recency rule Core's
/// `capability_state` applies.
fn latest_capability_entries(
    store: &impl Store,
    host: &HostInstanceId,
) -> Result<Option<Vec<CapabilityEntry>>, StoreError> {
    store.transact(|tx| {
        let mut latest: Option<(i64, usize, Vec<CapabilityEntry>)> = None;
        for (index, o) in tx.all_observations()?.iter().enumerate() {
            if let ObservationFact::CapabilityReport {
                host: report_host,
                capabilities,
            } = &o.fact
                && canonical_host_instance(report_host.as_str())
                    == canonical_host_instance(host.as_str())
            {
                let key = (o.observed_unix_seconds, index, capabilities.clone());
                latest = Some(match latest {
                    Some(old)
                        if old.0 > o.observed_unix_seconds
                            || (old.0 == o.observed_unix_seconds && old.1 > index) =>
                    {
                        old
                    }
                    _ => key,
                });
            }
        }
        Ok(latest.map(|(_, _, entries)| entries))
    })
}

/// Order-insensitive report comparison: identical when every
/// capability carries the same support and authorization, whatever
/// order the producer listed them in.
fn same_capability_set(a: &[CapabilityEntry], b: &[CapabilityEntry]) -> bool {
    a.len() == b.len()
        && a.iter().all(|entry| {
            b.iter().any(|other| {
                other.capability == entry.capability
                    && other.support == entry.support
                    && other.authorization == entry.authorization
            })
        })
}

fn work_bound_to_config(
    store: &impl Store,
    config: &WorkloopConfig,
    bound_host: Option<&HostInstanceId>,
    work_id: &WorkId,
) -> Result<bool, StoreError> {
    store.transact(|tx| {
        Ok(tx.work(work_id)?.is_some_and(|work| {
            work.binding.adapter == config.host.adapter
                && work.binding.repository == config.host.repository
                && work.binding.contract_version == config.host.contract_version
                && bound_host.is_none_or(|bound| *bound == work.binding.repository.host)
        }))
    })
}

fn poll_fact_is_new(
    store: &impl Store,
    work_id: &WorkId,
    execution_id: &Option<ExecutionId>,
    fact: &ObservationFact,
) -> Result<bool, StoreError> {
    let observations = store.transact(|tx| {
        Ok(tx
            .observations_for_work(work_id)?
            .into_iter()
            .filter(|o| &o.subject_execution_id == execution_id)
            .collect::<Vec<_>>())
    })?;
    let bytes = serde_json::to_vec(fact).unwrap_or_default();
    let same = |o: &Observation| serde_json::to_vec(&o.fact).unwrap_or_default() == bytes;
    match fact {
        ObservationFact::ChangeHead { change, .. } => {
            let latest = observations.iter().rev().find(
                |o| matches!(&o.fact, ObservationFact::ChangeHead { change: c, .. } if c == change),
            );
            Ok(!latest.is_some_and(same))
        }
        ObservationFact::Check {
            change,
            head,
            check_id,
            producer,
            ..
        } => {
            let latest = observations.iter().rev().find(|o| {
                matches!(&o.fact,
                    ObservationFact::Check { change: c, head: h, check_id: i, producer: pr, .. }
                    if c == change && h == head && i == check_id && pr == producer)
            });
            Ok(!latest.is_some_and(same))
        }
        ObservationFact::BranchState {
            repository, branch, ..
        } => {
            let latest = observations.iter().rev().find(|o| {
                matches!(&o.fact,
                    ObservationFact::BranchState { repository: r, branch: b, .. }
                    if r == repository && b == branch)
            });
            Ok(!latest.is_some_and(same))
        }
        ObservationFact::HostRead { subject, .. } => {
            let latest = observations.iter().rev().find(
                |o| matches!(&o.fact, ObservationFact::HostRead { subject: s, .. } if s == subject),
            );
            Ok(!latest.is_some_and(same))
        }
        ObservationFact::Review {
            target,
            source,
            source_revision,
            completeness,
            ..
        } => {
            if !completeness.is_complete() {
                let latest_any = observations.iter().rev().find(
                    |o| matches!(&o.fact, ObservationFact::Review { target: t, .. } if t == target),
                );
                return Ok(!latest_any.is_some_and(same));
            }
            let idx_same = observations.iter().rposition(|o| {
                matches!(&o.fact,
                    ObservationFact::Review {
                        target: t, source: s, source_revision: sr, ..
                    } if t == target && s == source && sr == source_revision)
            });
            let Some(idx) = idx_same else { return Ok(true) };
            if !same(&observations[idx]) {
                return Ok(true);
            }
            Ok(observations[idx + 1..].iter().any(|o| {
                matches!(&o.fact,
                    ObservationFact::Review {
                        target: t,
                        completeness: Completeness::Unavailable { .. } | Completeness::Partial { .. },
                        ..
                    } if t == target)
            }))
        }
        _ => Ok(true),
    }
}

fn recorded_review_target(
    store: &impl Store,
    work_id: &WorkId,
    change: &ChangeRequestRef,
    head: &CommitRevision,
    source: &str,
    source_revision: &str,
) -> Result<Option<CommitRevision>, StoreError> {
    store.transact(|tx| {
        Ok(tx
            .observations_for_work(work_id)?
            .iter()
            .find_map(|o| match &o.fact {
                ObservationFact::Review {
                    target,
                    source: s,
                    source_revision: sr,
                    ..
                } if &target.change == change
                    && &target.head == head
                    && s == source
                    && sr.as_deref() == Some(source_revision) =>
                {
                    target.target_revision.clone()
                }
                _ => None,
            }))
    })
}

fn latest_recorded_target_revision(
    store: &impl Store,
    work_id: &WorkId,
    repository: &RepositoryRef,
    branch: &str,
) -> Result<Option<CommitRevision>, StoreError> {
    store.transact(|tx| {
        Ok(tx
            .observations_for_work(work_id)?
            .iter()
            .enumerate()
            .filter_map(|(index, o)| match &o.fact {
                ObservationFact::BranchState {
                    repository: r,
                    branch: b,
                    state,
                } if r == repository && b == branch => {
                    Some((o.observed_unix_seconds, index, state))
                }
                _ => None,
            })
            .max_by_key(|(t, i, _)| (*t, *i))
            .and_then(|(_, _, state)| match state {
                BranchState::At { revision } => Some(revision.clone()),
                BranchState::Absent => None,
            }))
    })
}

fn change_pair_key(
    work_id: &WorkId,
    change: &ChangeRequestRef,
) -> (String, RepositoryRef, ChangeRequestId) {
    (
        work_id.0.as_str().to_string(),
        change.repository.clone(),
        change.change_request.clone(),
    )
}

fn attributed_change_pairs(
    store: &impl Store,
    scope: Option<&WorkId>,
) -> Result<Vec<(WorkId, ChangeRequestRef)>, StoreError> {
    let mut pairs: std::collections::BTreeMap<
        (String, RepositoryRef, ChangeRequestId),
        (WorkId, ChangeRequestRef),
    > = std::collections::BTreeMap::new();
    let mut note = |work_id: &WorkId, change: &ChangeRequestRef| {
        pairs.insert(
            change_pair_key(work_id, change),
            (work_id.clone(), change.clone()),
        );
    };
    for intent in store.transact(|tx| tx.action_intents())? {
        if let Some(work_id) = scope
            && intent.work_id != *work_id
        {
            continue;
        }
        match &intent.action {
            HostAction::UpdateChangeRequest { change, .. }
            | HostAction::PostComment { change, .. }
            | HostAction::PublishReviewFeedback { change, .. }
            | HostAction::ReplyToThread { change, .. }
            | HostAction::ResolveThread { change, .. }
            | HostAction::MarkReady { change, .. } => note(&intent.work_id, change),
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
                | NormalizedEffectFact::ReadyObserved { change, .. } => {
                    note(&intent.work_id, change)
                }
                _ => {}
            }
        }
    }
    for observation in store.transact(|tx| tx.all_observations())? {
        if let ObservationFact::ChangeHead { change, .. } = &observation.fact
            && let Some(work_id) = &observation.subject_work_id
            && scope.is_none_or(|scoped| scoped == work_id)
        {
            note(work_id, change);
        }
    }
    Ok(pairs.into_values().collect())
}

/// The real adapter on the shared integration port — the same two
/// calls the fixture implements, so the contract tests exercise one
/// port against both.
impl<C: GitHubApi> crate::port::IntegrationPort for GitHubAdapter<C> {
    fn ingest(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError> {
        GitHubAdapter::ingest(self, store, config, observation, now_unix_seconds)
    }

    fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        GitHubAdapter::apply_outbox(self, store, config, now_unix_seconds)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use workloop_core::identity::{HostInstanceId, RepositoryId};

    #[test]
    fn pair_keys_never_collapse_display_equal_tuples() {
        let work = WorkId("w-1".into());
        let a = change_pair_key(
            &work,
            &ChangeRequestRef {
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("github.com"),
                    RepositoryId::trusted("owner/name"),
                ),
                change_request: ChangeRequestId::trusted("7"),
                display_number: None,
            },
        );
        let b = change_pair_key(
            &work,
            &ChangeRequestRef {
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("github.com/owner"),
                    RepositoryId::trusted("name"),
                ),
                change_request: ChangeRequestId::trusted("7"),
                display_number: None,
            },
        );
        assert_eq!(
            format!("{}", a.1),
            format!("{}", b.1),
            "the crafted repos share a Display string"
        );
        assert_ne!(a, b, "the typed key keeps them distinct");
    }
}
