//! Intake settings contract (#213): the installation-wide pending-queue
//! limit enforced atomically inside the submit transaction, the
//! revision-guarded settings record, the read-only bounded preflight
//! view, retry/resume bypass, and the additive v8→current upgrade.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::projection::{
    self, IntakePreflightError, MAX_PREFLIGHT_SCOPE_ISSUES, intake_preflight,
};
use workloop_core::store::*;
use workloop_core::work::{
    DEFAULT_PENDING_QUEUE_LIMIT, HostTemplate, WorkloopConfig, handle_command, set_intake_settings,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

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
    use workloop_core::identity::{ActorRole, AdapterId, HostCapability, RoleBinding};
    WorkloopConfig {
        placement: workloop_core::placement::PlacementPolicy::default(),
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
            HostCapability::IssueRead,
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

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit(key: &str) -> CommandRequest {
    req(
        key,
        CommandBody::Submit {
            intent: format!("intent-{key}"),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    )
}

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn source(number: u64) -> SourceSnapshot {
    SourceSnapshot {
        issue: issue(number),
        source_revision: "rev-1".into(),
        content_digest: "digest-1".into(),
    }
}

fn submit_with_sources(key: &str, sources: Vec<SourceSnapshot>) -> CommandRequest {
    req(
        key,
        CommandBody::Submit {
            intent: format!("intent-{key}"),
            sources,
            constraints: vec![],
            verification_argv: vec![],
        },
    )
}

fn accepted(d: &CommandDecision) -> (WorkId, ExecutionId) {
    match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => execution_id.clone(),
                other => panic!("expected an execution effect, got {other:?}"),
            };
            (work_id.clone().expect("work_id"), eid)
        }
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn decision(store: &SqliteStore, request: &CommandRequest) -> CommandDecision {
    handle_command(store, &config(), request, NOW).unwrap()
}

fn reject_reason(d: &CommandDecision) -> RejectionCode {
    match d {
        CommandDecision::Rejected { reason, .. } => reason.clone(),
        other => panic!("expected rejected, got {other:?}"),
    }
}

/// The structured capacity rejection: code plus the limit and
/// occupancy evidence the issue requires, not a bare refusal.
fn capacity_rejection(d: &CommandDecision) -> (RejectionCode, Vec<String>) {
    match d {
        CommandDecision::Rejected {
            reason, evidence, ..
        } => (reason.clone(), evidence.clone()),
        other => panic!("expected rejected, got {other:?}"),
    }
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("setup effects apply");
}

fn read_execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .expect("read")
        .expect("execution exists")
}

fn force_state(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    let from = read_execution(store, id).state;
    let mut e = read_execution(store, id);
    e.state = to.clone();
    if to.is_terminal() {
        e.terminal_code = Some("setup".into());
        e.outcome = Some(Outcome {
            state: to.clone(),
            code: Some("setup".into()),
            detail: None,
            evidence_refs: vec![],
            recorded_seq: 0,
        });
    }
    apply_effects(
        store,
        &[Effect::UpdateExecution {
            execution: e,
            expected_state: from,
        }],
    );
}

fn work_version(store: &SqliteStore, work_id: &WorkId) -> WorkVersion {
    store
        .transact(|tx| tx.work(work_id))
        .expect("read")
        .expect("work exists")
        .version
}

fn settings(revision: &str, limit: u32) -> IntakeSettingsRecord {
    IntakeSettingsRecord {
        settings_revision: revision.into(),
        pending_queue_limit: limit,
        recorded_seq: 1,
    }
}

fn pending_count(store: &SqliteStore) -> u64 {
    store.transact(|tx| tx.pending_queue_count()).unwrap()
}

fn intake_settings(store: &SqliteStore) -> Option<IntakeSettingsRecord> {
    store.transact(|tx| tx.intake_settings()).unwrap()
}

fn preflight(
    store: &SqliteStore,
    scope: &[IssueRef],
) -> Result<projection::IntakePreflight, IntakePreflightError> {
    store
        .transact_deferred(|tx| Ok(intake_preflight(tx, scope)))
        .expect("deferred transaction")
}

// --- the settings record ----------------------------------------------

#[test]
fn absent_settings_resolve_to_the_documented_default() {
    let store = SqliteStore::in_memory(1).unwrap();
    assert!(intake_settings(&store).is_none());
    let view = preflight(&store, &[]).unwrap();
    assert_eq!(view.settings_revision, None);
    assert_eq!(view.pending_queue_limit, DEFAULT_PENDING_QUEUE_LIMIT);
    assert_eq!(view.pending_queue_limit, 10);
    assert_eq!(view.free_positions, 10);
}

#[test]
fn settings_record_writes_are_revision_guarded() {
    let store = SqliteStore::in_memory(1).unwrap();

    // The first write must declare the record absent.
    let stale_first = set_intake_settings(&store, settings("rev-a", 4), Some("rev-x"));
    assert!(matches!(
        stale_first,
        Err(StoreError::ConstraintConflict { .. })
    ));
    assert!(intake_settings(&store).is_none());

    set_intake_settings(&store, settings("rev-a", 4), None).unwrap();
    let active = intake_settings(&store).unwrap();
    assert_eq!(active.settings_revision, "rev-a");
    assert_eq!(active.pending_queue_limit, 4);

    // A stale writer cannot overwrite the newer record.
    let stale = set_intake_settings(&store, settings("rev-old", 9), Some("rev-old"));
    assert!(matches!(stale, Err(StoreError::ConstraintConflict { .. })));
    assert_eq!(intake_settings(&store).unwrap().settings_revision, "rev-a");

    // The writer that observed the current revision may update it.
    set_intake_settings(&store, settings("rev-b", 7), Some("rev-a")).unwrap();
    let active = intake_settings(&store).unwrap();
    assert_eq!(active.settings_revision, "rev-b");
    assert_eq!(active.pending_queue_limit, 7);

    // An absent-expectation write against a live record conflicts too.
    let absent_expectation = set_intake_settings(&store, settings("rev-c", 1), None);
    assert!(matches!(
        absent_expectation,
        Err(StoreError::ConstraintConflict { .. })
    ));
    assert_eq!(intake_settings(&store).unwrap().settings_revision, "rev-b");
}

#[test]
fn the_active_limit_is_what_submit_decides_against() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 2), None).unwrap();

    accepted(&decision(&store, &submit("cap-1")));
    accepted(&decision(&store, &submit("cap-2")));
    let cap3 = submit("cap-3");
    assert_eq!(
        reject_reason(&decision(&store, &cap3)),
        RejectionCode::CapacityExhausted
    );

    // Raising the limit through the guarded write admits again — and
    // the refused request's own key is decided fresh, because a
    // capacity refusal persists no replayable decision.
    set_intake_settings(&store, settings("rev-2", 4), Some("rev-1")).unwrap();
    accepted(&decision(&store, &cap3));
    accepted(&decision(&store, &submit("cap-4b")));
    assert_eq!(
        reject_reason(&decision(&store, &submit("cap-5b"))),
        RejectionCode::CapacityExhausted
    );
}

#[test]
fn lowering_the_limit_refuses_new_submits_without_cancelling_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 5), None).unwrap();
    let (w1, e1) = accepted(&decision(&store, &submit("low-1")));
    let (w2, e2) = accepted(&decision(&store, &submit("low-2")));
    let (w3, e3) = accepted(&decision(&store, &submit("low-3")));

    // Occupancy 3 > new limit 2: new submits refuse, existing Work is
    // untouched — still queued, still version 1.
    set_intake_settings(&store, settings("rev-2", 2), Some("rev-1")).unwrap();
    assert_eq!(
        reject_reason(&decision(&store, &submit("low-4"))),
        RejectionCode::CapacityExhausted
    );
    for (w, e) in [(&w1, &e1), (&w2, &e2), (&w3, &e3)] {
        let work = store
            .transact(|tx| tx.work(w))
            .unwrap()
            .expect("work survives the lowered limit");
        assert_eq!(work.version, 1);
        assert_eq!(read_execution(&store, e).state, ExecutionState::Queued);
    }
    assert_eq!(pending_count(&store), 3);
}

// --- occupancy semantics -----------------------------------------------

#[test]
fn only_queued_executions_hold_a_pending_position() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 2), None).unwrap();

    let (_wa, ea) = accepted(&decision(&store, &submit("occ-a")));
    let (_wb, eb) = accepted(&decision(&store, &submit("occ-b")));
    assert_eq!(pending_count(&store), 2);
    assert_eq!(
        reject_reason(&decision(&store, &submit("occ-c"))),
        RejectionCode::CapacityExhausted
    );

    // An executing Work holds no pending position. Fresh keys
    // throughout: rejected commands are durable, so a reused key would
    // replay its recorded rejection instead of re-deciding.
    force_state(&store, &ea, ExecutionState::Running);
    assert_eq!(pending_count(&store), 1);
    let (_wc, ec) = accepted(&decision(&store, &submit("occ-c2")));
    assert_eq!(pending_count(&store), 2);
    assert_eq!(
        reject_reason(&decision(&store, &submit("occ-d2"))),
        RejectionCode::CapacityExhausted
    );

    // A paused Work is excluded from pending occupancy too.
    force_state(&store, &eb, ExecutionState::Paused);
    assert_eq!(pending_count(&store), 1);
    let (_wd, _ed) = accepted(&decision(&store, &submit("occ-d3")));
    assert_eq!(pending_count(&store), 2);

    // A terminal Execution frees its position for good.
    force_state(&store, &ec, ExecutionState::Succeeded);
    assert_eq!(pending_count(&store), 1);
    accepted(&decision(&store, &submit("occ-e")));
    assert_eq!(pending_count(&store), 2);
}

#[test]
fn a_grouped_submission_consumes_exactly_one_position() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 2), None).unwrap();

    // One grouped proposal — two source issues, one Work — one position.
    let (_wg, _eg) = accepted(&decision(
        &store,
        &submit_with_sources("grp-1", vec![source(101), source(102)]),
    ));
    assert_eq!(pending_count(&store), 1);
    accepted(&decision(&store, &submit("grp-2")));
    assert_eq!(pending_count(&store), 2);
    assert_eq!(
        reject_reason(&decision(&store, &submit("grp-3"))),
        RejectionCode::CapacityExhausted
    );
}

#[test]
fn selector_origin_submits_share_the_same_capacity_check() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 1), None).unwrap();

    // The manual path fills the single position.
    accepted(&decision(&store, &submit("mix-cli")));

    // A selector-origin submit of the same shape hits the same gate.
    let selector_submit = CommandRequest {
        actor: actor(),
        origin: Origin("selector".into()),
        idempotency_key: "proposal/ref-selector:prop-1".into(),
        body: CommandBody::Submit {
            intent: "selector proposal".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    assert_eq!(
        reject_reason(&decision(&store, &selector_submit)),
        RejectionCode::CapacityExhausted
    );
}

// --- rejection atomicity and replay ------------------------------------

#[test]
fn a_capacity_rejection_leaves_no_partial_rows() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 1), None).unwrap();
    let (w1, _e1) = accepted(&decision(
        &store,
        &submit_with_sources("atom-1", vec![source(201)]),
    ));

    let works_before = store.transact(|tx| tx.works()).unwrap().len();
    let claim_before = store.transact(|tx| tx.issue_claim(&issue(202))).unwrap();
    assert!(claim_before.is_none());

    let d = decision(&store, &submit_with_sources("atom-2", vec![source(202)]));
    let (reason, evidence) = capacity_rejection(&d);
    assert_eq!(reason, RejectionCode::CapacityExhausted);
    assert!(
        evidence.iter().any(|e| e.contains("pending queue limit 1")),
        "evidence names the active limit: {evidence:?}"
    );
    assert!(
        evidence.iter().any(|e| e.contains("1 queued Works")),
        "evidence names the occupancy: {evidence:?}"
    );

    // No Work, Execution, claim, spec, or usage row for the rejected
    // submission — and no saved decision either: a capacity refusal is
    // transient, so the replay lookup must find nothing under the key.
    assert_eq!(store.transact(|tx| tx.works()).unwrap().len(), works_before);
    assert!(
        store
            .transact(|tx| tx.issue_claim(&issue(202)))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .transact(|tx| tx.spec(&WorkId("w-nonexistent".into()), None))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .transact(|tx| { tx.stored_decision(&Origin("cli".into()), &actor(), "atom-2",) })
            .unwrap()
            .is_none(),
        "a capacity refusal persists no decision under the idempotency key"
    );
    assert_eq!(pending_count(&store), 1);
    let _ = w1;
}

#[test]
fn a_capacity_rejection_is_redecided_when_capacity_returns() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 1), None).unwrap();
    accepted(&decision(&store, &submit("free-1")));

    // The queue is full: the identical request is refused, and nothing
    // is recorded under its key.
    let request = submit("free-2");
    assert_eq!(
        reject_reason(&decision(&store, &request)),
        RejectionCode::CapacityExhausted
    );
    assert!(
        store
            .transact(|tx| { tx.stored_decision(&Origin("cli".into()), &actor(), "free-2") })
            .unwrap()
            .is_none()
    );

    // Capacity returns (here via a raised limit): the very same key and
    // payload is decided fresh and accepted — the deferral contract a
    // selector replay key depends on.
    set_intake_settings(&store, settings("rev-2", 2), Some("rev-1")).unwrap();
    let (work_id, _e) = accepted(&decision(&store, &request));
    assert!(
        store
            .transact(|tx| tx.stored_decision(&Origin("cli".into()), &actor(), "free-2"))
            .unwrap()
            .is_some()
    );
    assert_eq!(pending_count(&store), 2);
    let _ = work_id;
}

#[test]
fn replay_returns_the_recorded_decision_under_a_full_queue() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 2), None).unwrap();

    let request = submit("full-replay");
    let (work_id, execution_id) = accepted(&decision(&store, &request));
    accepted(&decision(&store, &submit("full-filler")));
    assert_eq!(pending_count(&store), 2);
    assert_eq!(
        reject_reason(&decision(&store, &submit("full-extra"))),
        RejectionCode::CapacityExhausted
    );

    let replayed = decision(&store, &request);
    let (rw, re) = accepted(&replayed);
    assert_eq!(rw, work_id);
    assert_eq!(re, execution_id);
    assert_eq!(pending_count(&store), 2);
}

// --- retry / resume bypass ----------------------------------------------

#[test]
fn retry_bypasses_the_pending_limit() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 1), None).unwrap();

    let (wa, ea) = accepted(&decision(&store, &submit("retry-a")));
    force_state(&store, &ea, ExecutionState::Failed);
    // The queue is momentarily empty; fill it with another Work.
    let (_wb, _eb) = accepted(&decision(&store, &submit("retry-b")));
    assert_eq!(pending_count(&store), 1);

    // Retrying wa creates a new queued Execution even though the queue
    // is full — recovery is never capacity-gated.
    let d = decision(
        &store,
        &req(
            "retry-a-2",
            CommandBody::Retry {
                work_id: wa.clone(),
                expected_work_version: work_version(&store, &wa),
                expected_execution_id: ea.clone(),
            },
        ),
    );
    let (_w, e2) = accepted(&d);
    assert_eq!(read_execution(&store, &e2).state, ExecutionState::Queued);
    // Occupancy may now exceed the limit — that is the documented
    // bypass, not a leak: submits still refuse.
    assert_eq!(pending_count(&store), 2);
    assert_eq!(
        reject_reason(&decision(&store, &submit("retry-c"))),
        RejectionCode::CapacityExhausted
    );
}

#[test]
fn resume_bypasses_the_pending_limit() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 1), None).unwrap();

    let (wa, ea) = accepted(&decision(&store, &submit("resume-a")));
    force_state(&store, &ea, ExecutionState::Running);
    force_state(&store, &ea, ExecutionState::Paused);
    // Fill the single pending position with another Work.
    let (_wb, _eb) = accepted(&decision(&store, &submit("resume-b")));
    assert_eq!(pending_count(&store), 1);

    let d = decision(
        &store,
        &req(
            "resume-a-1",
            CommandBody::Resume {
                work_id: wa.clone(),
                expected_work_version: work_version(&store, &wa),
                execution_id: ea.clone(),
            },
        ),
    );
    match d {
        CommandDecision::Accepted { .. } => {}
        other => panic!("expected accepted, got {other:?}"),
    }
    assert_eq!(read_execution(&store, &ea).state, ExecutionState::Queued);
}

// --- the read-only preflight --------------------------------------------

#[test]
fn preflight_reports_positions_and_scoped_owners_from_one_snapshot() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-q", 3), None).unwrap();

    // queued owner
    let (_w1, e1) = accepted(&decision(
        &store,
        &submit_with_sources("pf-1", vec![source(301)]),
    ));
    // running owner
    let (_w2, e2) = accepted(&decision(
        &store,
        &submit_with_sources("pf-2", vec![source(302)]),
    ));
    force_state(&store, &e2, ExecutionState::Running);
    // paused owner — holds no claim yet still owns its issues
    let (_w3, e3) = accepted(&decision(
        &store,
        &submit_with_sources("pf-3", vec![source(303)]),
    ));
    force_state(&store, &e3, ExecutionState::Paused);
    // every remaining non-terminal state owns its issues too —
    // ready, waiting, stopping, and uncertain are unfinished Work
    // exactly like queued, running, and paused
    for (key, number, state) in [
        ("pf-5", 306, ExecutionState::Ready),
        ("pf-6", 307, ExecutionState::Waiting),
        ("pf-7", 308, ExecutionState::Stopping),
        ("pf-8", 309, ExecutionState::Uncertain),
    ] {
        let (_w, e) = accepted(&decision(
            &store,
            &submit_with_sources(key, vec![source(number)]),
        ));
        force_state(&store, &e, state);
    }
    // terminal owner — excluded entirely
    let (_w4, e4) = accepted(&decision(
        &store,
        &submit_with_sources("pf-4", vec![source(304)]),
    ));
    force_state(&store, &e4, ExecutionState::Succeeded);

    let scope = vec![
        issue(301),
        issue(302),
        issue(303),
        issue(304),
        issue(305), // unowned
        issue(306),
        issue(307),
        issue(308),
        issue(309),
    ];
    let view = preflight(&store, &scope).unwrap();
    assert_eq!(view.settings_revision.as_deref(), Some("rev-q"));
    assert_eq!(view.pending_queue_limit, 3);
    // Only the queued Work holds a position.
    assert_eq!(view.pending_count, 1);
    assert_eq!(view.free_positions, 2);

    let owners: Vec<(u64, ExecutionState)> = view
        .source_owners
        .iter()
        .map(|o| (o.issue.display_number.unwrap(), o.execution_state.clone()))
        .collect();
    assert_eq!(
        owners,
        vec![
            (301, ExecutionState::Queued),
            (302, ExecutionState::Running),
            (303, ExecutionState::Paused),
            (306, ExecutionState::Ready),
            (307, ExecutionState::Waiting),
            (308, ExecutionState::Stopping),
            (309, ExecutionState::Uncertain),
        ]
    );
    // The unfiltered owned-issue set the coordinator turns into
    // request exclusions follows the same non-terminal rule. Owned
    // refs keep their stored display number — the protocol's
    // exclusion rule matches on it within the same repository —
    // ordered by scoped identity.
    let owned: Vec<(String, Option<u64>)> = store
        .transact(|tx| tx.intake_owned_issues())
        .unwrap()
        .iter()
        .map(|i| (i.issue.as_str().to_string(), i.display_number))
        .collect();
    assert_eq!(
        owned,
        vec![
            ("issue-301".to_string(), Some(301)),
            ("issue-302".to_string(), Some(302)),
            ("issue-303".to_string(), Some(303)),
            ("issue-306".to_string(), Some(306)),
            ("issue-307".to_string(), Some(307)),
            ("issue-308".to_string(), Some(308)),
            ("issue-309".to_string(), Some(309)),
        ]
    );
    // Source claims and queue positions stay separate facts: the
    // paused owner reports an owner row but no pending position.
    assert_eq!(pending_count(&store), 1);
    let _ = e1;
}

#[test]
fn preflight_is_read_only_and_non_reserving() {
    let store = SqliteStore::in_memory(1).unwrap();
    set_intake_settings(&store, settings("rev-1", 2), None).unwrap();
    accepted(&decision(&store, &submit("ro-1")));

    let before = store.transact(|tx| tx.last_seq()).unwrap();
    let view = preflight(&store, &[]).unwrap();
    assert_eq!(view.free_positions, 1);
    // The advisory read wrote nothing.
    assert_eq!(store.transact(|tx| tx.last_seq()).unwrap(), before);

    // It reserves nothing: the single free position is still decided
    // authoritatively at submit time.
    accepted(&decision(&store, &submit("ro-2")));
    assert_eq!(
        reject_reason(&decision(&store, &submit("ro-3"))),
        RejectionCode::CapacityExhausted
    );
}

#[test]
fn an_over_bound_preflight_scope_fails_explicitly() {
    let store = SqliteStore::in_memory(1).unwrap();
    let scope: Vec<IssueRef> = (0..=MAX_PREFLIGHT_SCOPE_ISSUES as u64).map(issue).collect();
    match preflight(&store, &scope) {
        Err(IntakePreflightError::ScopeTooLarge { issues, limit }) => {
            assert_eq!(issues, MAX_PREFLIGHT_SCOPE_ISSUES + 1);
            assert_eq!(limit, MAX_PREFLIGHT_SCOPE_ISSUES);
        }
        other => panic!("expected ScopeTooLarge, got {other:?}"),
    }

    // The bound itself is accepted.
    let at_bound: Vec<IssueRef> = (0..MAX_PREFLIGHT_SCOPE_ISSUES as u64).map(issue).collect();
    assert!(preflight(&store, &at_bound).is_ok());
}

// --- the v8 → current upgrade ---------------------------------------------

#[test]
fn a_v8_store_upgrades_to_the_current_format_additively() {
    let root =
        std::env::temp_dir().join(format!("workloop-intake-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let (work_id, _) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        set_intake_settings(&store, settings("rev-old", 6), None).unwrap();
        let (w, e) = accepted(&decision(&store, &submit("mig-1")));
        drop(store);
        (w, e)
    };

    // Downgrade to a v8 file: the settings table absent, stamp 8.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE intake_settings; PRAGMA user_version=8;")
            .unwrap();
    }

    let store = SqliteStore::open(&db, 1).unwrap();
    // Every pre-upgrade row survives.
    let work = store
        .transact(|tx| tx.work(&work_id))
        .unwrap()
        .expect("the pre-upgrade Work survives");
    assert_eq!(work.id, work_id);
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 17);

    // The new table exists and reads empty — the documented default
    // applies until a guarded write lands.
    assert!(intake_settings(&store).is_none());
    let view = preflight(&store, &[]).unwrap();
    assert_eq!(view.pending_queue_limit, DEFAULT_PENDING_QUEUE_LIMIT);

    // And the guarded writer works on the upgraded file.
    set_intake_settings(&store, settings("rev-new", 8), None).unwrap();
    assert_eq!(intake_settings(&store).unwrap().pending_queue_limit, 8);

    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_truncated_v8_store_is_still_refused() {
    let root = std::env::temp_dir().join(format!("workloop-intake-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    // A v8 stamp missing a base table is foreign — never reshaped.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE intake_settings; DROP TABLE works; PRAGMA user_version=8;")
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
fn a_current_stamp_missing_intake_settings_is_refused() {
    let root = std::env::temp_dir().join(format!("workloop-intake-current-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    // A file already stamped at the current format but missing the v9
    // table is foreign — the post-upgrade completeness check refuses
    // it rather than silently recreating schema under a current stamp.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE intake_settings;").unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { .. }) => {}
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("expected UnsupportedFormat, got Ok"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

// --- concurrent admission ------------------------------------------------

#[test]
fn concurrent_submits_never_exceed_the_pending_limit() {
    let root = std::env::temp_dir().join(format!("workloop-intake-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let store = SqliteStore::open(&db, 1).unwrap();
    set_intake_settings(&store, settings("rev-1", 3), None).unwrap();
    let config = config();

    let mut accepted_count = 0usize;
    let mut capacity_count = 0usize;
    let mut other = Vec::new();
    std::thread::scope(|s| {
        let store_ref = &store;
        let config_ref = &config;
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let request = submit(&format!("race-{i}"));
                s.spawn(move || handle_command(store_ref, config_ref, &request, NOW).unwrap())
            })
            .collect();
        for h in handles {
            match h.join().unwrap() {
                CommandDecision::Accepted { .. } => accepted_count += 1,
                CommandDecision::Rejected {
                    reason: RejectionCode::CapacityExhausted,
                    ..
                } => capacity_count += 1,
                other_decision => other.push(other_decision),
            }
        }
    });

    assert!(other.is_empty(), "unexpected decisions: {other:?}");
    assert_eq!(accepted_count, 3, "exactly the limit may be admitted");
    assert_eq!(capacity_count, 5);
    assert_eq!(pending_count(&store), 3);

    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn concurrent_settings_writers_cannot_clobber_each_other() {
    let root = std::env::temp_dir().join(format!("workloop-settings-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let store = SqliteStore::open(&db, 1).unwrap();
    set_intake_settings(&store, settings("rev-1", 5), None).unwrap();

    // Two writers that both observed rev-1 race to replace it: exactly
    // one wins, the loser conflicts, and the active record is always
    // one coherent (revision, limit) pair — never a torn write.
    let results: Vec<Result<(), StoreError>> = std::thread::scope(|s| {
        let a = s.spawn(|| set_intake_settings(&store, settings("rev-a", 8), Some("rev-1")));
        let b = s.spawn(|| set_intake_settings(&store, settings("rev-b", 9), Some("rev-1")));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(StoreError::ConstraintConflict { .. })))
        .count();
    assert_eq!(wins, 1, "exactly one stale-observer write may land");
    assert_eq!(conflicts, 1);

    let active = intake_settings(&store).unwrap();
    assert!(
        (active.settings_revision == "rev-a" && active.pending_queue_limit == 8)
            || (active.settings_revision == "rev-b" && active.pending_queue_limit == 9),
        "the surviving record is one coherent write: {active:?}"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn owned_issues_normalize_a_zero_display_number_for_the_wire() {
    let store = SqliteStore::in_memory(1).unwrap();
    // A stored ref may carry display_number 0 — the store accepts any
    // u64 — but exclusions must never emit it: the wire requires a
    // display number to be positive or absent.
    let (_w, _e) = accepted(&decision(
        &store,
        &submit_with_sources("zero-display", vec![source(0)]),
    ));
    let owned = store.transact(|tx| tx.intake_owned_issues()).unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].issue.as_str(), "issue-0");
    assert_eq!(owned[0].display_number, None);
}
