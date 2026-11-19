//! Contract tests: durable Worker request bodies (`worker_request_bodies`,
//! store format 11, `docs/v0.3.md`).

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_core::worker_v2::WorkerRequestBody;
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

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn submit(key: &str) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "do the thing".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    }
}

fn accepted_with_execution(d: &CommandDecision) -> (WorkId, ExecutionId) {
    match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => execution_id.clone(),
                other => panic!("expected an execution effect, got {other:?}"),
            };
            (work_id.clone().expect("work_id"), eid)
        }
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("setup effects apply");
}

/// Open a store, admit one Work, and plant the durable request row a
/// v2 body binds to.
fn seeded_request(store: &SqliteStore, key: &str) -> String {
    let (work_id, execution_id) =
        accepted_with_execution(&handle_command(store, &config(), &submit(key), NOW).unwrap());
    let request_id = format!("{key}:inv:0");
    apply_effects(
        store,
        &[Effect::InsertRequest {
            request: InvocationRequest {
                request_id: request_id.clone(),
                work_id,
                execution_id,
                role: InvocationRole::Coding,
                spec_revision: 1,
                payload: serde_json::json!({}),
                adapter_binding: None,
            },
        }],
    );
    request_id
}

fn body(request_id: &str, goal: &str) -> WorkerRequestBody {
    let canonical = format!(
        "{{\"format_version\":\"taisan.worker.request/2\",\"goal\":\"{goal}\",\"request_id\":\"{request_id}\"}}"
    );
    WorkerRequestBody {
        request_id: request_id.into(),
        format_version: "taisan.worker.request/2".into(),
        canonical_body: canonical.into_bytes(),
        body_digest: format!("digest-of-{goal}"),
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "workloop-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A recorded body reads back verbatim — the same bytes, the same
/// digest, never reconstructed.
#[test]
fn a_recorded_body_reads_back_verbatim() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "body-rt");
    let recorded = body(&request_id, "ship it");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: recorded.clone(),
        }],
    );
    let read = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .unwrap()
        .expect("the body is durable");
    assert_eq!(read, recorded);
    assert_eq!(read.canonical_body, recorded.canonical_body);
}

/// A second insert with identical bytes is a replay — the write is
/// idempotent under crash-and-retry.
#[test]
fn reinserting_identical_bytes_is_a_replay() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "body-replay");
    let recorded = body(&request_id, "same");
    for _ in 0..2 {
        apply_effects(
            &store,
            &[Effect::InsertWorkerRequestBody {
                body: recorded.clone(),
            }],
        );
    }
    let read = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .unwrap()
        .expect("the replayed body is durable");
    assert_eq!(read, recorded);
}

/// A second insert with different bytes under the same request_id is
/// refused: the store never replaces the body an already-launched
/// worker may hold.
#[test]
fn a_conflicting_body_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "body-conflict");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "first"),
        }],
    );
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertWorkerRequestBody {
                body: body(&request_id, "second"),
            })
        })
        .expect_err("different bytes under one request_id conflict");
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "the refusal is a typed conflict: {err:?}"
    );
    let read = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .unwrap()
        .expect("the original body survives the refused write");
    assert_eq!(read, body(&request_id, "first"));
}

/// A body referencing no request row is refused by the foreign key —
/// bodies only exist attached to a durable dispatch request.
#[test]
fn a_body_without_a_request_row_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertWorkerRequestBody {
                body: body("req:nonexistent", "orphan"),
            })
        })
        .expect_err("an orphaned body row is refused");
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "the refusal is a typed conflict: {err:?}"
    );
}

/// Regression: a v10 store gains `worker_request_bodies` on open, keeps
/// every pre-upgrade row, and the new table accepts writes on the
/// upgraded file.
#[test]
fn a_v10_store_gains_the_request_bodies_table() {
    let root = temp_root("upgrade-v10");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = seeded_request(&store, "up-v10");
        drop(store);
        id
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE worker_request_bodies; PRAGMA user_version=10;")
            .unwrap();
        let has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='worker_request_bodies')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has, "the reshaped file is a real v10 store");
    }
    let store = SqliteStore::open(&db, 1).expect("a v10 store upgrades in place");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 11, "the upgrade lands on the current format");
    // The pre-upgrade request row survives and the new table accepts
    // its body on the upgraded file.
    assert!(
        store
            .transact(|tx| tx.request(&request_id))
            .unwrap()
            .is_some(),
        "the pre-upgrade request survives"
    );
    assert!(
        store
            .transact(|tx| tx.worker_request_body(&request_id))
            .unwrap()
            .is_none(),
        "the v10 file carried no body"
    );
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "post-upgrade"),
        }],
    );
    assert_eq!(
        store
            .transact(|tx| tx.worker_request_body(&request_id))
            .unwrap()
            .expect("the upgraded file records bodies")
            .canonical_body,
        body(&request_id, "post-upgrade").canonical_body
    );
    drop(store);
    // Re-opening after the upgrade is a no-op, not a second migration.
    let _ = SqliteStore::open(&db, 1).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// A file that claims v10 but lacks the v10-era tables is still
/// refused — the upgrade path accepts only a real previous release.
#[test]
fn a_truncated_v10_store_is_still_refused() {
    let root = temp_root("truncated-v10");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE worker_request_bodies; DROP TABLE intake_settings; \
             PRAGMA user_version=10;",
        )
        .unwrap();
    }
    let err = match SqliteStore::open(&db, 1) {
        Ok(_) => panic!("a truncated v10 file is refused"),
        Err(err) => err,
    };
    assert!(
        matches!(err, StoreError::UnsupportedFormat { .. }),
        "the refusal names the format, not a missing table: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
