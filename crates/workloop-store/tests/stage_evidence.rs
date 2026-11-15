//! Stage evidence is durable per stage: every stage the loop enters
//! persists a record with `{stage, status, revision, facts}` and a
//! reopened store replays exactly the recorded history.

use std::collections::BTreeSet;

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, ExecutionId, LanePolicy, Origin, WorkId,
    WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef,
    EvidenceRef, HostActorId, HostActorRef, HostCapability, HostInstanceId, RepositoryId,
    RepositoryRef, RoleBinding,
};
use workloop_core::observation::{Observation, ObservationFact};
use workloop_core::stage::{
    HeadObservation, LANE_DISABLED_REASON, NextStage, Stage, StageError, StageEvidence,
    StagePipeline, StageStatus,
};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{
    HostTemplate, IngestReceipt, WorkloopConfig, handle_command, handle_observation,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn repo() -> RepositoryRef {
    RepositoryRef::new(
        HostInstanceId::trusted("stage-host"),
        RepositoryId::trusted("stage-repo"),
    )
}

fn head() -> CommitRevision {
    CommitRevision::new(repo(), "head-durable").unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
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
                actor: HostActorRef {
                    host: HostInstanceId::trusted("stage-host"),
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

fn submit_work(store: &SqliteStore, config: &WorkloopConfig, key: &str) -> (WorkId, ExecutionId) {
    let decision = handle_command(
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
    let CommandDecision::Accepted {
        work_id,
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = decision
    else {
        panic!("submit: {decision:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn fact(id: &str) -> EvidenceRef {
    EvidenceRef {
        id: id.into(),
        digest: format!("digest-{id}"),
    }
}

fn persist(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    evidence: StageEvidence,
) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertObservation {
                observation: evidence.into_observation(work_id.clone(), execution_id.clone(), NOW),
            })
        })
        .unwrap();
}

#[test]
fn each_entered_stage_persists_a_durable_evidence_record_across_a_reopen() {
    let root = std::env::temp_dir().join(format!("workloop-stage-evidence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("stage.sqlite");
    let work_id = WorkId("w-stage".into());
    let execution_id = ExecutionId("e-stage".into());
    let revision = head();

    {
        let store = SqliteStore::open(&path, 2).unwrap();
        let mut pipeline = StagePipeline::new(repo(), LanePolicy::default());
        assert_eq!(pipeline.observe_head(&revision), HeadObservation::Recorded);
        loop {
            match pipeline.next_stage() {
                NextStage::Open(stage) => {
                    let entered = StageEvidence::entered(stage, revision.clone());
                    pipeline.record(entered.clone()).unwrap();
                    persist(&store, &work_id, &execution_id, entered);

                    let complete =
                        StageEvidence::complete(stage, revision.clone(), vec![fact(stage.name())])
                            .unwrap();
                    pipeline.record(complete.clone()).unwrap();
                    persist(&store, &work_id, &execution_id, complete);
                }
                NextStage::Disabled { .. } => {
                    for stage in pipeline.materialize_disabled_lanes(&revision).unwrap() {
                        persist(
                            &store,
                            &work_id,
                            &execution_id,
                            StageEvidence::disabled(stage, revision.clone(), LANE_DISABLED_REASON),
                        );
                    }
                }
                NextStage::Blocked { stage, status } => {
                    panic!("unexpected blocked stage {stage:?}: {status:?}")
                }
                NextStage::Complete => break,
            }
        }
    }

    let store = SqliteStore::open(&path, 2).unwrap();
    let observations = store
        .transact(|tx| tx.observations_for_execution(&execution_id))
        .unwrap();
    let records: Vec<StageEvidence> = observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            ObservationFact::StageEvidence { evidence } => Some(evidence.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        records.len(),
        Stage::ORDER.len() * 2,
        "one entry and one completion record per stage survived the reopen"
    );

    for stage in Stage::ORDER {
        let entered = records
            .iter()
            .find(|record| record.stage == stage && record.status == StageStatus::Entered)
            .unwrap_or_else(|| panic!("{stage:?} has no durable entry record"));
        let value = serde_json::to_value(entered).unwrap();
        let mut keys: Vec<String> = value
            .as_object()
            .expect("stage evidence is an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["facts", "revision", "stage", "status"]);
        assert_eq!(entered.revision, revision);
        assert!(entered.facts.is_empty());

        let complete = records
            .iter()
            .find(|record| record.stage == stage && record.status.is_complete())
            .unwrap_or_else(|| panic!("{stage:?} has no durable completion record"));
        assert!(!complete.facts.is_empty());
        assert_eq!(complete.revision, revision);
    }

    let replayed = StagePipeline::from_observations(repo(), LanePolicy::default(), &observations);
    assert_eq!(replayed.records(), records.as_slice());
    assert_eq!(replayed.next_stage(), NextStage::Complete);

    let _ = std::fs::remove_dir_all(&root);
}

/// U5.4 lane binding: the pipeline is judged under the lane policy
/// frozen into the Work's `spec.payload.lanes` at request creation. A
/// policy that differs from the frozen value is refused, the same value
/// is accepted, and a later installation-config change does not alter
/// what an existing Work is judged under.
#[test]
fn pipeline_binds_to_the_frozen_spec_lanes_and_refuses_a_differing_policy() {
    let store = SqliteStore::in_memory(1).unwrap();
    // The frozen spec disables the invariant-check lane; the live
    // installation config will differ from it below.
    let mut creation = config();
    creation.lanes = LanePolicy {
        invariant_check: false,
        code_review: true,
    };
    let (work_id, _execution_id) = submit_work(&store, &creation, "k-frozen-lanes");
    let frozen = store
        .transact(|tx| {
            Ok(tx
                .spec(&work_id, None)?
                .expect("a submitted work has a spec snapshot")
                .payload
                .lanes)
        })
        .unwrap();
    assert_eq!(frozen, creation.lanes);

    // A policy that differs from the frozen value — for example a live
    // installation config that re-enabled the lane after the request —
    // is refused, never silently adopted.
    let drifted = LanePolicy {
        invariant_check: true,
        code_review: true,
    };
    assert_eq!(
        StagePipeline::bind(repo(), &frozen, &drifted).unwrap_err(),
        StageError::LanePolicyMismatch
    );
    assert_eq!(
        StagePipeline::bind_from_observations(repo(), &frozen, &drifted, &[]).unwrap_err(),
        StageError::LanePolicyMismatch
    );

    // The same policy as the frozen value is accepted, and the existing
    // Work is still judged under the frozen lanes: the disabled lane
    // owes its explicit `disabled{reason}` record even though the drifted
    // config would have enabled it.
    let revision = head();
    let mut pipeline = StagePipeline::bind(repo(), &frozen, &frozen).unwrap();
    pipeline.observe_head(&revision);
    for stage in [Stage::Code, Stage::Verify] {
        pipeline
            .record(StageEvidence::entered(stage, revision.clone()))
            .unwrap();
        pipeline
            .record(
                StageEvidence::complete(stage, revision.clone(), vec![fact(stage.name())]).unwrap(),
            )
            .unwrap();
    }
    assert_eq!(
        pipeline.next_stage(),
        NextStage::Disabled {
            stage: Stage::InvariantCheck,
            reason: LANE_DISABLED_REASON.into()
        },
        "the existing Work is judged under its frozen spec lanes"
    );
}

/// U5.4 write authority: stage records are Core-authored durable state.
/// A `StageEvidence` fact arriving through the ingest path — the only
/// path adapters, bridges, and external producers can reach — is
/// refused before it can write pipeline history.
#[test]
fn ingest_refuses_a_delivered_stage_record() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, execution_id) = submit_work(&store, &config(), "k-stage-authority");
    let delivered = StageEvidence::entered(Stage::Code, head()).into_observation(
        work_id.clone(),
        execution_id.clone(),
        NOW,
    );
    let receipt = handle_observation(&store, &config(), &delivered, NOW).unwrap();
    assert!(
        matches!(receipt, IngestReceipt::Rejected(_)),
        "a delivered stage record carries no write authority: {receipt:?}"
    );
    let persisted = store
        .transact(|tx| tx.observations_for_execution(&execution_id))
        .unwrap();
    assert!(
        persisted
            .iter()
            .all(|o| !matches!(o.fact, ObservationFact::StageEvidence { .. })),
        "the refused record never entered the durable log"
    );
}

/// U5.4 restart safety: `from_observations` rebuilds the recorded head
/// from durable observations, so a stale-head observation can never
/// promote itself to the recorded head after a restart — it stays
/// history. Stage records pinned to a foreign revision, or whose
/// record fails the stage guards, are refused on replay: they cannot
/// move the head or satisfy a lane either. A record pinned to a head
/// the pipeline adopted live — even one a `change_head` observation
/// marked stale, or one it held before — restores that authoritative
/// move and replays its lane.
#[test]
fn a_reopened_pipeline_restores_the_recorded_head_and_keeps_stale_heads_stale() {
    let root = std::env::temp_dir().join(format!("workloop-stage-heads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("heads.sqlite");
    let current = head();
    let stale = CommitRevision::new(repo(), "head-stale").unwrap();
    let adopted = CommitRevision::new(repo(), "head-adopted").unwrap();
    let execution_id = {
        let store = SqliteStore::open(&path, 2).unwrap();
        let (work_id, execution_id) = submit_work(&store, &config(), "k-heads");
        let emit = |fact: ObservationFact| {
            handle_observation(
                &store,
                &config(),
                &Observation {
                    delivery: None,
                    subject_work_id: Some(work_id.clone()),
                    subject_execution_id: Some(execution_id.clone()),
                    fact,
                    observed_unix_seconds: NOW,
                },
                NOW,
            )
            .unwrap();
        };
        emit(ObservationFact::ChangeHead {
            change: change(),
            head: current.clone(),
        });
        persist(
            &store,
            &work_id,
            &execution_id,
            StageEvidence::entered(Stage::Code, current.clone()),
        );
        persist(
            &store,
            &work_id,
            &execution_id,
            StageEvidence::complete(Stage::Code, current.clone(), vec![fact("code")]).unwrap(),
        );
        emit(ObservationFact::ChangeHead {
            change: change(),
            head: stale.clone(),
        });
        emit(ObservationFact::ChangeHead {
            change: change(),
            head: adopted.clone(),
        });
        // The verify records pin the adopted head: live, Core moved the
        // recorded head through `record_current_head` and the evidence
        // bound to it — the pin is the only durable trace of that move.
        persist(
            &store,
            &work_id,
            &execution_id,
            StageEvidence::entered(Stage::Verify, adopted.clone()),
        );
        persist(
            &store,
            &work_id,
            &execution_id,
            StageEvidence::complete(Stage::Verify, adopted.clone(), vec![fact("verify")]).unwrap(),
        );
        // A pin whose record fails the stage guards cannot move the
        // head: publication is still blocked behind invariant-check,
        // and a foreign-repository pin is never this Work's head.
        let foreign = CommitRevision::new(
            RepositoryRef::new(
                HostInstanceId::trusted("foreign-host"),
                RepositoryId::trusted("foreign-repo"),
            ),
            "head-durable",
        )
        .unwrap();
        for revision in [current.clone(), foreign] {
            persist(
                &store,
                &work_id,
                &execution_id,
                StageEvidence::complete(
                    Stage::Publication,
                    revision,
                    vec![fact("publication-off-head")],
                )
                .unwrap(),
            );
        }
        // `record_current_head` places no restriction on its target
        // inside the Work's repository, so a legal backward move
        // replays the same way.
        persist(
            &store,
            &work_id,
            &execution_id,
            StageEvidence::entered(Stage::InvariantCheck, current.clone()),
        );
        execution_id
    };

    let store = SqliteStore::open(&path, 2).unwrap();
    let observations = store
        .transact(|tx| tx.observations_for_execution(&execution_id))
        .unwrap();
    let mut pipeline =
        StagePipeline::from_observations(repo(), LanePolicy::default(), &observations);
    assert_eq!(
        pipeline.current_head(),
        Some(&current),
        "the recorded head follows the last accepted pin — a legal backward move"
    );
    assert_eq!(
        pipeline.stale_heads(),
        [stale.clone(), adopted.clone()].as_slice(),
        "the differing observations stay history even when one was later adopted"
    );
    assert!(pipeline.head_history().contains(&stale));
    assert!(
        pipeline
            .latest(Stage::Code)
            .is_some_and(|record| record.status.is_complete()),
        "recorded evidence replays under the restored head"
    );
    let verify = pipeline.latest(Stage::Verify).unwrap();
    assert!(
        verify.status.is_complete() && verify.revision == adopted,
        "evidence bound to an observed-then-adopted head survives the restart"
    );
    assert!(
        pipeline.latest(Stage::Publication).is_none(),
        "guard-failing and foreign stage records never satisfy a lane after restart"
    );
    let invariant = pipeline.latest(Stage::InvariantCheck).unwrap();
    assert_eq!(
        invariant.revision, current,
        "the accepted entry record binds the restored head"
    );
    assert_eq!(
        pipeline.next_stage(),
        NextStage::Blocked {
            stage: Stage::InvariantCheck,
            status: StageStatus::Entered,
        }
    );
    assert_eq!(
        pipeline
            .record(StageEvidence::entered(Stage::Verify, stale.clone()))
            .unwrap_err(),
        StageError::StaleRevision {
            stage: Stage::Verify
        },
        "a stale head cannot satisfy a stage after restart either"
    );

    let _ = std::fs::remove_dir_all(&root);
}
