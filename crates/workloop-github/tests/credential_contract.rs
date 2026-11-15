//! Credential contract: `credential_ref`s resolve at the adapter
//! boundary through the `env:`/`file:`/`command:` providers; missing,
//! expired, and denied refs surface as distinct typed failure classes;
//! file refs demand owner-only permissions; and a renewed credential
//! must authenticate as the same actor or the intent is never
//! re-bound. Secrets never enter argv, logs, exports, or durable
//! intents.

use std::collections::BTreeSet;
use std::path::PathBuf;

use workloop_core::command::*;
use workloop_core::credential::CredentialProvider;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_github::credential::{
    CredentialError, bind_credential, resolve_credential, run_credential_command,
};
use workloop_github::fixture::{CredentialBook, FixtureGithub};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;
const SENTINEL: &str = "s3cr3t-sentinel-7f9a2c";

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

fn role_binding(role: ActorRole, slug: &str, credential_ref: &str) -> RoleBinding {
    RoleBinding {
        role,
        actor: actor(slug),
        credential_ref: CredentialRef::trusted(credential_ref),
    }
}

fn config_with(actors: Vec<RoleBinding>) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
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
    config_with(vec![role_binding(
        ActorRole::Publication,
        "pub-agent",
        "env:TAISAN_TEST_PUBLICATION_CRED",
    )])
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
            origin: Origin("credentials".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "credential contract work".into(),
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

fn insert_intent(
    store: &SqliteStore,
    work_id: &WorkId,
    role: ActorRole,
    actor: &HostActorRef,
    action: HostAction,
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
                    role,
                    actor: actor.clone(),
                    action,
                    payload_digest: "credentials".into(),
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
                .find(|i| i.action_id == action_id)
                .expect("intent recorded"))
        })
        .unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-cred-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn push(branch: &str, object: &str) -> HostAction {
    HostAction::PushRef {
        branch: branch.into(),
        produced: rev(object),
        expected_prior: None,
    }
}

/// AC: missing, expired, and denied refs each produce a distinct
/// explicit error class. `credential.missing` is the undefined slot;
/// `credential.expired` is a host-reported expiry; a denied ref is
/// `credential.unresolvable` — three distinct codes.
#[test]
fn missing_expired_and_denied_refs_have_distinct_error_classes() {
    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_EXPIRED", "expired-value");
    book.seat_expired("expired-value");
    book.seat_env("TAISAN_TEST_DENIED", "denied-value");
    book.seat_denied("denied-value");
    let authenticate = |c: &workloop_github::credential::ResolvedCredential| book.authenticate(c);

    let missing =
        bind_credential(None, &publisher(), &book, &authenticate).expect_err("missing must fail");
    assert_eq!(missing.code(), "credential.missing");
    assert!(matches!(missing, CredentialError::Missing { .. }));

    let expired_ref = CredentialRef::trusted("env:TAISAN_TEST_EXPIRED");
    let expired = bind_credential(Some(&expired_ref), &publisher(), &book, &authenticate)
        .expect_err("expired must fail");
    assert_eq!(expired.code(), "credential.expired");
    assert!(matches!(expired, CredentialError::Expired { .. }));

    let denied_ref = CredentialRef::trusted("env:TAISAN_TEST_DENIED");
    let denied = bind_credential(Some(&denied_ref), &publisher(), &book, &authenticate)
        .expect_err("denied must fail");
    assert_eq!(denied.code(), "credential.unresolvable");
    assert!(matches!(denied, CredentialError::Unresolvable { .. }));

    // The three classes are pairwise distinct, and no message carries
    // the seated secret values.
    let codes = [missing.code(), expired.code(), denied.code()];
    assert_ne!(codes[0], codes[1]);
    assert_ne!(codes[1], codes[2]);
    assert_ne!(codes[0], codes[2]);
    for error in [&missing, &expired, &denied] {
        let text = format!("{error}");
        assert!(!text.contains("expired-value"), "leaked: {text}");
        assert!(!text.contains("denied-value"), "leaked: {text}");
    }
}

/// AC: `file:` refs validate ownership and mode before use — a
/// group/world-readable file is refused before a single byte is read;
/// owner-only `0600` and `0400` resolve.
#[cfg(unix)]
#[test]
fn file_refs_reject_group_or_world_readable_files() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("file-mode");
    let path = dir.join("token");
    std::fs::write(&path, "file-secret-value").unwrap();
    let reference = CredentialRef::trusted(format!("file:{}", path.display()).as_str());
    let book = CredentialBook::new();

    for mode in [0o644, 0o640, 0o604, 0o666] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let error = resolve_credential(Some(&reference), &book).unwrap_err();
        assert_eq!(
            error.code(),
            "credential.unresolvable",
            "mode {mode:03o} must be refused"
        );
        assert!(!format!("{error}").contains("file-secret-value"));
    }
    for mode in [0o600, 0o400] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let resolved = resolve_credential(Some(&reference), &book).unwrap();
        assert_eq!(resolved.expose(), "file-secret-value", "mode {mode:03o}");
    }
    // A non-regular target (a directory) is unresolvable too.
    let dir_ref = CredentialRef::trusted(format!("file:{}", dir.display()).as_str());
    let error = resolve_credential(Some(&dir_ref), &book).unwrap_err();
    assert_eq!(error.code(), "credential.unresolvable");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The three providers resolve their slots at the boundary; provider
/// failures — unset variable, absent file, nonzero command — are all
/// `credential.unresolvable`.
#[test]
fn providers_resolve_slots_and_fail_unresolvable() {
    let dir = temp_dir("providers");
    let secret_file = dir.join("provider-token");
    std::fs::write(&secret_file, "cmd-or-file-value").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_OK", "env-value");

    let env_ref = CredentialRef::trusted("env:TAISAN_TEST_OK");
    assert_eq!(
        resolve_credential(Some(&env_ref), &book).unwrap().expose(),
        "env-value"
    );

    let file_ref = CredentialRef::trusted(format!("file:{}", secret_file.display()).as_str());
    assert_eq!(
        resolve_credential(Some(&file_ref), &book).unwrap().expose(),
        "cmd-or-file-value"
    );

    let command_ref =
        CredentialRef::trusted(format!("command:/bin/cat {}", secret_file.display()).as_str());
    assert_eq!(
        resolve_credential(Some(&command_ref), &book)
            .unwrap()
            .expose(),
        "cmd-or-file-value"
    );

    let unset_ref = CredentialRef::trusted("env:TAISAN_TEST_UNSET");
    let absent_ref = CredentialRef::trusted(format!("file:{}/absent", dir.display()).as_str());
    let failing_ref =
        CredentialRef::trusted(format!("command:/bin/cat {}/absent", dir.display()).as_str());
    for (reference, what) in [
        (&unset_ref, "unset env"),
        (&absent_ref, "absent file"),
        (&failing_ref, "nonzero command"),
    ] {
        let error = resolve_credential(Some(reference), &book).unwrap_err();
        assert_eq!(error.code(), "credential.unresolvable", "{what}: {error}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// AC: a renewed credential resolving to a different actor is rejected
/// `credential.actor_mismatch` and never re-binds outstanding actions;
/// a same-actor renewal resolves the new secret under the original
/// binding and applies.
#[test]
fn actor_replacement_is_mismatch_and_same_actor_renewal_applies() {
    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_PUBLICATION_CRED", "token-v1");
    book.seat_actor("token-v1", publisher());
    let mut fixture = FixtureGithub::new().with_credentials(book);

    let store_dir = temp_dir("renewal");
    let store = SqliteStore::open(&store_dir.join("store.db"), 4).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "renewal-1");

    let first = insert_intent(
        &store,
        &work_id,
        ActorRole::Publication,
        &publisher(),
        push("work/a", "head-1"),
    );
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(intent(&store, &first).status, HostActionStatus::Applied);
    assert_eq!(
        intent(&store, &first).authenticated_actor,
        Some(publisher())
    );

    // Renewal under a different actor: the outstanding intent is
    // blocked `actor_mismatch`, stays pending, and is never re-bound —
    // no write runs and no receipt is recorded.
    let book = fixture.credentials_mut().unwrap();
    book.seat_env("TAISAN_TEST_PUBLICATION_CRED", "token-v2");
    book.seat_actor("token-v2", actor("intruder-agent"));
    let second = insert_intent(
        &store,
        &work_id,
        ActorRole::Publication,
        &publisher(),
        push("work/b", "head-2"),
    );
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(
        report.credential_blocked,
        vec![workloop_github::fixture::CredentialBlock {
            action_id: second.clone(),
            code: "credential.actor_mismatch",
        }]
    );
    let blocked = intent(&store, &second);
    assert_eq!(blocked.status, HostActionStatus::Pending);
    assert_eq!(blocked.authenticated_actor, None);
    assert_eq!(blocked.actor, publisher());
    assert!(
        !fixture.applied().iter().any(|line| line.contains("head-2")),
        "a mismatched credential must never write: {:?}",
        fixture.applied()
    );

    // Same-actor renewal: the rotated value authenticates as the
    // original actor, so the outstanding intent applies under the
    // binding it was authorized with.
    let book = fixture.credentials_mut().unwrap();
    book.seat_env("TAISAN_TEST_PUBLICATION_CRED", "token-v3");
    book.seat_actor("token-v3", publisher());
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(report.applied, 1);
    assert!(report.credential_blocked.is_empty());
    let applied = intent(&store, &second);
    assert_eq!(applied.status, HostActionStatus::Applied);
    assert_eq!(applied.authenticated_actor, Some(publisher()));

    let _ = std::fs::remove_dir_all(&store_dir);
}

/// Each role resolves through its own distinct credential slot and
/// authenticates as its own bound actor.
#[test]
fn each_role_resolves_through_its_own_credential_slot() {
    let config = config_with(vec![
        role_binding(
            ActorRole::Publication,
            "pub-agent",
            "env:TAISAN_TEST_PUBLICATION_CRED",
        ),
        role_binding(
            ActorRole::Review,
            "rev-agent",
            "env:TAISAN_TEST_REVIEW_CRED",
        ),
        role_binding(
            ActorRole::Discussion,
            "disc-agent",
            "env:TAISAN_TEST_DISCUSSION_CRED",
        ),
    ]);
    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_PUBLICATION_CRED", "pub-value");
    book.seat_env("TAISAN_TEST_REVIEW_CRED", "rev-value");
    book.seat_env("TAISAN_TEST_DISCUSSION_CRED", "disc-value");
    book.seat_actor("pub-value", actor("pub-agent"));
    book.seat_actor("rev-value", actor("rev-agent"));
    book.seat_actor("disc-value", actor("disc-agent"));

    for binding in &config.host.actors {
        let verified =
            bind_credential(Some(&binding.credential_ref), &binding.actor, &book, &|c| {
                book.authenticate(c)
            })
            .unwrap();
        assert_eq!(verified, binding.actor, "role {}", binding.role.name());
    }

    // A ref resolving to another role's actor is a mismatch for this
    // role's binding, never an implicit re-bind.
    let review_binding = &config.host.actors[1];
    let pub_ref = &config.host.actors[0].credential_ref;
    let error = bind_credential(Some(pub_ref), &review_binding.actor, &book, &|c| {
        book.authenticate(c)
    })
    .unwrap_err();
    assert_eq!(error.code(), "credential.actor_mismatch");
}

/// AC: secret values appear in no argv entry, log line, exported
/// environment variable, or persisted Intent. The sentinel value drives
/// a real outbox apply end to end, then every artifact the adapter
/// produced is searched.
#[test]
fn secret_values_never_enter_argv_logs_exports_or_durable_intents() {
    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_PUBLICATION_CRED", SENTINEL);
    book.seat_actor(SENTINEL, publisher());
    let mut fixture = FixtureGithub::new().with_credentials(book);

    let store_dir = temp_dir("hygiene");
    let store = SqliteStore::open(&store_dir.join("store.db"), 4).unwrap();
    let config = config();
    let work_id = submit(&store, &config, "hygiene-1");
    insert_intent(
        &store,
        &work_id,
        ActorRole::Publication,
        &publisher(),
        push("work/h", "head-h"),
    );
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(report.applied, 1);

    // Every persisted and reported artifact is searched for the value.
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    let observations = store
        .transact(|tx| tx.observations_for_work(&work_id))
        .unwrap();
    let binding_json = store
        .transact(|tx| Ok(tx.work(&work_id)?.unwrap().binding))
        .map(|b| serde_json::to_string(&b).unwrap())
        .unwrap();
    for (label, blob) in [
        ("durable intents", serde_json::to_string(&intents).unwrap()),
        (
            "observations",
            serde_json::to_string(&observations).unwrap(),
        ),
        ("work binding", binding_json),
        ("write log", fixture.applied().join("\n")),
        ("outbox report", format!("{report:?}")),
    ] {
        assert!(
            !blob.contains(SENTINEL),
            "{label} carries the secret: {blob}"
        );
    }

    // The resolved value's only displays are redacted.
    let reference = CredentialRef::trusted("env:TAISAN_TEST_PUBLICATION_CRED");
    let book = fixture.credentials_mut().unwrap();
    let resolved = resolve_credential(Some(&reference), book).unwrap();
    assert_eq!(resolved.expose(), SENTINEL);
    assert_eq!(format!("{resolved}"), "<redacted>");
    assert_eq!(format!("{resolved:?}"), "<redacted>");

    // `command:` argv names the emitter, never the secret: the value
    // arrives on stdout. The parsed argv contains no secret, and no
    // spawned provider environment carries it either.
    let secret_file = store_dir.join("token");
    std::fs::write(&secret_file, SENTINEL).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let command_ref =
        CredentialRef::trusted(format!("command:/bin/cat {}", secret_file.display()).as_str());
    let CredentialProvider::Command { argv } = CredentialProvider::parse(&command_ref).unwrap()
    else {
        panic!("command: provider expected")
    };
    assert!(
        argv.iter().all(|arg| !arg.contains(SENTINEL)),
        "argv carries the secret: {argv:?}"
    );
    let via_command = resolve_credential(Some(&command_ref), book).unwrap();
    assert_eq!(via_command.expose(), SENTINEL);
    let spawned_env = run_credential_command(&["/usr/bin/printenv".to_string()]).unwrap();
    assert!(
        !spawned_env.contains(SENTINEL),
        "a spawned provider environment must not carry the secret"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

/// S-5 (#149): under the reviewer-App configuration a review intent
/// resolves the review role's own credential slot and authenticates as
/// the configured App slug — the produced review carries that identity.
/// A credential that authenticates as the generic automation bot is an
/// `actor_mismatch`: the review is never applied under
/// `github-actions[bot]`.
#[test]
fn review_feedback_binds_the_configured_reviewer_app_identity() {
    let reviewer = actor("s1-reviewer[bot]");
    let bot = actor("github-actions[bot]");
    let config = config_with(vec![
        role_binding(
            ActorRole::Publication,
            "pub-agent",
            "env:TAISAN_TEST_PUBLICATION_CRED",
        ),
        role_binding(
            ActorRole::Review,
            "s1-reviewer[bot]",
            "env:TAISAN_TEST_REVIEW_APP_CRED",
        ),
    ]);
    let mut book = CredentialBook::new();
    book.seat_env("TAISAN_TEST_REVIEW_APP_CRED", "app-token");
    book.seat_actor("app-token", reviewer.clone());
    let mut fixture = FixtureGithub::new().with_credentials(book);
    fixture.set_change_head(&change(), "head-1", &reviewer);

    let store_dir = temp_dir("reviewer-app");
    let store = SqliteStore::open(&store_dir.join("store.db"), 4).unwrap();
    let work_id = submit(&store, &config, "reviewer-app-1");

    let review_intent = |body: &str| {
        insert_intent(
            &store,
            &work_id,
            ActorRole::Review,
            &reviewer,
            HostAction::PublishReviewFeedback {
                change: change(),
                head: rev("head-1"),
                body: body.into(),
            },
        )
    };

    // The App's own credential applies the review as the App slug.
    let first = review_intent("ship it");
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(report.applied, 1);
    let applied = intent(&store, &first);
    assert_eq!(applied.status, HostActionStatus::Applied);
    assert_eq!(applied.authenticated_actor, Some(reviewer.clone()));
    assert_eq!(
        applied
            .authenticated_actor
            .as_ref()
            .map(|a| a.actor.as_str()),
        Some("s1-reviewer[bot]"),
        "the review carries the configured App slug"
    );
    assert_ne!(
        applied.authenticated_actor,
        Some(bot.clone()),
        "never the generic automation identity"
    );

    // A credential slot rotated to authenticate as the generic bot is
    // an actor mismatch: the outstanding review stays pending and is
    // never applied under `github-actions[bot]`.
    let book = fixture.credentials_mut().unwrap();
    book.seat_env("TAISAN_TEST_REVIEW_APP_CRED", "bot-token");
    book.seat_actor("bot-token", bot);
    let second = review_intent("re-check");
    let report = fixture
        .apply_outbox_bounded(&store, &config, &mut || NOW, None, None, usize::MAX)
        .unwrap();
    assert_eq!(
        report.credential_blocked,
        vec![workloop_github::fixture::CredentialBlock {
            action_id: second.clone(),
            code: "credential.actor_mismatch",
        }]
    );
    let blocked = intent(&store, &second);
    assert_eq!(blocked.status, HostActionStatus::Pending);
    assert_eq!(blocked.authenticated_actor, None);
    assert_eq!(blocked.actor, reviewer);

    let _ = std::fs::remove_dir_all(&store_dir);
}
