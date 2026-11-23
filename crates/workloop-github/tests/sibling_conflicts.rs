//! A recorded managed merge re-checks the remaining open managed
//! changes on the same target and turns each newly-conflicted pair
//! into one bounded Fix trigger. The scan is bounded per pass and a
//! steady conflicted pair never mints a second trigger.

use std::collections::BTreeSet;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, InvocationRole, LanePolicy, Origin, WorkId,
    WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef,
    HostActorId, HostActorRef, HostCapability, HostInstanceId, RepositoryId, RepositoryRef,
    RoleBinding,
};
use workloop_core::mergeability::{MAX_CONFLICT_RECHECKS_PER_PASS, MergeabilityState};
use workloop_core::observation::{
    DeliveryId, DeliveryScope, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::adapter::GitHubAdapter;
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

mod support;
use support::InMemoryGitHub;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("conflict-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("conflict-repo"))
}

fn actor() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("publisher"),
    }
}

fn change(id: &str) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(id),
        display_number: Some(1),
    }
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: actor(),
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

fn submit(store: &SqliteStore, config: &WorkloopConfig, key: &str) -> WorkId {
    let decision = handle_command(
        store,
        config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "operator".into(),
            },
            origin: Origin("sibling-conflicts".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "sibling conflict contract".into(),
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

/// Record the durable `ChangeOpened` effect that attributes a change to
/// a Work — the stable ownership edge the sibling scan reads.
fn record_change(store: &SqliteStore, work: &WorkId, change: &ChangeRequestRef) {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id,
                    work_id: work.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: actor(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work.0),
                        target_branch: "main".into(),
                        title: "sibling".into(),
                        body: String::new(),
                    },
                    payload_digest: "sibling-conflict".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: Some(actor()),
                    effects: vec![NormalizedEffectFact::ChangeOpened {
                        change: change.clone(),
                        head: rev(&format!("{}-head", change.change_request.as_str())),
                        source_branch: format!("work/{}", work.0),
                        target_branch: "main".into(),
                    }],
                    created_seq,
                },
            })?;
            Ok(())
        })
        .unwrap();
}

fn mergeability_facts(store: &SqliteStore, work: &WorkId) -> usize {
    store
        .transact(|tx| tx.observations_for_work(work))
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::Mergeability { .. }))
        .count()
}

fn fix_requests(store: &SqliteStore, work: &WorkId) -> usize {
    store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .filter(|r| &r.work_id == work && r.role == InvocationRole::Fix)
        .count()
}

#[test]
fn a_recorded_merge_rechecks_siblings_and_one_steady_conflict_triggers_once() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "merged-work");
    let sibling_work = submit(&store, &config, "sibling-work");
    let merged = change("cr-merged");
    let sibling = change("cr-sibling");
    record_change(&store, &merged_work, &merged);
    record_change(&store, &sibling_work, &sibling);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_change_head(&sibling, "sibling-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    fixture.set_change_mergeability(
        &sibling,
        MergeabilityState::Conflicting,
        &["src/a.rs", "src/b.rs"],
    );

    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
    let request = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .find(|r| r.work_id == sibling_work && r.role == InvocationRole::Fix)
        .unwrap();
    assert_eq!(
        request.payload["source_revision"],
        serde_json::json!(rev("sibling-head"))
    );
    assert_eq!(
        request.payload["target_revision"],
        serde_json::json!(rev("new-base"))
    );
    assert_eq!(
        request.payload["conflicting_paths"],
        serde_json::json!(["src/a.rs", "src/b.rs"])
    );

    // A second pass re-observes the same merged state and the same
    // conflicted pair: no new fact, no second trigger.
    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
}

#[test]
fn sibling_rechecks_stop_at_the_per_pass_bound() {
    let store = SqliteStore::in_memory(2).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "merged-work-bound");
    let merged = change("cr-merged-bound");
    record_change(&store, &merged_work, &merged);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    let siblings = MAX_CONFLICT_RECHECKS_PER_PASS + 2;
    for index in 0..siblings {
        let work = submit(&store, &config, &format!("sibling-{index}"));
        let change = change(&format!("cr-sibling-{index}"));
        record_change(&store, &work, &change);
        fixture.set_change_head(&change, &format!("head-{index}"), &actor());
        fixture.set_change_mergeability(
            &change,
            MergeabilityState::Conflicting,
            &["src/shared.rs"],
        );
    }

    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, MAX_CONFLICT_RECHECKS_PER_PASS);
    assert!(
        report
            .deferred
            .iter()
            .any(|d| d.starts_with("merge-siblings:")),
        "the remainder is deferred, not silently dropped: {:?}",
        report.deferred
    );
}

#[test]
fn siblings_deferred_by_the_per_pass_bound_are_checked_on_a_later_pass() {
    // Enough capacity slots that every sibling's Fix request can be
    // admitted; the per-pass bound under test is the conflict re-check
    // budget, not the execution capacity gate.
    let store = SqliteStore::in_memory(32).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "merged-work-progress");
    let merged = change("cr-merged-progress");
    record_change(&store, &merged_work, &merged);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    let siblings = MAX_CONFLICT_RECHECKS_PER_PASS + 2;
    let mut works = Vec::new();
    for index in 0..siblings {
        let work = submit(&store, &config, &format!("progress-sibling-{index}"));
        let change = change(&format!("cr-progress-sibling-{index}"));
        record_change(&store, &work, &change);
        fixture.set_change_head(&change, &format!("progress-head-{index}"), &actor());
        fixture.set_change_mergeability(
            &change,
            MergeabilityState::Conflicting,
            &["src/shared.rs"],
        );
        works.push(work);
    }

    let first = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.sibling_conflicts, MAX_CONFLICT_RECHECKS_PER_PASS);
    for (index, work) in works
        .iter()
        .enumerate()
        .skip(MAX_CONFLICT_RECHECKS_PER_PASS)
    {
        assert_eq!(mergeability_facts(&store, work), 0, "sibling {index} waits");
        assert_eq!(fix_requests(&store, work), 0, "sibling {index} waits");
    }

    // The second pass re-examines the already-recorded siblings, but
    // those duplicates must not consume the per-pass bound: the
    // deferred siblings are reached and triggered.
    fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    for (index, work) in works.iter().enumerate() {
        assert_eq!(
            mergeability_facts(&store, work),
            1,
            "sibling {index} is observed"
        );
        assert_eq!(
            fix_requests(&store, work),
            1,
            "sibling {index} triggers exactly once"
        );
    }
}

fn ingest_head(
    store: &SqliteStore,
    config: &WorkloopConfig,
    work: &WorkId,
    change: &ChangeRequestRef,
    head: &str,
) {
    workloop_core::work::handle_observation(
        store,
        config,
        &Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "sibling-test".into(),
                event_id: format!("head:{}:{}", work.0, change.change_request.as_str()),
            }),
            subject_work_id: Some(work.clone()),
            subject_execution_id: None,
            fact: ObservationFact::ChangeHead {
                change: change.clone(),
                head: rev(head),
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

#[test]
fn the_production_adapter_rechecks_siblings_after_a_merged_read() {
    use workloop_github::api::GitHubApi;

    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "adapter-merged-work");
    let sibling_work = submit(&store, &config, "adapter-sibling-work");

    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "work/merged", "merged-head");
    api.seed_branch(&repo(), "work/sibling", "sibling-head");
    api.seed_branch(&repo(), "main", "old-base");
    let merged_pull = api
        .open_pull(&repo(), "work/merged", "main", "merged", "")
        .unwrap();
    let sibling_pull = api
        .open_pull(&repo(), "work/sibling", "main", "sibling", "")
        .unwrap();
    let merged = change(&merged_pull.id);
    let sibling = change(&sibling_pull.id);
    record_change(&store, &merged_work, &merged);
    record_change(&store, &sibling_work, &sibling);
    ingest_head(&store, &config, &merged_work, &merged, "merged-head");
    ingest_head(&store, &config, &sibling_work, &sibling, "sibling-head");
    api.seed_branch(&repo(), "main", "new-base");
    api.set_pull_merged(&repo(), &merged_pull.id);
    api.set_pull_mergeability(
        &repo(),
        &sibling_pull.id,
        Some(false),
        "dirty",
        "new-base",
        &["src/a.rs"],
    );

    let mut adapter = GitHubAdapter::new(api);
    let report = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
    let requests = store.transact(|tx| tx.pending_requests()).unwrap();
    let request = requests
        .iter()
        .find(|r| r.work_id == sibling_work && r.role == InvocationRole::Fix)
        .unwrap();
    assert_eq!(
        request.payload["source_revision"],
        serde_json::json!(rev("sibling-head"))
    );
    assert_eq!(
        request.payload["target_revision"],
        serde_json::json!(rev("new-base"))
    );
}

#[test]
fn the_production_adapter_makes_progress_on_deferred_siblings() {
    use workloop_github::api::GitHubApi;

    // Enough capacity slots for every sibling's Fix request; the bound
    // under test is the conflict re-check budget.
    let store = SqliteStore::in_memory(32).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "adapter-progress-merged");
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "work/merged-progress", "merged-progress-head");
    api.seed_branch(&repo(), "main", "old-base");
    let merged_pull = api
        .open_pull(
            &repo(),
            "work/merged-progress",
            "main",
            "merged-progress",
            "",
        )
        .unwrap();
    let merged = change(&merged_pull.id);
    record_change(&store, &merged_work, &merged);
    ingest_head(
        &store,
        &config,
        &merged_work,
        &merged,
        "merged-progress-head",
    );

    let siblings = MAX_CONFLICT_RECHECKS_PER_PASS + 2;
    let mut works = Vec::new();
    for index in 0..siblings {
        let work = submit(
            &store,
            &config,
            &format!("adapter-progress-sibling-{index}"),
        );
        api.seed_branch(
            &repo(),
            &format!("work/progress-{index}"),
            &format!("progress-head-{index}"),
        );
        let pull = api
            .open_pull(
                &repo(),
                &format!("work/progress-{index}"),
                "main",
                &format!("progress-{index}"),
                "",
            )
            .unwrap();
        let sibling = change(&pull.id);
        record_change(&store, &work, &sibling);
        ingest_head(
            &store,
            &config,
            &work,
            &sibling,
            &format!("progress-head-{index}"),
        );
        works.push((work, pull.id));
    }
    api.seed_branch(&repo(), "main", "new-base");
    api.set_pull_merged(&repo(), &merged_pull.id);
    for (_, pull_id) in &works {
        api.set_pull_mergeability(
            &repo(),
            pull_id,
            Some(false),
            "dirty",
            "new-base",
            &["src/shared.rs"],
        );
    }

    let mut adapter = GitHubAdapter::new(api);
    let first = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.sibling_conflicts, MAX_CONFLICT_RECHECKS_PER_PASS);
    for (index, (work, _)) in works
        .iter()
        .enumerate()
        .skip(MAX_CONFLICT_RECHECKS_PER_PASS)
    {
        assert_eq!(mergeability_facts(&store, work), 0, "sibling {index} waits");
    }

    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    for (index, (work, _)) in works.iter().enumerate() {
        assert_eq!(
            mergeability_facts(&store, work),
            1,
            "sibling {index} is observed"
        );
        assert_eq!(
            fix_requests(&store, work),
            1,
            "sibling {index} triggers exactly once"
        );
    }
}
