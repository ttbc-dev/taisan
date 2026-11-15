//! Fixture host adapter contract: `apply_outbox` drains only recorded
//! `HostActionIntent` rows, re-checks the synthetic remote head
//! immediately before each write, and reports every result as a
//! `host_action_result` Observation reconciled by action ID.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

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

fn target(object: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(object),
        base: None,
        target_revision: None,
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
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
        ]),
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

/// Authorize the capabilities a recorded stage effect needs.
fn authorize(store: &SqliteStore, capabilities: &[HostCapability]) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
                host: host(),
                capabilities: capabilities
                    .iter()
                    .map(|capability| CapabilityEntry {
                        capability: *capability,
                        support: CapabilityState::Supported,
                        authorization: CapabilityState::Authorized,
                    })
                    .collect(),
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) -> String {
    insert_intent_with(store, work_id, action, HostActionStatus::Pending)
}

fn insert_intent_with(
    store: &SqliteStore,
    work_id: &WorkId,
    action: HostAction,
    status: HostActionStatus,
) -> String {
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
                    status,
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

fn record_finished_coding(tx: &mut dyn TxView, eid: &ExecutionId) -> Result<(), StoreError> {
    let request = tx
        .pending_requests()?
        .into_iter()
        .find(|request| request.execution_id == *eid)
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
    })
}

/// A repeated artifact report for the finished coding Invocation — the
/// observation that re-drives `maybe_publish` on the next pass.
fn artifact_report(work_id: &WorkId, eid: &ExecutionId, event: &str) -> Observation {
    Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "test".into(),
            event_id: event.into(),
        }),
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::Artifact {
            invocation_id: InvocationId(format!("{}-coding", eid.0)),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present {
                digest: "artifact-digest".into(),
            },
        },
        observed_unix_seconds: NOW,
    }
}

/// Shared setup for completion-vs-evidence contract tests: an admitted
/// Execution parked in `Waiting/Ci` with applied push_ref and
/// open_change_request intents, so completion turns solely on the
/// current-target evidence lanes.
fn waiting_with_applied_publish(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let (work_id, eid) = submit(store, key);
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::Ci);
            record_finished_coding(tx, &eid)?;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
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

#[test]
fn publish_writes_only_from_a_recorded_intent_and_reports_applied() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g1");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/w-1".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    // The intent row exists before the simulated write happens.
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Pending);

    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
    assert_eq!(gh.applied(), &["open_change:test-host/local-repo:cr-1"]);
    // The write surfaced the fresh head as an Observation.
    let heads = store
        .transact(|tx| {
            Ok(tx
                .observations_for_work(&work_id)?
                .iter()
                .filter(|o| matches!(o.fact, ObservationFact::ChangeHead { .. }))
                .count())
        })
        .unwrap();
    assert_eq!(heads, 1);
}

#[test]
fn head_mismatch_aborts_the_write_and_surfaces_the_fresh_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g2");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    // The realistic drain context: the execution is waiting on its
    // outstanding host write, and the Work's recorded transport account
    // is spent, so the definite failure pauses for repair.
    store
        .transact(|tx| {
            let mut w = tx.work(&work_id)?.unwrap();
            let expected = w.version;
            w.policy.transport_max_retries = 0;
            tx.apply(&Effect::UpdateWork { work: w, expected })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::HostWrite);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    // The remote moved since the intent was recorded.
    gh.set_head("cr-1", "sha-new");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change(),
            expected_head: rev("sha-old"),
            title: Some("edited".into()),
            body: Some("edited body".into()),
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    assert!(gh.applied().is_empty(), "no write on head mismatch");
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Failed {
            reason: "expected head moved".into()
        }
    );
    // A fresh change_head Observation records the actual remote head,
    // and no automatic re-issue is attempted.
    let observations = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    assert!(observations.iter().any(|o| matches!(
        &o.fact,
        ObservationFact::ChangeHead { change: c, head }
            if c.change_request.as_str() == "cr-1" && head.object == "sha-new"
    )));
    // The exhausted transport account stops the writer before pausing:
    // the Execution parks in `stopping` with a recorded `pause` desire
    // until writer-stop proof settles it.
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Stopping);
    assert_eq!(e.desired_action, Some(DesiredAction::Pause));
    assert!(
        e.pause_reason
            .as_deref()
            .unwrap_or("")
            .contains("transport")
    );
    gh.emit(
        &store,
        &config(),
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::ProcessState {
                invocation_id: InvocationId("inv-head".into()),
                process_identity: "1@1".into(),
                state: ProcessStateFact::Exited,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
}

#[test]
fn head_match_applies_the_write() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, "k-g3");
    let mut gh = FixtureGithub::new();
    gh.set_head("cr-1", "sha-x");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change(),
            expected_head: rev("sha-x"),
            marker: "workloop".into(),
            body: "hello".into(),
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["post_comment:test-host/local-repo:cr-1"]);
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
}

#[test]
fn unknown_result_stays_outstanding_until_reconciled_by_action_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g4");
    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "b".into(),
            produced: rev("head-x"),
            expected_prior: None,
        },
        HostActionStatus::Pending,
    );
    // Simulate a lost write: the transport reports the result unknown.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateHostActionIntent {
                action_id: action_id.clone(),
                status: HostActionStatus::Unknown {
                    reason: "transport dropped".into(),
                },
                authenticated_actor: None,
                effects: vec![],
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    // The outbox drain does not retry an `unknown` intent — it is
    // outstanding until reconciled by action ID.
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert!(gh.applied().is_empty());
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Unknown {
            reason: "transport dropped".into()
        }
    );
    let outstanding = store
        .transact(|tx| workloop_core::projection::outstanding_actions(tx, &work_id))
        .unwrap();
    assert_eq!(outstanding, vec![action_id.clone()]);

    // Reconciliation arrives keyed by the same action ID with the
    // normalized effect the intent authorized.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid),
            fact: ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: None,
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: repo(),
                    branch: "b".into(),
                    produced: rev("head-x"),
                    observed: rev("head-x"),
                    relation: RefRelation::Equal,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
}

/// An `applied` intent whose remote fact later diverges re-arms — but
/// only on an `unknown` receipt authenticated by the intent's bound
/// actor. Unauthenticated and unbound receipts are invalid evidence and
/// never re-open a reconciled intent.
#[test]
fn applied_intent_rearms_only_on_bound_actor_unknown_receipt() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-drift");
    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "b".into(),
            produced: rev("head-x"),
            expected_prior: None,
        },
        HostActionStatus::Applied,
    );
    let drift = |authenticated_actor: Option<HostActorRef>, event: &str| {
        handle_observation(
            &store,
            &config(),
            &obs(
                &work_id,
                &eid,
                Some(event),
                NOW,
                ObservationFact::HostActionResult {
                    action_id: action_id.clone(),
                    authenticated_actor,
                    status: HostActionStatus::Unknown {
                        reason: "remote no longer contains the pushed revision".into(),
                    },
                    effects: vec![NormalizedEffectFact::RefPublished {
                        repository: repo(),
                        branch: "b".into(),
                        produced: rev("head-x"),
                        observed: rev("head-y"),
                        relation: RefRelation::Diverged,
                    }],
                },
            ),
            NOW,
        )
        .unwrap();
    };

    // An unauthenticated drift receipt cannot resurrect the intent.
    drift(None, "drift-unauthenticated");
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);

    // A receipt authenticated as a different actor cannot either.
    drift(
        Some(HostActorRef {
            host: host(),
            actor: HostActorId::trusted("other-agent"),
        }),
        "drift-wrong-actor",
    );
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);

    // The bound actor's drift receipt re-opens reconciliation: the
    // intent is outstanding again, not stuck behind the applied
    // terminal.
    drift(Some(publisher()), "drift-bound-actor");
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Unknown {
            reason: "remote no longer contains the pushed revision".into()
        }
    );
    let outstanding = store
        .transact(|tx| workloop_core::projection::outstanding_actions(tx, &work_id))
        .unwrap();
    assert_eq!(outstanding, vec![action_id.clone()]);

    // Re-armed means reconcilable again: a later receipt authenticated
    // by the bound actor settles it.
    handle_observation(
        &store,
        &config(),
        &obs(
            &work_id,
            &eid,
            Some("resettle"),
            NOW,
            ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: repo(),
                    branch: "b".into(),
                    produced: rev("head-x"),
                    observed: rev("head-z"),
                    relation: RefRelation::Ancestor,
                }],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
}

#[test]
fn failed_intent_stays_terminal_under_unknown_receipts() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-drift-failed");
    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "b".into(),
            produced: rev("head-x"),
            expected_prior: None,
        },
        HostActionStatus::Failed {
            reason: "push rejected".into(),
        },
    );
    handle_observation(
        &store,
        &config(),
        &obs(
            &work_id,
            &eid,
            Some("drift-on-failed"),
            NOW,
            ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: Some(publisher()),
                status: HostActionStatus::Unknown {
                    reason: "remote fact moved".into(),
                },
                effects: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(
        intent_status(&store, &action_id),
        HostActionStatus::Failed {
            reason: "push rejected".into()
        }
    );
}

#[test]
fn head_bound_evidence_drives_success_only_for_the_current_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = waiting_with_applied_publish(&store, "k-g5");
    let gh = FixtureGithub::new();
    let emit = |fact: ObservationFact| {
        gh.emit(&store, &config(), obs(&work_id, &eid, None, NOW, fact), NOW)
            .unwrap();
    };
    // The opened change and its current head.
    emit(ObservationFact::ChangeHead {
        change: change(),
        head: rev("new"),
    });
    emit(ObservationFact::HostActionResult {
        action_id: "act-pub-open".into(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::ChangeOpened {
            change: change(),
            head: rev("new"),
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
        }],
    });
    // Evidence bound to a superseded target must not authorize the
    // current one.
    let stale_evidence = |emit: &dyn Fn(ObservationFact)| {
        emit(ObservationFact::Check {
            change: change(),
            head: rev("old"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        });
        emit(ObservationFact::Review {
            target: target("old"),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        });
        emit(ObservationFact::Assessment {
            target: target("old"),
            set_digest: "invariants".into(),
            complete: true,
            items: vec![AssessmentItem {
                id: "inv-1".into(),
                verdict: AssessmentVerdict::Positive,
                refs: vec![],
            }],
        });
    };
    stale_evidence(&emit);
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Waiting);

    // Evidence covering the current head completes it.
    emit(ObservationFact::Check {
        change: change(),
        head: rev("new"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Success,
    });
    emit(ObservationFact::Review {
        target: target("new"),
        source: "reviewer".into(),
        source_revision: None,
        completeness: Completeness::Complete,
        occurrences: vec![],
    });
    emit(ObservationFact::Assessment {
        target: target("new"),
        set_digest: "invariants".into(),
        complete: true,
        items: vec![AssessmentItem {
            id: "inv-1".into(),
            verdict: AssessmentVerdict::Positive,
            refs: vec![],
        }],
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}

#[test]
fn repeated_artifact_evidence_does_not_queue_a_second_publish() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g6");
    authorize(
        &store,
        &[HostCapability::PushRef, HostCapability::ChangeRequestCreate],
    );
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    // A finished writer whose required artifact is now reported, and the
    // execution waiting on reconciliation.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-1".into()),
                    request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::Reconciliation);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    let artifact_obs = |id: &str| Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "test".into(),
            event_id: id.into(),
        }),
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::Artifact {
            invocation_id: InvocationId("inv-1".into()),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present {
                digest: "artifact-digest".into(),
            },
        },
        observed_unix_seconds: NOW,
    };
    // First report queues the push intent; the outbox applies it and its
    // applied receipt records the open intent, which the same drain
    // applies — the receipt alone advances the chain.
    gh.emit(&store, &config(), artifact_obs("art-1"), NOW)
        .unwrap();
    assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 1);
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(
        gh.applied().len(),
        2,
        "the applied push receipt recorded and drained the open intent"
    );
    // A repeat of the same fact under a new delivery id is inert: no
    // second push and no second open.
    gh.emit(&store, &config(), artifact_obs("art-2"), NOW)
        .unwrap();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    let pushes = gh
        .applied()
        .iter()
        .filter(|a| a.starts_with("push_ref:"))
        .count();
    assert_eq!(pushes, 1, "no second publish intent");
    let opens = gh
        .applied()
        .iter()
        .filter(|a| a.starts_with("open_change:"))
        .count();
    assert_eq!(opens, 1);
    assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 2);
}

#[test]
fn terminal_execution_still_authorizes_outstanding_effect_reconciliation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "terminal-outbox");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/terminal".into(),
            produced: rev("head-t"),
            expected_prior: None,
        },
    );
    store
        .transact(|tx| {
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.state = ExecutionState::Cancelled;
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-t"]);
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Cancelled
    );
}

#[test]
fn execution_denial_is_not_overridden_by_installation_permissions() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "execution-denial");
    let actions = [
        HostAction::PushRef {
            branch: "work/restricted".into(),
            produced: rev("head-r"),
            expected_prior: None,
        },
        HostAction::UpdateChangeRequest {
            change: change(),
            expected_head: rev("head"),
            title: Some("edited".into()),
            body: None,
        },
        HostAction::PostComment {
            change: change(),
            expected_head: rev("head"),
            marker: "workloop".into(),
            body: "comment".into(),
        },
    ];
    let ids: Vec<_> = actions
        .into_iter()
        .map(|action| insert_intent(&store, &work_id, action))
        .collect();
    store
        .transact(|tx| {
            let mut execution = tx.execution(&eid)?.unwrap();
            execution.allowed_effects.host_write = false;
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    gh.set_head("cr-1", "head");
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert!(gh.applied().is_empty());
    for id in &ids {
        assert_eq!(intent_status(&store, id), HostActionStatus::Pending);
    }

    let (allowed_work, _) = submit(&store, "execution-allowed");
    let allowed_id = insert_intent(
        &store,
        &allowed_work,
        HostAction::PushRef {
            branch: "work/allowed".into(),
            produced: rev("head-a"),
            expected_prior: None,
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-a"]);
    assert_eq!(
        intent_status(&store, &allowed_id),
        HostActionStatus::Applied
    );
}

#[test]
fn host_write_denied_leaves_the_outbox_pending() {
    let store = SqliteStore::in_memory(1).unwrap();
    // The Work was admitted under a permissive installation, so the
    // recorded execution grant allows the write — but the
    // installation-level grant is the outermost control: a revoked
    // `host_write` stops the drain even for a granted execution.
    let (work_id, _eid) = submit(&store, "k-g8");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w-1".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    let mut read_only = config();
    read_only.allowed_effects.host_write = false;
    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &read_only, NOW).unwrap();
    assert!(
        gh.applied().is_empty(),
        "no host mutation while host_write is denied"
    );
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Pending);
    // The same recorded intent applies once the installation grants the
    // write — denial defers, it never reclassifies.
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-1"]);
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
}

/// A receipt that claims the change opened into a different target
/// branch than the immutable intent authorized does not satisfy the
/// effect match — applied evidence binds the whole payload.
#[test]
fn open_receipt_for_a_different_target_branch_is_unknown() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-wrong-target");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/w-1".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    let receipt = handle_observation(
        &store,
        &config(),
        &obs(
            &work_id,
            &eid,
            Some("open-target-mismatch"),
            NOW,
            ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: None,
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::ChangeOpened {
                    change: change(),
                    head: rev("head-1"),
                    source_branch: "work/w-1".into(),
                    target_branch: "develop".into(),
                }],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(receipt, IngestReceipt::Recorded);
    assert!(
        matches!(
            intent_status(&store, &action_id),
            HostActionStatus::Unknown { .. }
        ),
        "a different target branch is not the recorded effect"
    );
}

#[test]
fn superseded_check_evidence_does_not_complete_the_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = waiting_with_applied_publish(&store, "k-g7");
    let gh = FixtureGithub::new();
    let emit = |fact: ObservationFact| {
        gh.emit(&store, &config(), obs(&work_id, &eid, None, NOW, fact), NOW)
            .unwrap();
    };
    emit(ObservationFact::HostActionResult {
        action_id: "act-pub-open".into(),
        authenticated_actor: Some(publisher()),
        status: HostActionStatus::Applied,
        effects: vec![NormalizedEffectFact::ChangeOpened {
            change: change(),
            head: rev("sha-1"),
            source_branch: format!("work/{}", work_id.0),
            target_branch: "main".into(),
        }],
    });
    emit(ObservationFact::ChangeHead {
        change: change(),
        head: rev("sha-1"),
    });
    // A superseded success must not complete the work: only the latest
    // check recorded for the current head decides.
    emit(ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Success,
    });
    emit(ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Failure,
    });
    emit(ObservationFact::Review {
        target: target("sha-1"),
        source: "reviewer".into(),
        source_revision: None,
        completeness: Completeness::Complete,
        occurrences: vec![],
    });
    emit(ObservationFact::Assessment {
        target: target("sha-1"),
        set_digest: "invariants".into(),
        complete: true,
        items: vec![AssessmentItem {
            id: "inv-1".into(),
            verdict: AssessmentVerdict::Positive,
            refs: vec![],
        }],
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Waiting,
        "the latest check is a failure; superseded success cannot complete"
    );
    // A fresh pass over the same head completes once the latest check is
    // green again.
    emit(ObservationFact::Check {
        change: change(),
        head: rev("sha-1"),
        check_id: "ci".into(),
        producer: publisher(),
        conclusion: CheckConclusion::Success,
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}

#[test]
fn delayed_check_success_cannot_complete_the_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = waiting_with_applied_publish(&store, "k-g11");
    let gh = FixtureGithub::new();
    let emit = |event: &str, at: i64, fact: ObservationFact| {
        gh.emit(
            &store,
            &config(),
            obs(&work_id, &eid, Some(event), at, fact),
            at,
        )
        .unwrap()
    };
    emit(
        "opened",
        NOW,
        ObservationFact::HostActionResult {
            action_id: "act-pub-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev("sha-1"),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
    );
    emit(
        "head-1",
        NOW,
        ObservationFact::ChangeHead {
            change: change(),
            head: rev("sha-1"),
        },
    );
    // The current verdict for the head: a failing check observed at t+20.
    emit(
        "checks-fail",
        NOW + 20,
        ObservationFact::Check {
            change: change(),
            head: rev("sha-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Failure,
        },
    );
    emit(
        "review-ok",
        NOW + 20,
        ObservationFact::Review {
            target: target("sha-1"),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        },
    );
    emit(
        "assess-ok",
        NOW + 20,
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
    // A delayed delivery of an older green arrives after it and is still
    // recorded, but its earlier observation time must not outrank the
    // newer failure (docs/module-contracts.md: delayed webhooks stay
    // evidence for their own revision).
    emit(
        "checks-late-ok",
        NOW + 10,
        ObservationFact::Check {
            change: change(),
            head: rev("sha-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Waiting,
        "a delayed green observed earlier cannot complete over the newer failure"
    );
    // The delayed delivery is still on record — its rank in the
    // completion decision changes, never its persistence as evidence.
    let recorded = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    assert!(
        recorded.iter().any(|o| {
            o.observed_unix_seconds == NOW + 10
                && matches!(
                    &o.fact,
                    ObservationFact::Check {
                        conclusion: CheckConclusion::Success,
                        ..
                    }
                )
        }),
        "the delayed green remains recorded as evidence"
    );
    // A genuinely newer green check still completes the work.
    emit(
        "checks-ok-new",
        NOW + 30,
        ObservationFact::Check {
            change: change(),
            head: rev("sha-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}

#[test]
fn delayed_review_on_a_superseded_target_does_not_complete_the_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = waiting_with_applied_publish(&store, "k-g12");
    let gh = FixtureGithub::new();
    let emit = |event: &str, at: i64, fact: ObservationFact| {
        gh.emit(
            &store,
            &config(),
            obs(&work_id, &eid, Some(event), at, fact),
            at,
        )
        .unwrap()
    };
    emit(
        "opened",
        NOW,
        ObservationFact::HostActionResult {
            action_id: "act-pub-open".into(),
            authenticated_actor: Some(publisher()),
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev("sha-1"),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
    );
    emit(
        "head-1",
        NOW,
        ObservationFact::ChangeHead {
            change: change(),
            head: rev("sha-1"),
        },
    );
    emit(
        "checks-ok",
        NOW + 20,
        ObservationFact::Check {
            change: change(),
            head: rev("sha-1"),
            check_id: "ci".into(),
            producer: publisher(),
            conclusion: CheckConclusion::Success,
        },
    );
    emit(
        "assess-ok",
        NOW + 20,
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
    // A delayed review over a superseded head arrives last: recorded as
    // evidence and sealed as its own round, but a round bound to a stale
    // target never satisfies the current-target review lane.
    emit(
        "review-late",
        NOW + 10,
        ObservationFact::Review {
            target: target("sha-0"),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        },
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Waiting,
        "a round sealed over a superseded target cannot complete the current one"
    );
    // A review over the current head seals a matching round and completes.
    emit(
        "review-current",
        NOW + 30,
        ObservationFact::Review {
            target: target("sha-1"),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: vec![],
        },
    );
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}

#[test]
fn cancel_defers_terminal_settlement_until_the_outbox_reconciles() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g9");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w-1".into(),
            produced: rev("head-9"),
            expected_prior: None,
        },
    );
    store
        .transact(|tx| {
            let request_id = tx
                .pending_requests()?
                .into_iter()
                .next()
                .expect("one durable request")
                .request_id;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request_id.clone(),
            })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-g9".into()),
                    request_id,
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::HostWrite);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    // Cancel while the publish intent is still an outstanding effect.
    let version = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .unwrap()
        .version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "k-g9-cancel".into(),
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
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Stopping
    );
    // Writer stop is proven (the Invocation is Done): the exit fact
    // reaches settle, which must defer — the pending publish would be
    // drained by the outbox after a recorded cancellation otherwise.
    let gh = FixtureGithub::new();
    gh.emit(
        &store,
        &config(),
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::ProcessState {
                invocation_id: InvocationId("inv-g9".into()),
                process_identity: String::new(),
                state: ProcessStateFact::Exited,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(
        store
            .transact(|tx| tx.execution(&eid))
            .unwrap()
            .unwrap()
            .state,
        ExecutionState::Stopping,
        "terminal settlement defers while the publish intent is outstanding"
    );
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Pending);
    // The outbox drains the intent: the ref is published, its result
    // reconciles the row, and the deferred cancellation lands only
    // after that reconciliation.
    let mut gh = gh;
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-9"]);
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Cancelled,
        "reconciled outbox result re-drives the deferred terminal settle"
    );
    let (slots, reservations) = store
        .transact(|tx| Ok((tx.slot_holders()?.len(), tx.reservations()?.len())))
        .unwrap();
    assert_eq!((slots, reservations), (0, 0), "terminal release");
}

#[test]
fn budget_failure_waits_for_the_outstanding_outbox_effect() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g10");
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w-1".into(),
            produced: rev("head-10"),
            expected_prior: None,
        },
    );
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.invocations_used = e.budget.max_invocations;
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();
    // The budget is exhausted, but the work still has an outstanding
    // outbox effect: the terminal failure defers to reconciliation
    // (lifecycle.md) instead of recording failure under a pending write
    // the drain would still execute.
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Queued,
        "terminal failure defers while the publish intent is outstanding"
    );
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Pending);
    // Once the outbox reconciles the intent, the next admission pass
    // records the exhausted budget.
    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(intent_status(&store, &action_id), HostActionStatus::Applied);
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Failed,
        "reconciled effect lets the deferred budget failure land"
    );
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("invocation_budget_exhausted")
    );
}

/// U5.4 partial-effect durability: the publication lane records the push
/// receipt durably before the change-request effect exists. A crash
/// between the two effects is a process restart, not a rollback — the
/// push receipt survives and the missing create is never inferred; the
/// next publish pass after reopen records it from the durable receipt.
#[test]
fn crash_after_publication_keeps_the_push_receipt_durable() {
    let root =
        std::env::temp_dir().join(format!("workloop-u54-push-receipt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("store.sqlite");
    let mut gh = FixtureGithub::new();
    let (work_id, eid, push_id) = {
        let store = SqliteStore::open(&path, 1).unwrap();
        let (work_id, eid) = submit(&store, "k-u54-crash");
        // Only the push capability is confirmed: the publish chain can
        // record `push_ref`, but the `open_change_request` capability is
        // still unreported, so the create intent cannot exist yet — the
        // crash boundary this test exercises.
        authorize(&store, &[HostCapability::PushRef]);
        store
            .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
            .unwrap();
        store
            .transact(|tx| {
                let mut e = tx.execution(&eid)?.unwrap();
                e.state = ExecutionState::Waiting;
                e.waiting_reason = Some(WaitingReason::Reconciliation);
                record_finished_coding(tx, &eid)?;
                tx.apply(&Effect::UpdateExecution {
                    execution: e,
                    expected_state: ExecutionState::Ready,
                })
            })
            .unwrap();
        // The artifact report drives `maybe_publish`: the push intent is
        // recorded, and the outbox commits its receipt — all inside the
        // admitted execution's publication window.
        gh.emit(
            &store,
            &config(),
            artifact_report(&work_id, &eid, "art-u54-1"),
            NOW,
        )
        .unwrap();
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        assert_eq!(intents.len(), 1, "the publish chain queued the push");
        assert!(matches!(intents[0].action, HostAction::PushRef { .. }));
        let push_id = intents[0].action_id.clone();
        gh.apply_outbox(&store, &config(), NOW).unwrap();
        assert_eq!(intent_status(&store, &push_id), HostActionStatus::Applied);
        let intents = store.transact(|tx| tx.action_intents()).unwrap();
        assert_eq!(
            intents.len(),
            1,
            "the unconfirmed create capability left no second intent"
        );
        // Crash before the change-request effect exists: dropping the
        // store is the process boundary, and no create was ever recorded.
        (work_id, eid, push_id)
    };

    let store = SqliteStore::open(&path, 1).unwrap();
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let push = intents
        .iter()
        .find(|intent| intent.action_id == push_id)
        .expect("the applied push intent survives the crash");
    assert_eq!(push.work_id, work_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert!(
        matches!(
            push.effects.first(),
            Some(NormalizedEffectFact::RefPublished {
                produced,
                relation: RefRelation::Equal,
                ..
            }) if produced.object == "artifact-digest"
        ),
        "the push receipt retains its normalized effect fact: {:?}",
        push.effects
    );
    assert!(
        !intents
            .iter()
            .any(|intent| matches!(intent.action, HostAction::OpenChangeRequest { .. })),
        "the missing create is absent evidence, never an inferred success"
    );

    // The restart is not a rollback: once the change-request capability
    // is confirmed, the next publish pass records the create intent from
    // the durable push receipt and the outbox applies it.
    authorize(
        &store,
        &[HostCapability::PushRef, HostCapability::ChangeRequestCreate],
    );
    gh.emit(
        &store,
        &config(),
        artifact_report(&work_id, &eid, "art-u54-2"),
        NOW,
    )
    .unwrap();
    let open = store
        .transact(|tx| tx.action_intents())
        .unwrap()
        .into_iter()
        .find(|intent| matches!(intent.action, HostAction::OpenChangeRequest { .. }))
        .expect("the publish chain records the create once its capability is confirmed");
    assert_eq!(open.status, HostActionStatus::Pending);
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(
        intent_status(&store, &open.action_id),
        HostActionStatus::Applied
    );
    assert!(
        gh.applied().iter().any(|a| a.starts_with("open_change:")),
        "the resumed chain applied the recorded create: {:?}",
        gh.applied()
    );
    let _ = std::fs::remove_dir_all(&root);
}
