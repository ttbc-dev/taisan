//! Guarded push & change-request effects (U3.3): every write is a
//! separately persisted pending intent before execution, the push is
//! bound to the remote ref position recorded at intent time, and each
//! applied receipt reports the fresh read-back state — never the
//! attempted payload. A stale head refuses the write; a fault in one
//! effect leaves the others' receipts intact and publication is never
//! claimed complete on partial success.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_github::fixture::{FixtureGithub, RemoteMutation};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
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
                credential_ref: CredentialRef::trusted("cred-publication"),
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
        required_checks: vec!["ci".into()],
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
                intent: "publish the change".into(),
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
        panic!("submit: {d:?}")
    };
    (work_id.unwrap(), execution_id)
}

fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) -> String {
    store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: publisher(),
                    action,
                    payload_digest: "test".into(),
                    status: HostActionStatus::Pending,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(action_id)
        })
        .unwrap()
}

fn intent(store: &SqliteStore, action_id: &str) -> HostActionIntent {
    store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .into_iter()
                .find(|i| i.action_id == action_id))
        })
        .unwrap()
        .unwrap()
}

fn admit(store: &SqliteStore, eid: &ExecutionId) {
    store
        .transact(|tx| admit_queued(tx, &config(), eid, NOW).map(|_| ()))
        .unwrap();
}

/// AC: the ref position recorded at intent time is the guard — a remote
/// that moved since refuses the write, records `Failed{stale_head}`,
/// does not update the ref, and surfaces the fresh position.
#[test]
fn stale_head_refuses_the_write_and_surfaces_the_fresh_position() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u33-stale");
    admit(&store, &eid);

    let mut gh = FixtureGithub::new();
    // The intent was recorded against head-old; the remote is already
    // at head-other — a stale head.
    gh.set_branch(&repo(), "work/w", "head-other", &publisher());
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-new"),
            expected_prior: Some(rev("head-old")),
        },
    );
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    let i = intent(&store, &action_id);
    assert_eq!(
        i.status,
        HostActionStatus::Failed {
            reason: "stale_head".into()
        },
        "the stale-head refusal is an explicit failed receipt"
    );
    assert!(
        gh.applied().iter().all(|a| !a.starts_with("push_ref:")),
        "the refused write never reached the remote: {:?}",
        gh.applied()
    );
    assert_eq!(
        gh.remote_head(&repo(), "work/w"),
        Some("head-other".to_string()),
        "the remote ref is untouched"
    );
    // The refusal surfaces the fresh remote position so the caller can
    // re-issue against it.
    let positions: Vec<String> = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::BranchState {
                branch,
                state: BranchState::At { revision },
                ..
            } if branch == "work/w" => Some(revision.object.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(positions, vec!["head-other".to_string()]);
}

/// AC: each write effect persists its own pending intent before
/// execution and its own receipt after — and the push receipt reports
/// the post-state a fresh read observes, not the value the write
/// attempted. A racing actor's descendant lands between write and
/// read-back; the receipt records `Ancestor`, not `Equal`.
#[test]
fn per_effect_receipts_report_the_fresh_read_back_state() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u33-readback");
    admit(&store, &eid);

    let push_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    let update_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change(),
            expected_head: rev("head-1"),
            title: Some("updated title".into()),
            body: Some("updated body".into()),
        },
    );
    // Every effect is durable and pending before any write executes.
    for id in [&push_id, &update_id] {
        assert_eq!(intent(&store, id).status, HostActionStatus::Pending);
    }

    let mut gh = FixtureGithub::new();
    // The change's remote head matches the guard; a racing actor moves
    // the branch after the push write but before its read-back.
    gh.set_change_head(&change(), "head-1", &publisher());
    let racer = HostActorRef {
        host: host(),
        actor: HostActorId::trusted("racer"),
    };
    gh.queue_remote_mutation(RemoteMutation::MoveBranch {
        repository: repo(),
        branch: "work/w".into(),
        head: "head-2".into(),
        actor: racer,
    });
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    // Push: the write landed at head-1, but the read-back observes the
    // racer's descendant — the receipt binds observed, not attempted.
    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    let [
        NormalizedEffectFact::RefPublished {
            produced,
            observed,
            relation,
            ..
        },
    ] = push.effects.as_slice()
    else {
        panic!("push receipt: {:?}", push.effects)
    };
    assert_eq!(*produced, rev("head-1"));
    assert_eq!(*observed, rev("head-2"), "read-back, not attempted");
    assert_eq!(*relation, RefRelation::Ancestor);
    assert_eq!(gh.remote_head(&repo(), "work/w"), Some("head-2".into()));

    // Update: the receipt's field digest covers the remote's stored
    // fields — and a racing metadata write between write and read-back
    // is what the digest reports.
    let update = intent(&store, &update_id);
    assert_eq!(update.status, HostActionStatus::Applied);
    let [NormalizedEffectFact::MetadataUpdated { fields_digest, .. }] = update.effects.as_slice()
    else {
        panic!("update receipt: {:?}", update.effects)
    };
    let attempted = workloop_core::domain::sha256_hex(
        "change-meta-v1",
        &workloop_core::domain::canonical_json(&serde_json::json!({
            "title": Some("updated title"),
            "body": Some("updated body"),
        })),
    );
    assert_eq!(
        *fields_digest, attempted,
        "no race queued for the update: the digest covers the stored fields"
    );
    // The receipt is bound to the executing actor that authenticated.
    assert_eq!(push.authenticated_actor, Some(publisher()));
    assert_eq!(update.authenticated_actor, Some(publisher()));
}

/// AC: a raced update receipt reports the racing actor's fields — the
/// digest is a read-back of remote state, never the attempted payload.
#[test]
fn raced_metadata_update_reports_the_observed_fields() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u33-raced-meta");
    admit(&store, &eid);

    let update_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change(),
            expected_head: rev("head-1"),
            title: Some("my title".into()),
            body: Some("my body".into()),
        },
    );
    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &publisher());
    gh.queue_remote_mutation(RemoteMutation::SetChangeMeta {
        change: change(),
        title: Some("racer title".into()),
        body: None,
    });
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    let update = intent(&store, &update_id);
    assert_eq!(update.status, HostActionStatus::Applied);
    let [NormalizedEffectFact::MetadataUpdated { fields_digest, .. }] = update.effects.as_slice()
    else {
        panic!("update receipt: {:?}", update.effects)
    };
    let observed = workloop_core::domain::sha256_hex(
        "change-meta-v1",
        &workloop_core::domain::canonical_json(&serde_json::json!({
            "title": Some("racer title"),
            "body": Some("my body"),
        })),
    );
    assert_eq!(
        *fields_digest, observed,
        "the digest reports what the read-back observed"
    );
}

/// AC: push succeeds and the change-request create faults — the
/// successful push receipt and the failed create receipt are each
/// preserved, and no publication completion is claimed.
#[test]
fn push_success_create_failure_keeps_both_receipts() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u33-partial");
    admit(&store, &eid);

    let push_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/w".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: "b".into(),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.force_failure("open_change_request", "remote rejected the create");
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    // The push's applied receipt survives the sibling fault: the ref
    // publish is real remote state, not rewritten by the create's
    // failure.
    let push = intent(&store, &push_id);
    assert_eq!(push.status, HostActionStatus::Applied);
    assert!(matches!(
        push.effects.as_slice(),
        [NormalizedEffectFact::RefPublished { .. }]
    ));
    assert_eq!(gh.remote_head(&repo(), "work/w"), Some("head-1".into()));
    assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-1"]);

    // The failed create is explicit and no ChangeOpened fact exists —
    // publication is never claimed complete on partial success.
    let open = intent(&store, &open_id);
    assert_eq!(
        open.status,
        HostActionStatus::Failed {
            reason: "remote rejected the create".into()
        }
    );
    assert!(open.effects.is_empty());
    let any_opened = store
        .transact(|tx| {
            Ok(tx
                .action_intents()?
                .iter()
                .flat_map(|i| i.effects.iter())
                .any(|e| matches!(e, NormalizedEffectFact::ChangeOpened { .. })))
        })
        .unwrap();
    assert!(
        !any_opened,
        "no change-opened evidence exists — publication is not complete"
    );
}

/// AC: a fault between intent persistence and execution leaves the
/// pending intent durable — the outbox row, not the process, owns the
/// outstanding effect.
#[test]
fn fault_after_intent_persistence_leaves_the_pending_intent_durable() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u33-durable");
    admit(&store, &eid);

    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );
    // The simulated fault: the process dies before the outbox drains.
    // The durable row still records the effect and its pending status —
    // no receipt was fabricated.
    let i = intent(&store, &action_id);
    assert_eq!(i.status, HostActionStatus::Pending);
    assert!(i.effects.is_empty());
    let receipts = store
        .transact(|tx| tx.all_observations())
        .unwrap()
        .iter()
        .filter(|o| matches!(o.fact, ObservationFact::HostActionResult { .. }))
        .count();
    assert_eq!(receipts, 0, "no execution means no receipt");
    let outstanding = store
        .transact(|tx| workloop_core::projection::outstanding_actions(tx, &work_id))
        .unwrap();
    assert_eq!(outstanding, vec![action_id]);
}
