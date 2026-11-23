//! Conflict detection triggers: a mergeability conflict bound to the
//! exact `(head, base)` pair produces one bounded Fix-class request;
//! the same steady episode never mints a second trigger, and an
//! unbindable conflict escalates to Attention with its paths.

use std::collections::BTreeSet;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, ExecutionState, InvocationRole, LanePolicy,
    Origin, WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef,
    DiffTarget, HostActorId, HostActorRef, HostCapability, HostInstanceId, RepositoryId,
    RepositoryRef, RoleBinding,
};
use workloop_core::mergeability::{
    ConflictDisposition, ConflictEpisode, ConflictFixRequest, MergeabilityState, plan_conflict,
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

fn submit(
    store: &SqliteStore,
    config: &WorkloopConfig,
    key: &str,
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
                sources: vec![],
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
