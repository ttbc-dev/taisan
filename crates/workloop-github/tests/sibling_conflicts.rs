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
    MAX_CONFLICT_RECHECKS_PER_PASS, MAX_MERGE_CHECKS_PER_PASS, MergeabilityState,
};
use workloop_core::observation::{
    DeliveryId, DeliveryScope, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::adapter::GitHubAdapter;
use workloop_github::api::GitHubApi;
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

fn merge_check_facts(store: &SqliteStore, work: &WorkId) -> usize {
    store
        .transact(|tx| tx.observations_for_work(work))
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::MergeCheck { .. }))
        .count()
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
    // one check record, nothing observed for the other Works.
    let before = works
        .iter()
        .map(|w| merge_check_facts(&store, w))
        .collect::<Vec<_>>();
    adapter
        .refresh_sources(&store, &config, &mut || NOW + 3, None, Some(&works[0]))
        .unwrap();
    assert_eq!(
        adapter.client().pull_reads(),
        3 * MAX_MERGE_CHECKS_PER_PASS + 1
    );
    for (index, work) in works.iter().enumerate() {
        let expected = before[index] + usize::from(index == 0);
        assert_eq!(
            merge_check_facts(&store, work),
            expected,
            "a scoped pass observes only the scoped work's pair"
        );
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
