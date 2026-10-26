use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{WorkloopConfig, admit_queued, handle_command, handle_observation};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

#[test]
fn recovery_does_not_publish_from_an_earlier_invocations_artifacts() {
    let config = WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
    };
    let store = SqliteStore::in_memory(1).unwrap();
    let decision = handle_command(
        &store,
        &config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "operator".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: "submit".into(),
            body: CommandBody::Submit {
                intent: "continue the change".into(),
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
    let earlier = InvocationId("earlier".into());
    let current = InvocationId("current".into());
    store
        .transact(|tx| {
            admit_queued(tx, &config, &execution_id, NOW)?;
            for (id, state, required_artifacts) in [
                (
                    earlier.clone(),
                    InvocationState::Done,
                    vec!["earlier.json".into()],
                ),
                (
                    current.clone(),
                    InvocationState::Running,
                    vec!["first.json".into(), "second.json".into()],
                ),
            ] {
                tx.apply(&Effect::InsertInvocation {
                    invocation: Invocation {
                        request_id: format!("{}:{}", execution_id.0, id.0),
                        id,
                        execution_id: execution_id.clone(),
                        role: InvocationRole::Coding,
                        state,
                        process_identity: None,
                        required_artifacts,
                        evidence_refs: vec![],
                    },
                })?;
            }
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    source_event_id: None,
                    subject_work_id: Some(work.clone()),
                    subject_execution_id: Some(execution_id.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: earlier,
                        required_artifact: "earlier.json".into(),
                        state: ArtifactFact::Present,
                    },
                    observed_unix_seconds: NOW,
                },
            })?;
            let mut execution = tx.execution(&execution_id)?.unwrap();
            execution.state = ExecutionState::Running;
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let emit = |fact| {
        handle_observation(
            &store,
            &config,
            &Observation {
                source_event_id: None,
                subject_work_id: Some(work.clone()),
                subject_execution_id: Some(execution_id.clone()),
                fact,
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    };
    emit(ObservationFact::Artifact {
        invocation_id: current.clone(),
        required_artifact: "first.json".into(),
        state: ArtifactFact::Missing,
    });
    emit(ObservationFact::Artifact {
        invocation_id: current.clone(),
        required_artifact: "first.json".into(),
        state: ArtifactFact::Present,
    });
    assert_eq!(
        store.transact(|tx| tx.action_intents()).unwrap().len(),
        0,
        "an earlier completed invocation cannot authorize publication while the current writer is unresolved"
    );
    emit(ObservationFact::ProcessState {
        invocation_id: current.clone(),
        process_identity: "identity".into(),
        state: ProcessStateFact::Exited,
    });
    assert_eq!(
        store.transact(|tx| tx.action_intents()).unwrap().len(),
        0,
        "the current invocation still lacks its second required artifact after exit"
    );
    emit(ObservationFact::Artifact {
        invocation_id: current,
        required_artifact: "second.json".into(),
        state: ArtifactFact::Present,
    });
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    assert_eq!(intents.len(), 1);
    assert!(matches!(intents[0].action, PrAction::PublishPr { .. }));
}
