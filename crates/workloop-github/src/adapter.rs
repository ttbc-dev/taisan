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

use workloop_core::domain::{ExecutionId, WorkId, canonical_json, sha256_hex};
use workloop_core::identity::{
    ChangeRequestId, ChangeRequestRef, CommitRevision, DiffTarget, HostActorId, HostActorRef,
    HostInstanceId,
};
use workloop_core::observation::{
    BranchState, CheckConclusion, Completeness, DeliveryId, DeliveryScope, HostActionStatus,
    NormalizedEffectFact, Observation, ObservationFact, OccurrenceInput, RefRelation,
};
use workloop_core::store::{HostAction, HostActionIntent, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

use crate::api::{ApiError, GitHubApi, RemoteReview, RemoteReviewFinding};

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
#[derive(Debug, Default)]
pub struct GitHubAdapter<C> {
    client: C,
    /// When set, only intents recorded for this host instance are
    /// drained; another instance's intents stay `pending` for their
    /// own adapter — an effect never migrates across hosts.
    bound_host: Option<HostInstanceId>,
    applied: Vec<String>,
    /// Distinguishes successive local head reads. A read is evidence
    /// of what the host reports *now*, so re-observing a head after a
    /// force push must not deduplicate against an earlier read of the
    /// same revision.
    next_read: u64,
}

impl<C: GitHubApi> GitHubAdapter<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            bound_host: None,
            applied: Vec::new(),
            next_read: 0,
        }
    }

    /// Bound to one host instance — the adapter drains only intents
    /// recorded for `host`.
    pub fn for_host(host: HostInstanceId, client: C) -> Self {
        Self {
            client,
            bound_host: Some(host),
            applied: Vec::new(),
            next_read: 0,
        }
    }

    fn next_read_event_id(&mut self, change: &ChangeRequestRef, head: &str, at: i64) -> String {
        self.next_read = self.next_read.wrapping_add(1);
        format!("read:{change}@{}@{at}#{}", head, self.next_read)
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
        // The repository read gates every subject read: an unreachable
        // repository invents no facts and the next pass catches up.
        if self.client.read_repository(&change.repository).is_err() {
            return Ok(());
        }
        let pull = match self
            .client
            .read_pull(&change.repository, &change.change_request)
        {
            Ok(pull) => pull,
            // Outage or unknown change: no facts invented, nothing
            // recorded — the next successful read catches up.
            Err(_) => return Ok(()),
        };
        let head = CommitRevision::new(change.repository.clone(), pull.head_sha)
            .map_err(|detail| StoreError::Internal { detail })?;
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
        let read_id = self.next_read_event_id(change, &head.object, now_unix_seconds);
        self.ingest(
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
            Err(_) => return Ok(()),
        };
        for run in runs {
            let run_head = CommitRevision::new(change.repository.clone(), run.head_sha.clone())
                .map_err(|detail| StoreError::Internal { detail })?;
            self.ingest(
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
                    format!("check:{change}:{}@{:?}", run.check_run_id, run.conclusion),
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
        self.ingest_review_state(
            store,
            config,
            change,
            &work_id,
            &execution_id,
            now_unix_seconds,
            Some(&head),
        )
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
    ) -> Result<(), StoreError> {
        let Ok(reviews) = self
            .client
            .reviews(&change.repository, &change.change_request)
        else {
            // Outage or unknown change: no facts invented — the next
            // successful read catches up.
            return Ok(());
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
                        return Ok(());
                    };
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
            self.ingest(
                store,
                config,
                Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Host {
                            host: change.repository.host.clone(),
                        },
                        source: "github.poll".into(),
                        event_id: format!("review-empty:{change}@{}#{token}", head.object),
                    }),
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: execution_id.clone(),
                    fact: ObservationFact::Review {
                        target: DiffTarget {
                            change: change.clone(),
                            head,
                            base: None,
                            target_revision: None,
                        },
                        // No reviewer: the stream itself is the
                        // source of this fact.
                        source: "github.reviews".into(),
                        source_revision: None,
                        completeness,
                        occurrences,
                    },
                    observed_unix_seconds: now_unix_seconds,
                },
                now_unix_seconds,
            )?;
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
            self.ingest(
                store,
                config,
                Observation {
                    delivery: Some(DeliveryId {
                        scope: DeliveryScope::Host {
                            host: change.repository.host.clone(),
                        },
                        source: "github.poll".into(),
                        event_id: format!("review:{change}@{}#{token}", review.review_id),
                    }),
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: execution_id.clone(),
                    fact: ObservationFact::Review {
                        target: DiffTarget {
                            change: change.clone(),
                            head: review_head,
                            base: None,
                            target_revision: None,
                        },
                        source: review.author.clone(),
                        source_revision: Some(review.review_id.clone()),
                        completeness,
                        occurrences,
                    },
                    observed_unix_seconds: now_unix_seconds,
                },
                now_unix_seconds,
            )?;
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
                HostAction::PostComment { change, marker, .. } => {
                    if let Ok(found) =
                        self.client
                            .find_comment(&intent.repository, &change.change_request, marker)
                    {
                        match found {
                            Some(comment) => {
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

    fn attempt(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        intent: HostActionIntent,
        now: i64,
    ) -> Result<(), StoreError> {
        // Not this instance's intent: leave it pending for the adapter
        // bound to the recorded host — an effect never migrates across
        // host instances. It is not a real attempt, so it never
        // reaches the host.
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
            return Ok(());
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
                                    return Ok(());
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
                                        return Ok(());
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
                        return Ok(());
                    }
                    Err(ApiError::Unknown { .. }) => {
                        // The guard read itself is indeterminate: no
                        // write ran, so the intent stays `pending` for
                        // the next pass — never a fabricated result.
                        return Ok(());
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
                    return Ok(());
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
                        return Ok(());
                    }
                    Err(ApiError::Unknown { .. }) => return Ok(()),
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
                    return Ok(());
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
        Ok(())
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
