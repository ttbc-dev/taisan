use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
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

fn applied_intent(
    action_id: &str,
    work_id: &WorkId,
    action: HostAction,
    seq: i64,
) -> HostActionIntent {
    HostActionIntent {
        action_id: action_id.into(),
        work_id: work_id.clone(),
        host: host(),
        repository: repo(),
        role: ActorRole::Publication,
        actor: publisher(),
        action,
        payload_digest: format!("digest-{action_id}"),
        status: HostActionStatus::Applied,
        authenticated_actor: None,
        effects: vec![],
        created_seq: seq,
    }
}

fn setup(state: InvocationState) -> (SqliteStore, WorkId, ExecutionId, InvocationId) {
    let store = SqliteStore::in_memory(1).unwrap();
    let decision = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "operator".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "submit".into(),
            body: CommandBody::Submit {
                intent: "change".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        work_id: Some(work),
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = decision
    else {
        panic!("submit must succeed")
    };
    let invocation_id = InvocationId("coding".into());
    store
        .transact(|tx| {
            admit_queued(tx, &config(), &execution_id, NOW)?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: invocation_id.clone(),
                    request_id: format!("{}:inv:0", execution_id.0),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state,
                    process_identity: None,
                    required_artifacts: vec![artifact("artifact.json")],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            // The invariant lane's own durable Invocation: a completed
            // read-only check recorded after the producing Invocation.
            // Publication and completion gate on it, never on an
            // assessment fact alone — and on its proven exit status,
            // so the check's own `Exited{0}` evidence is recorded too.
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("invariant-check".into()),
                    request_id: format!("{}:inv:1", execution_id.0),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::InvariantCheck,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: None,
                    subject_execution_id: Some(execution_id.clone()),
                    fact: ObservationFact::ProcessState {
                        invocation_id: InvocationId("invariant-check".into()),
                        process_identity: String::new(),
                        state: ProcessStateFact::Exited,
                        exit_code: Some(0),
                    },
                    observed_unix_seconds: NOW,
                },
            })?;
            let mut execution = tx.execution(&execution_id)?.unwrap();
            execution.state = ExecutionState::Waiting;
            execution.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Ready,
            })?;
            Ok(())
        })
        .unwrap();
    (store, work, execution_id, invocation_id)
}

fn emit(store: &SqliteStore, work: &WorkId, execution: &ExecutionId, fact: ObservationFact) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work.clone()),
            subject_execution_id: Some(execution.clone()),
            fact,
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

fn state(store: &SqliteStore, execution: &ExecutionId) -> ExecutionState {
    store
        .transact(|tx| Ok(tx.execution(execution)?.unwrap().state))
        .unwrap()
}

/// Completion waits on current-target evidence in every lane: a landed
/// push_ref and open_change_request receipt, the artifact digests, the
/// declared check on the current head, a sealed review round, and a
/// positive invariant assessment. The gate is order-independent.
#[test]
fn completion_is_independent_of_delivery_order() {
    for a in 0..6 {
        for b in 0..6 {
            for c in 0..6 {
                for d in 0..6 {
                    for e in 0..6 {
                        let mut order = [a, b, c, d, e, 0];
                        if order[..5]
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            != 5
                        {
                            continue;
                        }
                        let f = (0..6).find(|i| !order[..5].contains(i)).unwrap();
                        order[5] = f;
                        let (store, work, execution, invocation) = setup(InvocationState::Running);
                        let produced = rev("artifact-digest");
                        store
                            .transact(|tx| {
                                tx.apply(&Effect::InsertObservation {
                                    observation: Observation {
                                        delivery: None,
                                        subject_work_id: Some(work.clone()),
                                        subject_execution_id: Some(execution.clone()),
                                        fact: ObservationFact::Artifact {
                                            invocation_id: invocation.clone(),
                                            required_artifact: "artifact.json".into(),
                                            state: ArtifactFact::Present {
                                                digest: "artifact-digest".into(),
                                            },
                                        },
                                        observed_unix_seconds: NOW,
                                    },
                                })?;
                                tx.apply(&Effect::InsertHostActionIntent {
                                    intent: applied_intent(
                                        "act-push",
                                        &work,
                                        HostAction::PushRef {
                                            branch: format!("work/{}", work.0),
                                            produced: produced.clone(),
                                            expected_prior: None,
                                        },
                                        0,
                                    ),
                                })?;
                                tx.apply(&Effect::InsertHostActionIntent {
                                    intent: applied_intent(
                                        "act-open",
                                        &work,
                                        HostAction::OpenChangeRequest {
                                            source_branch: format!("work/{}", work.0),
                                            target_branch: "main".into(),
                                            title: "change".into(),
                                            body: String::new(),
                                        },
                                        0,
                                    ),
                                })
                            })
                            .unwrap();
                        let facts = [
                            ObservationFact::HostActionResult {
                                action_id: "act-open".into(),
                                authenticated_actor: Some(publisher()),
                                status: HostActionStatus::Applied,
                                effects: vec![NormalizedEffectFact::ChangeOpened {
                                    change: change(),
                                    head: rev("head"),
                                    source_branch: format!("work/{}", work.0),
                                    target_branch: "main".into(),
                                }],
                            },
                            ObservationFact::ChangeHead {
                                change: change(),
                                head: rev("head"),
                            },
                            ObservationFact::Check {
                                change: change(),
                                head: rev("head"),
                                check_id: "ci".into(),
                                producer: publisher(),
                                conclusion: CheckConclusion::Success,
                            },
                            ObservationFact::Review {
                                target: target("head"),
                                source: "reviewer".into(),
                                source_revision: None,
                                completeness: Completeness::Complete,
                                occurrences: vec![],
                            },
                            ObservationFact::Assessment {
                                target: target("head"),
                                set_digest: "invariants".into(),
                                complete: true,
                                items: vec![AssessmentItem {
                                    id: "inv-1".into(),
                                    verdict: AssessmentVerdict::Positive,
                                    refs: vec![],
                                }],
                            },
                            ObservationFact::ProcessState {
                                invocation_id: invocation.clone(),
                                process_identity: "identity".into(),
                                state: ProcessStateFact::Exited,
                                exit_code: Some(0),
                            },
                        ];
                        for (index, position) in order.iter().enumerate() {
                            emit(&store, &work, &execution, facts[*position].clone());
                            assert_eq!(
                                state(&store, &execution),
                                if index == 5 {
                                    ExecutionState::Succeeded
                                } else {
                                    ExecutionState::Waiting
                                },
                                "order={order:?}, index={index}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn exit_publishes_artifacts_recorded_before_the_exit() {
    let (store, work, execution, invocation) = setup(InvocationState::Running);
    store
        .transact(|tx| {
            let mut row = tx.execution(&execution)?.unwrap();
            row.state = ExecutionState::Running;
            row.waiting_reason = None;
            tx.apply(&Effect::UpdateExecution {
                execution: row,
                expected_state: ExecutionState::Waiting,
            })?;
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work.clone()),
                    subject_execution_id: Some(execution.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: invocation.clone(),
                        required_artifact: "artifact.json".into(),
                        state: ArtifactFact::Present {
                            digest: "artifact-digest".into(),
                        },
                    },
                    observed_unix_seconds: NOW,
                },
            })
        })
        .unwrap();
    // The stage effect requires an authorized push_ref capability.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
                host: host(),
                capabilities: vec![CapabilityEntry {
                    capability: HostCapability::PushRef,
                    support: CapabilityState::Supported,
                    authorization: CapabilityState::Authorized,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    emit(
        &store,
        &work,
        &execution,
        ObservationFact::ProcessState {
            invocation_id: invocation,
            process_identity: "identity".into(),
            state: ProcessStateFact::Exited,
            exit_code: Some(0),
        },
    );
    assert_eq!(state(&store, &execution), ExecutionState::Waiting);
    assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 1);
}

#[test]
fn stop_settlement_preserves_a_never_started_invocation() {
    let (store, work, execution, invocation) = setup(InvocationState::Abandoned);
    store
        .transact(|tx| {
            let mut row = tx.execution(&execution)?.unwrap();
            row.state = ExecutionState::Stopping;
            row.desired_action = Some(DesiredAction::Cancel);
            tx.apply(&Effect::UpdateExecution {
                execution: row,
                expected_state: ExecutionState::Waiting,
            })
        })
        .unwrap();
    emit(
        &store,
        &work,
        &execution,
        ObservationFact::ProcessState {
            invocation_id: invocation,
            process_identity: String::new(),
            state: ProcessStateFact::Exited,
            exit_code: Some(0),
        },
    );
    assert_eq!(state(&store, &execution), ExecutionState::Cancelled);
    assert_eq!(
        store
            .transact(|tx| Ok(tx.invocations_for_execution(&execution)?[0].state.clone()))
            .unwrap(),
        InvocationState::Abandoned
    );
}
