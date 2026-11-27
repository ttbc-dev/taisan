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

fn config() -> WorkloopConfig {
    WorkloopConfig {
        placement: workloop_core::placement::PlacementPolicy::default(),
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: host(),
                    actor: HostActorId::trusted("pub-agent"),
                },
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
        required_checks: vec![],
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

/// The capability report authorizing `push_ref` on the bound host —
/// without it no stage-effect intent is ever recorded.
fn authorize(store: &SqliteStore, capability: HostCapability) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
                host: host(),
                capabilities: vec![CapabilityEntry {
                    capability,
                    support: CapabilityState::Supported,
                    authorization: CapabilityState::Authorized,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

#[test]
fn recovery_does_not_publish_from_an_earlier_invocations_artifacts() {
    let config = config();
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
    let earlier = InvocationId("earlier".into());
    let current = InvocationId("current".into());
    store
        .transact(|tx| {
            admit_queued(tx, &config, &execution_id, NOW)?;
            for (id, state, required_artifacts) in [
                (
                    earlier.clone(),
                    InvocationState::Done,
                    vec![artifact("earlier.json")],
                ),
                (
                    current.clone(),
                    InvocationState::Running,
                    vec![artifact("first.json"), artifact("second.json")],
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
                        descendants: vec![],
                    },
                })?;
            }
            tx.apply(&Effect::InsertObservation {
                observation: Observation {
                    delivery: None,
                    subject_work_id: Some(work.clone()),
                    subject_execution_id: Some(execution_id.clone()),
                    fact: ObservationFact::Artifact {
                        invocation_id: earlier,
                        required_artifact: "earlier.json".into(),
                        state: ArtifactFact::Present {
                            digest: "earlier-digest".into(),
                        },
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
    // The stage effect requires an authorized push_ref capability.
    authorize(&store, HostCapability::PushRef);
    let emit = |fact| {
        handle_observation(
            &store,
            &config,
            &Observation {
                delivery: None,
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
        state: ArtifactFact::Present {
            digest: "first-digest".into(),
        },
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
        exit_code: Some(0),
    });
    assert_eq!(
        store.transact(|tx| tx.action_intents()).unwrap().len(),
        0,
        "the current invocation still lacks its second required artifact after exit"
    );
    // The invariant lane's own durable record: publication waits for a
    // completed check Invocation recorded after the producing one.
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    request_id: format!("{}:inv:check", execution_id.0),
                    id: InvocationId("current-check".into()),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::InvariantCheck,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
    // The check's own proven `Exited{0}` evidence — `Done` alone is
    // resolved-but-unproven and never opens the publish gate.
    emit(ObservationFact::ProcessState {
        invocation_id: InvocationId("current-check".into()),
        process_identity: String::new(),
        state: ProcessStateFact::Exited,
        exit_code: Some(0),
    });
    emit(ObservationFact::Artifact {
        invocation_id: current,
        required_artifact: "second.json".into(),
        state: ArtifactFact::Present {
            digest: "second-digest".into(),
        },
    });
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    assert_eq!(intents.len(), 1);
    assert!(matches!(intents[0].action, HostAction::PushRef { .. }));
}
