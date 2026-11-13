//! Real adapter contract (`docs/module-contracts.md` GitHub rows):
//! the same port the slice-0 fixture implements — `ingest(Observation)`
//! in, recorded `HostActionIntent`s out — plus the adapter-specific
//! rules: webhook dedup on `source_event_id`, a fresh head read behind
//! every delivery, exact-head check/review facts, head-guarded writes
//! with no automatic re-issue, unknown results reconciled by action ID
//! before any terminal settlement, and merge outside the adapter.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{QueryResult, outstanding_actions};
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, admit_queued, handle_command,
};
use workloop_github::adapter::{GitHubAdapter, WebhookDelivery, WebhookFact};
use workloop_github::api::InMemoryGitHub;
use workloop_github::fixture::FixtureGithub;
use workloop_github::port::IntegrationPort;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

type Adapter = GitHubAdapter<InMemoryGitHub>;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("github"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: Adapter::declared_capabilities()
            .iter()
            .map(|name| HostCapability::from_name(name).expect("contract capability name"))
            .collect(),
        lanes: LanePolicy::default(),
        required_checks: vec!["ci".into()],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn artifact(kind: &str) -> RequiredArtifact {
    RequiredArtifact {
        kind: kind.into(),
        path: kind.into(),
        expected_digest: None,
    }
}

fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "do the thing".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        work_id,
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = d
    else {
        panic!("submit: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) -> String {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "test".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(action_id)
        })
        .unwrap()
}

fn intent_status(store: &SqliteStore, action_id: &str) -> HostActionStatus {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id)
                .map(|i| i.status))
        })
        .unwrap()
        .unwrap()
}

fn observations(store: &SqliteStore, work_id: &WorkId) -> Vec<Observation> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
}

/// The Work's recorded current head — the same latest-by-(observed
/// time, insertion order) rule every head comparison uses.
fn current_head(store: &SqliteStore, work_id: &WorkId) -> Option<CommitRevision> {
    match store
        .transact(|tx| workloop_core::projection::work_detail(tx, work_id, NOW))
        .unwrap()
    {
        QueryResult::Found { value, .. } => value.current_change.and_then(|(_, head)| head),
        other => panic!("work_detail: {other:?}"),
    }
}

fn obs(
    work_id: &WorkId,
    eid: &ExecutionId,
    event: Option<&str>,
    at: i64,
    fact: ObservationFact,
) -> Observation {
    Observation {
        delivery: event.map(|event_id| DeliveryId {
            scope: DeliveryScope::Local,
            source: "test".into(),
            event_id: event_id.into(),
        }),
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact,
        observed_unix_seconds: at,
    }
}

/// Push and open through the adapter's outbox, returning the Work, the
/// change the host issued, and its opened head.
fn push_and_open(
    adapter: &mut Adapter,
    store: &SqliteStore,
    config: &WorkloopConfig,
    key: &str,
) -> (WorkId, ChangeRequestRef, CommitRevision) {
    let (work_id, _eid) = submit(store, key);
    let branch = format!("work/{key}");
    insert_intent(
        store,
        &work_id,
        HostAction::PushRef {
            branch: branch.clone(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    let open_id = insert_intent(
        store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch,
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    adapter.apply_outbox(store, config, NOW).unwrap();
    assert_eq!(intent_status(store, &open_id), HostActionStatus::Applied);
    match &intent_status_effects(store, &open_id)[..] {
        [NormalizedEffectFact::ChangeOpened { change, head, .. }] => {
            (work_id, change.clone(), head.clone())
        }
        other => panic!("open receipt missing ChangeOpened: {other:?}"),
    }
}

fn intent_status_effects(store: &SqliteStore, action_id: &str) -> Vec<NormalizedEffectFact> {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id)
                .map(|i| i.effects))
        })
        .unwrap()
        .unwrap()
}

/// The shared setup for completion tests: an admitted Execution parked
/// in `Waiting/Ci` with applied push_ref and open_change_request
/// intents, so completion turns solely on current-target evidence.
fn waiting_with_applied_publish(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let (work_id, eid) = submit(store, key);
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            let request = tx
                .pending_requests()?
                .into_iter()
                .find(|request| request.execution_id == eid)
                .expect("admitted coding request");
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("{}-coding", eid.0)),
                    request_id: request.request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: None,
                    subject_execution_id: Some(eid.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: InvocationId(format!("{}-coding", eid.0)),
                        required_artifact: "artifact.json".into(),
                        state: ArtifactFact::Present {
                            digest: "artifact-digest".into(),
                        },
                    },
                    observed_unix_seconds: NOW,
                },
            })?;
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.state = ExecutionState::Waiting;
            execution.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Ready,
            })?;
            for (action_id, action) in [
                (
                    "act-pub-push",
                    HostAction::PushRef {
                        branch: format!("work/{}", work_id.0),
                        produced: rev("artifact-digest"),
                        expected_prior: None,
                    },
                ),
                (
                    "act-pub-open",
                    HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "t".into(),
                        body: "b".into(),
                    },
                ),
            ] {
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id: action_id.into(),
                        work_id: work_id.clone(),
                        host: host(),
                        repository: repo(),
                        role: ActorRole::Publication,
                        actor: publisher(),
                        action,
                        payload_digest: "test".into(),
                        status: HostActionStatus::Applied,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
            }
            Ok(())
        })
        .unwrap();
    (work_id, eid)
}

/// One generic port test: the fixture and the real adapter answer the
/// same two calls — inbound `ingest`, outbound `apply_outbox` — with
/// the same receipts, so the contract tests exercise one port.
fn drives_the_port<P: IntegrationPort>(port: &mut P, key: &str) {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, key);
    // Inbound half: one Observation through the shared pipeline.
    let receipt = port
        .ingest(
            &store,
            &config(),
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::BranchState {
                    repository: repo(),
                    branch: "port".into(),
                    state: BranchState::Absent,
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);
    // Outbound half: a recorded intent drains into a result receipt.
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/port".into(),
            produced: rev("head-port"),
            expected_prior: None,
        },
    );
    port.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Applied,
        "both adapters drain a recorded intent to an applied receipt"
    );
}

#[test]
fn the_fixture_and_the_real_adapter_run_the_same_integration_port() {
    drives_the_port(&mut FixtureGithub::new(), "port-fixture");
    drives_the_port(&mut Adapter::new(InMemoryGitHub::new()), "port-adapter");
}

#[test]
fn webhook_deliveries_deduplicate_on_source_event_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "wh-dedup");
    let delivery = WebhookDelivery {
        event_id: "delivery-guid-1".into(),
        emitted_at: NOW + 10,
        fact: WebhookFact::ChangeHead {
            change: change.clone(),
            head: rev("head-1"),
        },
    };
    let first = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 10)
        .unwrap();
    assert_eq!(first, IngestReceipt::Recorded);
    let recorded = observations(&store, &work_id).len();

    // The same delivery GUID redelivered: one recorded effect under
    // the same source-event identity, never a second one.
    let second = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 20)
        .unwrap();
    assert_eq!(second, IngestReceipt::Duplicate);
    assert_eq!(
        observations(&store, &work_id).len(),
        recorded,
        "a redelivered webhook records no new observation"
    );
}

#[test]
fn delayed_webhook_for_an_old_head_fresh_reads_the_current_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "wh-late");
    // The remote moved on: the current head is sha-2 while this
    // installation has only ever recorded the opened head.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "sha-2");
    assert_eq!(current_head(&store, &work_id), Some(opened_head));

    // A delayed delivery for the old head arrives last — stamped
    // *after* the head was first recorded, so without the fresh read
    // its own head observation would outrank the current one.
    let receipt = adapter
        .deliver_webhook(
            &store,
            &config,
            &WebhookDelivery {
                event_id: "delivery-old-head".into(),
                emitted_at: NOW + 30,
                fact: WebhookFact::Check {
                    change: change.clone(),
                    head: rev("sha-1"),
                    check_id: "ci".into(),
                    producer: publisher(),
                    conclusion: CheckConclusion::Success,
                },
            },
            NOW + 60,
        )
        .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);

    // The webhook remains evidence for its own revision…
    assert!(
        observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Check { head, .. } if head.object == "sha-1"
        )),
        "the delayed check stays recorded against the head it examined"
    );
    // …while a fresh read of the current head is what the store
    // records as current: never a backward move.
    assert_eq!(
        current_head(&store, &work_id),
        Some(rev("sha-2")),
        "the delayed delivery never moves the recorded head backward"
    );
}

#[test]
fn check_and_review_facts_carry_the_examined_head_and_inherit_nothing() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "exact-head");
    assert_eq!(opened_head.object, "head-1");
    adapter.client_mut().seed_check(
        &repo(),
        "head-1",
        "ci",
        "run-1",
        CheckConclusion::Success,
        "ci-app",
    );
    adapter.client_mut().seed_review(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "reviewer",
    );

    // Phase A: the head the host reports is examined — both facts are
    // bound to that exact revision.
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    let phase_a = observations(&store, &work_id);
    assert!(phase_a.iter().any(|o| matches!(
        &o.fact,
        ObservationFact::Check { head, check_id, .. }
            if head.object == "head-1" && check_id == "ci"
    )));
    assert!(phase_a.iter().any(|o| matches!(
        &o.fact,
        ObservationFact::Review { target, .. } if target.head.object == "head-1"
    )));

    // Phase B: a newer head arrives. The prior head's conclusions
    // stay in evidence; the new head inherits no check and no review.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "head-2");
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    let phase_b = observations(&store, &work_id);
    assert!(
        !phase_b.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Check { head, .. } if head.object == "head-2"
        )),
        "a check for the superseded head never becomes a check for the new one"
    );
    assert!(
        !phase_b.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review { target, .. } if target.head.object == "head-2"
        )),
        "the prior review never marks the new head reviewed"
    );
    assert!(
        phase_b.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Check { head, .. } if head.object == "head-1"
        )),
        "the prior head's conclusion stays in evidence"
    );
    assert_eq!(current_head(&store, &work_id), Some(rev("head-2")));
}

#[test]
fn head_mismatch_aborts_both_head_bound_writes_and_never_reissues() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "guard");
    // The remote head moved since the intents were recorded.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "moved-head");
    let writes_before = adapter.client().writes().len();
    let applied_before = adapter.applied().len();

    let update_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change.clone(),
            expected_head: opened_head.clone(),
            title: Some("edited".into()),
            body: Some("edited body".into()),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent_status(&store, &update_id),
        HostActionStatus::Failed {
            reason: "expected head moved".into()
        }
    );

    let comment_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change.clone(),
            expected_head: opened_head.clone(),
            marker: "workloop".into(),
            body: "hello".into(),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent_status(&store, &comment_id),
        HostActionStatus::Failed {
            reason: "expected head moved".into()
        }
    );

    // No write landed at the stale target, and both aborts surfaced a
    // fresh `pr_head` Observation of what the host actually holds.
    assert_eq!(adapter.client().writes().len(), writes_before);
    assert_eq!(adapter.applied().len(), applied_before);
    assert!(
        observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::ChangeHead { change: c, head }
                if c == &change && head.object == "moved-head"
        )),
        "the abort records a fresh head observation"
    );

    // v0.1 never re-issues automatically: later drains leave both
    // terminal receipts alone and reach for no write.
    adapter.apply_outbox(&store, &config, NOW + 10).unwrap();
    adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert_eq!(adapter.client().writes().len(), writes_before);
    assert_eq!(
        intent_status(&store, &update_id),
        HostActionStatus::Failed {
            reason: "expected head moved".into()
        }
    );
    assert_eq!(
        intent_status(&store, &comment_id),
        HostActionStatus::Failed {
            reason: "expected head moved".into()
        }
    );
}

#[test]
fn unknown_result_stays_outstanding_until_reconciled_by_action_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, _eid) = submit(&store, "unknown-open");
    insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/unknown".into(),
            produced: rev("head-u"),
            expected_prior: None,
        },
    );
    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/unknown".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    // The create's response is lost after the request landed.
    adapter.client_mut().lose_response("open_pull");
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent_status(&store, &open_id),
        HostActionStatus::Unknown {
            reason: "response lost".into()
        }
    );
    assert_eq!(
        store
            .transact(|tx| outstanding_actions(tx, &work_id))
            .unwrap(),
        vec![open_id.clone()],
        "the unresolved effect is outstanding"
    );

    // A further drain never replays an `unknown` intent.
    let writes_before = adapter.client().writes().len();
    adapter.apply_outbox(&store, &config, NOW + 5).unwrap();
    assert_eq!(adapter.client().writes().len(), writes_before);
    assert!(matches!(
        intent_status(&store, &open_id),
        HostActionStatus::Unknown { .. }
    ));

    // Reconciliation by action ID resolves it against the host's
    // current state — the same identity, a fresh terminal result.
    let resolved = adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert_eq!(resolved, vec![open_id.clone()]);
    assert_eq!(intent_status(&store, &open_id), HostActionStatus::Applied);
    assert!(
        store
            .transact(|tx| outstanding_actions(tx, &work_id))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn terminal_cancellation_defers_until_the_unknown_effect_reconciles() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = submit(&store, "cancel-unknown");
    store
        .transact(|tx| admit_queued(tx, &config, &eid, NOW).map(|_| ()))
        .unwrap();
    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/cancel".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    store
        .transact(|tx| {
            let request = tx
                .pending_requests()?
                .into_iter()
                .find(|request| request.execution_id == eid)
                .expect("admitted coding request");
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-cancel".into()),
                    request_id: request.request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.state = ExecutionState::Waiting;
            execution.waiting_reason = Some(WaitingReason::HostWrite);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    // The publication branch exists (pushed earlier in the real
    // flow); the create's response is lost after it lands: the
    // effect is outstanding.
    adapter
        .client_mut()
        .seed_branch(&repo(), "work/cancel", "head-1");
    adapter.client_mut().lose_response("open_pull");
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert!(matches!(
        intent_status(&store, &open_id),
        HostActionStatus::Unknown { .. }
    ));

    // Cancel while the effect is unresolved.
    let version = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .unwrap()
        .version;
    let d = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "cancel-unknown-1".into(),
            body: CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(
        matches!(d, CommandDecision::Accepted { .. }),
        "cancel: {d:?}"
    );
    // Writer stop is proven; the exit fact reaches settle, which must
    // defer — the outstanding effect is not reconciled yet.
    adapter
        .ingest(
            &store,
            &config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: Some(eid.clone()),
                fact: ObservationFact::ProcessState {
                    invocation_id: InvocationId("inv-cancel".into()),
                    process_identity: String::new(),
                    state: ProcessStateFact::Exited,
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Stopping,
        "terminal cancellation defers while the effect is outstanding"
    );
    assert!(matches!(
        intent_status(&store, &open_id),
        HostActionStatus::Unknown { .. }
    ));

    // Only reconciliation by action ID lets the cancellation land.
    adapter
        .reconcile_outstanding(&store, &config, NOW + 5)
        .unwrap();
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Cancelled,
        "the reconciled result re-drives the deferred terminal settle"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!((slots, reservations), (0, 0), "terminal release");
}

#[test]
fn merge_is_outside_the_adapter_and_never_part_of_succeeded() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = waiting_with_applied_publish(&store, "no-merge");
    let emit = |adapter: &Adapter, fact: ObservationFact| {
        adapter
            .ingest(&store, &config, obs(&work_id, &eid, None, NOW, fact), NOW)
            .unwrap();
    };
    let opened = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    emit(
        &adapter,
        ObservationFact::HostActionResult {
            action_id: "act-pub-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: opened.clone(),
                head: rev("sha-1"),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
    );
    emit(
        &adapter,
        ObservationFact::ChangeHead {
            change: opened.clone(),
            head: rev("sha-1"),
        },
    );
    emit(
        &adapter,
        ObservationFact::Check {
            change: opened.clone(),
            head: rev("sha-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    emit(
        &adapter,
        ObservationFact::Review {
            target: DiffTarget {
                change: opened.clone(),
                head: rev("sha-1"),
                base: None,
                target_revision: None,
            },
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        },
    );
    emit(
        &adapter,
        ObservationFact::Assessment {
            target: DiffTarget {
                change: opened.clone(),
                head: rev("sha-1"),
                base: None,
                target_revision: None,
            },
            set_digest: "invariants".into(),
            complete: true,
            items: vec![AssessmentItem {
                id: "inv-1".into(),
                verdict: AssessmentVerdict::Positive,
                refs: vec![],
            }],
        },
    );

    // `succeeded` lands on evidence alone — no merge was performed,
    // requested, or required.
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(execution.state, ExecutionState::Succeeded);
    assert!(
        adapter
            .applied()
            .iter()
            .all(|entry| !entry.contains("merge")),
        "the adapter never issued a merge write"
    );
    assert!(
        adapter
            .client()
            .writes()
            .iter()
            .all(|entry| !entry.contains("merge")),
        "the host never received a merge"
    );
    assert!(
        serde_json::from_value::<HostAction>(serde_json::json!({ "action": "merge" })).is_err(),
        "merge is not in the executable action vocabulary"
    );
}

#[test]
fn declared_capabilities_are_backed_by_the_transport_surface() {
    // Every declared capability has an executable operation behind it;
    // an operation outside the set is refused explicitly, never as a
    // permission verdict.
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, _eid) = submit(&store, "unsupported-ops");
    let ready_id = insert_intent(
        &store,
        &work_id,
        HostAction::MarkReady {
            change: change(),
            expected_head: rev("head-1"),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    match intent_status(&store, &ready_id) {
        HostActionStatus::Unsupported { reason } => {
            assert!(reason.contains("unsupported") && reason.contains("mark_ready"));
        }
        other => panic!("undeclared operation must stay explicit: {other:?}"),
    }
    assert!(adapter.applied().is_empty());
    assert!(adapter.client().writes().is_empty());
}
