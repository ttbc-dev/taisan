//! A recorded managed merge re-checks the remaining open managed
//! changes on the same target and turns each newly-conflicted pair
//! into one bounded Fix trigger. The scan is bounded per pass and a
//! steady conflicted pair never mints a second trigger.

use std::collections::BTreeSet;

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, ExecutionState, InvocationRole, LanePolicy,
    Origin, WorkId, WorkPolicy,
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
fn historical_merges_on_one_target_are_scanned_once() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();
    // Three historical merges already landed on `main`; each would have
    // triggered its own full scan if the recheck were per merge.
    for index in 0..3 {
        let work = submit(&store, &config, &format!("history-merged-{index}"));
        let change = change(&format!("cr-history-merged-{index}"));
        record_change(&store, &work, &change);
        fixture.set_change_head(&change, &format!("history-head-{index}"), &actor());
        fixture.set_merged(&change);
    }
    let sibling_work = submit(&store, &config, "history-sibling");
    let sibling = change("cr-history-sibling");
    record_change(&store, &sibling_work, &sibling);
    fixture.set_change_head(&sibling, "history-sibling-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_change_mergeability(&sibling, MergeabilityState::Conflicting, &["src/shared.rs"]);

    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert!(
        report.merge_rechecks <= 2,
        "one merge scan plus its one sibling, not one per historical merge: {report:?}"
    );
    assert_eq!(
        report.sibling_conflicts, 1,
        "the conflicting sibling is observed once, not once per merge: {report:?}"
    );
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);

    // A later pass still scans the target once and never mints a second
    // trigger for the steady conflicted pair.
    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert!(report.merge_rechecks <= 2, "{report:?}");
    assert_eq!(report.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
}

#[test]
fn the_production_adapter_does_not_rescan_each_historical_merge() {
    use workloop_github::api::GitHubApi;

    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    // Three historical merges on `main`, each with its own pull and
    // recorded head — the accumulated history the recheck must not
    // multiply.
    let mut merged_pulls = Vec::new();
    for index in 0..3 {
        let work = submit(&store, &config, &format!("adapter-history-{index}"));
        let branch = format!("work/history-{index}");
        let head = format!("history-head-{index}");
        api.seed_branch(&repo(), &branch, &head);
        let pull = api
            .open_pull(&repo(), &branch, "main", &format!("history-{index}"), "")
            .unwrap();
        let merged = change(&pull.id);
        record_change(&store, &work, &merged);
        ingest_head(&store, &config, &work, &merged, &head);
        merged_pulls.push(pull.id);
    }
    let sibling_work = submit(&store, &config, "adapter-history-sibling");
    api.seed_branch(&repo(), "work/history-sibling", "history-sibling-head");
    let sibling_pull = api
        .open_pull(
            &repo(),
            "work/history-sibling",
            "main",
            "history-sibling",
            "",
        )
        .unwrap();
    let sibling = change(&sibling_pull.id);
    record_change(&store, &sibling_work, &sibling);
    ingest_head(
        &store,
        &config,
        &sibling_work,
        &sibling,
        "history-sibling-head",
    );
    api.seed_branch(&repo(), "main", "new-base");
    for pull_id in &merged_pulls {
        api.set_pull_merged(&repo(), pull_id);
    }
    api.set_pull_mergeability(
        &repo(),
        &sibling_pull.id,
        Some(false),
        "dirty",
        "new-base",
        &["src/shared.rs"],
    );

    let mut adapter = GitHubAdapter::new(api);
    let first = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert!(
        first.merge_rechecks <= 2,
        "three historical merges on one target take one scan: {first:?}"
    );
    assert_eq!(
        first.sibling_conflicts, 1,
        "the conflicting sibling is observed once, not once per merge: {first:?}"
    );

    let second = adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert!(
        second.merge_rechecks <= 2,
        "a recorded merge is not re-read and the target is scanned once: {second:?}"
    );
    assert_eq!(second.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
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

    // A steady pass re-reads the leading siblings, and every verdict
    // read consumes the per-pass bound — duplicates included — so the
    // deferred tail keeps waiting while the leading set is unchanged.
    let second = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(second.sibling_conflicts, MAX_CONFLICT_RECHECKS_PER_PASS);
    for (index, work) in works
        .iter()
        .enumerate()
        .skip(MAX_CONFLICT_RECHECKS_PER_PASS)
    {
        assert_eq!(mergeability_facts(&store, work), 0, "sibling {index} waits");
    }

    // The tail is reached once the leading set leaves the open list:
    // the first repairs landed and those changes merged, so the next
    // pass's bound reaches the deferred siblings.
    for index in 0..MAX_CONFLICT_RECHECKS_PER_PASS {
        fixture.set_merged(&change(&format!("cr-progress-sibling-{index}")));
    }
    let third = fixture
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(
        third.sibling_conflicts,
        siblings - MAX_CONFLICT_RECHECKS_PER_PASS
    );
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

#[test]
fn the_pass_deadline_defers_the_merge_recheck() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "deadline-merged");
    let sibling_work = submit(&store, &config, "deadline-sibling");
    let merged = change("cr-deadline-merged");
    let sibling = change("cr-deadline-sibling");
    record_change(&store, &merged_work, &merged);
    record_change(&store, &sibling_work, &sibling);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_change_head(&sibling, "sibling-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    fixture.set_change_mergeability(&sibling, MergeabilityState::Conflicting, &["src/shared.rs"]);

    // The clock advances on every read: the pass begins inside the
    // deadline and crosses it mid-pass, so the explicit bound must
    // defer the merge re-check on the live clock — a comparison
    // against the refresh-start timestamp could never fire.
    let mut tick = 0i64;
    let mut clock = move || {
        tick += 1;
        NOW + tick
    };
    let report = fixture
        .refresh_sources(&store, &config, &mut clock, Some(NOW + 4), None)
        .unwrap();
    assert!(
        report.deferred.iter().any(|d| d.starts_with("merge:")),
        "the merge re-check defers once the live clock passes the deadline: {:?}",
        report.deferred
    );
    assert_eq!(mergeability_facts(&store, &sibling_work), 0);
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

    // A steady pass re-reads the leading siblings, and every verdict
    // read consumes the per-pass bound — duplicates included — so the
    // deferred tail keeps waiting while the leading set is unchanged.
    let second = adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(second.sibling_conflicts, MAX_CONFLICT_RECHECKS_PER_PASS);
    for (index, (work, _)) in works
        .iter()
        .enumerate()
        .skip(MAX_CONFLICT_RECHECKS_PER_PASS)
    {
        assert_eq!(mergeability_facts(&store, work), 0, "sibling {index} waits");
    }

    // The tail is reached once the leading set leaves the open list:
    // the first repairs landed and those pulls merged, so the next
    // pass's bound reaches the deferred siblings.
    for (_, pull_id) in works.iter().take(MAX_CONFLICT_RECHECKS_PER_PASS) {
        adapter.client_mut().set_pull_merged(&repo(), pull_id);
    }
    let third = adapter
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(
        third.sibling_conflicts,
        works.len() - MAX_CONFLICT_RECHECKS_PER_PASS
    );
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

#[test]
fn the_pass_deadline_defers_sibling_rechecks_mid_scan() {
    use workloop_github::api::GitHubApi;

    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "deadline-adapter-merged");
    let sibling_work = submit(&store, &config, "deadline-adapter-sibling");
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "work/deadline-merged", "merged-head");
    api.seed_branch(&repo(), "work/deadline-sibling", "sibling-head");
    api.seed_branch(&repo(), "main", "old-base");
    let merged_pull = api
        .open_pull(
            &repo(),
            "work/deadline-merged",
            "main",
            "deadline-merged",
            "",
        )
        .unwrap();
    let sibling_pull = api
        .open_pull(
            &repo(),
            "work/deadline-sibling",
            "main",
            "deadline-sibling",
            "",
        )
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
        &["src/shared.rs"],
    );

    let mut adapter = GitHubAdapter::new(api);
    // The clock advances on every read: the merge scan itself starts
    // inside the deadline, but the sibling re-check's live-clock
    // read lands past it and must defer — a comparison against the
    // refresh-start timestamp could never fire.
    let mut tick = 0i64;
    let mut clock = move || {
        tick += 1;
        NOW + tick
    };
    let report = adapter
        .refresh_sources(&store, &config, &mut clock, Some(NOW + 5), None)
        .unwrap();
    assert!(
        report
            .deferred
            .iter()
            .any(|d| d.starts_with("merge-siblings:")),
        "the sibling re-check defers once the live clock passes the deadline: {:?}",
        report.deferred
    );
    assert_eq!(mergeability_facts(&store, &sibling_work), 0);
}

#[test]
fn a_scoped_refresh_never_rechecks_a_sibling_outside_the_scope() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "scoped-merged");
    let outside_work = submit(&store, &config, "scoped-outside");
    let merged = change("cr-scoped-merged");
    let outside = change("cr-scoped-outside");
    record_change(&store, &merged_work, &merged);
    record_change(&store, &outside_work, &outside);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_change_head(&outside, "outside-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    fixture.set_change_mergeability(&outside, MergeabilityState::Conflicting, &["src/shared.rs"]);

    // The merge belongs to the scoped Work, but the only conflicted
    // sibling belongs to another Work: a scoped (`run --work`) pass
    // must record nothing for it.
    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, Some(&merged_work))
        .unwrap();
    assert_eq!(report.sibling_conflicts, 0);
    assert_eq!(mergeability_facts(&store, &outside_work), 0);
    assert_eq!(fix_requests(&store, &outside_work), 0);
}

#[test]
fn the_production_adapter_does_not_recheck_an_out_of_scope_sibling() {
    use workloop_github::api::GitHubApi;

    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "adapter-scoped-merged");
    let outside_work = submit(&store, &config, "adapter-scoped-outside");
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "work/scoped-merged", "merged-head");
    api.seed_branch(&repo(), "work/scoped-outside", "outside-head");
    api.seed_branch(&repo(), "main", "old-base");
    let merged_pull = api
        .open_pull(&repo(), "work/scoped-merged", "main", "merged", "")
        .unwrap();
    let outside_pull = api
        .open_pull(&repo(), "work/scoped-outside", "main", "outside", "")
        .unwrap();
    let merged = change(&merged_pull.id);
    let outside = change(&outside_pull.id);
    record_change(&store, &merged_work, &merged);
    record_change(&store, &outside_work, &outside);
    ingest_head(&store, &config, &merged_work, &merged, "merged-head");
    ingest_head(&store, &config, &outside_work, &outside, "outside-head");
    api.seed_branch(&repo(), "main", "new-base");
    api.set_pull_merged(&repo(), &merged_pull.id);
    api.set_pull_mergeability(
        &repo(),
        &outside_pull.id,
        Some(false),
        "dirty",
        "new-base",
        &["src/shared.rs"],
    );

    let mut adapter = GitHubAdapter::new(api);
    let report = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, Some(&merged_work))
        .unwrap();
    assert_eq!(report.sibling_conflicts, 0);
    assert_eq!(mergeability_facts(&store, &outside_work), 0);
    assert_eq!(fix_requests(&store, &outside_work), 0);
}

#[test]
fn an_unusable_sibling_head_is_bound_to_the_last_recorded_head() {
    use workloop_github::api::GitHubApi;

    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "unknown-head-merged");
    let sibling_work = submit(&store, &config, "unknown-head-sibling");
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "work/unknown-merged", "merged-head");
    api.seed_branch(&repo(), "work/unknown-sibling", "sibling-head");
    api.seed_branch(&repo(), "main", "old-base");
    let merged_pull = api
        .open_pull(&repo(), "work/unknown-merged", "main", "merged", "")
        .unwrap();
    let sibling_pull = api
        .open_pull(&repo(), "work/unknown-sibling", "main", "sibling", "")
        .unwrap();
    let merged = change(&merged_pull.id);
    let sibling = change(&sibling_pull.id);
    record_change(&store, &merged_work, &merged);
    record_change(&store, &sibling_work, &sibling);
    ingest_head(&store, &config, &merged_work, &merged, "merged-head");
    ingest_head(&store, &config, &sibling_work, &sibling, "sibling-head");
    // The sibling's own head poll has nothing current to read; finish
    // it so the re-check (not the ordinary head poll) exercises the
    // unusable-head path.
    store
        .transact(|tx| {
            let execution = tx
                .active_execution(&sibling_work)?
                .expect("active execution");
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
    // The mergeability read carries no usable head.
    api.set_pull_head(&repo(), &sibling_pull.id, "");
    api.seed_branch(&repo(), "main", "new-base");
    api.set_pull_merged(&repo(), &merged_pull.id);
    api.set_pull_mergeability(
        &repo(),
        &sibling_pull.id,
        Some(false),
        "dirty",
        "new-base",
        &["src/shared.rs"],
    );

    let mut adapter = GitHubAdapter::new(api);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();

    let observed = store
        .transact(|tx| tx.observations_for_work(&sibling_work))
        .unwrap();
    let target = observed
        .iter()
        .find_map(|o| match &o.fact {
            ObservationFact::Mergeability { target, .. } => Some(target.clone()),
            _ => None,
        })
        .expect("the reported paths are retained as an unknown observation");
    assert_eq!(target.head.object, "sibling-head");
    assert_eq!(
        target.base.as_ref().map(|b| b.object.as_str()),
        Some("new-base"),
        "the unknown observation must not fabricate a head==base pair"
    );
}
