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
use workloop_core::projection::{Availability, QueryResult, outstanding_actions};
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, admit_queued, handle_command,
};
use workloop_github::adapter::{GitHubAdapter, WebhookDelivery, WebhookFact};
use workloop_github::api::GitHubApi;
use workloop_github::fixture::FixtureGithub;
use workloop_github::port::IntegrationPort;
use workloop_store::sqlite::SqliteStore;

mod support;

use support::InMemoryGitHub;

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
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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

/// The ambient credential's verified login binds the write: a token
/// acting as another identity never authors the bound actor's intent —
/// the receipt records the real login and the ingest settles
/// `unknown{actor_mismatch}`, leaving the intent outstanding.
#[test]
fn a_foreign_ambient_credential_blocks_the_guarded_write() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, "viewer-mismatch");
    let mut gh = InMemoryGitHub::new();
    gh.set_viewer("other-login");
    let mut adapter = Adapter::new(gh);
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    adapter.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Unknown {
            reason: "actor_mismatch".into()
        },
        "a foreign credential never records an applied write"
    );
    let intent = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id)
                .map(|i| i.authenticated_actor))
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        intent,
        Some(HostActorRef {
            host: host(),
            actor: HostActorId::trusted("other-login"),
        }),
        "the receipt attests the verified login, not the claim"
    );
}

/// A credential whose identity cannot be attested never authors a
/// write either — an indeterminate actor is fail-closed, reported
/// `credential_blocked` and retried on the next pass once `gh api
/// user` answers again.
#[test]
fn an_unverifiable_ambient_credential_blocks_the_guarded_write() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config.host.actors[0].credential_ref = CredentialRef::trusted("default");
    let d = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "viewer-unverifiable".into(),
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
    let CommandDecision::Accepted { work_id, .. } = d else {
        panic!("submit: {d:?}")
    };
    let work_id = work_id.unwrap();
    let mut gh = InMemoryGitHub::new();
    gh.lose_response("viewer");
    let mut adapter = Adapter::new(gh)
        .with_credentials(Box::new(workloop_github::fixture::CredentialBook::new()));
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    let report = adapter
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, 10)
        .unwrap();
    assert!(
        report
            .credential_blocked
            .iter()
            .any(|b| b.action_id == action_id && b.code == "credential.unresolvable"),
        "{report:?}"
    );
    assert!(
        adapter.client().writes().is_empty(),
        "an unattested credential never writes"
    );
    assert!(
        matches!(intent_status(&store, &action_id), HostActionStatus::Pending),
        "a blocked intent stays pending for the next pass"
    );
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
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("{}-invariant-check", eid.0)),
                    request_id: format!("{}:inv:check", eid.0),
                    execution_id: eid.clone(),
                    role: InvocationRole::InvariantCheck,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: obs(
                    &work_id,
                    &eid,
                    None,
                    NOW,
                    ObservationFact::ProcessState {
                        invocation_id: InvocationId(format!("{}-invariant-check", eid.0)),
                        process_identity: String::new(),
                        state: ProcessStateFact::Exited,
                        exit_code: Some(0),
                    },
                ),
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
fn redelivered_webhook_after_a_head_move_records_the_new_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "wh-move");
    let delivery = WebhookDelivery {
        event_id: "delivery-guid-move".into(),
        emitted_at: NOW + 10,
        fact: WebhookFact::Check {
            change: change.clone(),
            head: rev("head-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    };
    let first = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 10)
        .unwrap();
    assert_eq!(first, IngestReceipt::Recorded);

    // The same delivery GUID arrives again after the host moved: the
    // event deduplicates, but its fresh read still records the head
    // the host reports now instead of reusing the old-read identity.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "moved-head");
    let second = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 20)
        .unwrap();
    assert_eq!(second, IngestReceipt::Duplicate);
    assert_eq!(
        current_head(&store, &work_id),
        Some(rev("moved-head")),
        "the redelivered webhook's fresh read records the new head"
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

/// The credential gate before any write compares the transport's
/// proven identity with the intent's bound actor: a credential acting
/// as another account never executes the effect — the branch stays
/// unpushed and `actor_mismatch` is recorded, never an `applied`
/// receipt under the wrong name. When the transport cannot prove an
/// identity the write still runs and the receipt is unauthenticated —
/// never a fabricated `intent.actor`.
#[test]
fn a_write_receipt_carries_the_proven_actor_not_the_configured_one() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, _eid) = submit(&store, "foreign-actor");
    let push_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/foreign".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );

    // The ambient credential acts as an account that is not the bound
    // publisher: the write never executes, and the receipt
    // authenticates the foreign login — `actor_mismatch` records a
    // prevented effect, not a landed one.
    adapter.client_mut().set_viewer("foreign-account");
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert!(
        adapter.client().writes().is_empty(),
        "no write executed under the foreign credential: {:?}",
        adapter.client().writes()
    );
    assert_eq!(
        intent_status(&store, &push_id),
        HostActionStatus::Unknown {
            reason: "actor_mismatch".into()
        }
    );
    let authenticated = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == push_id)
                .map(|i| i.authenticated_actor))
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        authenticated,
        Some(HostActorRef {
            host: host(),
            actor: HostActorId::new("foreign-account".to_string()).unwrap(),
        }),
        "the receipt records who the credential actually acted as"
    );

    // A transport that cannot prove an identity records an
    // unauthenticated receipt — never the configured actor by
    // fabrication.
    adapter.client_mut().clear_viewer();
    let (work_id2, _eid2) = submit(&store, "unproven-actor");
    let push_id2 = insert_intent(
        &store,
        &work_id2,
        HostAction::PushRef {
            branch: "work/unproven".into(),
            produced: rev("head-2"),
            expected_prior: None,
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(intent_status(&store, &push_id2), HostActionStatus::Applied);
    let authenticated = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == push_id2)
                .map(|i| i.authenticated_actor))
        })
        .unwrap()
        .unwrap();
    assert_eq!(authenticated, None, "an unproven identity is `None`");
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
    // The real publish chain pushes the publication branch before the
    // open, so reconciliation has the recorded source revision this
    // Work pushed.
    let push_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/cancel".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
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
    // The push creates the publication branch through the outbox; the
    // open's response is lost after it lands: the effect is
    // outstanding.
    adapter.client_mut().lose_response("open_pull");
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent_status(&store, &push_id),
        HostActionStatus::Applied,
        "the push lands before the open in the publish chain"
    );
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
                    exit_code: None,
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

/// The invariant lane binds the **latest** assessment on the current
/// target: an earlier all-positive assessment is evidence of a state
/// the lane already superseded — when a later assessment reports the
/// set incomplete, completion stays closed even though a positive one
/// exists in the log.
#[test]
fn a_superseded_positive_assessment_never_completes_the_invariant_lane() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = waiting_with_applied_publish(&store, "stale-assessment");
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
    let target = |head: &str| DiffTarget {
        change: opened.clone(),
        head: rev(head),
        base: None,
        target_revision: None,
    };
    // The earlier all-positive assessment lands while CI is still
    // pending — then the lane's next assessment supersedes it with an
    // incomplete coverage report before the other evidence arrives.
    emit(
        &adapter,
        ObservationFact::Assessment {
            target: target("sha-1"),
            set_digest: "invariants".into(),
            complete: true,
            items: vec![AssessmentItem {
                id: "inv-1".into(),
                verdict: AssessmentVerdict::Positive,
                refs: vec![],
            }],
        },
    );
    emit(
        &adapter,
        ObservationFact::Assessment {
            target: target("sha-1"),
            set_digest: "invariants".into(),
            complete: false,
            items: vec![AssessmentItem {
                id: "inv-1".into(),
                verdict: AssessmentVerdict::Negative,
                refs: vec![],
            }],
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
            target: target("sha-1"),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        },
    );

    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Waiting,
        "the latest assessment is incomplete — a superseded positive \
         never seals the lane"
    );
}

/// The invariant lane binds the **latest** check invocation after the
/// producing one: an earlier `Done`/exit-0 check is evidence of a state
/// the lane already superseded — when the newest check exits non-zero,
/// completion stays closed even though a passed check exists in the
/// log.
#[test]
fn a_superseded_passed_invariant_check_never_completes_the_lane() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = waiting_with_applied_publish(&store, "stale-check");
    // A second, later check invocation exits non-zero: the earlier
    // pass is superseded and the lane must stay closed.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("{}-invariant-check-2", eid.0)),
                    request_id: format!("{}:inv:check-2", eid.0),
                    execution_id: eid.clone(),
                    role: InvocationRole::InvariantCheck,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: obs(
                    &work_id,
                    &eid,
                    None,
                    NOW,
                    ObservationFact::ProcessState {
                        invocation_id: InvocationId(format!("{}-invariant-check-2", eid.0)),
                        process_identity: String::new(),
                        state: ProcessStateFact::Exited,
                        exit_code: Some(1),
                    },
                ),
            })
        })
        .unwrap();
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

    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_ne!(
        execution.state,
        ExecutionState::Succeeded,
        "the newest check failed — a superseded pass never seals \
         the lane"
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

#[test]
fn fresh_read_records_a_head_that_was_observed_before() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "force-back");
    assert_eq!(opened_head.object, "head-1");

    // A poll while the change is at head-1 uses the content-derived
    // read identity `pr:<change>@head-1`.
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    // The host moves to head-2, then force-pushes back to head-1.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "head-2");
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    assert_eq!(current_head(&store, &work_id), Some(rev("head-2")));
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "head-1");

    // A webhook arrives for another fact; its mandatory fresh read
    // reports the re-used head-1, which must become the recorded
    // current head even though head-1 was observed before.
    adapter
        .deliver_webhook(
            &store,
            &config,
            &WebhookDelivery {
                event_id: "delivery-force-back".into(),
                emitted_at: NOW + 30,
                fact: WebhookFact::Check {
                    change: change.clone(),
                    head: rev("head-1"),
                    check_id: "ci".into(),
                    producer: publisher(),
                    conclusion: CheckConclusion::Success,
                },
            },
            NOW + 40,
        )
        .unwrap();
    assert_eq!(
        current_head(&store, &work_id),
        Some(rev("head-1")),
        "the fresh read records a head the host reports again after a force push"
    );
}

#[test]
fn unknown_open_reconciliation_requires_the_recorded_source_revision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, _eid) = submit(&store, "foreign-open");
    let branch = "work/foreign";

    // A foreign change request already exists from the same source
    // branch into the same target.
    adapter
        .client_mut()
        .seed_branch(&repo(), branch, "foreign-head");
    let foreign = adapter
        .client_mut()
        .open_pull(&repo(), branch, "main", "foreign", "")
        .unwrap();
    assert_eq!(foreign.head_sha, "foreign-head");

    // Our publication moves the branch to the revision this Work
    // recorded; the foreign change stays on its own older head.
    insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: branch.into(),
            produced: rev("our-head"),
            expected_prior: Some(rev("foreign-head")),
        },
    );
    adapter.apply_outbox(&store, &config, NOW).unwrap();

    // GitHub keeps an open pull request on its source branch's head:
    // the foreign change follows our push onto the recorded revision.
    // A revision-only check would find it there — the intent's own
    // payload is what still marks it foreign.
    assert_eq!(
        adapter
            .client()
            .pull(&repo(), foreign.id.as_str())
            .map(|pull| pull.head_sha),
        Some("our-head".to_string()),
        "the foreign change's head follows the branch onto our revision"
    );

    // The open's result is unknown, so reconciliation must identify it
    // by the recorded source revision, not merely by the branch.
    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch.into(),
            target_branch: "main".into(),
            title: "ours".into(),
            body: "b".into(),
        },
    );
    adapter
        .ingest(
            &store,
            &config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: open_id.clone(),
                    authenticated_actor: Some(publisher()),
                    status: HostActionStatus::Unknown {
                        reason: "response lost".into(),
                    },
                    effects: vec![],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();

    let resolved = adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert!(
        !resolved.contains(&open_id),
        "a foreign change request at another head is not this Work's effect"
    );
    assert!(
        matches!(
            intent_status(&store, &open_id),
            HostActionStatus::Unknown { .. }
        ),
        "the unreconciled open stays outstanding"
    );
    assert!(
        !observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::HostActionResult { action_id, effects, .. }
                if action_id == &open_id
                    && effects.iter().any(|effect| matches!(
                        effect,
                        NormalizedEffectFact::ChangeOpened { change, .. }
                            if change.change_request.as_str() == foreign.id
                    ))
        )),
        "the adapter never adopts a change request it did not open"
    );
}

#[test]
fn in_memory_transport_is_not_a_library_surface() {
    let api = include_str!("../src/api.rs");
    assert!(
        !api.contains("pub struct InMemoryGitHub"),
        "the in-memory transport double must stay out of the library surface"
    );
    let support = include_str!("support/mod.rs");
    assert!(
        support.contains("pub struct InMemoryGitHub"),
        "the test-only in-memory transport lives in the test tree"
    );
}

#[test]
fn review_findings_from_the_host_block_the_review_lane_until_dispositioned() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = waiting_with_applied_publish(&store, "review-lane");

    // The host holds this Work's change: one review that both carries
    // an inline finding and requests changes.
    let branch = format!("work/{}", work_id.0);
    adapter.client_mut().seed_branch(&repo(), &branch, "head-1");
    let pull = adapter
        .client_mut()
        .open_pull(&repo(), &branch, "main", "t", "b")
        .unwrap();
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(pull.id.clone()),
        display_number: Some(pull.display_number),
    };
    assert_eq!(pull.head_sha, "head-1");
    adapter.client_mut().seed_changes_requested_review(
        &repo(),
        pull.id.as_str(),
        "rev-1",
        "head-1",
        "reviewer",
        "apply the rename",
    );
    adapter.client_mut().seed_review_finding(
        &repo(),
        pull.id.as_str(),
        "rev-1",
        "head-1",
        "nit: rename the helper",
    );

    let emit = |adapter: &Adapter, fact: ObservationFact| {
        adapter
            .ingest(&store, &config, obs(&work_id, &eid, None, NOW, fact), NOW)
            .unwrap();
    };
    emit(
        &adapter,
        ObservationFact::HostActionResult {
            action_id: "act-pub-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change.clone(),
                head: rev("head-1"),
                source_branch: branch.clone(),
                target_branch: "main".into(),
            }],
        },
    );
    emit(
        &adapter,
        ObservationFact::ChangeHead {
            change: change.clone(),
            head: rev("head-1"),
        },
    );
    emit(
        &adapter,
        ObservationFact::Check {
            change: change.clone(),
            head: rev("head-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    emit(
        &adapter,
        ObservationFact::Assessment {
            target: DiffTarget {
                change: change.clone(),
                head: rev("head-1"),
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
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Waiting,
        "every other lane is green; the review lane waits on a round"
    );

    // The poll reads the reviews plus the findings surface: the round
    // it seals carries both occurrences — never an empty clean round.
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    let occurrences = observations(&store, &work_id)
        .iter()
        .find_map(|o| match &o.fact {
            ObservationFact::Review {
                completeness: Completeness::Complete,
                occurrences,
                ..
            } => Some(occurrences.clone()),
            _ => None,
        })
        .expect("the findings read seals a complete review fact");
    let keys: Vec<&str> = occurrences.iter().map(|o| o.source_key.as_str()).collect();
    assert_eq!(keys, ["changes-requested:rev-1", "rf-rev-1-1"]);

    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_ne!(
        execution.state,
        ExecutionState::Succeeded,
        "a finding-bearing review is never recorded as clean"
    );
    assert_eq!(execution.state, ExecutionState::Stopping);
    assert_eq!(
        execution.pause_reason.as_deref(),
        Some("review findings require human disposition"),
        "the findings demand a human disposition"
    );

    // JudgeFindings covers the sealed round exactly.
    let (occurrence_ids, version) = store
        .transact(|tx| {
            let round = tx.latest_round(&work_id)?.expect("sealed round");
            let ids: Vec<String> = tx
                .occurrences(&round.id)?
                .into_iter()
                .map(|o| o.id)
                .collect();
            let version = tx.work(&work_id)?.expect("work").version;
            Ok((ids, version))
        })
        .unwrap();
    let head = rev("head-1");
    let d = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "judge-review-lane-1".into(),
            body: CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: version,
                expected_head: head.clone(),
                decisions: occurrence_ids
                    .iter()
                    .map(|occurrence_id| FindingDecision {
                        occurrence_id: occurrence_id.clone(),
                        choice: FindingChoice::Accept,
                        rationale: Some("acknowledged".into()),
                    })
                    .collect(),
            },
        },
        NOW + 20,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    // The writer stop proves the pause, and the recorded judgment
    // releases the gate.
    emit(
        &adapter,
        ObservationFact::ProcessState {
            invocation_id: InvocationId(format!("{}-coding", eid.0)),
            process_identity: String::new(),
            state: ProcessStateFact::Exited,
            exit_code: None,
        },
    );
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Paused,
        "the repair pause settles once the writer stop is proven"
    );
    let version = store
        .transact(|tx| Ok(tx.work(&work_id)?.expect("work").version))
        .unwrap();
    let d = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "release-review-lane-1".into(),
            body: CommandBody::ReleaseJudgment {
                work_id: work_id.clone(),
                expected_work_version: version,
                expected_head: head.clone(),
            },
        },
        NOW + 30,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    // Back in the ci wait, the dispositioned round passes the review
    // lane and the Work completes on evidence.
    store
        .transact(|tx| {
            let mut execution = tx.execution(&eid)?.expect("execution");
            assert_eq!(execution.state, ExecutionState::Queued);
            execution.state = ExecutionState::Waiting;
            execution.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Queued,
            })?;
            Ok(())
        })
        .unwrap();
    emit(
        &adapter,
        ObservationFact::ChangeHead {
            change: change.clone(),
            head: rev("head-1"),
        },
    );
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Succeeded,
        "only a dispositioned round lets the review lane pass"
    );
}

#[test]
fn webhook_review_delivery_records_partial_evidence_and_reads_findings_fresh() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "wh-review");
    adapter.client_mut().seed_review(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "reviewer",
    );
    adapter.client_mut().seed_review_finding(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "nit: rename the helper",
    );

    let delivery = WebhookDelivery {
        event_id: "delivery-review-1".into(),
        emitted_at: NOW + 10,
        fact: WebhookFact::Review {
            change: change.clone(),
            head: rev("head-1"),
            review_id: "rev-1".into(),
            source: "reviewer".into(),
        },
    };
    let receipt = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 10)
        .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);

    let recorded = observations(&store, &work_id);
    // The event carries no findings: it records partial evidence and
    // never a complete round that could pass the review lane alone.
    assert!(
        recorded
            .iter()
            .any(|o| o.delivery.as_ref().is_some_and(|d| {
                d.source == "github.webhook"
                    && matches!(
                        &o.fact,
                        ObservationFact::Review {
                            completeness: Completeness::Partial { .. },
                            ..
                        }
                    )
            })),
        "the delivery itself is partial evidence"
    );
    assert!(
        !recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if occurrences.is_empty()
        )),
        "no review is ever recorded as a clean empty round"
    );
    // The fresh review read behind the delivery records the findings
    // the host reports for the review's exact head.
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if occurrences.iter().any(|occ| occ.source_key == "rf-rev-1-1")
        )),
        "the fresh read carries the host's findings"
    );

    // A redelivered event deduplicates — the fact it asserts (partial
    // evidence) is deterministic — while its fresh reads stay
    // idempotent too: the whole delivery records nothing new.
    let before_redelivery = observations(&store, &work_id).len();
    let second = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 20)
        .unwrap();
    assert_eq!(second, IngestReceipt::Duplicate);
    let after = observations(&store, &work_id);
    assert_eq!(
        after.len(),
        before_redelivery,
        "a redelivered review event records no new observation"
    );
    assert_eq!(
        after
            .iter()
            .filter(|o| o
                .delivery
                .as_ref()
                .is_some_and(|d| d.source == "github.webhook"))
            .count(),
        1,
        "exactly one webhook-scoped observation per delivery GUID"
    );
}

#[test]
fn a_failed_findings_read_records_the_review_unavailable_not_clean() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "findings-down");
    adapter.client_mut().seed_review(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "reviewer",
    );

    // The reviews arrive but the findings surface does not: the
    // review is recorded `unavailable` — source availability is part
    // of the fact, so an empty clean round is never claimed.
    adapter.client_mut().lose_response("review_findings");
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    let recorded = observations(&store, &work_id);
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Unavailable { .. },
                ..
            }
        )),
        "the review records its findings source as unavailable"
    );
    assert!(
        !recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Complete,
                ..
            }
        )),
        "an unread findings surface never produces a complete round"
    );

    // The next successful read records the now-available state under a
    // fresh content identity instead of colliding with the recorded
    // unavailable fact.
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    let later = observations(&store, &work_id);
    assert!(
        later.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if occurrences.is_empty()
        )),
        "an available source with zero findings is a meaningful clean round"
    );
}

#[test]
fn unknown_open_without_a_recorded_push_is_never_reconciled_from_the_branch_alone() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, _eid) = submit(&store, "no-push-open");
    let branch = "work/no-push";

    // A foreign change request already exists on the branch, at the
    // branch's current head — exactly where a branch-only check would
    // look — and a second change carries this Work's own payload
    // (same branch, same target, same title and body).
    adapter
        .client_mut()
        .seed_branch(&repo(), branch, "seed-head");
    let foreign = adapter
        .client_mut()
        .open_pull(&repo(), branch, "main", "foreign", "")
        .unwrap();
    let ours = adapter
        .client_mut()
        .open_pull(&repo(), branch, "main", "ours", "b")
        .unwrap();

    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: branch.into(),
            target_branch: "main".into(),
            title: "ours".into(),
            body: "b".into(),
        },
    );
    adapter
        .ingest(
            &store,
            &config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: open_id.clone(),
                    authenticated_actor: Some(publisher()),
                    status: HostActionStatus::Unknown {
                        reason: "response lost".into(),
                    },
                    effects: vec![],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();

    let resolved = adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert!(
        !resolved.contains(&open_id),
        "with no recorded source revision the intent stays unknown"
    );
    assert!(matches!(
        intent_status(&store, &open_id),
        HostActionStatus::Unknown { .. }
    ));
    assert!(
        !observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::HostActionResult { action_id, effects, .. }
                if action_id == &open_id
                    && effects.iter().any(|effect| matches!(
                        effect,
                        NormalizedEffectFact::ChangeOpened { change, .. }
                            if change.change_request.as_str() == foreign.id
                                || change.change_request.as_str() == ours.id
                    ))
        )),
        "neither the foreign change nor the payload match is adopted without a recorded push"
    );
}

#[test]
fn a_new_heads_review_inherits_no_finding_from_a_previous_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "head-leak");
    adapter.client_mut().seed_review(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "reviewer",
    );
    adapter.client_mut().seed_review_finding(
        &repo(),
        change.change_request.as_str(),
        "rev-1",
        "head-1",
        "nit: rename the helper",
    );
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    assert!(
        observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-1"
                && occurrences.iter().any(|occ| occ.source_key == "rf-rev-1-1")
        )),
        "the finding is recorded against the head its review examined"
    );

    // A newer head with its own clean review: the old head's finding
    // never migrates onto it, while the old-head evidence stays.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "head-2");
    adapter.client_mut().seed_review(
        &repo(),
        change.change_request.as_str(),
        "rev-2",
        "head-2",
        "reviewer",
    );
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    let recorded = observations(&store, &work_id);
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-2" && occurrences.is_empty()
        )),
        "the new head's review carries no finding from the superseded head"
    );
    assert!(
        !recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review { target, occurrences, .. }
                if target.head.object == "head-2"
                    && occurrences.iter().any(|occ| occ.source_key == "rf-rev-1-1")
        )),
        "an old-head finding is never re-labeled onto the new head"
    );
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review { target, occurrences, .. }
                if target.head.object == "head-1"
                    && occurrences.iter().any(|occ| occ.source_key == "rf-rev-1-1")
        )),
        "the superseded head keeps its own finding evidence"
    );
}

#[test]
fn an_empty_review_stream_seals_a_complete_round_and_passes_the_review_lane() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, eid) = waiting_with_applied_publish(&store, "empty-stream");

    // The host holds this Work's change with zero submitted reviews:
    // the findings surface answers, the stream is empty.
    let branch = format!("work/{}", work_id.0);
    adapter.client_mut().seed_branch(&repo(), &branch, "head-1");
    let pull = adapter
        .client_mut()
        .open_pull(&repo(), &branch, "main", "t", "b")
        .unwrap();
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(pull.id.clone()),
        display_number: Some(pull.display_number),
    };
    assert_eq!(pull.head_sha, "head-1");

    let emit = |adapter: &Adapter, fact: ObservationFact| {
        adapter
            .ingest(&store, &config, obs(&work_id, &eid, None, NOW, fact), NOW)
            .unwrap();
    };
    emit(
        &adapter,
        ObservationFact::HostActionResult {
            action_id: "act-pub-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change.clone(),
                head: rev("head-1"),
                source_branch: branch.clone(),
                target_branch: "main".into(),
            }],
        },
    );
    emit(
        &adapter,
        ObservationFact::ChangeHead {
            change: change.clone(),
            head: rev("head-1"),
        },
    );
    emit(
        &adapter,
        ObservationFact::Check {
            change: change.clone(),
            head: rev("head-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    emit(
        &adapter,
        ObservationFact::Assessment {
            target: DiffTarget {
                change: change.clone(),
                head: rev("head-1"),
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
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Waiting,
        "every other lane is green; the review lane waits on a round"
    );

    // The poll reads an empty stream with the findings surface
    // answering: the complete-empty round is recorded for the head
    // just observed, so the enabled review lane passes on it.
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    assert!(
        observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-1" && occurrences.is_empty()
        )),
        "an answered empty review stream is a complete-empty round"
    );
    let round = store
        .transact(|tx| tx.latest_round(&work_id))
        .unwrap()
        .expect("the empty stream seals a round");
    assert_eq!(round.completeness, "complete");
    assert!(
        store
            .transact(|tx| tx.occurrences(&round.id))
            .unwrap()
            .is_empty(),
        "the sealed round carries no occurrences"
    );
    let sources = match store
        .transact(|tx| workloop_core::projection::work_detail(tx, &work_id, NOW + 10))
        .unwrap()
    {
        QueryResult::Found { sources, .. } => sources,
        other => panic!("work_detail: {other:?}"),
    };
    assert!(
        sources
            .iter()
            .any(|s| s.source.starts_with("review") && s.availability == Availability::Known),
        "the review source answers as known, never unavailable: {sources:?}"
    );
    let execution = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        execution.state,
        ExecutionState::Succeeded,
        "a complete-empty round passes the enabled review lane"
    );

    // A head move: the empty stream at the new head records under its
    // own identity instead of colliding with the recorded one, and the
    // superseded head's complete-empty evidence stays.
    adapter
        .client_mut()
        .set_pull_head(&repo(), change.change_request.as_str(), "head-2");
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    let moved = observations(&store, &work_id);
    assert!(
        moved.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-2" && occurrences.is_empty()
        )),
        "the empty stream records again at the new head under its own identity"
    );
    assert!(
        moved.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-1" && occurrences.is_empty()
        )),
        "the superseded head's complete-empty evidence stays"
    );
}

#[test]
fn a_review_delivery_on_an_empty_stream_records_the_complete_empty_round() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "wh-empty");
    // No reviews are seeded: the fresh review-state read behind the
    // delivery finds an empty stream and must still answer with the
    // complete-empty round bound to the head GitHub reports now.
    let delivery = WebhookDelivery {
        event_id: "delivery-empty-1".into(),
        emitted_at: NOW + 10,
        fact: WebhookFact::Review {
            change: change.clone(),
            head: rev("head-1"),
            review_id: "rev-9".into(),
            source: "reviewer".into(),
        },
    };
    let receipt = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 10)
        .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);

    let recorded = observations(&store, &work_id);
    assert!(
        recorded
            .iter()
            .any(|o| o.delivery.as_ref().is_some_and(|d| {
                d.source == "github.webhook"
                    && matches!(
                        &o.fact,
                        ObservationFact::Review {
                            completeness: Completeness::Partial { .. },
                            ..
                        }
                    )
            })),
        "the delivery itself is partial evidence"
    );
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-1" && occurrences.is_empty()
        )),
        "the fresh read behind the delivery records the complete-empty round"
    );

    // A redelivery records nothing new: the empty-stream fact
    // deduplicates under its content-derived identity.
    let before_redelivery = recorded.len();
    let second = adapter
        .deliver_webhook(&store, &config, &delivery, NOW + 20)
        .unwrap();
    assert_eq!(second, IngestReceipt::Duplicate);
    assert_eq!(
        observations(&store, &work_id).len(),
        before_redelivery,
        "a redelivered delivery records no new observation"
    );
}

#[test]
fn an_empty_stream_with_an_unread_findings_source_records_no_complete_round() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, _head) = push_and_open(&mut adapter, &store, &config, "empty-down");
    // No reviews, and the findings surface does not answer: the empty
    // stream records `unavailable` — never an empty clean round.
    adapter.client_mut().lose_response("review_findings");
    adapter
        .refresh_change(&store, &config, &change, NOW + 10)
        .unwrap();
    let recorded = observations(&store, &work_id);
    assert!(
        recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Unavailable { .. },
                ..
            }
        )),
        "the empty stream records its findings source as unavailable"
    );
    assert!(
        !recorded.iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                completeness: Completeness::Complete,
                ..
            }
        )),
        "an unread findings surface never produces a complete round"
    );

    // The next successful read records the now-available empty state
    // under its own identity instead of colliding with the recorded
    // unavailable fact.
    adapter
        .refresh_change(&store, &config, &change, NOW + 20)
        .unwrap();
    assert!(
        observations(&store, &work_id).iter().any(|o| matches!(
            &o.fact,
            ObservationFact::Review {
                target,
                completeness: Completeness::Complete,
                occurrences,
                ..
            } if target.head.object == "head-1" && occurrences.is_empty()
        )),
        "an available source with an empty stream is a meaningful clean round"
    );
}

/// A comment that carries the intent's marker but a stored body that
/// differs from the recorded payload is foreign evidence: the guarded
/// write never posts a duplicate, and a lost-response reconcile never
/// settles the intent `applied` on somebody else's comment.
#[test]
fn a_marker_match_with_a_different_body_never_settles_a_comment_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "cmtbody");
    adapter.client_mut().seed_comment(
        &repo(),
        change.change_request.as_str(),
        "workloop",
        "a foreign body under our marker",
    );

    // Pending intent: reconcile-before-write sees the marker but a
    // different stored body — the write must not post a duplicate.
    let pending_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change.clone(),
            expected_head: opened_head.clone(),
            marker: "workloop".into(),
            body: "hello".into(),
        },
    );
    let writes_before = adapter.client().writes().len();
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        adapter.client().writes().len(),
        writes_before,
        "no duplicate post under a marker a foreign comment already carries"
    );
    assert!(
        !matches!(
            intent_status(&store, &pending_id),
            HostActionStatus::Applied
        ),
        "a body mismatch never settles the intent applied"
    );

    // Unknown intent: a lost-response reconcile meets the same foreign
    // marker and must stay outstanding rather than adopt the comment.
    let unknown_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change.clone(),
            expected_head: opened_head.clone(),
            marker: "workloop".into(),
            body: "hello".into(),
        },
    );
    adapter
        .ingest(
            &store,
            &config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: unknown_id.clone(),
                    authenticated_actor: Some(publisher()),
                    status: HostActionStatus::Unknown {
                        reason: "response lost".into(),
                    },
                    effects: vec![],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    let resolved = adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert!(
        !resolved.contains(&unknown_id),
        "a marker match with a different body is not this intent's effect"
    );
    assert!(
        matches!(
            intent_status(&store, &unknown_id),
            HostActionStatus::Unknown { .. }
        ),
        "the unreconciled comment stays outstanding"
    );
}

/// A comment this adapter already posted carries the marker envelope
/// (`{body}\n\n<!-- {marker} -->`) on live transports — reconcile
/// must recognize the stored body as this intent's effect and settle
/// it `applied` after a lost response, not hold it `unknown` forever.
#[test]
fn a_stored_comment_with_the_marker_envelope_settles_applied() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let mut adapter = Adapter::new(InMemoryGitHub::new());
    let (work_id, change, opened_head) = push_and_open(&mut adapter, &store, &config, "cmtenv");
    adapter.client_mut().seed_comment(
        &repo(),
        change.change_request.as_str(),
        "workloop",
        "hello\n\n<!-- workloop -->",
    );

    let unknown_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change.clone(),
            expected_head: opened_head.clone(),
            marker: "workloop".into(),
            body: "hello".into(),
        },
    );
    adapter
        .ingest(
            &store,
            &config,
            Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::HostActionResult {
                    action_id: unknown_id.clone(),
                    authenticated_actor: Some(publisher()),
                    status: HostActionStatus::Unknown {
                        reason: "response lost".into(),
                    },
                    effects: vec![],
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    let resolved = adapter
        .reconcile_outstanding(&store, &config, NOW + 10)
        .unwrap();
    assert!(
        resolved.contains(&unknown_id),
        "the stored envelope still proves this intent's effect"
    );
    assert!(
        matches!(
            intent_status(&store, &unknown_id),
            HostActionStatus::Applied
        ),
        "the lost-response comment settles applied"
    );
}

/// The adapter-boundary credential gate (parity with the fixture's
/// book): a pending write resolves the role binding's `credential_ref`
/// and the resolved value authenticates as the intent's authorized
/// actor before any write — never the ambient login. A credential the
/// provider cannot resolve, or one that authenticates as another
/// actor, blocks the intent without a write.
#[test]
fn a_pending_write_binds_the_declared_credential_before_writing() {
    let submit_with = |store: &SqliteStore, key: &str, config: &WorkloopConfig| {
        let d = handle_command(
            store,
            config,
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
        let CommandDecision::Accepted { work_id, .. } = d else {
            panic!("submit: {d:?}")
        };
        work_id.unwrap()
    };
    let push = |branch: &str| HostAction::PushRef {
        branch: branch.into(),
        produced: rev("head-1"),
        expected_prior: None,
    };

    // Happy path: `env:` resolves, the host reports the bound actor,
    // and the write runs under the resolved credential.
    let store = SqliteStore::in_memory(1).unwrap();
    let mut config = config();
    config.host.actors[0].credential_ref = CredentialRef::trusted("env:TAISAN_TEST_TOKEN");
    let mut book = workloop_github::fixture::CredentialBook::new();
    book.seat_env("TAISAN_TEST_TOKEN", "tok-bound");
    let mut adapter = Adapter::new(InMemoryGitHub::new()).with_credentials(Box::new(book));
    adapter
        .client_mut()
        .seat_credential("tok-bound", publisher());
    let work_id = submit_with(&store, "cred-ok", &config);
    let push_id = insert_intent(&store, &work_id, push("work/cred-ok"));
    adapter.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(intent_status(&store, &push_id), HostActionStatus::Applied);
    assert!(
        adapter
            .client()
            .writes()
            .iter()
            .any(|w| w.starts_with("push_ref")),
        "the bound credential's write landed"
    );

    // Mismatch: the resolved value authenticates as another actor —
    // `credential.actor_mismatch` blocks the intent with no write.
    let store = SqliteStore::in_memory(1).unwrap();
    let mut book = workloop_github::fixture::CredentialBook::new();
    book.seat_env("TAISAN_TEST_TOKEN", "tok-foreign");
    let mut adapter = Adapter::new(InMemoryGitHub::new()).with_credentials(Box::new(book));
    adapter.client_mut().seat_credential(
        "tok-foreign",
        HostActorRef {
            host: host(),
            actor: HostActorId::trusted("someone-else"),
        },
    );
    let work_id = submit_with(&store, "cred-mm", &config);
    let push_id = insert_intent(&store, &work_id, push("work/cred-mm"));
    let report = adapter
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, 10)
        .unwrap();
    assert!(
        report
            .credential_blocked
            .iter()
            .any(|b| b.action_id == push_id && b.code == "credential.actor_mismatch"),
        "{report:?}"
    );
    assert_eq!(
        adapter.client().writes().len(),
        0,
        "a foreign credential never writes"
    );
    assert!(
        matches!(intent_status(&store, &push_id), HostActionStatus::Pending),
        "a blocked intent stays pending for repair"
    );

    // Unresolvable: the env slot holds no value — `credential.unresolvable`.
    let store = SqliteStore::in_memory(1).unwrap();
    let adapter_book = workloop_github::fixture::CredentialBook::new();
    let mut adapter = Adapter::new(InMemoryGitHub::new()).with_credentials(Box::new(adapter_book));
    let work_id = submit_with(&store, "cred-unres", &config);
    let push_id = insert_intent(&store, &work_id, push("work/cred-unres"));
    let report = adapter
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, 10)
        .unwrap();
    assert!(
        report
            .credential_blocked
            .iter()
            .any(|b| b.action_id == push_id && b.code == "credential.unresolvable"),
        "{report:?}"
    );
    assert_eq!(adapter.client().writes().len(), 0);
    assert!(matches!(
        intent_status(&store, &push_id),
        HostActionStatus::Pending
    ));
}
