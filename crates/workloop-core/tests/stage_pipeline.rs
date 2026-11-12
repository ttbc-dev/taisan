//! Stage-pipeline contract: lanes advance only from recorded `Complete`
//! evidence in canonical order, disabled lanes carry an explicit reason,
//! and stale-head observations remain history instead of satisfying a
//! transition.

use workloop_core::domain::LanePolicy;
use workloop_core::identity::{
    CommitRevision, EvidenceRef, HostInstanceId, RepositoryId, RepositoryRef,
};
use workloop_core::stage::{
    HeadObservation, LANE_DISABLED_REASON, NextStage, Stage, StageError, StageEvidence,
    StagePipeline, StageStatus,
};

fn repository() -> RepositoryRef {
    RepositoryRef::new(
        HostInstanceId::trusted("stage-host"),
        RepositoryId::trusted("stage-repo"),
    )
}

fn head(object: &str) -> CommitRevision {
    CommitRevision::new(repository(), object).unwrap()
}

fn fact(id: &str) -> EvidenceRef {
    EvidenceRef {
        id: id.into(),
        digest: format!("digest-{id}"),
    }
}

fn all_lanes() -> LanePolicy {
    LanePolicy {
        invariant_check: true,
        code_review: true,
    }
}

fn enter_and_complete(pipeline: &mut StagePipeline, stage: Stage, revision: &CommitRevision) {
    pipeline
        .record(StageEvidence::entered(stage, revision.clone()))
        .unwrap();
    pipeline
        .record(StageEvidence::complete(stage, revision.clone(), vec![fact(stage.name())]).unwrap())
        .unwrap();
}

/// Drive one full pass where every stage leaves an entry record and a
/// complete record, persisting nothing outside the pipeline value.
fn drive_pass(lanes: LanePolicy, revision: &CommitRevision) -> StagePipeline {
    let mut pipeline = StagePipeline::new(lanes);
    assert_eq!(pipeline.observe_head(revision), HeadObservation::Recorded);
    loop {
        match pipeline.next_stage() {
            NextStage::Open(stage) => enter_and_complete(&mut pipeline, stage, revision),
            NextStage::Disabled { .. } => {
                pipeline.materialize_disabled_lanes(revision).unwrap();
            }
            NextStage::Blocked { stage, status } => {
                panic!("a driven pass must not block on {stage:?}: {status:?}")
            }
            NextStage::Complete => break,
        }
    }
    pipeline
}

#[test]
fn review_lane_never_opens_while_verify_is_incomplete() {
    let revision = head("head-1");
    let mut pipeline = StagePipeline::new(all_lanes());
    pipeline.observe_head(&revision);
    enter_and_complete(&mut pipeline, Stage::Code, &revision);
    pipeline
        .record(StageEvidence::entered(Stage::Verify, revision.clone()))
        .unwrap();
    pipeline
        .record(StageEvidence::partial(
            Stage::Verify,
            revision.clone(),
            vec![fact("verify-attempt-1")],
            "mechanical verification receipt lost",
        ))
        .unwrap();

    assert!(
        !pipeline.lane_opens(Stage::Review),
        "review cannot open while an earlier enabled lane is incomplete"
    );
    let refused = pipeline
        .record(
            StageEvidence::complete(Stage::Review, revision.clone(), vec![fact("review-1")])
                .unwrap(),
        )
        .unwrap_err();
    assert_eq!(
        refused,
        StageError::LaneNotOpen {
            stage: Stage::Review,
            blocked_by: Stage::Verify,
        }
    );
    assert!(
        pipeline.latest(Stage::Review).is_none(),
        "the rejected review evidence never becomes a recorded stage"
    );
}

#[test]
fn every_entered_stage_persists_a_record_with_the_four_contract_fields() {
    let revision = head("head-2");
    let pipeline = drive_pass(all_lanes(), &revision);
    assert_eq!(pipeline.next_stage(), NextStage::Complete);

    for stage in Stage::ORDER {
        let entered = pipeline
            .records()
            .iter()
            .find(|record| record.stage == stage && record.status == StageStatus::Entered)
            .unwrap_or_else(|| panic!("{stage:?} entered without a durable record"));
        let value = serde_json::to_value(entered).unwrap();
        let mut keys: Vec<String> = value
            .as_object()
            .expect("stage evidence is an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["facts", "revision", "stage", "status"],
            "every entered stage carries the four fields"
        );
        assert_eq!(value["stage"], serde_json::to_value(stage).unwrap());
        assert_eq!(entered.revision, revision);
        assert!(
            entered.facts.is_empty(),
            "entry has no completion facts yet"
        );

        let complete = pipeline
            .records()
            .iter()
            .find(|record| record.stage == stage && record.status.is_complete())
            .unwrap_or_else(|| panic!("{stage:?} lost its completion record"));
        assert!(!complete.facts.is_empty());
        assert_eq!(complete.revision, revision);
    }
}

#[test]
fn a_missing_or_partial_verify_receipt_keeps_verify_incomplete_and_publication_closed() {
    let revision = head("head-3");
    let mut pipeline = StagePipeline::new(all_lanes());
    pipeline.observe_head(&revision);
    enter_and_complete(&mut pipeline, Stage::Code, &revision);

    // The verify receipt is lost: no record ever marks it complete.
    assert_eq!(pipeline.next_stage(), NextStage::Open(Stage::Verify));
    assert!(
        !pipeline
            .latest(Stage::Verify)
            .is_some_and(|record| record.status.is_complete()),
        "missing evidence is never inferred as completion"
    );
    assert_eq!(
        pipeline
            .record(StageEvidence::complete(
                Stage::Publication,
                revision.clone(),
                vec![fact("push-1")],
            )
            .unwrap())
            .unwrap_err(),
        StageError::LaneNotOpen {
            stage: Stage::Publication,
            blocked_by: Stage::Verify,
        }
    );
    assert!(
        pipeline.latest(Stage::Publication).is_none(),
        "publication never opens without the verify receipt"
    );

    // A partial receipt is retained but still cannot complete the stage.
    pipeline
        .record(StageEvidence::entered(Stage::Verify, revision.clone()))
        .unwrap();
    pipeline
        .record(StageEvidence::partial(
            Stage::Verify,
            revision.clone(),
            vec![fact("verify-partial")],
            "partial verification result",
        ))
        .unwrap();
    assert!(!pipeline.latest(Stage::Verify).unwrap().status.is_complete());
    assert_eq!(
        pipeline
            .record(StageEvidence::complete(
                Stage::Publication,
                revision.clone(),
                vec![fact("push-2")],
            )
            .unwrap())
            .unwrap_err(),
        StageError::LaneNotOpen {
            stage: Stage::Publication,
            blocked_by: Stage::Verify,
        }
    );
    assert_eq!(
        StageEvidence::complete(Stage::Verify, revision, vec![]),
        Err(StageError::MissingFacts {
            stage: Stage::Verify
        })
    );
}

#[test]
fn a_disabled_invariant_lane_records_its_reason_and_publication_proceeds() {
    let revision = head("head-4");
    let mut pipeline = StagePipeline::new(LanePolicy {
        invariant_check: false,
        code_review: true,
    });
    pipeline.observe_head(&revision);
    enter_and_complete(&mut pipeline, Stage::Code, &revision);
    enter_and_complete(&mut pipeline, Stage::Verify, &revision);
    assert!(
        !pipeline.lane_opens(Stage::InvariantCheck),
        "a disabled lane never opens; it owes an explicit disabled record"
    );
    assert_eq!(
        pipeline
            .record(
                StageEvidence::complete(
                    Stage::InvariantCheck,
                    revision.clone(),
                    vec![fact("invariant-1")],
                )
                .unwrap(),
            )
            .unwrap_err(),
        StageError::LaneDisabled {
            stage: Stage::InvariantCheck,
        },
        "a config-disabled lane cannot be completed as if it passed"
    );

    let recorded = pipeline.materialize_disabled_lanes(&revision).unwrap();
    assert_eq!(recorded, vec![Stage::InvariantCheck]);
    let disabled = pipeline.latest(Stage::InvariantCheck).unwrap();
    assert_eq!(
        disabled.status,
        StageStatus::Disabled {
            reason: LANE_DISABLED_REASON.into()
        }
    );
    assert!(
        !matches!(disabled.status, StageStatus::Complete),
        "a disabled lane never pretends to have passed"
    );
    assert_eq!(pipeline.next_stage(), NextStage::Open(Stage::Publication));
    enter_and_complete(&mut pipeline, Stage::Publication, &revision);
    assert!(
        pipeline
            .latest(Stage::Publication)
            .unwrap()
            .status
            .is_complete(),
        "publication proceeds on the remaining enabled evidence"
    );
}

#[test]
fn stale_head_observations_are_history_and_change_no_stage_state() {
    let newer = head("head-newer");
    let older = head("head-older");
    let mut pipeline = StagePipeline::new(all_lanes());
    assert_eq!(pipeline.observe_head(&newer), HeadObservation::Recorded);
    enter_and_complete(&mut pipeline, Stage::Code, &newer);

    let records_before = pipeline.records().to_vec();
    let next_before = pipeline.next_stage();
    assert_eq!(pipeline.observe_head(&older), HeadObservation::Stale);

    assert_eq!(
        pipeline.current_head(),
        Some(&newer),
        "an old-head observation never moves the recorded head"
    );
    assert_eq!(
        pipeline.records(),
        records_before.as_slice(),
        "a stale observation never creates or satisfies stage evidence"
    );
    assert_eq!(
        pipeline.next_stage(),
        next_before,
        "the stage state is unchanged by stale history"
    );
    assert_eq!(pipeline.stale_heads(), std::slice::from_ref(&older));
    assert!(
        pipeline.head_history().contains(&older),
        "the stale observation is retained as history"
    );
}

#[test]
fn a_completed_stage_cannot_be_reopened_and_a_later_lane_cannot_arrive_early() {
    let revision = head("head-5");
    let mut pipeline = StagePipeline::new(all_lanes());
    pipeline.observe_head(&revision);
    enter_and_complete(&mut pipeline, Stage::Code, &revision);
    assert_eq!(
        pipeline
            .record(StageEvidence::disabled(
                Stage::Verify,
                revision.clone(),
                "not a configured lane",
            ))
            .unwrap_err(),
        StageError::LaneEnabled {
            stage: Stage::Verify,
        },
        "an enabled lane cannot be recorded as disabled"
    );

    assert_eq!(
        pipeline
            .record(
                StageEvidence::complete(
                    Stage::Review,
                    revision.clone(),
                    vec![fact("review-early")],
                )
                .unwrap()
            )
            .unwrap_err(),
        StageError::LaneNotOpen {
            stage: Stage::Review,
            blocked_by: Stage::Verify,
        },
        "a later lane cannot arrive before verify closes"
    );
    pipeline
        .record(
            StageEvidence::complete(Stage::Verify, revision.clone(), vec![fact("verify")]).unwrap(),
        )
        .unwrap();
    assert_eq!(
        pipeline
            .record(StageEvidence::entered(Stage::Code, revision.clone()))
            .unwrap_err(),
        StageError::LaneNotOpen {
            stage: Stage::Code,
            blocked_by: Stage::InvariantCheck,
        },
        "a closed earlier stage is never re-entered through the pipeline"
    );

    let mut complete = drive_pass(all_lanes(), &revision);
    assert_eq!(complete.next_stage(), NextStage::Complete);
    assert_eq!(
        complete
            .record(StageEvidence::entered(
                Stage::Reassessment,
                revision.clone()
            ))
            .unwrap_err(),
        StageError::LaneClosed {
            stage: Stage::Reassessment,
        },
        "a completed pipeline accepts no further stage record"
    );
}

#[test]
fn evidence_against_a_stale_or_foreign_revision_never_satisfies_a_lane() {
    let current = head("head-current");
    let stale = head("head-stale");
    let foreign = CommitRevision::new(
        RepositoryRef::new(
            HostInstanceId::trusted("foreign-host"),
            RepositoryId::trusted("foreign-repo"),
        ),
        "head-current",
    )
    .unwrap();
    let mut pipeline = StagePipeline::new(all_lanes());
    pipeline.observe_head(&current);
    enter_and_complete(&mut pipeline, Stage::Code, &current);

    for revision in [stale, foreign] {
        assert_eq!(
            pipeline
                .record(
                    StageEvidence::complete(
                        Stage::Verify,
                        revision.clone(),
                        vec![fact("verify-off-head")],
                    )
                    .unwrap(),
                )
                .unwrap_err(),
            StageError::StaleRevision {
                stage: Stage::Verify
            },
            "evidence that does not bind to the recorded head is refused"
        );
    }
    assert!(
        pipeline.latest(Stage::Verify).is_none(),
        "a refused record never enters durable history"
    );

    pipeline
        .record(
            StageEvidence::complete(Stage::Verify, current.clone(), vec![fact("verify")]).unwrap(),
        )
        .unwrap();
    assert_eq!(
        pipeline.next_stage(),
        NextStage::Open(Stage::InvariantCheck),
        "the lane closes only on evidence bound to the recorded head"
    );
}

#[test]
fn a_pipeline_refuses_a_lane_policy_that_differs_from_the_frozen_spec() {
    let frozen = LanePolicy {
        invariant_check: false,
        code_review: true,
    };
    let drifted = all_lanes();
    assert_eq!(
        StagePipeline::bind(&frozen, &drifted).unwrap_err(),
        StageError::LanePolicyMismatch,
        "a policy that differs from the frozen spec is refused"
    );
    assert_eq!(
        StagePipeline::bind_from_observations(&frozen, &drifted, &[]).unwrap_err(),
        StageError::LanePolicyMismatch,
        "rebuilding under a drifted policy is refused the same way"
    );

    let pipeline = StagePipeline::bind(&frozen, &frozen).unwrap();
    assert_eq!(pipeline.lanes(), &frozen);
    assert!(
        !pipeline.lane_opens(Stage::InvariantCheck),
        "the bound pipeline judges the frozen policy: the lane is disabled"
    );
}
