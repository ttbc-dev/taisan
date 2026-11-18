//! Intake storage contract (#206/#207): durable run/candidate records,
//! the single-active-run guard, guarded state transitions, decision
//! links into the command ledger, the additive v7→v8 upgrade, reopen
//! durability, and the installation-wide single-flight intake lease.
//! Spec: docs/intake-identity-storage.md.

use rusqlite::Connection;
use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::ports::ProcessEvidence;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_store::sqlite::{RunLeaseError, RunLeaseGrant, RunLeaseOwner, SqliteStore};

const NOW: i64 = 1_700_000_000;

fn lease_owner(pid: i32, nonce: &str) -> RunLeaseOwner {
    RunLeaseOwner {
        pid,
        started_at: "Mon Jan  1 00:00:00 2024".into(),
        nonce: nonce.into(),
    }
}

fn alive(id: &ProcessIdentity) -> ProcessEvidence {
    ProcessEvidence::Alive {
        identity: id.clone(),
    }
}

fn stopped(id: &ProcessIdentity) -> ProcessEvidence {
    ProcessEvidence::Stopped {
        identity: id.clone(),
        exit_code: None,
    }
}

fn uninspectable(id: &ProcessIdentity) -> ProcessEvidence {
    let _ = id;
    ProcessEvidence::Unknown {
        reason: "process table unreadable".into(),
    }
}

fn run(run_id: &str, state: IntakeRunState, trigger: IntakeTrigger) -> IntakeRun {
    IntakeRun {
        run_id: run_id.into(),
        trigger,
        selector_id: AdapterId::trusted("ref-selector"),
        selector_version: 1,
        policy_revision: 7,
        settings_revision: Some("set-3".into()),
        state,
        detail_code: None,
        request_payload: br#"{"schema_version":1,"max_candidates":8}"#.to_vec(),
        request_digest: format!("req-{run_id}"),
        owner_pid: 4242,
        owner_started_at: "Mon Jan  1 00:00:00 2024".into(),
        owner_nonce: "nonce-run".into(),
        created_unix_seconds: NOW,
        finished_unix_seconds: None,
        recorded_seq: 1,
    }
}

fn candidate(run_id: &str, ordinal: u32, proposal_key: &str) -> IntakeCandidate {
    IntakeCandidate {
        run_id: run_id.into(),
        ordinal,
        proposal_key: proposal_key.into(),
        replay_key: format!("proposal/ref-selector:{proposal_key}"),
        payload: br#"{"schema_version":1}"#.to_vec(),
        payload_digest: format!("payload-{proposal_key}"),
        status: IntakeCandidateStatus::Pending,
        status_reason: None,
        command_id: None,
        work_id: None,
        committed_seq: None,
        recorded_seq: 2,
    }
}

fn insert_run(store: &SqliteStore, run: IntakeRun) {
    store
        .transact(|tx| tx.apply(&Effect::InsertIntakeRun { run }))
        .unwrap();
}

fn admit_batch(store: &SqliteStore, run_id: &str, candidates: Vec<IntakeCandidate>) {
    store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: run_id.into(),
                candidates,
            })
        })
        .unwrap();
}

fn run_state(store: &SqliteStore, run_id: &str) -> IntakeRunState {
    store
        .transact(|tx| tx.intake_run(run_id))
        .unwrap()
        .unwrap()
        .state
}

#[test]
fn single_active_run_and_terminal_guards() {
    let store = SqliteStore::in_memory(1).unwrap();
    insert_run(
        &store,
        run("r-open", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    // A second live run conflicts — the lease is the mutex, this is the
    // store-level backstop.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertIntakeRun {
                run: run(
                    "r-second",
                    IntakeRunState::Open,
                    IntakeTrigger::ScheduledInterval,
                ),
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "second active run must conflict: {err:?}"
    );
    // Terminal records never occupy the active slot: skipped and
    // completed rows can be written while another run is live. A
    // terminal insert must record its finish time and diagnostic — the
    // old shape of a `skipped` run with `finished_unix_seconds = None`
    // is refused.
    for (detail_code, finished_unix_seconds) in [
        (Some("capacity_full".into()), None),
        (None, Some(NOW)),
        (None, None),
    ] {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::InsertIntakeRun {
                    run: IntakeRun {
                        state: IntakeRunState::Skipped,
                        detail_code,
                        finished_unix_seconds,
                        ..run(
                            "r-skip-bad",
                            IntakeRunState::Skipped,
                            IntakeTrigger::ScheduledInterval,
                        )
                    },
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    }
    insert_run(
        &store,
        IntakeRun {
            state: IntakeRunState::Skipped,
            detail_code: Some("capacity_full".into()),
            finished_unix_seconds: Some(NOW),
            ..run(
                "r-skip",
                IntakeRunState::Skipped,
                IntakeTrigger::ScheduledInterval,
            )
        },
    );
    // Once the live run goes terminal, a new invocation may start.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-open".into(),
                state: IntakeRunState::Failed,
                detail_code: Some("transport_failure".into()),
                finished_unix_seconds: Some(NOW + 1),
                expected_state: IntakeRunState::Open,
            })
        })
        .unwrap();
    insert_run(
        &store,
        run("r-next", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    // Terminal states never regress.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-open".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: None,
                expected_state: IntakeRunState::Failed,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
}

#[test]
fn candidate_batch_atomic_with_admitting() {
    let store = SqliteStore::in_memory(1).unwrap();
    insert_run(
        &store,
        run("r-batch", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(
        &store,
        "r-batch",
        vec![
            candidate("r-batch", 0, "prop-a"),
            candidate("r-batch", 1, "prop-b"),
        ],
    );
    assert_eq!(run_state(&store, "r-batch"), IntakeRunState::Admitting);
    let candidates = store
        .transact(|tx| tx.intake_candidates("r-batch"))
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].ordinal, 0);
    assert_eq!(candidates[1].ordinal, 1);
    assert!(
        candidates
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::Pending)
    );

    // Once a run leaves `open` its batch is sealed: a second
    // AdmitIntakeBatch under `admitting` (or any later state)
    // conflicts — there is no way to append candidates after B1.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: "r-batch".into(),
                candidates: vec![candidate("r-batch", 2, "prop-e")],
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert_eq!(
        store
            .transact(|tx| tx.intake_candidates("r-batch"))
            .unwrap()
            .len(),
        2
    );

    // The single-active rule means the next `open` run needs the live
    // one terminal first.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-batch".into(),
                state: IntakeRunState::Interrupted,
                detail_code: Some("owner_lost".into()),
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap();

    // A failing batch rolls the whole apply back: the duplicate
    // (run_id, ordinal) inside one batch violates the primary key, so
    // neither candidate nor the run's admitting transition may land.
    insert_run(
        &store,
        run("r-atomic", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: "r-atomic".into(),
                candidates: vec![
                    candidate("r-atomic", 0, "prop-c"),
                    candidate("r-atomic", 0, "prop-d"),
                ],
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert_eq!(run_state(&store, "r-atomic"), IntakeRunState::Open);
    assert!(
        store
            .transact(|tx| tx.intake_candidates("r-atomic"))
            .unwrap()
            .is_empty(),
        "the rolled-back batch left no partial candidate rows"
    );

    // A batch is persisted all-`pending` with no decision links; a
    // pre-decided status, a carried link, or a foreign run_id is a
    // transition-table violation, not an insert. `r-atomic` goes
    // terminal first so `r-pre` may open.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-atomic".into(),
                state: IntakeRunState::Failed,
                detail_code: Some("transport_failure".into()),
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Open,
            })
        })
        .unwrap();
    insert_run(
        &store,
        run("r-pre", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    let mut decided = candidate("r-pre", 0, "prop-f");
    decided.status = IntakeCandidateStatus::Accepted;
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: "r-pre".into(),
                candidates: vec![decided],
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    let mut linked = candidate("r-pre", 0, "prop-g");
    linked.command_id = Some(CommandId("cmd-1".into()));
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: "r-pre".into(),
                candidates: vec![linked],
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::AdmitIntakeBatch {
                run_id: "r-pre".into(),
                candidates: vec![candidate("r-other", 0, "prop-h")],
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert!(
        store
            .transact(|tx| tx.intake_candidates("r-pre"))
            .unwrap()
            .is_empty(),
        "no refused batch left candidate rows behind"
    );
}

#[test]
fn run_state_transition_guards() {
    let store = SqliteStore::in_memory(1).unwrap();
    // Runs are born `open` or `skipped` only; every other state is a
    // guarded transition away, never an insert.
    for state in [
        IntakeRunState::Admitting,
        IntakeRunState::Completed,
        IntakeRunState::Failed,
        IntakeRunState::Interrupted,
    ] {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::InsertIntakeRun {
                    run: run("r-born", state, IntakeTrigger::Manual),
                })
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::ConstraintConflict { .. }),
            "insert in state {state:?} must conflict: {err:?}"
        );
    }
    insert_run(
        &store,
        run(
            "r-guard",
            IntakeRunState::Open,
            IntakeTrigger::ScheduledStartup,
        ),
    );
    // Wrong expected state conflicts.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-guard".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // The happy path: open → admitting → completed.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-guard".into(),
                state: IntakeRunState::Admitting,
                detail_code: None,
                finished_unix_seconds: None,
                expected_state: IntakeRunState::Open,
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-guard".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW + 5),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap();
    let finished = store
        .transact(|tx| tx.intake_run("r-guard"))
        .unwrap()
        .unwrap();
    assert_eq!(finished.finished_unix_seconds, Some(NOW + 5));
    assert_eq!(finished.trigger, IntakeTrigger::ScheduledStartup);
}

#[test]
fn run_transition_field_invariants_follow_state_table() {
    let store = SqliteStore::in_memory(1).unwrap();
    // A live insert keeps both columns NULL.
    for (detail_code, finished_unix_seconds) in
        [(Some("transport_failure".into()), None), (None, Some(NOW))]
    {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::InsertIntakeRun {
                    run: IntakeRun {
                        detail_code,
                        finished_unix_seconds,
                        ..run("r-bad-open", IntakeRunState::Open, IntakeTrigger::Manual)
                    },
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    }

    insert_run(
        &store,
        run("r-fields", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    // A live transition cannot carry a finish time or a failure code.
    for (detail_code, finished_unix_seconds) in [
        (Some("transport_failure".into()), None),
        (None, Some(NOW)),
        (Some("transport_failure".into()), Some(NOW)),
    ] {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::UpdateIntakeRun {
                    run_id: "r-fields".into(),
                    state: IntakeRunState::Admitting,
                    detail_code,
                    finished_unix_seconds,
                    expected_state: IntakeRunState::Open,
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    }
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-fields".into(),
                state: IntakeRunState::Admitting,
                detail_code: None,
                finished_unix_seconds: None,
                expected_state: IntakeRunState::Open,
            })
        })
        .unwrap();
    // `completed` records a finish time and carries no failure code.
    for (detail_code, finished_unix_seconds) in
        [(Some("transport_failure".into()), Some(NOW)), (None, None)]
    {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::UpdateIntakeRun {
                    run_id: "r-fields".into(),
                    state: IntakeRunState::Completed,
                    detail_code,
                    finished_unix_seconds,
                    expected_state: IntakeRunState::Admitting,
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    }
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-fields".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap();

    // `failed` and `interrupted` both require a bounded diagnostic and
    // a finish time.
    insert_run(
        &store,
        run("r-fields-fail", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    for (detail_code, finished_unix_seconds) in
        [(None, Some(NOW)), (Some("owner_lost".into()), None)]
    {
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::UpdateIntakeRun {
                    run_id: "r-fields-fail".into(),
                    state: IntakeRunState::Failed,
                    detail_code,
                    finished_unix_seconds,
                    expected_state: IntakeRunState::Open,
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    }
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-fields-fail".into(),
                state: IntakeRunState::Failed,
                detail_code: Some("transport_failure".into()),
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Open,
            })
        })
        .unwrap();
}

#[test]
fn completed_requires_every_candidate_terminal() {
    let store = SqliteStore::in_memory(1).unwrap();
    insert_run(
        &store,
        run("r-comp", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(
        &store,
        "r-comp",
        vec![
            candidate("r-comp", 0, "prop-c0"),
            candidate("r-comp", 1, "prop-c1"),
        ],
    );
    // Two pending candidates mean the run is not complete, even though
    // the transition itself is otherwise legal.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-comp".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert_eq!(run_state(&store, "r-comp"), IntakeRunState::Admitting);

    // One terminal, one still unresolved is still not complete.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::CapacityDeferred,
                    status_reason: Some("capacity_exhausted".into()),
                    ..candidate("r-comp", 0, "prop-c0")
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-comp".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));

    // Once the last candidate is terminal the run may complete.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Interrupted,
                    status_reason: Some("no_saved_decision".into()),
                    ..candidate("r-comp", 1, "prop-c1")
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: "r-comp".into(),
                state: IntakeRunState::Completed,
                detail_code: None,
                finished_unix_seconds: Some(NOW),
                expected_state: IntakeRunState::Admitting,
            })
        })
        .unwrap();
    assert_eq!(run_state(&store, "r-comp"), IntakeRunState::Completed);
}

#[test]
fn candidate_identity_immutable_status_guarded() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (command_id, committed_at) = {
        let (cid, _wid, cat) = submit(&store, "proposal/ref-selector:prop-x");
        (cid, cat)
    };
    insert_run(
        &store,
        run("r-idem", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(&store, "r-idem", vec![candidate("r-idem", 0, "prop-x")]);

    // A rejection links its durable command decision but never a Work.
    let decided = IntakeCandidate {
        status: IntakeCandidateStatus::Rejected,
        status_reason: Some("source_overlap".into()),
        command_id: Some(command_id),
        committed_seq: Some(committed_at),
        ..candidate("r-idem", 0, "prop-x")
    };
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: decided.clone(),
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    // A stale writer still holding `pending` conflicts; the row keeps
    // its recorded decision.
    let stale = IntakeCandidate {
        status: IntakeCandidateStatus::Accepted,
        ..candidate("r-idem", 0, "prop-x")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: stale,
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));

    let rows = store.transact(|tx| tx.intake_candidates("r-idem")).unwrap();
    let c = &rows[0];
    assert_eq!(c.status, IntakeCandidateStatus::Rejected);
    assert_eq!(c.status_reason.as_deref(), Some("source_overlap"));
    assert_eq!(c.proposal_key, "prop-x");
    assert_eq!(c.replay_key, "proposal/ref-selector:prop-x");
    assert_eq!(c.payload, br#"{"schema_version":1}"#.to_vec());
}

#[test]
fn candidate_decision_links_follow_the_status_contract() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (command_id, work_id, committed_at) = submit(&store, "proposal/ref-selector:prop-links");
    let (_, other_work_id, _) = submit(&store, "proposal/ref-selector:prop-other-work");
    insert_run(
        &store,
        run("r-links", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(
        &store,
        "r-links",
        vec![
            candidate("r-links", 0, "prop-a"),
            candidate("r-links", 1, "prop-b"),
            candidate("r-links", 2, "prop-c"),
            candidate("r-links", 3, "prop-d"),
        ],
    );
    let decide = |candidate: IntakeCandidate| {
        store
            .transact(|tx| {
                tx.apply(&Effect::UpdateIntakeCandidate {
                    candidate,
                    expected_status: IntakeCandidateStatus::Pending,
                })
            })
            .unwrap();
    };

    // Unresolved statuses must keep every link NULL — a deferred
    // candidate can never look decided.
    for (ordinal, key, status) in [
        (0u32, "prop-a", IntakeCandidateStatus::CapacityDeferred),
        (1, "prop-b", IntakeCandidateStatus::Unknown),
        (2, "prop-c", IntakeCandidateStatus::Interrupted),
    ] {
        let mut bad = candidate("r-links", ordinal, key);
        bad.status = status;
        bad.command_id = Some(command_id.clone());
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::UpdateIntakeCandidate {
                    candidate: bad,
                    expected_status: IntakeCandidateStatus::Pending,
                })
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::ConstraintConflict { .. }),
            "{status:?} with a command link must conflict: {err:?}"
        );
    }

    // A rejection never carries a Work id.
    let mut bad_reject = IntakeCandidate {
        status: IntakeCandidateStatus::Rejected,
        status_reason: Some("invalid_request".into()),
        command_id: Some(command_id.clone()),
        committed_seq: Some(committed_at),
        work_id: Some(work_id.clone()),
        ..candidate("r-links", 0, "prop-a")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: bad_reject.clone(),
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    bad_reject.work_id = None;
    decide(bad_reject);

    // Accepted requires the full link set.
    let mut thin_accept = IntakeCandidate {
        status: IntakeCandidateStatus::Accepted,
        command_id: Some(command_id.clone()),
        committed_seq: Some(committed_at),
        ..candidate("r-links", 1, "prop-b")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: thin_accept.clone(),
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    thin_accept.work_id = Some(work_id.clone());
    decide(thin_accept);

    // Replayed links the saved decision. The saved decision accepted
    // and created a Work, so the replay must carry that Work — a
    // replay without it would silently lose the link while still
    // reading as replayed.
    let replay_without_work = IntakeCandidate {
        status: IntakeCandidateStatus::Replayed,
        command_id: Some(command_id.clone()),
        committed_seq: Some(committed_at),
        ..candidate("r-links", 2, "prop-c")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: replay_without_work,
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // A different Work does not satisfy the original decision's link.
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Replayed,
                    command_id: Some(command_id.clone()),
                    work_id: Some(other_work_id),
                    committed_seq: Some(committed_at),
                    ..candidate("r-links", 2, "prop-c")
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    decide(IntakeCandidate {
        status: IntakeCandidateStatus::Replayed,
        command_id: Some(command_id.clone()),
        work_id: Some(work_id.clone()),
        committed_seq: Some(committed_at),
        ..candidate("r-links", 2, "prop-c")
    });

    let rows = store
        .transact(|tx| tx.intake_candidates("r-links"))
        .unwrap();
    assert_eq!(rows[0].status, IntakeCandidateStatus::Rejected);
    assert_eq!(rows[0].work_id, None);
    assert_eq!(rows[1].status, IntakeCandidateStatus::Accepted);
    assert_eq!(rows[1].work_id, Some(work_id.clone()));
    assert_eq!(rows[2].status, IntakeCandidateStatus::Replayed);
    assert_eq!(rows[2].command_id.as_ref(), Some(&command_id));
    assert_eq!(rows[2].work_id.as_ref(), Some(&work_id));
    // prop-d was never touched — still pending, still link-free.
    assert_eq!(rows[3].status, IntakeCandidateStatus::Pending);
    assert!(rows[3].command_id.is_none());
}

// --- decision links into the command ledger -------------------------

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

fn config() -> WorkloopConfig {
    use std::collections::BTreeSet;
    use workloop_core::identity::{ActorRole, AdapterId, HostCapability, RoleBinding};
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred"),
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

fn submit(store: &SqliteStore, key: &str) -> (CommandId, WorkId, EventSequence) {
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("selector".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        command_id,
        work_id,
        committed_at,
        ..
    } = d
    else {
        panic!("submit {key}: {d:?}")
    };
    (command_id, work_id.unwrap(), committed_at)
}

#[test]
fn candidate_command_work_links_round_trip_and_fk_guard() {
    let store = SqliteStore::in_memory(2).unwrap();
    let (command_id, work_id, committed_at) = submit(&store, "proposal/ref-selector:prop-linked");
    insert_run(
        &store,
        run("r-link", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(
        &store,
        "r-link",
        vec![
            candidate("r-link", 0, "prop-linked"),
            candidate("r-link", 1, "prop-other"),
        ],
    );

    let linked = IntakeCandidate {
        status: IntakeCandidateStatus::Accepted,
        command_id: Some(command_id.clone()),
        work_id: Some(work_id.clone()),
        committed_seq: Some(committed_at),
        ..candidate("r-link", 0, "prop-linked")
    };
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: linked,
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    let rows = store.transact(|tx| tx.intake_candidates("r-link")).unwrap();
    assert_eq!(rows[0].status, IntakeCandidateStatus::Accepted);
    assert_eq!(rows[0].command_id.as_ref(), Some(&command_id));
    assert_eq!(rows[0].work_id, Some(work_id));
    assert_eq!(rows[0].committed_seq, Some(committed_at));

    // Lookup by command identity recovers the candidate(s) that
    // recorded the decision — the lost-response reconciliation path.
    // Oldest first: the original recorder leads any later replayers.
    let by_command = store
        .transact(|tx| tx.intake_candidates_for_command(&command_id))
        .unwrap();
    assert_eq!(by_command.len(), 1);
    assert_eq!(by_command[0].run_id, "r-link");
    assert_eq!(by_command[0].status, IntakeCandidateStatus::Accepted);
    let absent = store
        .transact(|tx| tx.intake_candidates_for_command(&CommandId("cmd-absent".into())))
        .unwrap();
    assert!(absent.is_empty());
    let found = store
        .transact(|tx| tx.intake_candidates_for_proposal("prop-linked"))
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run_id, "r-link");

    // Links to absent ledger rows are refused by the foreign keys. The
    // pending `prop-other` candidate (ordinal 1, admitted above) takes
    // the bad-link updates — link-consistent shapes so the FK, not the
    // status contract, is what rejects them.
    let bad_command = IntakeCandidate {
        status: IntakeCandidateStatus::Rejected,
        status_reason: Some("invalid_request".into()),
        command_id: Some(CommandId("cmd-absent".into())),
        committed_seq: Some(committed_at),
        ..candidate("r-link", 1, "prop-other")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: bad_command,
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    let bad_work = IntakeCandidate {
        status: IntakeCandidateStatus::Accepted,
        command_id: Some(command_id.clone()),
        work_id: Some(WorkId("w-absent".into())),
        committed_seq: Some(committed_at),
        ..candidate("r-link", 1, "prop-other")
    };
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: bad_work,
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
}

#[test]
fn deferred_candidates_hold_no_decision_links() {
    let store = SqliteStore::in_memory(1).unwrap();
    insert_run(
        &store,
        run(
            "r-defer",
            IntakeRunState::Open,
            IntakeTrigger::ScheduledInterval,
        ),
    );
    admit_batch(
        &store,
        "r-defer",
        vec![candidate("r-defer", 0, "prop-deferred")],
    );
    // The deferral is decided during admission: pending → deferred with
    // a reason and no decision links.
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::CapacityDeferred,
                    status_reason: Some("capacity_exhausted".into()),
                    ..candidate("r-defer", 0, "prop-deferred")
                },
                expected_status: IntakeCandidateStatus::Pending,
            })
        })
        .unwrap();

    let rows = store
        .transact(|tx| tx.intake_candidates("r-defer"))
        .unwrap();
    let c = &rows[0];
    assert_eq!(c.status, IntakeCandidateStatus::CapacityDeferred);
    assert!(c.command_id.is_none());
    assert!(c.work_id.is_none());
    assert!(c.committed_seq.is_none());

    // The unresolved-status sweep sees pending/unknown only — a deferred
    // candidate is not recovery work.
    let unresolved = store
        .transact(|tx| {
            tx.intake_candidates_by_status(&[
                IntakeCandidateStatus::Pending,
                IntakeCandidateStatus::Unknown,
            ])
        })
        .unwrap();
    assert!(unresolved.is_empty());
}

#[test]
fn intake_records_survive_reopen_and_compete() {
    let root = std::env::temp_dir().join(format!("workloop-intake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let store = SqliteStore::open(&db, 1).unwrap();
    insert_run(
        &store,
        run("r-persist", IntakeRunState::Open, IntakeTrigger::Manual),
    );
    admit_batch(
        &store,
        "r-persist",
        vec![
            candidate("r-persist", 0, "prop-1"),
            candidate("r-persist", 1, "prop-2"),
        ],
    );
    drop(store);

    // Reopen: run and candidates round-trip byte-identical.
    let store = SqliteStore::open(&db, 1).unwrap();
    let persisted = store
        .transact(|tx| tx.intake_run("r-persist"))
        .unwrap()
        .unwrap();
    assert_eq!(persisted.state, IntakeRunState::Admitting);
    assert_eq!(persisted.selector_id.as_str(), "ref-selector");
    assert_eq!(persisted.policy_revision, 7);
    assert_eq!(persisted.settings_revision.as_deref(), Some("set-3"));
    assert_eq!(
        persisted.request_payload,
        br#"{"schema_version":1,"max_candidates":8}"#.to_vec()
    );
    let candidates = store
        .transact(|tx| tx.intake_candidates("r-persist"))
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].proposal_key, "prop-1");
    assert_eq!(candidates[1].proposal_key, "prop-2");

    // A competing connection sees the active run and cannot start
    // another — single-flight holds across processes.
    let other = SqliteStore::open(&db, 1).unwrap();
    let err = other
        .transact(|tx| {
            tx.apply(&Effect::InsertIntakeRun {
                run: run("r-other", IntakeRunState::Open, IntakeTrigger::Manual),
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn intake_lease_single_flight() {
    let store = SqliteStore::in_memory(1).unwrap();
    let a = lease_owner(101, "nonce-a");
    assert_eq!(
        store.acquire_intake_lease(&a, alive, NOW).unwrap(),
        RunLeaseGrant::Acquired
    );
    // A second intake owner — automatic or manual — is refused while a
    // live owner holds the lease.
    let b = lease_owner(102, "nonce-b");
    match store.acquire_intake_lease(&b, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 101),
        other => panic!("expected lock.held, got {other:?}"),
    }
    // A provably gone owner is reclaimed.
    assert_eq!(
        store.acquire_intake_lease(&b, stopped, NOW + 1).unwrap(),
        RunLeaseGrant::Reclaimed { previous_pid: 101 }
    );
    // Unknown liveness retains exclusion: no grant, no disturbance.
    let c = lease_owner(103, "nonce-c");
    match store.acquire_intake_lease(&c, uninspectable, NOW + 2) {
        Err(RunLeaseError::Unavailable { reason }) => {
            assert!(reason.contains("unreadable"), "{reason}");
        }
        other => panic!("expected lock.unavailable, got {other:?}"),
    }
    // The stale first owner's release cannot clear the reclaimed lease.
    store.release_intake_lease(&a).unwrap();
    assert_eq!(
        store.acquire_intake_lease(&b, alive, NOW + 3).unwrap(),
        RunLeaseGrant::Acquired
    );
    store.release_intake_lease(&b).unwrap();
}

#[test]
fn intake_lease_is_independent_of_the_driver_lease() {
    let store = SqliteStore::in_memory(1).unwrap();
    let driver = lease_owner(201, "nonce-driver");
    let intake = lease_owner(202, "nonce-intake");
    store.acquire_run_lease(&driver, alive, NOW).unwrap();
    // The run driver holding its lease does not block an intake
    // invocation claim — they are separate single-flight domains.
    store.acquire_intake_lease(&intake, alive, NOW).unwrap();
    store.release_intake_lease(&intake).unwrap();
    store.release_run_lease(&driver).unwrap();
}

// --- store upgrade ----------------------------------------------------

fn db_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("workloop-intake-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn v7_store_upgrades_additively() {
    let root = db_root("upgrade");
    let db = root.join("store.db");
    let (command_id, work_id, _) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let ids = submit(&store, "proposal/ref-selector:prop-keep");
        drop(store);
        ids
    };

    // Downgrade the fixture to a faithful v7 file: none of the intake
    // tables a v7 store could not have, stamp 7.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE intake_candidates; \
             DROP TABLE intake_runs; \
             DROP TABLE intake_settings; \
             PRAGMA user_version=7;",
        )
        .unwrap();
    }

    let store = SqliteStore::open(&db, 1).unwrap();
    // Existing records are untouched by the additive upgrade.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("the pre-upgrade Work survives");
    assert_eq!(work.id, work_id);
    let records = store.transact(|tx| tx.commands()).unwrap();
    assert!(
        records.iter().any(|r| r.command_id == command_id),
        "the pre-upgrade command decision survives"
    );

    // The intake schema is installed and usable.
    insert_run(
        &store,
        run(
            "r-post-upgrade",
            IntakeRunState::Open,
            IntakeTrigger::Manual,
        ),
    );
    assert_eq!(run_state(&store, "r-post-upgrade"), IntakeRunState::Open);
    // The v9 settings table arrived empty — absent record, documented
    // default limit.
    assert!(store.transact(|tx| tx.intake_settings()).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn v8_stamp_without_intake_tables_is_refused() {
    let root = db_root("foreign");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    // A file stamped v8 but missing the intake tables a v8 store must
    // hold is foreign — never silently reshaped. (A faithful v8 file
    // also lacks intake_settings; dropping it keeps the fixture honest,
    // though the version gate refuses on the missing v8 tables first.)
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE intake_candidates; \
             DROP TABLE intake_runs; \
             DROP TABLE intake_settings; \
             PRAGMA user_version=8;",
        )
        .unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { .. }) => {}
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got Ok"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn newer_format_is_refused() {
    let root = db_root("newer");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    {
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 10i64).unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { .. }) => {}
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got Ok"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

// --- closed vocabularies ----------------------------------------------

#[test]
fn enum_vocabulary_is_check_constrained() {
    let root = db_root("enums");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

    // An unknown trigger/state/status tag cannot be written even by a
    // caller bypassing the typed API.
    let bad_run = conn.execute(
        "INSERT INTO intake_runs (run_id, trigger, selector_id, \
         selector_version, policy_revision, state, request_payload, \
         request_digest, owner_pid, owner_started_at, owner_nonce, \
         created_unix_seconds, recorded_seq) \
         VALUES ('r-bad', 'bogus', 'sel', 1, 0, 'open', X'00', 'd', \
         1, 't', 'n', 0, 0)",
        [],
    );
    assert!(bad_run.is_err(), "CHECK must reject an unknown trigger");
    let bad_state = conn.execute(
        "INSERT INTO intake_runs (run_id, trigger, selector_id, \
         selector_version, policy_revision, state, request_payload, \
         request_digest, owner_pid, owner_started_at, owner_nonce, \
         created_unix_seconds, recorded_seq) \
         VALUES ('r-bad2', 'manual', 'sel', 1, 0, 'bogus', X'00', 'd', \
         1, 't', 'n', 0, 0)",
        [],
    );
    assert!(bad_state.is_err(), "CHECK must reject an unknown state");

    conn.execute(
        "INSERT INTO intake_runs (run_id, trigger, selector_id, \
         selector_version, policy_revision, state, request_payload, \
         request_digest, owner_pid, owner_started_at, owner_nonce, \
         created_unix_seconds, recorded_seq) \
         VALUES ('r-ok', 'manual', 'sel', 1, 0, 'open', X'00', 'd', \
         1, 't', 'n', 0, 0)",
        [],
    )
    .unwrap();
    let bad_status = conn.execute(
        "INSERT INTO intake_candidates (run_id, ordinal, proposal_key, \
         replay_key, payload, payload_digest, status, recorded_seq) \
         VALUES ('r-ok', 0, 'p', 'k', X'00', 'd', 'bogus', 0)",
        [],
    );
    assert!(bad_status.is_err(), "CHECK must reject an unknown status");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn active_run_index_rejects_a_second_live_run_structurally() {
    let root = db_root("index");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    let conn = Connection::open(&db).unwrap();
    let insert = |state: &str| {
        conn.execute(
            "INSERT INTO intake_runs (run_id, trigger, selector_id, \
             selector_version, policy_revision, state, request_payload, \
             request_digest, owner_pid, owner_started_at, owner_nonce, \
             created_unix_seconds, recorded_seq) \
             VALUES (?1, 'manual', 'sel', 1, 0, ?2, X'00', 'd', \
             1, 't', 'n', 0, 0)",
            rusqlite::params![format!("r-{state}"), state],
        )
    };
    insert("open").unwrap();
    // The partial unique index refuses a second open run even for a
    // writer that bypasses the store's apply-level check.
    assert!(insert("admitting").is_err());
    assert!(insert("open").is_err());
    // Terminal rows never collide.
    insert("completed").unwrap();
    insert("skipped").unwrap();
    insert("failed").unwrap();
    insert("interrupted").unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
