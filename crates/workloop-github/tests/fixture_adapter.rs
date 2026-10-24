//! Fixture GitHub adapter contract tests (`docs/v0.1.md`: outbox-first
//! writes, head recheck before `update_pr`/`post_pr_comment`, synthetic
//! Observations, unknown-result reconciliation by action ID).

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{WorkloopConfig, admit_queued, handle_command};
use workloop_github::fixture::FixtureGithub;
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
                intent: "ship it".into(),
                issue_refs: vec![],
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

fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: PrAction) -> String {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            tx.apply(&Effect::InsertPrActionIntent {
                intent: PrActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    repository: "local/repo".into(),
                    action,
                    status: PrActionStatus::Pending,
                },
            })?;
            Ok(action_id)
        })
        .unwrap()
}

fn intent_status(store: &SqliteStore, action_id: &str) -> PrActionStatus {
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
        PrAction::PublishPr {
            branch: "work/w-1".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    // The intent row exists before the simulated write happens.
    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Pending);

    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Applied);
    assert_eq!(gh.applied(), &["publish_pr:local/repo:1"]);
    // The write surfaced the fresh head as an Observation.
    let heads = store
        .transact(|tx| {
            Ok(tx
                .observations_for_work(&work_id)?
                .iter()
                .filter(|o| matches!(o.fact, ObservationFact::PrHead { .. }))
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
    // outstanding GitHub write.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::GithubWrite);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    let mut gh = FixtureGithub::new();
    // The remote moved since the intent was recorded.
    gh.set_head("local/repo", 7, "sha-new");
    let action_id = insert_intent(
        &store,
        &work_id,
        PrAction::UpdatePr {
            number: 7,
            expected_head: "sha-old".into(),
            body: "edited body".into(),
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    assert!(gh.applied().is_empty(), "no write on head mismatch");
    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Failed);
    // A fresh pr_head Observation records the actual remote head, and no
    // automatic re-issue is attempted.
    let observations = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    assert!(observations.iter().any(|o| matches!(
        &o.fact,
        ObservationFact::PrHead { number: 7, head_sha, .. } if head_sha == "sha-new"
    )));
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Paused);
    assert!(e.pause_reason.as_deref().unwrap_or("").contains("repair"));
}

#[test]
fn head_match_applies_the_write() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _eid) = submit(&store, "k-g3");
    let mut gh = FixtureGithub::new();
    gh.set_head("local/repo", 9, "sha-x");
    let action_id = insert_intent(
        &store,
        &work_id,
        PrAction::PostPrComment {
            number: 9,
            expected_head: "sha-x".into(),
            body: "hello".into(),
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied(), &["post_pr_comment:local/repo:9"]);
    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Applied);
}

#[test]
fn unknown_result_stays_outstanding_until_reconciled_by_action_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g4");
    let action_id = insert_intent(
        &store,
        &work_id,
        PrAction::PublishPr {
            branch: "b".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );
    // Simulate a lost write: the transport reports the result unknown.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateActionIntent {
                action_id: action_id.clone(),
                status: PrActionStatus::Unknown,
            })
        })
        .unwrap();
    let gh = FixtureGithub::new();
    // The outbox drain does not retry an `unknown` intent — it is
    // outstanding until reconciled by action ID.
    let mut gh = gh;
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert!(gh.applied().is_empty());
    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Unknown);
    let outstanding = store
        .transact(|tx| workloop_core::projection::outstanding_actions(tx, &work_id))
        .unwrap();
    assert_eq!(outstanding, vec![action_id.clone()]);

    // Reconciliation arrives keyed by the same action ID.
    workloop_core::work::handle_observation(
        &store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid),
            fact: ObservationFact::PrActionResult {
                action_id: action_id.clone(),
                action_kind: "publish_pr".into(),
                result: ActionResult::Applied,
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(intent_status(&store, &action_id), PrActionStatus::Applied);
}

#[test]
fn head_bound_evidence_drives_success_only_for_the_current_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g5");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    // Move the execution into waiting(ci) the way a clean exit +
    // publish intent would.
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })?;
            // Completion requires the published PR: the applied
            // publish intent the outbox would have produced.
            tx.apply(&Effect::InsertPrActionIntent {
                intent: PrActionIntent {
                    action_id: "act-pub-1".into(),
                    work_id: work_id.clone(),
                    repository: "local/repo".into(),
                    action: PrAction::PublishPr {
                        branch: "work/w-1".into(),
                        title: "t".into(),
                        body: "b".into(),
                    },
                    status: PrActionStatus::Applied,
                },
            })
        })
        .unwrap();
    let gh = FixtureGithub::new();
    let obs = |fact| Observation {
        source_event_id: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact,
        observed_unix_seconds: NOW,
    };
    // The head already moved; evidence bound to the old head must not
    // authorize the new one.
    gh.emit(
        &store,
        &config(),
        obs(ObservationFact::PrHead {
            repository: "local/repo".into(),
            number: 1,
            head_sha: "new".into(),
        }),
        NOW,
    )
    .unwrap();
    gh.emit(
        &store,
        &config(),
        obs(ObservationFact::PrChecks {
            repository: "local/repo".into(),
            head_sha: "old".into(),
            conclusion: CheckConclusion::Success,
        }),
        NOW,
    )
    .unwrap();
    gh.emit(
        &store,
        &config(),
        obs(ObservationFact::PrReview {
            repository: "local/repo".into(),
            head_sha: "old".into(),
            reviewer: "rev".into(),
            verdict: ReviewVerdict::Approve,
        }),
        NOW,
    )
    .unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Waiting);

    // Evidence covering the current head completes it.
    gh.emit(
        &store,
        &config(),
        obs(ObservationFact::PrChecks {
            repository: "local/repo".into(),
            head_sha: "new".into(),
            conclusion: CheckConclusion::Success,
        }),
        NOW,
    )
    .unwrap();
    gh.emit(
        &store,
        &config(),
        obs(ObservationFact::PrReview {
            repository: "local/repo".into(),
            head_sha: "new".into(),
            reviewer: "rev".into(),
            verdict: ReviewVerdict::Approve,
        }),
        NOW,
    )
    .unwrap();
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}

#[test]
fn repeated_artifact_evidence_does_not_queue_a_second_publish() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g6");
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
                    required_artifacts: vec!["artifact.json".into()],
                    evidence_refs: vec![],
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
    let obs = |id: &str| Observation {
        source_event_id: Some(id.into()),
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact: ObservationFact::Artifact {
            invocation_id: InvocationId("inv-1".into()),
            required_artifact: "artifact.json".into(),
            state: ArtifactFact::Present,
        },
        observed_unix_seconds: NOW,
    };
    // First report queues the publish intent; the outbox applies it.
    gh.emit(&store, &config(), obs("art-1"), NOW).unwrap();
    assert_eq!(store.transact(|tx| tx.action_intents()).unwrap().len(), 1);
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(gh.applied().len(), 1, "one PR published");
    // A repeat of the same fact under a new delivery id must not queue
    // a second publish — the applied intent already covers this work.
    gh.emit(&store, &config(), obs("art-2"), NOW).unwrap();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    assert_eq!(
        store.transact(|tx| tx.action_intents()).unwrap().len(),
        1,
        "no second publish intent"
    );
    assert_eq!(gh.applied().len(), 1, "still one PR published");
}

#[test]
fn superseded_head_evidence_does_not_complete_the_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "k-g7");
    store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW).map(|_| ()))
        .unwrap();
    store
        .transact(|tx| {
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })?;
            tx.apply(&Effect::InsertPrActionIntent {
                intent: PrActionIntent {
                    action_id: "act-pub-2".into(),
                    work_id: work_id.clone(),
                    repository: "local/repo".into(),
                    action: PrAction::PublishPr {
                        branch: "work/w-1".into(),
                        title: "t".into(),
                        body: "b".into(),
                    },
                    status: PrActionStatus::Applied,
                },
            })
        })
        .unwrap();
    let gh = FixtureGithub::new();
    let obs = |fact| Observation {
        source_event_id: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: Some(eid.clone()),
        fact,
        observed_unix_seconds: NOW,
    };
    let emit = |fact| gh.emit(&store, &config(), obs(fact), NOW).unwrap();
    emit(ObservationFact::PrHead {
        repository: "local/repo".into(),
        number: 3,
        head_sha: "sha-1".into(),
    });
    // A superseded success/approve must not complete the work: only the
    // latest fact recorded for the current head decides.
    emit(ObservationFact::PrChecks {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        conclusion: CheckConclusion::Success,
    });
    emit(ObservationFact::PrChecks {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        conclusion: CheckConclusion::Failure,
    });
    emit(ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        reviewer: "rev".into(),
        verdict: ReviewVerdict::Approve,
    });
    emit(ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        reviewer: "rev".into(),
        verdict: ReviewVerdict::ChangesRequested,
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(
        e.state,
        ExecutionState::Waiting,
        "superseded approval cannot complete"
    );
    // A fresh pass over the same head still completes once the latest
    // check and review are green.
    emit(ObservationFact::PrChecks {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        conclusion: CheckConclusion::Success,
    });
    emit(ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: "sha-1".into(),
        reviewer: "rev".into(),
        verdict: ReviewVerdict::Approve,
    });
    let e = store.transact(|tx| tx.execution(&eid)).unwrap().unwrap();
    assert_eq!(e.state, ExecutionState::Succeeded);
}
