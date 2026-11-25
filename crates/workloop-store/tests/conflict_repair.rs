//! Conflict detection triggers: a mergeability conflict bound to the
//! exact `(head, base)` pair produces one bounded Fix-class request;
//! the same steady episode never mints a second trigger, and an
//! unbindable conflict escalates to Attention with its paths.

use std::collections::BTreeSet;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, DesiredAction, ExecutionBudget, ExecutionState,
    InvocationRole, LanePolicy, Origin, RepairPrerequisite, WorkPolicy, WorkUsage,
};
use workloop_core::identity::{
    ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef,
    DiffTarget, HostActorId, HostActorRef, HostCapability, HostInstanceId, IssueId, IssueRef,
    RepositoryId, RepositoryRef, RoleBinding, SourceSnapshot,
};
use workloop_core::mergeability::{
    ConflictDisposition, ConflictEpisode, ConflictFixRequest, MAX_CONFLICT_PATH_CHARS,
    MAX_CONFLICT_PATHS, MergeabilityState, plan_conflict,
};
use workloop_core::observation::{DeliveryId, DeliveryScope, Observation, ObservationFact};
use workloop_core::projection;
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command, handle_observation};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("conflict-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("conflict-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-conflict"),
        display_number: Some(7),
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
                    host: host(),
                    actor: HostActorId::trusted("publisher"),
                },
                credential_ref: CredentialRef::trusted("default"),
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

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn submit(
    store: &SqliteStore,
    config: &WorkloopConfig,
    key: &str,
) -> workloop_core::domain::WorkId {
    submit_with_sources(store, config, key, vec![])
}

fn submit_with_sources(
    store: &SqliteStore,
    config: &WorkloopConfig,
    key: &str,
    sources: Vec<SourceSnapshot>,
) -> workloop_core::domain::WorkId {
    let decision = handle_command(
        store,
        config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "operator".into(),
            },
            origin: Origin("conflict-tests".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "conflict repair contract".into(),
                sources,
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    let CommandDecision::Accepted {
        work_id: Some(work_id),
        ..
    } = decision
    else {
        panic!("submit accepted: {decision:?}");
    };
    work_id
}

fn finish(store: &SqliteStore, work_id: &workloop_core::domain::WorkId) {
    store
        .transact(|tx| {
            let execution = tx.active_execution(work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Succeeded;
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            tx.apply(&Effect::ReleaseSlot {
                execution_id: execution.id,
            })?;
            Ok(())
        })
        .unwrap();
}

fn mergeability(
    event: &str,
    work_id: &workloop_core::domain::WorkId,
    target: DiffTarget,
    state: MergeabilityState,
    paths: &[&str],
) -> Observation {
    Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "conflict-test".into(),
            event_id: event.into(),
        }),
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: None,
        fact: ObservationFact::Mergeability {
            target,
            state,
            conflicting_paths: paths.iter().map(|p| (*p).to_string()).collect(),
            reason: (state == MergeabilityState::Unknown).then(|| "host unknown".into()),
        },
        observed_unix_seconds: NOW,
    }
}

#[test]
fn an_episode_identity_binds_both_revisions() {
    let episode = ConflictEpisode::new(change(), rev("head-1"), rev("base-2")).unwrap();
    let request = ConflictFixRequest::new(episode.clone(), vec!["src/a.rs".into()]);
    assert_eq!(request.role(), InvocationRole::Fix);
    assert_eq!(request.source_revision().object, "head-1");
    assert_eq!(request.target_revision().object, "base-2");
    assert_eq!(
        request.payload["source_revision"],
        serde_json::json!(rev("head-1"))
    );
    assert_eq!(
        request.payload["target_revision"],
        serde_json::json!(rev("base-2"))
    );
    assert_eq!(request.payload["head"], serde_json::json!(rev("head-1")));
    assert_eq!(request.payload["base"], serde_json::json!(rev("base-2")));
    assert_eq!(request.payload["episode_id"], episode.episode_id());

    // A different base tip on the same head is a different episode.
    let advanced = ConflictEpisode::new(change(), rev("head-1"), rev("base-3")).unwrap();
    assert_ne!(episode.episode_id(), advanced.episode_id());
}

#[test]
fn a_conflict_without_a_bindable_pair_escalates_with_its_paths() {
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    let disposition = plan_conflict(
        Some(&target),
        MergeabilityState::Conflicting,
        &["src/a.rs".into(), "src/b.rs".into()],
        None,
    );
    let ConflictDisposition::Escalate(escalation) = disposition else {
        panic!("missing base must escalate, got {disposition:?}");
    };
    assert_eq!(escalation.paths, ["src/a.rs", "src/b.rs"]);
    assert!(escalation.reason.contains("source and target revision"));
}

#[test]
fn a_conflict_mints_one_fix_request_and_repeats_do_not_mint_again() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-work");
    finish(&store, &work_id);
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-1",
            &work_id,
            target.clone(),
            MergeabilityState::Conflicting,
            &["src/a.rs", "src/b.rs"],
        ),
        NOW,
    )
    .unwrap();

    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(requests.len(), 1, "one bounded fix request: {requests:?}");
    let request = &requests[0];
    assert_eq!(request.role, InvocationRole::Fix);
    assert_eq!(
        request.payload["kind"],
        serde_json::json!("conflict_repair")
    );
    assert_eq!(
        request.payload["source_revision"],
        serde_json::json!(rev("head-1"))
    );
    assert_eq!(
        request.payload["target_revision"],
        serde_json::json!(rev("base-2"))
    );
    assert_eq!(
        request.payload["conflicting_paths"],
        serde_json::json!(["src/a.rs", "src/b.rs"])
    );

    // The steady conflicted state is the same episode even under a new
    // delivery identity: it must not mint a second trigger.
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-2",
            &work_id,
            target.clone(),
            MergeabilityState::Conflicting,
            &["src/a.rs", "src/b.rs"],
        ),
        NOW + 1,
    )
    .unwrap();
    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the steady conflict stays idempotent: {requests:?}"
    );
    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    assert_eq!(executions.len(), 2, "one terminal + one repair successor");

    // Complete the first repair, then advance the target tip: the
    // advanced base is a new episode and may trigger a fresh repair.
    finish(&store, &work_id);
    let advanced = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-3")),
        target_revision: Some(rev("base-3")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-3",
            &work_id,
            advanced,
            MergeabilityState::Conflicting,
            &["src/a.rs"],
        ),
        NOW + 2,
    )
    .unwrap();
    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    assert_eq!(executions.len(), 3, "advanced base is a new episode");
}

#[test]
fn an_unbindable_conflict_escalates_to_attention_with_paths() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-unbindable");
    finish(&store, &work_id);
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "unbindable-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/a.rs", "src/b.rs"],
        ),
        NOW,
    )
    .unwrap();

    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    let current = executions.last().expect("escalation execution");
    assert_eq!(current.state, ExecutionState::Paused);
    let reason = current.pause_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("src/a.rs"), "{reason}");
    assert!(reason.contains("src/b.rs"), "{reason}");

    // A steady unbindable conflict is still one episode: another scan
    // must not stack a second paused successor.
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "unbindable-2",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/a.rs", "src/b.rs"],
        ),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(
        store
            .transact(|tx| tx.executions_for_work(&work_id))
            .unwrap()
            .len(),
        2,
        "the steady unbindable conflict does not escalate twice"
    );

    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert_eq!(value.len(), 1, "one conflict escalation: {value:?}");
    assert_eq!(value[0].reason, "paused_judgment");
}

#[test]
fn an_automatic_time_exhausted_work_escalates_a_conflict_instead_of_failing_terminal() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-timeout");
    finish(&store, &work_id);
    // First dispatch was more than the default four hours ago, so the
    // Work-wide automatic-time budget is already spent when the merge
    // is observed now.
    store
        .transact(|tx| {
            let mut usage = tx
                .work_usage(&work_id)?
                .unwrap_or_else(|| WorkUsage::new(&work_id));
            usage.first_dispatch_unix_seconds = Some(NOW - 5 * 3600);
            tx.apply(&Effect::UpdateWorkUsage { usage })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-timeout-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/a.rs"],
        ),
        NOW,
    )
    .unwrap();

    // The exhausted budget forbids a new automatic dispatch, but the
    // conflict is never terminally failed with a bare budget code: it
    // is held for an operator decision carrying the conflict paths.
    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        0,
        "no Fix request is admitted past the automatic-time ceiling"
    );
    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    assert!(
        executions
            .iter()
            .all(|execution| execution.state != ExecutionState::Failed),
        "the completed Work is not failed with automatic_time_exhausted: {executions:?}"
    );
    let current = executions.last().expect("escalation execution");
    assert_eq!(current.state, ExecutionState::Paused);
    let reason = current.pause_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("automatic time"), "{reason}");
    assert!(reason.contains("src/a.rs"), "{reason}");
}

#[test]
fn a_fix_budget_exhausted_work_escalates_a_conflict_with_its_paths() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-fix-bound");
    finish(&store, &work_id);
    // The Work-wide fix bound is already spent when the merge is
    // observed: admission would hold the successor under a bare
    // `fix_budget_exhausted`, but the recorded episode never
    // re-triggers — the hold must carry the conflict cause and paths.
    store
        .transact(|tx| {
            let mut usage = tx
                .work_usage(&work_id)?
                .unwrap_or_else(|| WorkUsage::new(&work_id));
            usage.fix_invocations_used = config.policy.max_fix_invocations;
            tx.apply(&Effect::UpdateWorkUsage { usage })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-fix-bound-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/a.rs"],
        ),
        NOW,
    )
    .unwrap();

    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        0,
        "no Fix request is admitted past the Work-wide fix bound"
    );
    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    let current = executions.last().expect("escalation execution");
    assert_eq!(current.state, ExecutionState::Paused);
    let reason = current.pause_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("fix invocation"), "{reason}");
    assert!(reason.contains("src/a.rs"), "{reason}");
}

#[test]
fn a_conflict_while_the_active_execution_is_paused_is_recorded_not_dropped() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-paused");
    // A human-held pause already carries its own recorded reason: the
    // new conflict episode must not overwrite it — but it must not
    // drop silently either, since an identical re-observation dedupes
    // at ingest and could never mint the hold again.
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("repair: earlier operator hold".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-paused-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/c.rs"],
        ),
        NOW + 1,
    )
    .unwrap();

    // No repair is minted and the held pause keeps its recorded path…
    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    assert_eq!(executions.len(), 1);
    let current = &executions[0];
    assert_eq!(current.state, ExecutionState::Paused);
    assert_eq!(
        current.pause_reason.as_deref(),
        Some("repair: earlier operator hold")
    );
    assert_eq!(
        store.transact(|tx| tx.pending_requests()).unwrap().len(),
        0,
        "a held Work mints no automatic repair"
    );
    // …but the escalation is not dropped: the decision ledger carries
    // the conflict reason and paths.
    let decisions = store
        .transact(|tx| tx.recovery_decisions(&work_id))
        .unwrap();
    assert_eq!(decisions.len(), 1, "one recorded escalation: {decisions:?}");
    let decision = &decisions[0];
    assert_eq!(decision.classification, "conflict_unresolvable");
    assert_eq!(
        decision.prerequisites,
        vec![RepairPrerequisite::OperatorDecision]
    );
    assert!(decision.detail.contains("src/c.rs"), "{}", decision.detail);
}

#[test]
fn a_conflict_while_the_active_execution_is_stopping_is_recorded_not_dropped() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-stopping");
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Stopping;
            next.desired_action = Some(DesiredAction::Pause);
            next.pause_reason = Some("repair: earlier operator hold".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-stopping-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/d.rs"],
        ),
        NOW + 1,
    )
    .unwrap();

    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    assert_eq!(executions.len(), 1);
    let current = &executions[0];
    assert_eq!(current.state, ExecutionState::Stopping);
    assert_eq!(
        current.pause_reason.as_deref(),
        Some("repair: earlier operator hold")
    );
    let decisions = store
        .transact(|tx| tx.recovery_decisions(&work_id))
        .unwrap();
    assert_eq!(decisions.len(), 1, "one recorded escalation: {decisions:?}");
    let decision = &decisions[0];
    assert_eq!(decision.classification, "conflict_unresolvable");
    assert!(decision.detail.contains("src/d.rs"), "{}", decision.detail);
}

#[test]
fn a_conflict_escalation_for_a_reclaimed_issue_records_the_hold() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let source = |number: u64| SourceSnapshot {
        issue: issue(number),
        source_revision: "rev-1".into(),
        content_digest: "digest-1".into(),
    };
    // The completed Work's issue claim was released at terminal
    // settlement; a newer Work now owns it — the exact state an
    // escalation arrives into.
    let owner = submit_with_sources(&store, &config, "conflict-owner-1", vec![source(7)]);
    finish(&store, &owner);
    store
        .transact(|tx| {
            tx.apply(&Effect::ReleaseIssueClaims {
                work_id: owner.clone(),
            })
        })
        .unwrap();
    let holder = submit_with_sources(&store, &config, "conflict-owner-2", vec![source(7)]);
    assert_eq!(
        store.transact(|tx| tx.issue_claim(&issue(7))).unwrap(),
        Some(holder.clone()),
        "the newer Work owns the reclaimed issue"
    );

    // An unbindable conflict for the completed Work escalates: the
    // paused successor holds no claims, so the ingest must not roll
    // back on the owner row — the hold is recorded, the claim stays.
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-owner-esc",
            &owner,
            target,
            MergeabilityState::Conflicting,
            &["src/a.rs"],
        ),
        NOW + 1,
    )
    .unwrap();
    let executions = store.transact(|tx| tx.executions_for_work(&owner)).unwrap();
    let current = executions.last().expect("escalation successor");
    assert_eq!(current.state, ExecutionState::Paused);
    let reason = current.pause_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("src/a.rs"), "{reason}");
    assert_eq!(
        store.transact(|tx| tx.issue_claim(&issue(7))).unwrap(),
        Some(holder),
        "the live Work's claim is untouched by the escalation"
    );
}

#[test]
fn a_conflict_while_paused_surfaces_attention_with_the_conflict_detail() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-paused-attention");
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("repair: earlier operator hold".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    // The earlier hold's attention was already acknowledged — the new
    // conflict episode must still reach the operator with its own
    // reason and paths.
    let acknowledged = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW))
        .unwrap();
    let workloop_core::projection::QueryResult::Found {
        value: acknowledged,
        ..
    } = acknowledged
    else {
        panic!("attention projection found");
    };
    for view in &acknowledged {
        assert_eq!(view.reason, "paused_judgment");
        store
            .transact(|tx| {
                tx.apply(&Effect::InsertAttentionAck {
                    work_id: work_id.clone(),
                    attention_id: view.attention_id.clone(),
                    actor_id: "operator".into(),
                })
            })
            .unwrap();
    }
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-paused-attention-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/c.rs"],
        ),
        NOW + 1,
    )
    .unwrap();

    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 2))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    let conflict = value
        .iter()
        .find(|view| view.reason == "conflict_unresolvable")
        .unwrap_or_else(|| panic!("the escalated conflict surfaces on Attention: {value:?}"));
    assert!(
        conflict.evidence.iter().any(|e| e.contains("src/c.rs")),
        "the surfaced attention carries the conflict paths: {conflict:?}"
    );
}

#[test]
fn a_moved_conflict_escalation_does_not_double_surface() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-moved-attention");
    // An unbindable conflict on an active Execution escalates by moving
    // it: the pause reason already carries the detail, so the decision
    // ledger entry must not mint a second Attention for the same hold.
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-moved-attention-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/e.rs"],
        ),
        NOW,
    )
    .unwrap();

    let executions = store
        .transact(|tx| tx.executions_for_work(&work_id))
        .unwrap();
    let current = executions.last().expect("escalation execution");
    assert_eq!(current.state, ExecutionState::Paused);
    let reason = current.pause_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("src/e.rs"), "{reason}");

    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert_eq!(value.len(), 1, "one surfaced hold: {value:?}");
    assert_eq!(value[0].reason, "paused_judgment");
}

#[test]
fn a_resolved_conflict_escalation_does_not_resurface_after_resume() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-resume-attention");
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("repair: earlier operator hold".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-resume-attention-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/c.rs"],
        ),
        NOW + 1,
    )
    .unwrap();
    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 2))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert!(
        value.iter().any(|v| v.reason == "conflict_unresolvable"),
        "the held escalation surfaces while the Execution stays paused: {value:?}"
    );

    // The operator resumes the paused Execution: the hold clears, the
    // execution requeues — production `paused_requeue_effects` also
    // re-stamps `enqueued_seq`, retiring the queue generation the
    // escalation was recorded under. The recorded decision no longer
    // describes an escalation that could not move the Execution, so it
    // must not resurface as a ghost Attention for the rest of its
    // lifetime.
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Queued;
            next.enqueued_seq = tx.alloc_seq()?;
            next.pause_reason = None;
            next.repair_prerequisites = vec![];
            next.paused_since_unix_seconds = None;
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 3))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert!(
        value.iter().all(|v| v.reason != "conflict_unresolvable"),
        "a resolved escalation does not resurface after resume: {value:?}"
    );
}

#[test]
fn host_reported_conflict_paths_are_bounded_before_they_reach_durable_state() {
    // The host controls the path list: an over-limit report must be
    // trimmed to the same bound the human-facing pause reason applies
    // before it reaches the request payload, the escalation record, or
    // the worker prompt — an unbounded report cannot grow durable
    // state or the prompt without bound.
    let episode = ConflictEpisode::new(change(), rev("head-1"), rev("base-2")).unwrap();
    let paths: Vec<String> = (0..40).map(|i| format!("src/path-{i:02}.rs")).collect();
    let request = ConflictFixRequest::new(episode, paths);
    assert_eq!(request.paths.len(), MAX_CONFLICT_PATHS);
    assert_eq!(
        request.payload["conflicting_paths"],
        serde_json::json!(request.paths),
        "the payload carries the same bounded set"
    );

    // The character cap drops a single over-long entry but keeps the
    // smaller ones that still fit within the bound.
    let paths = vec![
        "x".repeat(MAX_CONFLICT_PATH_CHARS + 1),
        "src/kept.rs".into(),
    ];
    let request = ConflictFixRequest::new(
        ConflictEpisode::new(change(), rev("head-1"), rev("base-2")).unwrap(),
        paths,
    );
    assert_eq!(request.paths, ["src/kept.rs"]);

    // An unbindable conflict escalates under the same bound.
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: None,
        target_revision: None,
    };
    let paths: Vec<String> = (0..40).map(|i| format!("src/path-{i:02}.rs")).collect();
    let ConflictDisposition::Escalate(escalation) =
        plan_conflict(Some(&target), MergeabilityState::Conflicting, &paths, None)
    else {
        panic!("missing base must escalate");
    };
    assert_eq!(escalation.paths.len(), MAX_CONFLICT_PATHS);
}

#[test]
fn a_stored_mergeability_fact_is_bounded_at_the_ingest_boundary() {
    // The ingest boundary — not the producer — is the choke point: a
    // Mergeability fact carrying an over-limit path list and a giant
    // reason must be stored only in bounded form, so any producer that
    // skips adapter-side construction is still covered.
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "mergeability-sink-bound");
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    let path_strings: Vec<String> = (0..40).map(|i| format!("src/path-{i:02}.rs")).collect();
    let path_refs: Vec<&str> = path_strings.iter().map(String::as_str).collect();
    let mut obs = mergeability(
        "sink-bound-1",
        &work_id,
        target,
        MergeabilityState::Conflicting,
        &path_refs,
    );
    if let ObservationFact::Mergeability { reason, .. } = &mut obs.fact {
        *reason = Some("r".repeat(MAX_CONFLICT_PATH_CHARS * 2));
    }
    handle_observation(&store, &config, &obs, NOW + 1).unwrap();

    let stored = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    let ObservationFact::Mergeability {
        conflicting_paths,
        reason,
        ..
    } = &stored
        .iter()
        .find(|o| matches!(o.fact, ObservationFact::Mergeability { .. }))
        .expect("mergeability observation recorded")
        .fact
    else {
        unreachable!()
    };
    assert!(conflicting_paths.len() <= MAX_CONFLICT_PATHS);
    assert!(
        conflicting_paths
            .iter()
            .map(|p| p.chars().count())
            .sum::<usize>()
            <= MAX_CONFLICT_PATH_CHARS
    );
    assert_eq!(
        reason.as_deref().map(|r| r.chars().count()),
        Some(MAX_CONFLICT_PATH_CHARS),
        "the stored reason is truncated at the ingest boundary"
    );
}

#[test]
fn a_prior_episode_escalation_does_not_resurface_after_a_later_pause() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "conflict-ghost-attention");
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("repair: earlier operator hold".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let target = DiffTarget {
        change: change(),
        head: rev("head-1"),
        base: Some(rev("base-2")),
        target_revision: Some(rev("base-2")),
    };
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-ghost-1",
            &work_id,
            target,
            MergeabilityState::Conflicting,
            &["src/c.rs"],
        ),
        NOW + 1,
    )
    .unwrap();
    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 2))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert!(
        value.iter().any(|v| v.reason == "conflict_unresolvable"),
        "the held escalation surfaces while the Execution stays paused: {value:?}"
    );

    // The operator resumes: the real resume re-stamps `enqueued_seq`,
    // so the recorded decision belongs to the hold that just cleared.
    // A later pause for an unrelated reason is a new episode — the old
    // decision must not resurface as a ghost Attention on it.
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Queued;
            next.pause_reason = None;
            next.repair_prerequisites = vec![];
            next.paused_since_unix_seconds = None;
            next.enqueued_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    store
        .transact(|tx| {
            let execution = tx.active_execution(&work_id)?.expect("active execution");
            let mut next = execution.clone();
            next.state = ExecutionState::Paused;
            next.pause_reason = Some("budget exhausted: needs top-up".into());
            next.repair_prerequisites = vec![RepairPrerequisite::OperatorDecision];
            next.paused_since_unix_seconds = Some(NOW + 3);
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: execution.state,
            })?;
            Ok(())
        })
        .unwrap();
    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 4))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert!(
        value.iter().all(|v| v.reason != "conflict_unresolvable"),
        "a prior-episode escalation does not resurface on a later hold: {value:?}"
    );

    // A fresh conflict during the new hold is a decision recorded after
    // the latest queueing: it still surfaces on its own.
    handle_observation(
        &store,
        &config,
        &mergeability(
            "conflict-ghost-2",
            &work_id,
            DiffTarget {
                change: change(),
                head: rev("head-1"),
                base: Some(rev("base-3")),
                target_revision: Some(rev("base-3")),
            },
            MergeabilityState::Conflicting,
            &["src/d.rs"],
        ),
        NOW + 5,
    )
    .unwrap();
    let attention = store
        .transact(|tx| projection::attention(tx, Some(&work_id), NOW + 6))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention projection found");
    };
    assert!(
        value.iter().any(|v| v.reason == "conflict_unresolvable"),
        "a conflict in the current hold still surfaces: {value:?}"
    );
}
