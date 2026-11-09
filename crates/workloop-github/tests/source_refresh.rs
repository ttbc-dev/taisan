//! Source-refresh and bounded-outbox contract for the loop's phase 1
//! and phase 4 drivers: heads are read (never invented), a movement is
//! ingested once, an unchanged head is a no-op, and the outbox stops at
//! the per-tick bound reporting the untouched remainder.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_github::fixture::FixtureGithub;
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
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: "cred".into(),
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
        work_id,
        effect: CommandEffect::WorkCreated { execution_id },
        ..
    } = d
    else {
        panic!("submit {key}: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn admit(store: &SqliteStore, execution_id: &ExecutionId) {
    store
        .transact(|tx| admit_queued(tx, &config(), execution_id, NOW).map(|_| ()))
        .unwrap();
}

fn queue_open_change(store: &SqliteStore, work_id: &WorkId) {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id,
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "t".into(),
                        body: String::new(),
                    },
                    payload_digest: "digest".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(())
        })
        .unwrap();
}

fn change_head_observations(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::ChangeHead { head, .. } => Some(head.object),
            _ => None,
        })
        .collect()
}

/// Two admitted Works, each with one applied change request.
fn two_changes() -> (SqliteStore, FixtureGithub, WorkId, WorkId) {
    let store = SqliteStore::in_memory(4).unwrap();
    let (w1, e1) = submit(&store, "k-1");
    let (w2, e2) = submit(&store, "k-2");
    admit(&store, &e1);
    admit(&store, &e2);
    queue_open_change(&store, &w1);
    queue_open_change(&store, &w2);
    let mut gh = FixtureGithub::new();
    gh.apply_outbox(&store, &config(), NOW).unwrap();
    (store, gh, w1, w2)
}

#[test]
fn refresh_ingests_a_moved_head_once_and_a_matching_head_is_a_noop() {
    let (store, mut gh, w1, _) = two_changes();
    let before = change_head_observations(&store, &w1);
    assert_eq!(before.len(), 1, "the outbox recorded the opening head");

    // The synthetic remote moves; the read-back ingests exactly one
    // new observation under the Work that owns the change.
    gh.set_head("cr-1", "sha-moved");
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, None)
        .unwrap();
    assert_eq!(report.ingested, 1);
    assert_eq!(report.deferred, Vec::<String>::new());
    let after = change_head_observations(&store, &w1);
    assert_eq!(after.len(), 2);
    assert_eq!(after.last().unwrap(), "sha-moved");

    // A re-read with no movement ingests nothing — a restart re-entering
    // phase order observes the same facts.
    let again = gh
        .refresh_sources(&store, &config(), &mut || NOW + 1, None, None)
        .unwrap();
    assert_eq!(again.ingested, 0);
    assert_eq!(again.unchanged, 2, "both known changes match their records");
    assert_eq!(change_head_observations(&store, &w1).len(), 2);
}

#[test]
fn refresh_is_scoped_to_one_work() {
    let (store, mut gh, w1, w2) = two_changes();
    gh.set_head("cr-1", "sha-1");
    gh.set_head("cr-2", "sha-2");

    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, None, Some(&w1))
        .unwrap();
    assert_eq!(report.ingested, 1, "only the scoped Work's head moved here");
    assert_eq!(change_head_observations(&store, &w1).len(), 2);
    assert_eq!(
        change_head_observations(&store, &w2).len(),
        1,
        "the other Work's head was not read in this pass"
    );
}

#[test]
fn refresh_defers_remaining_sources_at_the_deadline() {
    let (store, mut gh, w1, _) = two_changes();
    gh.set_head("cr-1", "sha-moved");
    // Deadline already reached: the source is reported, never read.
    let report = gh
        .refresh_sources(&store, &config(), &mut || NOW, Some(NOW), None)
        .unwrap();
    assert_eq!(report.ingested, 0);
    assert_eq!(
        report.deferred,
        vec!["cr-1".to_string(), "cr-2".to_string()]
    );
    assert_eq!(
        change_head_observations(&store, &w1).len(),
        1,
        "a deferred source is handled next pass, never half-read"
    );
}

#[test]
fn the_outbox_stops_at_the_tick_bound_and_reports_the_remainder() {
    let store = SqliteStore::in_memory(4).unwrap();
    let mut gh = FixtureGithub::new();
    let mut works = Vec::new();
    for i in 0..3 {
        let (w, e) = submit(&store, &format!("k-{i}"));
        admit(&store, &e);
        queue_open_change(&store, &w);
        works.push(w);
    }

    let first = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, 2)
        .unwrap();
    assert_eq!(first.attempted, 2);
    assert_eq!(first.applied, 2);
    assert_eq!(first.deferred.len(), 1, "the third intent waits");
    assert_eq!(gh.applied().len(), 2);

    let second = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, 2)
        .unwrap();
    assert_eq!(second.attempted, 1);
    assert!(second.deferred.is_empty());
    assert_eq!(gh.applied().len(), 3, "every intent eventually applied");

    // All three heads are recorded now.
    for w in &works {
        assert_eq!(change_head_observations(&store, w).len(), 1);
    }
}

#[test]
fn a_tick_deadline_stops_the_outbox_drain_and_defers_the_remainder() {
    let store = SqliteStore::in_memory(4).unwrap();
    let mut gh = FixtureGithub::new();
    for i in 0..3 {
        let (w, e) = submit(&store, &format!("k-{i}"));
        admit(&store, &e);
        queue_open_change(&store, &w);
    }

    // Deadline already reached when the drain starts: the phase-entry
    // style gate — no write is attempted, every pending intent is
    // reported for the next pass.
    let early = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, Some(NOW), None, 8)
        .unwrap();
    assert_eq!(early.attempted, 0, "a late drain starts nothing");
    assert_eq!(
        early.deferred.len(),
        3,
        "every intent is reported, none dropped"
    );
    assert!(gh.applied().is_empty(), "no host write happened");

    // Deadline passes mid-drain: the item that started runs, the rest
    // of the loop checks the deadline before starting anything new.
    let mut calls = 0u32;
    let mid = gh
        .apply_outbox_bounded(
            &store,
            &config(),
            &mut || {
                calls += 1;
                if calls <= 2 { NOW } else { NOW + 60 }
            },
            Some(NOW + 30),
            None,
            8,
        )
        .unwrap();
    assert_eq!(mid.attempted, 1, "the item started before the deadline");
    assert_eq!(
        mid.deferred.len(),
        2,
        "the remainder waits for the next pass"
    );
    assert_eq!(gh.applied().len(), 1);
}

#[test]
fn a_scoped_outbox_never_touches_another_works_intent() {
    let (store, mut gh, w1, w2) = two_changes();
    // Fresh intents on both Works; drain only w1's scope.
    queue_open_change(&store, &w1);
    queue_open_change(&store, &w2);
    let report = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, Some(&w1), 8)
        .unwrap();
    assert_eq!(report.attempted, 1, "only the scoped Work's intent ran");
    assert_eq!(report.applied, 1);
    let pending: Vec<String> = store
        .transact(|tx| tx.action_intents())
        .unwrap()
        .into_iter()
        .filter(|i| i.status == HostActionStatus::Pending)
        .map(|i| i.work_id.0)
        .collect();
    assert_eq!(
        pending,
        vec![w2.0.clone()],
        "the out-of-scope intent stayed pending for an unscoped pass"
    );
}
