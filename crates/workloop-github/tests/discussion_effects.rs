//! Discussion & review effects (U3.4): `post_comment`,
//! `publish_review_feedback`, `thread_reply`, `resolve_thread`, and
//! `mark_ready` are each a separately persisted intent/receipt pair.
//! Before execution the adapter validates support (the operation is in
//! the host capability map) and actor eligibility (the bound actor may
//! perform it on this subject) — `unsupported`, `denied`, and
//! `ineligible` are distinct explicit classes, and every applied
//! receipt cites the fresh read-back object identity.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
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

fn reviewer() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("review-agent"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Discussion,
                actor: reviewer(),
                credential_ref: CredentialRef::trusted("cred-discussion"),
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
                intent: "discuss the change".into(),
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
                    role: ActorRole::Discussion,
                    actor: reviewer(),
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

/// AC: each of the five effect types persists its own intent/receipt
/// pair — five intents, five distinct receipts, each carrying its own
/// normalized effect fact.
#[test]
fn each_effect_type_persists_its_own_intent_and_receipt() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-five");
    admit(&store, &eid);

    let comment_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change(),
            expected_head: rev("head-1"),
            marker: "workloop".into(),
            body: "noted".into(),
        },
    );
    let review_id = insert_intent(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change(),
            head: rev("head-1"),
            body: "looks right".into(),
        },
    );
    let reply_id = insert_intent(
        &store,
        &work_id,
        HostAction::ReplyToThread {
            change: change(),
            thread_id: "th-1".into(),
            body: "answered".into(),
        },
    );
    let resolve_id = insert_intent(
        &store,
        &work_id,
        HostAction::ResolveThread {
            change: change(),
            thread_id: "th-1".into(),
            head: rev("head-1"),
        },
    );
    let ready_id = insert_intent(
        &store,
        &work_id,
        HostAction::MarkReady {
            change: change(),
            expected_head: rev("head-1"),
        },
    );
    let ids = [&comment_id, &review_id, &reply_id, &resolve_id, &ready_id];
    for id in ids {
        assert_eq!(intent(&store, id).status, HostActionStatus::Pending);
    }

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.add_thread(&change(), "th-1");
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    // Five separately persisted receipts — each intent carries the
    // normalized fact of its own effect type.
    for id in ids {
        assert_eq!(
            intent(&store, id).status,
            HostActionStatus::Applied,
            "intent {id} applied"
        );
    }
    let kinds: Vec<&str> = ids
        .iter()
        .map(|id| {
            let i = intent(&store, id);
            assert_eq!(i.effects.len(), 1, "one fact per receipt");
            match &i.effects[0] {
                NormalizedEffectFact::CommentPosted { .. } => "comment",
                NormalizedEffectFact::ReviewPublished { .. } => "review",
                NormalizedEffectFact::ReplyPosted { .. } => "reply",
                NormalizedEffectFact::ThreadResolved { .. } => "thread",
                NormalizedEffectFact::ReadyObserved { .. } => "ready",
                other => panic!("unexpected fact: {other:?}"),
            }
        })
        .collect();
    assert_eq!(kinds, ["comment", "review", "reply", "thread", "ready"]);
    // And the remote state moved for each effect independently.
    assert!(gh.is_ready("cr-1"));
    assert_eq!(gh.thread("th-1").unwrap().replies.len(), 1);
    assert!(gh.thread("th-1").unwrap().resolved);
}

/// AC: applied receipts cite the fresh read-back object identity —
/// the remote-issued comment/review/reply ids and the observed thread
/// state — never the attempted payload values.
#[test]
fn applied_receipts_cite_the_freshly_read_object_identity() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-readback");
    admit(&store, &eid);

    let comment_id = insert_intent(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change(),
            expected_head: rev("head-1"),
            marker: "workloop".into(),
            body: "the body".into(),
        },
    );
    let review_id = insert_intent(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change(),
            head: rev("head-1"),
            body: "review body".into(),
        },
    );
    let resolve_id = insert_intent(
        &store,
        &work_id,
        HostAction::ResolveThread {
            change: change(),
            thread_id: "th-1".into(),
            head: rev("head-1"),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.add_thread(&change(), "th-1");
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    // The comment receipt binds the remote-issued id and the digest of
    // the remote's stored body.
    let comment = intent(&store, &comment_id);
    let [
        NormalizedEffectFact::CommentPosted {
            comment_id: cid,
            body_digest,
            ..
        },
    ] = comment.effects.as_slice()
    else {
        panic!("comment receipt: {:?}", comment.effects)
    };
    assert_eq!(*cid, format!("c-{comment_id}"), "remote-issued id");
    let stored_digest = workloop_core::domain::sha256_hex(
        "comment-v1",
        &workloop_core::domain::canonical_json(&serde_json::json!({
            "body": "the body",
        })),
    );
    assert_eq!(*body_digest, stored_digest);

    // The review receipt binds the remote-issued review id.
    let review = intent(&store, &review_id);
    let [
        NormalizedEffectFact::ReviewPublished {
            review_id: rid,
            head,
            ..
        },
    ] = review.effects.as_slice()
    else {
        panic!("review receipt: {:?}", review.effects)
    };
    assert_eq!(*rid, format!("rv-{review_id}"));
    assert_eq!(*head, rev("head-1"));

    // The resolve receipt reports the observed thread state and head.
    let resolve = intent(&store, &resolve_id);
    let [
        NormalizedEffectFact::ThreadResolved {
            thread_id,
            target,
            observed,
            relation,
            ..
        },
    ] = resolve.effects.as_slice()
    else {
        panic!("resolve receipt: {:?}", resolve.effects)
    };
    assert_eq!(thread_id, "th-1");
    assert_eq!(target.head, rev("head-1"));
    assert_eq!(*observed, rev("head-1"));
    assert_eq!(*relation, RefRelation::Equal);
    assert!(gh.thread("th-1").unwrap().resolved);
}

/// AC: a denied operation records `Failed{reason: denied}` explicitly
/// and never satisfies its stage — the failed receipt is terminal, so
/// even a later applied claim cannot resurrect it.
#[test]
fn denied_review_stays_explicit_and_never_satisfies_the_stage() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-denied");
    admit(&store, &eid);

    let review_id = insert_intent(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change(),
            head: rev("head-1"),
            body: "feedback".into(),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    // The bound actor lacks the grant for review publication.
    gh.deny("publish_review_feedback", &reviewer());
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    let review = intent(&store, &review_id);
    assert_eq!(
        review.status,
        HostActionStatus::Failed {
            reason: "denied".into()
        }
    );
    assert!(review.effects.is_empty());
    assert!(
        gh.applied().is_empty(),
        "the denied write never reached the remote"
    );
    // The stage stays unsatisfied: no applied evidence exists, and the
    // intent is not outstanding (it is a terminal explicit failure).
    let outstanding = store
        .transact(|tx| workloop_core::projection::outstanding_actions(tx, &work_id))
        .unwrap();
    assert!(outstanding.is_empty());

    // A later applied claim for the same action_id cannot resurrect
    // the denied intent — terminal receipts stick.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: Some(eid),
            fact: ObservationFact::HostActionResult {
                action_id: review_id.clone(),
                authenticated_actor: Some(reviewer()),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::ReviewPublished {
                    change: change(),
                    review_id: "rv-forged".into(),
                    head: rev("head-1"),
                    body_digest: "d".into(),
                }],
            },
            observed_unix_seconds: NOW + 1,
        },
        NOW + 1,
    )
    .unwrap();
    assert_eq!(
        intent(&store, &review_id).status,
        HostActionStatus::Failed {
            reason: "denied".into()
        },
        "a denied receipt cannot be upgraded by a later claim"
    );
}

/// AC: `unsupported` and `denied` record as distinct result classes —
/// one for an operation absent from the capability map, one for an
/// actor that lacks the grant.
#[test]
fn unsupported_and_denied_are_distinct_result_classes() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-classes");
    admit(&store, &eid);

    let resolve_id = insert_intent(
        &store,
        &work_id,
        HostAction::ResolveThread {
            change: change(),
            thread_id: "th-1".into(),
            head: rev("head-1"),
        },
    );
    let review_id = insert_intent(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change(),
            head: rev("head-1"),
            body: "feedback".into(),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.add_thread(&change(), "th-1");
    // This host does not implement thread resolution at all.
    gh.remove_support("resolve_thread");
    // And the bound actor lacks the review grant.
    gh.deny("publish_review_feedback", &reviewer());
    let report = gh
        .apply_outbox_bounded(&store, &config(), &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(
        report.unsupported, 1,
        "the terminal unsupported receipt is counted in the pass report"
    );
    assert_eq!(report.failed, 1);
    assert_eq!(report.applied, 0);

    let resolve = intent(&store, &resolve_id);
    assert!(
        matches!(resolve.status, HostActionStatus::Unsupported { .. }),
        "unsupported is its own class: {:?}",
        resolve.status
    );
    let review = intent(&store, &review_id);
    assert_eq!(
        review.status,
        HostActionStatus::Failed {
            reason: "denied".into()
        }
    );
    assert!(
        !matches!(review.status, HostActionStatus::Unsupported { .. }),
        "denied is never reported as unsupported"
    );
    assert!(gh.applied().is_empty());
    assert!(
        !gh.thread("th-1").unwrap().resolved,
        "the unsupported write never landed"
    );
}

/// An actor whose class cannot perform the operation records
/// `Failed{reason: ineligible}` — distinct from `denied` and from
/// `unsupported`.
#[test]
fn ineligible_actor_is_explicit_and_distinct() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-ineligible");
    admit(&store, &eid);

    let reply_id = insert_intent(
        &store,
        &work_id,
        HostAction::ReplyToThread {
            change: change(),
            thread_id: "th-1".into(),
            body: "reply".into(),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.add_thread(&change(), "th-1");
    gh.mark_ineligible("thread_reply", &reviewer());
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    assert_eq!(
        intent(&store, &reply_id).status,
        HostActionStatus::Failed {
            reason: "ineligible".into()
        }
    );
    assert!(gh.thread("th-1").unwrap().replies.is_empty());
}

/// AC: a racing actor moving the change across the write/read gap does
/// not wedge a landed head-bound write — the resolve receipt binds the
/// observed head's `Ancestor` relation to the bound head, and the
/// intent reconciles `applied` instead of degrading to `unknown`, like
/// the push path's `RefPublished` evidence.
#[test]
fn raced_resolve_reconciles_applied_on_ancestor_observation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-raced-resolve");
    admit(&store, &eid);

    let resolve_id = insert_intent(
        &store,
        &work_id,
        HostAction::ResolveThread {
            change: change(),
            thread_id: "th-1".into(),
            head: rev("head-1"),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.add_thread(&change(), "th-1");
    // The racer moves the change head between the write and its
    // read-back — the resolve lands, the fresh read sees the moved head.
    gh.queue_remote_mutation(RemoteMutation::MoveChange {
        change: change(),
        head: "head-2".into(),
        actor: HostActorRef {
            host: host(),
            actor: HostActorId::trusted("racer"),
        },
    });
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    let resolve = intent(&store, &resolve_id);
    assert_eq!(
        resolve.status,
        HostActionStatus::Applied,
        "the raced resolve reconciles applied, not unknown: {:?}",
        resolve.status
    );
    let [
        NormalizedEffectFact::ThreadResolved {
            target,
            observed,
            relation,
            ..
        },
    ] = resolve.effects.as_slice()
    else {
        panic!("resolve receipt: {:?}", resolve.effects)
    };
    assert_eq!(target.head, rev("head-1"), "the bound head is echoed");
    assert_eq!(
        *observed,
        rev("head-2"),
        "the read-back head is the racer's"
    );
    assert_eq!(*relation, RefRelation::Ancestor);
}

/// AC: the same write/read race on `mark_ready` — the receipt reports
/// the observed head's `Ancestor` relation and reconciles `applied`.
#[test]
fn raced_mark_ready_reconciles_applied_on_ancestor_observation() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, eid) = submit(&store, "u34-raced-ready");
    admit(&store, &eid);

    let ready_id = insert_intent(
        &store,
        &work_id,
        HostAction::MarkReady {
            change: change(),
            expected_head: rev("head-1"),
        },
    );

    let mut gh = FixtureGithub::new();
    gh.set_change_head(&change(), "head-1", &reviewer());
    gh.queue_remote_mutation(RemoteMutation::MoveChange {
        change: change(),
        head: "head-2".into(),
        actor: HostActorRef {
            host: host(),
            actor: HostActorId::trusted("racer"),
        },
    });
    gh.apply_outbox(&store, &config(), NOW).unwrap();

    let ready = intent(&store, &ready_id);
    assert_eq!(
        ready.status,
        HostActionStatus::Applied,
        "the raced mark_ready reconciles applied, not unknown: {:?}",
        ready.status
    );
    let [
        NormalizedEffectFact::ReadyObserved {
            head,
            observed,
            relation,
            ..
        },
    ] = ready.effects.as_slice()
    else {
        panic!("ready receipt: {:?}", ready.effects)
    };
    assert_eq!(*head, rev("head-1"), "the bound head is echoed");
    assert_eq!(*observed, rev("head-2"));
    assert_eq!(*relation, RefRelation::Ancestor);
}
