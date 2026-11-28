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
use workloop_core::mergeability::{
    MAX_CONFLICT_RECHECKS_PER_PASS, MAX_MERGE_CHECKS_PER_PASS, MAX_MERGE_RECHECKS_PER_PASS,
    MergeabilityState,
};
use workloop_core::observation::{
    DeliveryId, DeliveryScope, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::adapter::GitHubAdapter;
use workloop_github::api::{GitHubApi, PullState, RemotePull};
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
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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

/// How many pairs on the Work hold a service record. The merge-check
/// service is a per-pair cursor on the usage row — one bounded record
/// per pair, overwritten in place, never an observation per pass.
fn merge_check_facts(store: &SqliteStore, work: &WorkId) -> usize {
    store
        .transact(|tx| tx.work_usage(work))
        .unwrap()
        .map(|usage| usage.merge_check_services.len())
        .unwrap_or(0)
}

/// The Work's service records `(observed, seq)`: the positions the
/// least-recently-serviced ordering reads, one per serviced pair.
fn merge_check_positions(store: &SqliteStore, work: &WorkId) -> Vec<(i64, i64)> {
    store
        .transact(|tx| tx.work_usage(work))
        .unwrap()
        .map(|usage| {
            usage
                .merge_check_services
                .iter()
                .map(|s| (s.observed_unix_seconds, s.serviced_seq))
                .collect()
        })
        .unwrap_or_default()
}

/// Settle the Work's active Execution: a terminal Work owes no head
/// poll, so every pull read it still causes is merge detection.
fn finish_execution(store: &SqliteStore, work: &WorkId) {
    store
        .transact(|tx| {
            let execution = tx.active_execution(work)?.expect("active execution");
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

/// Seed `count` terminal Works each holding one recorded, never-merged
/// open change — the merge-detection backlog the bound must cover.
fn unmerged_works(
    store: &SqliteStore,
    config: &WorkloopConfig,
    api: &mut InMemoryGitHub,
    prefix: &str,
    count: usize,
) -> Vec<WorkId> {
    (0..count)
        .map(|index| {
            let work = submit(store, config, &format!("{prefix}-{index}"));
            api.seed_branch(
                &repo(),
                &format!("work/{prefix}-{index}"),
                &format!("{prefix}-head-{index}"),
            );
            let pull = api
                .open_pull(
                    &repo(),
                    &format!("work/{prefix}-{index}"),
                    "main",
                    &format!("{prefix}-{index}"),
                    "",
                )
                .unwrap();
            let change = change(&pull.id);
            record_change(store, &work, &change);
            ingest_head(
                store,
                config,
                &work,
                &change,
                &format!("{prefix}-head-{index}"),
            );
            finish_execution(store, &work);
            work
        })
        .collect()
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

    // A second pass re-observes the same merged state: the conflicted
    // pair's verdict is already recorded and settled, so it is never
    // re-read — no new fact, no second trigger.
    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, 0);
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

    // A later pass still scans the target once, but the steady
    // conflicted pair's recorded verdict is settled: it is never
    // re-read, so no second trigger can mint.
    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert!(report.merge_rechecks <= 2, "{report:?}");
    assert_eq!(report.sibling_conflicts, 0);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 1);
}

#[test]
fn the_production_adapter_does_not_rescan_each_historical_merge() {
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
    // The steady conflicted pair's recorded verdict is settled, so the
    // second pass never re-reads it — no new fact, no second trigger.
    assert_eq!(second.sibling_conflicts, 0);
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

    // A steady pass reaches the deferred tail first: the leading
    // siblings' recorded verdicts are settled for the same pair, so
    // they are never re-read and the whole bound lands on the
    // unrecorded remainder.
    let second = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(
        second.sibling_conflicts,
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

    // With every pair recorded and settled, a steady pass reads nothing
    // at all — re-reading a settled pair could only re-mint a deduped
    // fact — even when the leading set has since merged away.
    for index in 0..MAX_CONFLICT_RECHECKS_PER_PASS {
        fixture.set_merged(&change(&format!("cr-progress-sibling-{index}")));
    }
    let third = fixture
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(third.sibling_conflicts, 0);
}

#[test]
fn a_settled_pair_is_not_re_read_but_a_provisional_one_is_resolved() {
    // Two siblings on one target: one settles Conflicting on the first
    // pass, the other is still provisional. A settled verdict at an
    // exact `(head, base)` pair is final for the episode — a later
    // pass never re-reads it, so a same-pair host change mints
    // nothing — while a provisional `Unknown` verdict is re-read until
    // it settles.
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let merged_work = submit(&store, &config, "settle-merged");
    let settled_work = submit(&store, &config, "settle-sibling");
    let pending_work = submit(&store, &config, "pending-sibling");
    let merged = change("cr-settle-merged");
    let settled = change("cr-settle-sibling");
    let pending = change("cr-pending-sibling");
    record_change(&store, &merged_work, &merged);
    record_change(&store, &settled_work, &settled);
    record_change(&store, &pending_work, &pending);

    let mut fixture = FixtureGithub::new();
    fixture.set_change_head(&merged, "merged-head", &actor());
    fixture.set_change_head(&settled, "settled-head", &actor());
    fixture.set_change_head(&pending, "pending-head", &actor());
    fixture.set_branch(&repo(), "main", "new-base", &actor());
    fixture.set_merged(&merged);
    fixture.set_change_mergeability(&settled, MergeabilityState::Conflicting, &["src/a.rs"]);
    fixture.set_change_mergeability(&pending, MergeabilityState::Unknown, &[]);

    let first = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.sibling_conflicts, 1);
    assert_eq!(mergeability_facts(&store, &settled_work), 1);
    assert_eq!(mergeability_facts(&store, &pending_work), 1);
    assert_eq!(fix_requests(&store, &pending_work), 0);

    fixture.set_change_mergeability(&settled, MergeabilityState::Mergeable, &[]);
    fixture.set_change_mergeability(&pending, MergeabilityState::Conflicting, &["src/b.rs"]);
    let second = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(
        second.sibling_conflicts, 1,
        "only the provisional sibling is re-read"
    );
    assert_eq!(
        mergeability_facts(&store, &settled_work),
        1,
        "a settled pair is never re-read"
    );
    assert_eq!(
        mergeability_facts(&store, &pending_work),
        2,
        "the provisional verdict resolves to a new fact"
    );
    assert_eq!(fix_requests(&store, &pending_work), 1);
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

    // A steady pass reaches the deferred tail first: the leading
    // siblings' recorded verdicts are settled for the same pair, so
    // they are never re-read and the whole bound lands on the
    // unrecorded remainder.
    let second = adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(
        second.sibling_conflicts,
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

    // With every pair recorded and settled, a steady pass reads nothing
    // at all — re-reading a settled pair could only re-mint a deduped
    // fact — even when the leading set has since merged away.
    for (_, pull_id) in works.iter().take(MAX_CONFLICT_RECHECKS_PER_PASS) {
        adapter.client_mut().set_pull_merged(&repo(), pull_id);
    }
    let third = adapter
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(third.sibling_conflicts, 0);
}

#[test]
fn the_pass_deadline_defers_sibling_rechecks_mid_scan() {
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
        .refresh_sources(&store, &config, &mut clock, Some(NOW + 6), None)
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

#[test]
fn merge_detection_reads_are_bounded_per_pass() {
    let store = SqliteStore::in_memory(8).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    // More recorded changes than one pass may check. Every Work's
    // Execution is terminal — the documented detection set still
    // includes a completed Work's open change — so the bounded
    // detection loop is the only pull reader this pass.
    let unmerged = MAX_MERGE_CHECKS_PER_PASS + 2;
    let works = unmerged_works(&store, &config, &mut api, "detect", unmerged);

    let mut adapter = GitHubAdapter::new(api);
    let first = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    assert_eq!(
        adapter.client().pull_reads(),
        MAX_MERGE_CHECKS_PER_PASS,
        "a terminal backlog cannot make remote reads unbounded"
    );
    assert_eq!(
        first
            .deferred
            .iter()
            .filter(|d| d.starts_with("merge-check:"))
            .count(),
        unmerged - MAX_MERGE_CHECKS_PER_PASS,
        "the remainder is deferred, not silently dropped: {:?}",
        first.deferred
    );
    let checked: usize = works.iter().map(|w| merge_check_facts(&store, w)).sum();
    assert_eq!(checked, MAX_MERGE_CHECKS_PER_PASS);

    // The deferred tail is reached next pass: the pairs already
    // checked hold the newest durable records, so the never-checked
    // remainder orders first — the rest of the bound re-reads the
    // least recently checked pairs.
    let second = adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(second.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    for (index, work) in works.iter().enumerate() {
        assert!(
            merge_check_facts(&store, work) >= 1,
            "change {index} was checked"
        );
    }

    // A steady pass still re-reads inside the bound — least recently
    // checked first — instead of walking the whole backlog.
    let third = adapter
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    assert_eq!(third.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    assert_eq!(adapter.client().pull_reads(), 3 * MAX_MERGE_CHECKS_PER_PASS);

    // A scoped pass detects only the scoped Work's pair: one read,
    // its service record re-stamped, nothing recorded for the other
    // Works.
    let before = works
        .iter()
        .map(|w| merge_check_positions(&store, w))
        .collect::<Vec<_>>();
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 3, None, Some(&works[0]))
        .unwrap();
    assert_eq!(
        adapter.client().pull_reads(),
        3 * MAX_MERGE_CHECKS_PER_PASS + 1
    );
    for (index, work) in works.iter().enumerate() {
        let after = merge_check_positions(&store, work);
        if index == 0 {
            assert!(
                after.first() > before[index].first(),
                "a scoped pass re-stamps the scoped work's service record"
            );
        } else {
            assert_eq!(
                after, before[index],
                "a scoped pass records nothing for another work's pair"
            );
        }
    }

    // The check record is inert: recorded with no subject execution it
    // resolves nothing and mints no trigger — a merge-detection pass
    // must leave every work's request ledger and execution state
    // untouched.
    for work in &works {
        assert_eq!(
            fix_requests(&store, work),
            0,
            "a merge check is not a repair trigger"
        );
        assert!(
            store
                .transact(|tx| tx.active_execution(work))
                .unwrap()
                .is_none(),
            "a merge check resolves no execution"
        );
    }
}

#[test]
fn the_pass_deadline_defers_merge_detection() {
    let store = SqliteStore::in_memory(8).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    let unmerged = 2;
    let _works = unmerged_works(&store, &config, &mut api, "deadline-detect", unmerged);

    let mut adapter = GitHubAdapter::new(api);
    // The clock advances on every read: detection candidates must check
    // the live clock before paying a pull read, exactly like the merge
    // scan and sibling rechecks ahead of them.
    let mut tick = 0i64;
    let mut clock = move || {
        tick += 1;
        NOW + tick
    };
    let report = adapter
        .refresh_sources(&store, &config, &mut clock, Some(NOW + 2), None)
        .unwrap();
    assert_eq!(report.merge_detections, 0);
    assert_eq!(
        adapter.client().pull_reads(),
        0,
        "the deadline gates every detection read"
    );
    assert_eq!(
        report
            .deferred
            .iter()
            .filter(|d| d.starts_with("merge-check:"))
            .count(),
        unmerged,
        "both unmerged pairs defer on the live clock: {:?}",
        report.deferred
    );
}

#[test]
fn merge_detection_reaches_the_tail_at_equal_timestamps() {
    let store = SqliteStore::in_memory(8).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    let unmerged = MAX_MERGE_CHECKS_PER_PASS + 2;
    let works = unmerged_works(&store, &config, &mut api, "equal-ts", unmerged);

    // Every pass reports the same `now`: the least-recently-checked key
    // is second-granular, but never-checked pairs always sort first, so
    // the whole backlog is still reached within ceil(N/bound) passes.
    let mut adapter = GitHubAdapter::new(api);
    let passes = unmerged.div_ceil(MAX_MERGE_CHECKS_PER_PASS);
    for _ in 0..passes {
        adapter
            .refresh_sources(&store, &config, &mut || NOW, None, None)
            .unwrap();
    }
    for (index, work) in works.iter().enumerate() {
        assert!(
            merge_check_facts(&store, work) >= 1,
            "change {index} was checked within {passes} passes at equal timestamps"
        );
    }
}

#[test]
fn a_failed_detection_read_still_advances_the_ordering_key() {
    let store = SqliteStore::in_memory(8).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    let unmerged = MAX_MERGE_CHECKS_PER_PASS + 1;
    let works = unmerged_works(&store, &config, &mut api, "lost-detect", unmerged);

    let mut adapter = GitHubAdapter::new(api);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    // Exactly one pair was deferred — it is still never-checked, so it
    // leads the ordering next pass and its read is the first issued.
    let deferred: Vec<&WorkId> = works
        .iter()
        .filter(|w| merge_check_facts(&store, w) == 0)
        .collect();
    assert_eq!(deferred.len(), 1, "one pair stayed never-checked");
    let deferred = deferred[0].clone();

    // The leading read is refused this pass: it consumed the bound and
    // must leave the durable ordering record — bound to the last
    // recorded head, the only truthful head the pair has — or the
    // failing pair would lead the queue forever and starve the tail.
    adapter.client_mut().lose_response("read_pull");
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    let checks: Vec<String> = store
        .transact(|tx| tx.work_usage(&deferred))
        .unwrap()
        .map(|usage| {
            usage
                .merge_check_services
                .iter()
                .map(|s| s.head.object.clone())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        checks.len(),
        1,
        "a failed detection read records the check against the last recorded head"
    );
    assert!(
        checks[0].starts_with("lost-detect-head-"),
        "the record binds the pair's recorded head, got {checks:?}"
    );

    // The next pass reads the remaining backlog — the failed pair
    // holds the newest key and waits its turn like any checked pair.
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 2, None, None)
        .unwrap();
    for (index, work) in works.iter().enumerate() {
        assert!(
            merge_check_facts(&store, work) >= 1,
            "change {index} was checked within three passes"
        );
    }
}

#[test]
fn an_unbindable_merged_read_still_advances_the_ordering_key() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    // `main` is never seeded: the merged pull's new base can neither be
    // read nor hinted, so the merge can never be bound. The detection
    // read still ran — it must advance the ordering key every pass or
    // the unbindable pair would lead the queue forever.
    let merged_work = submit(&store, &config, "unbindable-merge");
    api.seed_branch(&repo(), "work/unbindable", "unbindable-head");
    let pull = api
        .open_pull(&repo(), "work/unbindable", "main", "unbindable", "")
        .unwrap();
    let change = change(&pull.id);
    record_change(&store, &merged_work, &change);
    ingest_head(&store, &config, &merged_work, &change, "unbindable-head");
    finish_execution(&store, &merged_work);
    api.set_pull_merged(&repo(), &pull.id);

    let mut adapter = GitHubAdapter::new(api);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let first = merge_check_positions(&store, &merged_work);
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    let second = merge_check_positions(&store, &merged_work);
    assert_eq!(
        second.len(),
        1,
        "the service record is one bounded cursor, not a log"
    );
    assert!(
        second > first,
        "every issued detection read advances the ordering key"
    );
    let merges = store
        .transact(|tx| tx.observations_for_work(&merged_work))
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::ChangeMerged { .. }))
        .count();
    assert_eq!(merges, 0, "an unbindable merge is never recorded");
}

#[test]
fn the_fixture_records_a_check_when_no_current_head_exists() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let work = submit(&store, &config, "fixture-no-head");
    let change = change("cr-fixture-no-head");
    record_change(&store, &work, &change);
    // The recorded head is the only revision the pair has; the fixture
    // has no current head for the change, so the issued check must
    // still leave the ordering record bound to the recorded head.
    let fixture = FixtureGithub::new();
    ingest_head(&store, &config, &work, &change, "fixture-no-head");
    finish_execution(&store, &work);

    let report = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.merge_detections, 1);
    assert_eq!(
        merge_check_facts(&store, &work),
        1,
        "an issued detection check leaves the ordering record even with no current head"
    );
}

#[test]
fn the_fixture_bounds_merge_detection_the_same_way() {
    let store = SqliteStore::in_memory(8).unwrap();
    let config = config();
    let unmerged = MAX_MERGE_CHECKS_PER_PASS + 2;
    let mut works = Vec::new();
    let mut fixture = FixtureGithub::new();
    for index in 0..unmerged {
        let work = submit(&store, &config, &format!("fixture-detect-{index}"));
        let change = change(&format!("cr-fixture-detect-{index}"));
        record_change(&store, &work, &change);
        fixture.set_change_head(&change, &format!("detect-head-{index}"), &actor());
        works.push(work);
    }

    let first = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    assert_eq!(
        first
            .deferred
            .iter()
            .filter(|d| d.starts_with("merge-check:"))
            .count(),
        unmerged - MAX_MERGE_CHECKS_PER_PASS,
        "the remainder is deferred, not silently dropped: {:?}",
        first.deferred
    );
    let checked: usize = works.iter().map(|w| merge_check_facts(&store, w)).sum();
    assert_eq!(checked, MAX_MERGE_CHECKS_PER_PASS);

    let second = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(second.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    for (index, work) in works.iter().enumerate() {
        assert!(
            merge_check_facts(&store, work) >= 1,
            "change {index} was checked"
        );
    }
}

#[test]
fn recorded_merges_on_distinct_targets_do_not_starve_the_tail() {
    // More recorded merges on distinct target branches than the
    // per-pass scan bound: the leading recorded set must not consume
    // the bound every pass — detection reads and merge scans share
    // one least-recently-serviced ordering, so the deferred tail is
    // reached on a later pass.
    let store = SqliteStore::in_memory(32).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    let targets = MAX_MERGE_RECHECKS_PER_PASS + 1;
    let mut tail_sibling_work = None;
    for index in 0..targets {
        let target = format!("scan-target-{index}");
        let merged_work = submit(&store, &config, &format!("scan-merged-{index}"));
        let sibling_work = submit(&store, &config, &format!("scan-sibling-{index}"));
        api.seed_branch(
            &repo(),
            &format!("work/scan-merged-{index}"),
            &format!("scan-merged-head-{index}"),
        );
        api.seed_branch(
            &repo(),
            &format!("work/scan-sibling-{index}"),
            &format!("scan-sibling-head-{index}"),
        );
        api.seed_branch(&repo(), &target, &format!("scan-base-{index}"));
        let merged_pull = api
            .open_pull(
                &repo(),
                &format!("work/scan-merged-{index}"),
                &target,
                &format!("scan-merged-{index}"),
                "",
            )
            .unwrap();
        let sibling_pull = api
            .open_pull(
                &repo(),
                &format!("work/scan-sibling-{index}"),
                &target,
                &format!("scan-sibling-{index}"),
                "",
            )
            .unwrap();
        let merged = change(&merged_pull.id);
        let sibling = change(&sibling_pull.id);
        record_change(&store, &merged_work, &merged);
        record_change(&store, &sibling_work, &sibling);
        ingest_head(
            &store,
            &config,
            &merged_work,
            &merged,
            &format!("scan-merged-head-{index}"),
        );
        ingest_head(
            &store,
            &config,
            &sibling_work,
            &sibling,
            &format!("scan-sibling-head-{index}"),
        );
        api.set_pull_merged(&repo(), &merged_pull.id);
        api.set_pull_mergeability(
            &repo(),
            &sibling_pull.id,
            Some(false),
            "dirty",
            &format!("scan-base-{index}"),
            &["src/shared.rs"],
        );
        // A merged Work is done: its recorded pair still seeds merge
        // detection and the sibling scan, and settling it keeps the
        // queued set under the pending-queue limit.
        finish_execution(&store, &merged_work);
        tail_sibling_work = Some(sibling_work);
    }
    let tail_sibling_work = tail_sibling_work.unwrap();

    let mut adapter = GitHubAdapter::new(api);
    // The detection bound services only the leading pairs in the
    // first pass; the tail merge is not recorded yet and its target's
    // sibling is unobserved.
    let first = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.merge_detections, MAX_MERGE_CHECKS_PER_PASS);
    assert_eq!(mergeability_facts(&store, &tail_sibling_work), 0);

    // Serviced pairs sort behind the never-serviced remainder, so the
    // queue drains toward the tail: within a bounded number of passes
    // the last merge's detection, scan, and sibling re-check all land
    // instead of starving behind a constant leading set.
    let mut passes = 1usize;
    while mergeability_facts(&store, &tail_sibling_work) == 0 && passes < 4 {
        adapter
            .refresh_sources(&store, &config, &mut || NOW + passes as i64, None, None)
            .unwrap();
        passes += 1;
    }
    assert_eq!(
        mergeability_facts(&store, &tail_sibling_work),
        1,
        "the deferred tail's sibling is observed within {passes} passes"
    );
    assert_eq!(fix_requests(&store, &tail_sibling_work), 1);

    // The recorded conflicted pair is settled for its episode: a
    // steady pass mints no second trigger.
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 10, None, None)
        .unwrap();
    assert_eq!(fix_requests(&store, &tail_sibling_work), 1);
}

#[test]
fn the_fixture_does_not_starve_deferred_merge_scans() {
    // The fixture mirrors the production bound: more merged changes on
    // distinct targets than the scan bound must still reach the
    // deferred tail on a later pass.
    let store = SqliteStore::in_memory(32).unwrap();
    let config = config();
    let mut fixture = FixtureGithub::new();
    let targets = MAX_MERGE_RECHECKS_PER_PASS + 1;
    let mut tail_sibling_work = None;
    for index in 0..targets {
        let target = format!("fixture-scan-target-{index}");
        let merged_work = submit(&store, &config, &format!("fixture-scan-merged-{index}"));
        let sibling_work = submit(&store, &config, &format!("fixture-scan-sibling-{index}"));
        let merged = change(&format!("cr-fixture-scan-merged-{index}"));
        let sibling = change(&format!("cr-fixture-scan-sibling-{index}"));
        record_change(&store, &merged_work, &merged);
        record_change(&store, &sibling_work, &sibling);
        fixture.register_change(
            &merged,
            &format!("work/fscan-merged-{index}"),
            &target,
            &format!("fscan-merged-head-{index}"),
            &actor(),
        );
        fixture.register_change(
            &sibling,
            &format!("work/fscan-sibling-{index}"),
            &target,
            &format!("fscan-sibling-head-{index}"),
            &actor(),
        );
        fixture.set_branch(&repo(), &target, &format!("fscan-base-{index}"), &actor());
        fixture.set_merged(&merged);
        fixture.set_change_mergeability(
            &sibling,
            MergeabilityState::Conflicting,
            &["src/shared.rs"],
        );
        finish_execution(&store, &merged_work);
        tail_sibling_work = Some(sibling_work);
    }
    let tail_sibling_work = tail_sibling_work.unwrap();

    let first = fixture
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(first.sibling_conflicts, MAX_MERGE_RECHECKS_PER_PASS);
    assert_eq!(mergeability_facts(&store, &tail_sibling_work), 0);

    let second = fixture
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(second.sibling_conflicts, 1);
    assert_eq!(
        mergeability_facts(&store, &tail_sibling_work),
        1,
        "the deferred target's sibling is reached on a later pass"
    );
    assert_eq!(fix_requests(&store, &tail_sibling_work), 1);
}

#[test]
fn same_second_merge_checks_keep_the_scan_rotation_moving() {
    // A fixed clock stamps every service record in the same second:
    // without a durable secondary key the stable order would freeze on
    // the leading set after the first full round and the tail pair
    // would never be re-scanned.
    let store = SqliteStore::in_memory(32).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    let targets = MAX_MERGE_RECHECKS_PER_PASS + 1;
    let mut merged_works = Vec::new();
    for index in 0..targets {
        let target = format!("rot-target-{index}");
        let merged_work = submit(&store, &config, &format!("rot-merged-{index}"));
        api.seed_branch(
            &repo(),
            &format!("work/rot-merged-{index}"),
            &format!("rot-merged-head-{index}"),
        );
        api.seed_branch(&repo(), &target, &format!("rot-base-{index}"));
        let merged_pull = api
            .open_pull(
                &repo(),
                &format!("work/rot-merged-{index}"),
                &target,
                &format!("rot-merged-{index}"),
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
            &format!("rot-merged-head-{index}"),
        );
        api.set_pull_merged(&repo(), &merged_pull.id);
        finish_execution(&store, &merged_work);
        merged_works.push(merged_work);
    }
    let tail_work = merged_works.last().unwrap().clone();

    let mut adapter = GitHubAdapter::new(api);
    // Every stamp lands in the same second, so ordering turns on the
    // durable record order alone.
    let mut passes = 0usize;
    while merged_works
        .iter()
        .any(|work| merge_check_facts(&store, work) == 0)
        && passes < 8
    {
        adapter
            .refresh_sources(&store, &config, &mut || NOW, None, None)
            .unwrap();
        passes += 1;
    }
    assert!(
        merged_works
            .iter()
            .all(|work| merge_check_facts(&store, work) >= 1),
        "every recorded merge is serviced once within {passes} passes"
    );

    let before = merge_check_positions(&store, &tail_work);
    let mut extra = 0usize;
    while merge_check_positions(&store, &tail_work) == before && extra < 3 {
        adapter
            .refresh_sources(&store, &config, &mut || NOW, None, None)
            .unwrap();
        extra += 1;
    }
    assert!(
        merge_check_positions(&store, &tail_work) > before,
        "the tail pair is re-serviced after a full same-second round ({extra} extra passes)"
    );
}

#[test]
fn merge_check_services_are_one_bounded_record_not_an_observation_log() {
    // Repeated detection passes must not grow the durable log: the
    // service record is a per-pair cursor overwritten in place, so the
    // only thing a steady pass moves is the pair's position.
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    let works = unmerged_works(&store, &config, &mut api, "bounded-check", 1);
    let work = &works[0];

    let observation_count = |store: &SqliteStore| {
        store
            .transact(|tx| tx.observations_for_work(work))
            .unwrap()
            .len()
    };
    let mut adapter = GitHubAdapter::new(api);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    let first = observation_count(&store);
    let first_position = merge_check_positions(&store, work);
    for pass in 1..4i64 {
        adapter
            .refresh_sources(&store, &config, &mut || NOW + pass, None, None)
            .unwrap();
    }
    assert_eq!(
        observation_count(&store),
        first,
        "detection passes append nothing to the observation log"
    );
    assert_eq!(
        merge_check_facts(&store, work),
        1,
        "the service record stays one bounded cursor"
    );
    assert!(
        merge_check_positions(&store, work) > first_position,
        "every pass still advances the pair's position"
    );
}

#[test]
fn a_recorded_merge_is_never_re_stamped_with_a_later_base() {
    // A merge the first pass records binds the target revision observed
    // at that time; a later pass whose branch advanced still scans the
    // siblings against the fresh base but must never re-ingest the
    // recorded merge with the later base.
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    let merged_work = submit(&store, &config, "restamp-merged");
    let sibling_work = submit(&store, &config, "restamp-sibling");
    api.seed_branch(&repo(), "work/restamp-merged", "restamp-merged-head");
    api.seed_branch(&repo(), "work/restamp-sibling", "restamp-sibling-head");
    api.seed_branch(&repo(), "main", "restamp-base-1");
    let merged_pull = api
        .open_pull(&repo(), "work/restamp-merged", "main", "restamp-merged", "")
        .unwrap();
    let sibling_pull = api
        .open_pull(
            &repo(),
            "work/restamp-sibling",
            "main",
            "restamp-sibling",
            "",
        )
        .unwrap();
    let merged = change(&merged_pull.id);
    let sibling = change(&sibling_pull.id);
    record_change(&store, &merged_work, &merged);
    record_change(&store, &sibling_work, &sibling);
    ingest_head(
        &store,
        &config,
        &merged_work,
        &merged,
        "restamp-merged-head",
    );
    ingest_head(
        &store,
        &config,
        &sibling_work,
        &sibling,
        "restamp-sibling-head",
    );
    finish_execution(&store, &merged_work);
    api.set_pull_merged(&repo(), &merged_pull.id);

    let merged_pairs = |store: &SqliteStore| -> Vec<(String, String)> {
        store
            .transact(|tx| tx.observations_for_work(&merged_work))
            .unwrap()
            .iter()
            .filter_map(|o| match &o.fact {
                ObservationFact::ChangeMerged { head, base, .. } => {
                    Some((head.object.clone(), base.object.clone()))
                }
                _ => None,
            })
            .collect()
    };
    let mut adapter = GitHubAdapter::new(api);
    adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(
        merged_pairs(&store),
        vec![(
            "restamp-merged-head".to_string(),
            "restamp-base-1".to_string()
        )],
        "the merge binds the target revision observed at detection"
    );

    // The target branch advances; the scan still reads the fresh base —
    // the sibling verdict binds it — but the recorded merge is durable
    // proof and is never re-stamped.
    adapter
        .client_mut()
        .seed_branch(&repo(), "main", "restamp-base-2");
    adapter.client_mut().set_pull_mergeability(
        &repo(),
        &sibling_pull.id,
        Some(false),
        "dirty",
        "restamp-base-2",
        &["src/shared.rs"],
    );
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(
        merged_pairs(&store),
        vec![(
            "restamp-merged-head".to_string(),
            "restamp-base-1".to_string()
        )],
        "a recorded merge is never re-ingested with a later base"
    );
    let sibling_base = store
        .transact(|tx| tx.observations_for_work(&sibling_work))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::Mergeability { target, .. } => {
                target.base.as_ref().map(|b| b.object.clone())
            }
            _ => None,
        })
        .next_back();
    assert_eq!(
        sibling_base.as_deref(),
        Some("restamp-base-2"),
        "the sibling scan still binds the fresh base"
    );
    assert_eq!(fix_requests(&store, &sibling_work), 1);
}

#[test]
fn the_mergeability_verdict_never_guesses_a_conflict_from_the_tristate() {
    let pull = |mergeable: Option<bool>, state: &str| RemotePull {
        id: "pr-1".into(),
        display_number: 1,
        head_sha: "h".into(),
        source_branch: "work/w-1".into(),
        target_branch: "main".into(),
        title: String::new(),
        body: String::new(),
        state: PullState::Open,
        merged: false,
        mergeable,
        mergeable_state: state.into(),
        base_sha: "b".into(),
        conflicting_paths: vec![],
    };
    // An explicit conflict state is a conflict verdict either way.
    for mergeable in [None, Some(false)] {
        for state in ["dirty", "conflicting"] {
            assert_eq!(
                pull(mergeable, state).mergeability_state(),
                MergeabilityState::Conflicting,
                "{mergeable:?}/{state}"
            );
        }
    }
    // A negative tri-state without an explicit conflict state is not a
    // conflict: `mergeable=false` also answers a required-review or
    // check block, a stale head, and a draft.
    for state in ["blocked", "behind", "draft", "unknown", ""] {
        assert_eq!(
            pull(Some(false), state).mergeability_state(),
            MergeabilityState::Unknown,
            "Some(false)/{state} is provisional, never a repair trigger"
        );
    }
    // Positive answers stay mergeable.
    assert_eq!(
        pull(Some(true), "clean").mergeability_state(),
        MergeabilityState::Mergeable
    );
    assert_eq!(
        pull(None, "unstable").mergeability_state(),
        MergeabilityState::Mergeable
    );
    // A contradictory negative tri-state under a positive state is
    // Unknown, not a clean pass.
    assert_eq!(
        pull(Some(false), "clean").mergeability_state(),
        MergeabilityState::Unknown
    );
}

#[test]
fn a_blocked_sibling_is_unknown_not_a_conflict_trigger() {
    let store = SqliteStore::in_memory(4).unwrap();
    let config = config();
    let mut api = InMemoryGitHub::new();
    api.seed_branch(&repo(), "main", "old-base");
    let merged_work = submit(&store, &config, "blocked-merged-work");
    api.seed_branch(&repo(), "work/blocked-merged", "merged-head");
    let merged_pull = api
        .open_pull(&repo(), "work/blocked-merged", "main", "merged", "")
        .unwrap();
    let merged = change(&merged_pull.id);
    record_change(&store, &merged_work, &merged);
    ingest_head(&store, &config, &merged_work, &merged, "merged-head");

    let sibling_work = submit(&store, &config, "blocked-sibling-work");
    api.seed_branch(&repo(), "work/blocked-sibling", "sibling-head");
    let sibling_pull = api
        .open_pull(&repo(), "work/blocked-sibling", "main", "sibling", "")
        .unwrap();
    let sibling = change(&sibling_pull.id);
    record_change(&store, &sibling_work, &sibling);
    ingest_head(&store, &config, &sibling_work, &sibling, "sibling-head");

    api.seed_branch(&repo(), "main", "new-base");
    api.set_pull_merged(&repo(), &merged_pull.id);
    // Required reviews/checks block the sibling: `mergeable=false`
    // with a non-conflict state is a provisional Unknown, never a
    // repair trigger.
    api.set_pull_mergeability(
        &repo(),
        &sibling_pull.id,
        Some(false),
        "blocked",
        "new-base",
        &[],
    );

    let mut adapter = GitHubAdapter::new(api);
    let report = adapter
        .refresh_sources(&store, &config, &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.sibling_conflicts, 0);
    assert_eq!(mergeability_facts(&store, &sibling_work), 1);
    assert_eq!(fix_requests(&store, &sibling_work), 0);
    let state = store
        .transact(|tx| tx.observations_for_work(&sibling_work))
        .unwrap()
        .iter()
        .find_map(|o| match &o.fact {
            ObservationFact::Mergeability { state, .. } => Some(*state),
            _ => None,
        })
        .expect("a mergeability fact was recorded");
    assert_eq!(state, MergeabilityState::Unknown);
}
