//! U3.5 unknown-effect reconciliation: outstanding intents settle
//! from a read-only comparison of the remote's observed post-state
//! against the intent's recorded fields — never by replaying the
//! effect. A receipt authenticated by an actor other than the one the
//! intent bound records `unknown{actor_mismatch}` and settles
//! nothing; applied receipts and pending intents survive store
//! restart; and neither a rotated credential nor a changed
//! configuration retargets an outstanding action.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::fixture::{CredentialBook, FixtureGithub};
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

fn actor(slug: &str) -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted(slug),
    }
}

fn publisher() -> HostActorRef {
    actor("pub-agent")
}

fn change(id: &str) -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted(id),
        display_number: Some(1),
    }
}

fn role_binding(role: ActorRole, slug: &str, credential_ref: &str) -> RoleBinding {
    RoleBinding {
        role,
        actor: actor(slug),
        credential_ref: CredentialRef::trusted(credential_ref),
    }
}

fn config_with(repository: &RepositoryRef, actors: Vec<RoleBinding>) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repository.clone(),
            contract_version: 1,
            actors,
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

fn config() -> WorkloopConfig {
    config_with(
        &repo(),
        vec![role_binding(
            ActorRole::Publication,
            "pub-agent",
            "env:TAISAN_TEST_RECONCILE_CRED",
        )],
    )
}

fn submit(store: &SqliteStore, config: &WorkloopConfig, key: &str) -> WorkId {
    let d = handle_command(
        store,
        config,
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("reconciliation".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "reconcile contract work".into(),
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
    } = d
    else {
        panic!("submit: {d:?}")
    };
    work_id
}

fn insert_intent(store: &SqliteStore, work_id: &WorkId, action: HostAction) -> String {
    insert_intent_with(store, work_id, action, HostActionStatus::Pending)
}

fn insert_intent_with(
    store: &SqliteStore,
    work_id: &WorkId,
    action: HostAction,
    status: HostActionStatus,
) -> String {
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
                    payload_digest: "reconcile".into(),
                    status,
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
                .find(|i| i.action_id == action_id)
                .expect("intent recorded"))
        })
        .unwrap()
}

/// AC: the worker is killed between the remote write landing and its
/// receipt being persisted — the durable record keeps the intent
/// `pending` while the remote already shows the effect. Recovery must
/// reread the host facts and settle each intent from the observed
/// evidence; replaying the effect to learn its outcome is never
/// allowed.
#[test]
fn kill_between_apply_and_receipt_settles_from_the_host_read_not_a_replay() {
    let dir =
        std::env::temp_dir().join(format!("workloop-u35-kill-{}-{}", std::process::id(), NOW));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let config = config();
    let mut gh = FixtureGithub::new();

    // Process 1: the drain attempts the first intent and dies right
    // after the remote write — before the receipt can persist.
    let (push_id, open_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let work_id = submit(&store, &config, "u35-lost");
        let push_id = insert_intent(
            &store,
            &work_id,
            HostAction::PushRef {
                branch: "work/lost".into(),
                produced: rev("head-9"),
                expected_prior: None,
            },
        );
        let open_id = insert_intent(
            &store,
            &work_id,
            HostAction::OpenChangeRequest {
                source_branch: "work/lost".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
        );
        gh.set_crash_after_apply();
        gh.apply_outbox(&store, &config, NOW).unwrap();
        assert_eq!(
            intent(&store, &push_id).status,
            HostActionStatus::Pending,
            "the receipt died with the worker — the intent never settled"
        );
        assert_eq!(
            intent(&store, &open_id).status,
            HostActionStatus::Pending,
            "the dead worker never reached the second intent"
        );
        assert_eq!(
            gh.remote_head(&repo(), "work/lost"),
            Some("head-9".to_string()),
            "the remote write did land"
        );
        assert_eq!(gh.applied(), &["push_ref:test-host/local-repo:head-9"]);
        (push_id, open_id)
    };

    // Restart: the store reopens and the durable records survive —
    // the remote, a different process, kept the landed effect. The
    // new drain's pre-write read adopts the landed push rather than
    // pushing again, then the worker dies the same way on the open:
    // its remote write lands and the receipt is lost.
    let store = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(
        intent(&store, &push_id).status,
        HostActionStatus::Pending,
        "the lost receipt survives the restart as outstanding"
    );
    assert_eq!(intent(&store, &open_id).status, HostActionStatus::Pending);

    gh.set_crash_after_apply();
    gh.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent(&store, &push_id).status,
        HostActionStatus::Applied,
        "the pre-write read adopted the landed push — no second push"
    );
    assert_eq!(
        intent(&store, &open_id).status,
        HostActionStatus::Pending,
        "the open's receipt died with the worker"
    );
    assert_eq!(
        gh.applied().len(),
        2,
        "one push plus one open — never a replayed write: {:?}",
        gh.applied()
    );

    // Recovery settles the open intent through the host read alone:
    // the reconcile pass replays nothing.
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 1, "the open intent settles from the read");
    assert_eq!(intent(&store, &open_id).status, HostActionStatus::Applied);
    match &intent(&store, &open_id).effects[..] {
        [
            NormalizedEffectFact::ChangeOpened {
                change: opened,
                head,
                source_branch,
                target_branch,
            },
        ] => {
            // The remote-issued id the lost receipt never captured is
            // recovered from the read.
            assert_eq!(opened.repository, repo());
            assert_eq!(opened.change_request.as_str(), "cr-1");
            assert_eq!(*head, rev("head-9"));
            assert_eq!(source_branch, "work/lost");
            assert_eq!(target_branch, "main");
        }
        other => panic!("open receipt missing ChangeOpened: {other:?}"),
    }
    assert_eq!(
        gh.applied().len(),
        2,
        "reconciliation performed no remote write: {:?}",
        gh.applied()
    );

    // A write drain afterwards changes nothing: terminal intents are
    // never re-attempted.
    gh.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(gh.applied().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC: a `pending` intent whose effect already landed is adopted from
/// the pre-write read — the apply pass never re-executes the write to
/// discover whether it happened.
#[test]
fn pending_intent_whose_effect_landed_is_adopted_without_a_second_write() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-adopt");
    let mut gh = FixtureGithub::new();
    gh.register_change(&change("cr-3"), "work/w", "main", "head-1", &publisher());

    let open_id = insert_intent(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/w".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
    );
    gh.apply_outbox(&store, &config, NOW).unwrap();

    assert_eq!(intent(&store, &open_id).status, HostActionStatus::Applied);
    assert!(
        gh.applied().is_empty(),
        "the pre-write read adopted the effect — no second create: {:?}",
        gh.applied()
    );
}

/// AC: a receipt authenticated by actor B for an intent bound to
/// actor A records `unknown{actor_mismatch}` and never settles the
/// intent — whatever status the receipt claims. The intent stays
/// reconcilable by a later honest read.
#[test]
fn receipt_by_another_actor_records_actor_mismatch_and_never_settles() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-actor");
    let mut gh = FixtureGithub::new();
    gh.set_branch(&repo(), "work/w", "head-1", &publisher());
    let action_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/w".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
    );

    // A synthetic receipt claims applied with a fact that matches the
    // payload — but authenticated as another actor. The claim is
    // invalid evidence: recorded as actor_mismatch, never applied.
    gh.emit(
        &store,
        &config,
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: Some(actor("other-agent")),
                status: HostActionStatus::Applied,
                effects: vec![NormalizedEffectFact::RefPublished {
                    repository: repo(),
                    branch: "work/w".into(),
                    produced: rev("head-1"),
                    observed: rev("head-1"),
                    relation: RefRelation::Equal,
                }],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let recorded = intent(&store, &action_id);
    assert_eq!(
        recorded.status,
        HostActionStatus::Unknown {
            reason: "actor_mismatch".into()
        },
        "a foreign-authenticated applied claim settles nothing"
    );
    assert_eq!(recorded.authenticated_actor, Some(actor("other-agent")));

    // A foreign-authenticated `failed` claim settles nothing either —
    // a mismatched receipt can never kill the intent.
    gh.emit(
        &store,
        &config,
        Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::HostActionResult {
                action_id: action_id.clone(),
                authenticated_actor: Some(actor("other-agent")),
                status: HostActionStatus::Failed {
                    reason: "denied".into(),
                },
                effects: vec![],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(
        intent(&store, &action_id).status,
        HostActionStatus::Unknown {
            reason: "actor_mismatch".into()
        },
        "a foreign-authenticated failed claim settles nothing"
    );

    // The intent stays outstanding and reconcilable: the remote's own
    // evidence — attributed to the bound actor — settles it later.
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(intent(&store, &action_id).status, HostActionStatus::Applied);
}

/// AC: the remote's own attribution authenticates reconciliation — an
/// effect the remote credits to another actor is recorded
/// `unknown{actor_mismatch}` by the durable ingest path, never
/// adopted as the bound actor's work.
#[test]
fn effect_attributed_to_another_actor_never_settles_the_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-foreign");
    let mut gh = FixtureGithub::new();
    gh.register_change(
        &change("cr-9"),
        "work/w",
        "main",
        "head-1",
        &actor("other-agent"),
    );
    let open_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/w".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );

    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.still_unknown, 1);
    assert_eq!(
        intent(&store, &open_id).status,
        HostActionStatus::Unknown {
            reason: "actor_mismatch".into()
        },
        "another actor's identical effect is never adopted"
    );
    assert_eq!(
        intent(&store, &open_id).authenticated_actor,
        Some(actor("other-agent")),
        "the receipt records the attribution it observed"
    );
}

/// AC: a persisted applied receipt beside a still-pending intent
/// remains readable after restart, and reconciliation continues from
/// the persisted records.
#[test]
fn partial_receipts_survive_restart_and_reconcile_after_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "workloop-u35-restart-{}-{}",
        std::process::id(),
        NOW
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let config = config();

    let (push_id, open_id) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let work_id = submit(&store, &config, "u35-restart");
        let mut gh = FixtureGithub::new();
        let push_id = insert_intent(
            &store,
            &work_id,
            HostAction::PushRef {
                branch: "work/r".into(),
                produced: rev("head-1"),
                expected_prior: None,
            },
        );
        gh.apply_outbox(&store, &config, NOW).unwrap();
        assert_eq!(
            intent(&store, &push_id).status,
            HostActionStatus::Applied,
            "the first effect's receipt is durable before the crash"
        );
        // The second effect was still pending when the process died.
        let open_id = insert_intent(
            &store,
            &work_id,
            HostAction::OpenChangeRequest {
                source_branch: "work/r".into(),
                target_branch: "main".into(),
                title: "t".into(),
                body: String::new(),
            },
        );
        // The remote already shows the second effect — its receipt was
        // lost with the process.
        gh.register_change(&change("cr-2"), "work/r", "main", "head-1", &publisher());
        (push_id, open_id)
    };

    let store = SqliteStore::open(&db, 1).unwrap();
    let push = intent(&store, &push_id);
    assert_eq!(
        push.status,
        HostActionStatus::Applied,
        "the applied receipt survived the restart"
    );
    assert_eq!(push.effects.len(), 1);
    assert_eq!(
        intent(&store, &open_id).status,
        HostActionStatus::Pending,
        "the pending intent survived the restart"
    );

    // Reconciliation after the restart reads the remote's own record —
    // under the restarted process the remote survived while the
    // receipt did not.
    let mut gh = FixtureGithub::new();
    gh.set_branch(&repo(), "work/r", "head-1", &publisher());
    gh.register_change(&change("cr-2"), "work/r", "main", "head-1", &publisher());
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(intent(&store, &open_id).status, HostActionStatus::Applied);
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC: credential rotation never rebinds an outstanding intent — a
/// renewed value authenticating as another actor blocks both writes
/// and reconciliation, and the intent retains the actor identity
/// recorded at intent time. A same-actor renewal proceeds under the
/// original binding.
#[test]
fn credential_rotation_keeps_outstanding_intents_on_their_recorded_actor() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-rotate");
    let mut book = CredentialBook::default();
    book.seat_env("TAISAN_TEST_RECONCILE_CRED", "token-v1");
    book.seat_actor("token-v1", publisher());
    let mut gh = FixtureGithub::new().with_credentials(book);

    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/rot".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    gh.set_branch(&repo(), "work/rot", "head-1", &publisher());

    // The credential rotates to a value authenticating as a different
    // actor: the outstanding intent cannot be rebound, so neither
    // reconcile nor apply may act on it.
    gh.credentials_mut()
        .unwrap()
        .seat_env("TAISAN_TEST_RECONCILE_CRED", "token-v2");
    gh.credentials_mut()
        .unwrap()
        .seat_actor("token-v2", actor("other-agent"));
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        report.credential_blocked,
        vec![workloop_github::fixture::CredentialBlock {
            action_id: action_id.clone(),
            code: "credential.actor_mismatch",
        }]
    );
    let blocked = intent(&store, &action_id);
    assert!(
        matches!(blocked.status, HostActionStatus::Unknown { .. }),
        "the intent stays outstanding: {:?}",
        blocked.status
    );
    assert_eq!(
        blocked.actor,
        publisher(),
        "the intent keeps its recorded actor"
    );

    // A same-actor renewal resolves the new secret under the original
    // binding: reconciliation settles from the observed evidence.
    gh.credentials_mut()
        .unwrap()
        .seat_env("TAISAN_TEST_RECONCILE_CRED", "token-v3");
    gh.credentials_mut()
        .unwrap()
        .seat_actor("token-v3", publisher());
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 1);
    let settled = intent(&store, &action_id);
    assert_eq!(settled.status, HostActionStatus::Applied);
    assert_eq!(settled.actor, publisher());
}

/// AC: configuration changes between intent and observation do not
/// rewrite reconciliation inputs — the compare runs on the persisted
/// intent's recorded fields, not the live installation config.
#[test]
fn config_change_midflight_reconciles_from_the_recorded_intent_fields() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config_a = config();
    let work_id = submit(&store, &config_a, "u35-config");
    let mut gh = FixtureGithub::new();
    gh.set_branch(&repo(), "work/cfg", "head-1", &publisher());
    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/cfg".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );

    // The installation config moved to another repository and actor
    // between the intent and the read: reconciliation must still
    // compare the persisted intent against the observed remote under
    // its recorded fields.
    let config_b = config_with(
        &RepositoryRef::new(host(), RepositoryId::trusted("other-repo")),
        vec![role_binding(
            ActorRole::Publication,
            "other-agent",
            "env:TAISAN_TEST_RECONCILE_CRED",
        )],
    );
    let report = gh.reconcile_outbox(&store, &config_b, NOW).unwrap();
    assert_eq!(report.applied, 1, "the recorded fields reconcile");
    let settled = intent(&store, &action_id);
    assert_eq!(settled.status, HostActionStatus::Applied);
    assert_eq!(settled.repository, repo(), "recorded binding untouched");
    assert_eq!(settled.actor, publisher(), "recorded actor untouched");
    match &settled.effects[..] {
        [NormalizedEffectFact::RefPublished { repository, .. }] => {
            assert_eq!(
                *repository,
                repo(),
                "the receipt keeps the recorded repository"
            )
        }
        other => panic!("missing RefPublished fact: {other:?}"),
    }
}

/// A definitive-absence read settles an `unknown` intent `failed` —
/// the effect demonstrably did not land. A `pending` intent is
/// possibly never attempted, so the same absence leaves it pending.
#[test]
fn unknown_effect_absent_at_read_settles_failed_pending_stays_pending() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-absent");
    let gh = FixtureGithub::new();

    let unknown_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/never".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    let pending_id = insert_intent(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/later".into(),
            produced: rev("head-2"),
            expected_prior: None,
        },
    );

    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.failed, 1);
    match intent(&store, &unknown_id).status {
        HostActionStatus::Failed { ref reason } => {
            assert!(
                reason.contains("not observed"),
                "the verdict says why: {reason}"
            )
        }
        other => panic!("definitive absence settles failed: {other:?}"),
    }
    let pending = intent(&store, &pending_id);
    assert_eq!(
        pending.status,
        HostActionStatus::Pending,
        "absence is not failure for a possibly-unattempted intent"
    );
    assert_eq!(pending.authenticated_actor, None);
}

/// The reconcile pass performs host reads only: remote state is
/// identical before and after, and the write history is untouched.
#[test]
fn reconcile_pass_mutates_no_remote_state() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-readonly");
    let mut gh = FixtureGithub::new();
    gh.set_branch(&repo(), "work/r", "head-1", &publisher());
    gh.register_change(&change("cr-1"), "work/r", "main", "head-1", &publisher());
    gh.add_thread(&change("cr-1"), "th-1");

    // Outstanding intents covering present, absent, and unresolved
    // reads — none may move the remote.
    insert_intent_with(
        &store,
        &work_id,
        HostAction::PushRef {
            branch: "work/r".into(),
            produced: rev("head-1"),
            expected_prior: None,
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    insert_intent_with(
        &store,
        &work_id,
        HostAction::PostComment {
            change: change("cr-1"),
            expected_head: rev("head-1"),
            marker: "m".into(),
            body: "note".into(),
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    insert_intent(
        &store,
        &work_id,
        HostAction::ReplyToThread {
            change: change("cr-1"),
            thread_id: "th-1".into(),
            marker: "m2".into(),
            body: "reply".into(),
        },
    );

    gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert!(
        gh.applied().is_empty(),
        "reconcile performs no write: {:?}",
        gh.applied()
    );
    assert_eq!(
        gh.remote_head(&repo(), "work/r"),
        Some("head-1".to_string())
    );
    assert!(!gh.is_ready("cr-1"));
    assert!(!gh.thread("th-1").unwrap().resolved);
    assert!(gh.thread("th-1").unwrap().replies.is_empty());
}

/// A read that establishes neither presence nor absence settles
/// nothing: the intent stays outstanding with the read's reason
/// recorded, reconcilable by a later pass.
#[test]
fn indeterminate_read_keeps_the_intent_unknown() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-ambiguous");
    let mut gh = FixtureGithub::new();
    // Two remote change requests carry the same (source, target)
    // tuple — the read cannot say which is the intent's.
    gh.register_change(&change("cr-a"), "work/dup", "main", "head-1", &publisher());
    gh.register_change(&change("cr-b"), "work/dup", "main", "head-1", &publisher());
    let action_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::OpenChangeRequest {
            source_branch: "work/dup".into(),
            target_branch: "main".into(),
            title: "t".into(),
            body: String::new(),
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );

    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.still_unknown, 1);
    match intent(&store, &action_id).status {
        HostActionStatus::Unknown { ref reason } => {
            assert!(
                reason.contains("multiple change requests"),
                "the recorded reason explains the ambiguity: {reason}"
            )
        }
        other => panic!("indeterminate read settles nothing: {other:?}"),
    }
}

/// A review is bound to the revision it was posted against: a
/// body-identical review on an older head is not the intent's effect.
/// Reconciling a lost-receipt intent against it settles `failed` —
/// the bound effect is definitively absent — and a fresh attempt
/// writes the review at the bound head rather than adopting the old
/// record.
#[test]
fn review_on_old_revision_does_not_settle_new_revision_intent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-rev");
    let mut gh = FixtureGithub::new();
    gh.register_change(&change("cr-5"), "work/w", "main", "head-1", &publisher());
    gh.seed_review(
        &change("cr-5"),
        "rv-old",
        &publisher(),
        "looks good",
        "head-1",
    );
    // The head raced on: the intent binds the new revision while the
    // only matching review belongs to the old one.
    gh.set_change_head(&change("cr-5"), "head-2", &actor("other-agent"));

    let lost_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change("cr-5"),
            head: rev("head-2"),
            body: "looks good".into(),
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(report.applied, 0, "the old-revision review is not evidence");
    match intent(&store, &lost_id).status {
        HostActionStatus::Failed { .. } => {}
        other => panic!("definitive absence settles failed, never applied: {other:?}"),
    }
    assert!(
        gh.applied().is_empty(),
        "reconciliation performed no remote write: {:?}",
        gh.applied()
    );

    // A fresh attempt must actually write the review at the bound
    // head — never adopt the pre-existing record for the old one.
    let write_id = insert_intent(
        &store,
        &work_id,
        HostAction::PublishReviewFeedback {
            change: change("cr-5"),
            head: rev("head-2"),
            body: "looks good".into(),
        },
    );
    gh.apply_outbox(&store, &config, NOW).unwrap();
    assert_eq!(intent(&store, &write_id).status, HostActionStatus::Applied);
    assert_eq!(
        gh.applied(),
        &["publish_review_feedback:test-host/local-repo:cr-5"],
        "the write ran — the old-revision review was not adopted"
    );
}

/// A `unknown` update intent whose bound head raced on stays
/// `unknown`: the moved head cannot distinguish "our update landed,
/// then the head raced" from "never landed" — neither a `failed` nor
/// an `applied` verdict is honest. A `pending` intent in the same
/// spot still lets the guarded write arbitrate: its stale-head
/// refusal is the contract's verdict.
#[test]
fn lost_update_receipt_after_head_move_remains_unknown() {
    let store = SqliteStore::in_memory(1).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "u35-headraced");
    let mut gh = FixtureGithub::new();
    gh.register_change(&change("cr-6"), "work/u", "main", "head-1", &publisher());
    // The update landed at head-1 — the remote attributes the stored
    // fields to the bound actor — but the receipt was lost, and the
    // head has since raced to head-2.
    gh.set_change_meta(&change("cr-6"), Some("done"), None, &publisher());
    gh.set_change_head(&change("cr-6"), "head-2", &actor("other-agent"));

    let lost_id = insert_intent_with(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change("cr-6"),
            expected_head: rev("head-1"),
            title: Some("done".into()),
            body: None,
        },
        HostActionStatus::Unknown {
            reason: "response lost".into(),
        },
    );
    let report = gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        report.applied, 0,
        "coincidentally-matching fields are not evidence"
    );
    assert_eq!(report.failed, 0, "a raced head is not definitive absence");
    assert_eq!(report.still_unknown, 1);
    assert!(
        matches!(
            intent(&store, &lost_id).status,
            HostActionStatus::Unknown { .. }
        ),
        "the intent stays outstanding: {:?}",
        intent(&store, &lost_id).status
    );
    assert!(
        gh.applied().is_empty(),
        "reconciliation performed no remote write: {:?}",
        gh.applied()
    );

    // A pending intent bound to the same moved head takes the guarded
    // write path: the stale-head refusal — not the reconcile read —
    // produces the contract's verdict.
    let pending_id = insert_intent(
        &store,
        &work_id,
        HostAction::UpdateChangeRequest {
            change: change("cr-6"),
            expected_head: rev("head-1"),
            title: Some("later".into()),
            body: None,
        },
    );
    gh.reconcile_outbox(&store, &config, NOW).unwrap();
    assert_eq!(
        intent(&store, &pending_id).status,
        HostActionStatus::Pending,
        "a possibly-unattempted intent is left for the write path"
    );
    gh.apply_outbox(&store, &config, NOW).unwrap();
    match intent(&store, &pending_id).status {
        HostActionStatus::Failed { ref reason } => {
            assert!(
                reason.contains("head moved"),
                "the guarded write's refusal is the verdict: {reason}"
            )
        }
        other => panic!("the stale-head guard decides: {other:?}"),
    }
}
