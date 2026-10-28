use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{WorkloopConfig, admit_queued, handle_command, handle_observation};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn config() -> WorkloopConfig {
    WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
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
                issue_refs: vec![],
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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

fn emit(
    store: &SqliteStore,
    work: &WorkId,
    execution: &ExecutionId,
    fact: ObservationFact,
    execution_only: bool,
) {
    handle_observation(
        store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: (!execution_only).then(|| work.clone()),
            subject_execution_id: execution_only.then(|| execution.clone()),
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

#[test]
fn completion_is_independent_of_delivery_order_and_subject_scope() {
    for execution_only in [false, true] {
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    if a == b || a == c || b == c {
                        continue;
                    }
                    let d = (0..4).find(|i| *i != a && *i != b && *i != c).unwrap();
                    let order = [a, b, c, d];
                    let (store, work, execution, invocation) = setup(InvocationState::Done);
                    store
                        .transact(|tx| {
                            tx.apply(&Effect::InsertObservation {
                                observation: Observation {
                                    source_event_id: None,
                                    subject_work_id: Some(work.clone()),
                                    subject_execution_id: Some(execution.clone()),
                                    fact: ObservationFact::Artifact {
                                        invocation_id: invocation,
                                        required_artifact: "artifact.json".into(),
                                        state: ArtifactFact::Present,
                                    },
                                    observed_unix_seconds: NOW,
                                },
                            })?;
                            tx.apply(&Effect::InsertPrActionIntent {
                                intent: PrActionIntent {
                                    action_id: "publish".into(),
                                    work_id: work.clone(),
                                    repository: "local/repo".into(),
                                    action: PrAction::PublishPr {
                                        branch: "work/change".into(),
                                        title: "change".into(),
                                        body: String::new(),
                                    },
                                    status: PrActionStatus::Pending,
                                },
                            })
                        })
                        .unwrap();
                    let facts = [
                        ObservationFact::PrHead {
                            repository: "local/repo".into(),
                            number: 1,
                            head_sha: "head".into(),
                        },
                        ObservationFact::PrChecks {
                            repository: "local/repo".into(),
                            head_sha: "head".into(),
                            conclusion: CheckConclusion::Success,
                        },
                        ObservationFact::PrReview {
                            repository: "local/repo".into(),
                            head_sha: "head".into(),
                            reviewer: "reviewer".into(),
                            verdict: ReviewVerdict::Approve,
                            findings: vec![],
                        },
                        ObservationFact::PrActionResult {
                            action_id: "publish".into(),
                            action_kind: "publish_pr".into(),
                            result: ActionResult::Applied,
                        },
                    ];
                    for (index, position) in order.iter().enumerate() {
                        emit(
                            &store,
                            &work,
                            &execution,
                            facts[*position].clone(),
                            execution_only,
                        );
                        assert_eq!(
                            state(&store, &execution),
                            if index == 3 {
                                ExecutionState::Succeeded
                            } else {
                                ExecutionState::Waiting
                            },
                            "order={order:?}, index={index}, execution_only={execution_only}"
                        );
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
                    source_event_id: None,
                    subject_work_id: Some(work.clone()),
                    subject_execution_id: Some(execution.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: invocation.clone(),
                        required_artifact: "artifact.json".into(),
                        state: ArtifactFact::Present,
                    },
                    observed_unix_seconds: NOW,
                },
            })
        })
        .unwrap();
    emit(
        &store,
        &work,
        &execution,
        ObservationFact::ProcessState {
            invocation_id: invocation,
            process_identity: "identity".into(),
            state: ProcessStateFact::Exited,
        },
        true,
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
        },
        true,
    );
    assert_eq!(state(&store, &execution), ExecutionState::Cancelled);
    assert_eq!(
        store
            .transact(|tx| Ok(tx.invocations_for_execution(&execution)?[0].state.clone()))
            .unwrap(),
        InvocationState::Abandoned
    );
}
