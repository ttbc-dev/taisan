//! Contract tests: the durable remote-collection record
//! (`executor_collections`, store format 15,
//! `docs/executor-transport-contract.md` §6, #376). One bound request
//! admits exactly one collection identity — a second collection naming
//! a different collection or digest conflicts, never overwrites — and
//! the lifecycle is monotonic: `collecting → collected →
//! acknowledged`, or `invalid`, which is terminal. `recorded_seq` is
//! the compare-and-set cursor the retention CAS rows already use.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::executor::{
    CollectionEntry, EXECUTOR_PROTOCOL_V1, ExecutorBinding, ExecutorBindingFields,
    ExecutorCollection, ExecutorCollectionRecord, ExecutorCollectionState, ExecutorLimits,
    collection_digest, item_content_digest,
};
use workloop_core::identity::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: host(),
                    actor: HostActorId::trusted("pub-agent"),
                },
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
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
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
/// binding attaches to.
fn seeded_request(store: &SqliteStore, key: &str) -> String {
    let (work_id, execution_id) = match handle_command(
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
                intent: "do the thing".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap()
    {
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
    };
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

fn binding(request_id: &str) -> ExecutorBinding {
    ExecutorBinding::new(ExecutorBindingFields {
        installation_id: "install-a".into(),
        endpoint_id: "executor-a".into(),
        endpoint_identity_digest: "a".repeat(64),
        bound_incarnation_id: "boot-7f1c".into(),
        protocol_version: EXECUTOR_PROTOCOL_V1.into(),
        capabilities_digest: "e".repeat(64),
        launch_config_digest: "c".repeat(64),
        request_id: request_id.into(),
        invocation_id: format!("inv-{request_id}"),
        request_digest: "b".repeat(64),
        limits: ExecutorLimits {
            max_input_bytes: 4 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_item_count: 64,
            operation_timeout_ms: 3_600_000,
        },
    })
    .expect("binding seals")
}

/// A bound request — the collection row's foreign key requires the
/// binding to exist first.
fn bound_request(store: &SqliteStore, key: &str) -> String {
    let request_id = seeded_request(store, key);
    apply_effects(
        store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    request_id
}

/// The collection record a validated `collect` produced: entries
/// sorted by item_id, the contract digest over them, honest
/// truncation flags.
fn collection(
    request_id: &str,
    collection_id: &str,
    seq: i64,
    state: ExecutorCollectionState,
    invalid_reason: Option<&str>,
) -> ExecutorCollection {
    let entries = vec![
        CollectionEntry {
            item_id: "handoff.json".into(),
            bytes: 7,
            content_digest: item_content_digest(b"handoff"),
            source: "quarantine".into(),
        },
        CollectionEntry {
            item_id: "report.json".into(),
            bytes: 6,
            content_digest: item_content_digest(b"report"),
            source: "worker".into(),
        },
    ];
    let record = ExecutorCollectionRecord {
        request_id: request_id.into(),
        invocation_id: format!("inv-{request_id}"),
        collection_id: collection_id.into(),
        collection_digest: collection_digest(&entries).expect("digest seals"),
        stop_proof: "d".repeat(64),
        entries,
        artifact_ids: vec![],
        report_truncated: false,
        log_truncated: false,
    };
    ExecutorCollection::new(record, state, invalid_reason.map(str::to_string), seq)
        .expect("collection seals")
}

fn upsert(collection: &ExecutorCollection, expected_seq: i64) -> Effect {
    Effect::UpsertExecutorCollection {
        collection: collection.clone(),
        expected_seq,
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

/// The durable collection reads back verbatim — canonical body,
/// digests, state, and cursor intact.
#[test]
fn a_recorded_collection_reads_back_verbatim() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-rt");
    let recorded = collection(
        &request_id,
        "col-1",
        9,
        ExecutorCollectionState::Collecting,
        None,
    );
    apply_effects(&store, &[upsert(&recorded, 0)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .expect("the collection is durable");
    assert_eq!(read, recorded);
}

/// A collection for a request with no binding refuses — the row is
/// meaningless without the immutable remote identity it pins.
#[test]
fn a_collection_for_an_unbound_request_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "coll-nb");
    let coll = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collecting,
        None,
    );
    let err = store
        .transact(|tx| tx.apply(&upsert(&coll, 0)))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "unbound request conflicts, got {err:?}"
    );
    assert!(
        store
            .transact(|tx| tx.executor_collection(&request_id))
            .unwrap()
            .is_none(),
        "the refused write left no row"
    );
}

/// One request admits one collection identity: a second upsert naming
/// a different collection_id for the same bound request is a conflict
/// — never an overwrite (contract §6).
#[test]
fn a_second_collection_identity_for_one_request_conflicts() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-dup");
    let first = collection(
        &request_id,
        "col-1",
        3,
        ExecutorCollectionState::Collected,
        None,
    );
    apply_effects(&store, &[upsert(&first, 0)]);
    let other = collection(
        &request_id,
        "col-2",
        9,
        ExecutorCollectionState::Collected,
        None,
    );
    let err = store
        .transact(|tx| tx.apply(&upsert(&other, 3)))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "a second collection identity conflicts, got {err:?}"
    );
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        read, first,
        "the conflicting upsert never touched the recorded collection"
    );
}

/// The lifecycle is monotonic: collecting → collected → acknowledged
/// advances; a backward step or a move out of a terminal state is a
/// conflict.
#[test]
fn collection_state_moves_are_monotonic() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-mono");

    let collecting = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collecting,
        None,
    );
    apply_effects(&store, &[upsert(&collecting, 0)]);
    let collected = collection(
        &request_id,
        "col-1",
        2,
        ExecutorCollectionState::Collected,
        None,
    );
    apply_effects(&store, &[upsert(&collected, 1)]);
    let acknowledged = collection(
        &request_id,
        "col-1",
        3,
        ExecutorCollectionState::Acknowledged,
        None,
    );
    apply_effects(&store, &[upsert(&acknowledged, 2)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read.state, ExecutorCollectionState::Acknowledged);

    // acknowledged is terminal — a repeated collect replaying the
    // recorded state is the idempotent case, anything else conflicts.
    let regressed = collection(
        &request_id,
        "col-1",
        4,
        ExecutorCollectionState::Collected,
        None,
    );
    let err = store
        .transact(|tx| tx.apply(&upsert(&regressed, 3)))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "acknowledged never steps back, got {err:?}"
    );
}

/// `invalid` is reachable from any live state and is itself terminal:
/// an invalid collection is never acknowledged and never collects
/// again.
#[test]
fn an_invalid_collection_is_terminal() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-inv");
    let collecting = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collecting,
        None,
    );
    apply_effects(&store, &[upsert(&collecting, 0)]);
    let invalid = collection(
        &request_id,
        "col-1",
        2,
        ExecutorCollectionState::Invalid,
        Some("collection_incomplete"),
    );
    apply_effects(&store, &[upsert(&invalid, 1)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        read.invalid_reason.as_deref(),
        Some("collection_incomplete")
    );

    let revived = collection(
        &request_id,
        "col-1",
        3,
        ExecutorCollectionState::Collected,
        None,
    );
    let err = store
        .transact(|tx| tx.apply(&upsert(&revived, 2)))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "invalid never collects again, got {err:?}"
    );
}

/// The same state written again is an idempotent replay (a crashed
/// collect committing its staged outcome twice) — it applies under
/// the same cursor and changes nothing.
#[test]
fn a_repeated_same_state_upsert_is_idempotent() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-idem");
    let collected = collection(
        &request_id,
        "col-1",
        5,
        ExecutorCollectionState::Collected,
        None,
    );
    apply_effects(&store, &[upsert(&collected, 0)]);
    apply_effects(&store, &[upsert(&collected, 5)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read, collected);
}

/// The same idempotent rule holds out of a terminal state: a replayed
/// `acknowledge` rewriting the already-`acknowledged` row — the
/// lost-ack retry — is a no-op, and a replayed `invalid` verdict the
/// same. Only a *different* target state out of a terminal row is a
/// conflict.
#[test]
fn a_replayed_terminal_state_is_the_same_idempotent_no_op() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-term-idem");
    let collected = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collected,
        None,
    );
    apply_effects(&store, &[upsert(&collected, 0)]);
    let acknowledged = collection(
        &request_id,
        "col-1",
        2,
        ExecutorCollectionState::Acknowledged,
        None,
    );
    apply_effects(&store, &[upsert(&acknowledged, 1)]);
    // The acknowledge's own replay applies under the same cursor and
    // changes nothing — it must not read as a forbidden move out of
    // the terminal state.
    apply_effects(&store, &[upsert(&acknowledged, 2)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read, acknowledged);

    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-inv-idem");
    let invalid = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Invalid,
        Some("collection_incomplete"),
    );
    apply_effects(&store, &[upsert(&invalid, 0)]);
    apply_effects(&store, &[upsert(&invalid, 1)]);
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read, invalid);
}

/// The cursor is compare-and-set: an upsert whose `expected_seq`
/// disagrees with the recorded row conflicts instead of regressing a
/// concurrent collector's commit.
#[test]
fn a_stale_cursor_conflicts() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = bound_request(&store, "coll-cas");
    let collected = collection(
        &request_id,
        "col-1",
        4,
        ExecutorCollectionState::Collected,
        None,
    );
    apply_effects(&store, &[upsert(&collected, 0)]);
    let acknowledged = collection(
        &request_id,
        "col-1",
        9,
        ExecutorCollectionState::Acknowledged,
        None,
    );
    let err = store
        .transact(|tx| tx.apply(&upsert(&acknowledged, 99)))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "a stale cursor conflicts, got {err:?}"
    );
    let read = store
        .transact(|tx| tx.executor_collection(&request_id))
        .unwrap()
        .unwrap();
    assert_eq!(read, collected, "the stale upsert never touched the row");
}

/// `incomplete_executor_collections` is the restart/recovery read: it
/// returns live collections only — acknowledged and invalid rows are
/// settled and never resurface.
#[test]
fn incomplete_collections_exclude_settled_rows() {
    let store = SqliteStore::in_memory(1).unwrap();
    let live = bound_request(&store, "coll-live");
    let settled = bound_request(&store, "coll-done");
    apply_effects(
        &store,
        &[
            upsert(
                &collection(
                    &live,
                    "col-live",
                    1,
                    ExecutorCollectionState::Collected,
                    None,
                ),
                0,
            ),
            upsert(
                &collection(
                    &settled,
                    "col-done",
                    2,
                    ExecutorCollectionState::Acknowledged,
                    None,
                ),
                0,
            ),
        ],
    );
    let incomplete = store
        .transact(|tx| tx.incomplete_executor_collections())
        .unwrap();
    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0].request_id, live);
}

/// A tampered collection row reads as corrupt — an error, never a
/// silently absent record the caller could misread as "not yet
/// collected" and collect again.
#[test]
fn a_corrupt_collection_row_reads_as_an_error() {
    let root = temp_root("coll-corrupt");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = bound_request(&store, "coll-corrupt");
        apply_effects(
            &store,
            &[upsert(
                &collection(&id, "col-1", 1, ExecutorCollectionState::Collected, None),
                0,
            )],
        );
        drop(store);
        id
    };
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("UPDATE executor_collections SET collection_body=X'7b7d';")
        .unwrap();
    let store = SqliteStore::open(&db, 1).unwrap();
    assert!(
        store
            .transact(|tx| tx.executor_collection(&request_id))
            .is_err(),
        "a forged collection row is an error, never silently absent"
    );
    assert!(
        store
            .transact(|tx| tx.incomplete_executor_collections())
            .is_err(),
        "the recovery read surfaces the same corruption"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A v14 file — the executor tables without the collection table —
/// upgrades in place and accepts writes on the new table.
#[test]
fn a_v14_store_gains_the_collections_table() {
    let root = temp_root("upgrade-v14");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = bound_request(&store, "up-v14");
        drop(store);
        id
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE executor_collections; PRAGMA user_version=14;")
            .unwrap();
        let has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='executor_collections')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has, "the reshaped file is a real v14 store");
    }
    let store = SqliteStore::open(&db, 1).expect("a v14 store upgrades in place");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 16, "the upgrade lands on the current format");
    assert!(
        store
            .transact(|tx| tx.executor_collection(&request_id))
            .unwrap()
            .is_none(),
        "the v14 file carried no collection"
    );
    let coll = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collecting,
        None,
    );
    apply_effects(&store, &[upsert(&coll, 0)]);
    assert_eq!(
        store
            .transact(|tx| tx.executor_collection(&request_id))
            .unwrap()
            .unwrap(),
        coll,
        "the upgraded file records collections"
    );
    drop(store);
    let _ = SqliteStore::open(&db, 1).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// A v13 file — before any executor table — still upgrades cleanly:
/// the collections table lands in the same additive pass.
#[test]
fn a_v13_store_upgrades_through_v14_and_v15() {
    let root = temp_root("upgrade-v13-coll");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = bound_request(&store, "up-13-15");
        drop(store);
        id
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE executor_collections; DROP TABLE executor_operations; \
             DROP TABLE executor_bindings; PRAGMA user_version=13;",
        )
        .unwrap();
    }
    let store = SqliteStore::open(&db, 1).expect("a v13 store upgrades in place");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 16, "the upgrade lands on the current format");
    // The request row survived; re-attach the binding the v13 reshape
    // dropped so the collection's foreign key holds.
    assert!(
        store
            .transact(|tx| tx.executor_binding(&request_id))
            .unwrap()
            .is_none()
    );
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let coll = collection(
        &request_id,
        "col-1",
        1,
        ExecutorCollectionState::Collecting,
        None,
    );
    apply_effects(&store, &[upsert(&coll, 0)]);
    assert_eq!(
        store
            .transact(|tx| tx.executor_collection(&request_id))
            .unwrap()
            .unwrap(),
        coll
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// A file stamped v14 must hold every table the v13 → v14 migration
/// installed: an `executor_bindings`/`executor_operations`-less v14
/// file is a truncated or foreign store, refused untouched — never
/// silently upgraded past the missing evidence.
#[test]
fn a_v14_store_missing_executor_tables_is_refused() {
    let root = temp_root("upgrade-v14-truncated");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE executor_collections; DROP TABLE executor_operations; \
             DROP TABLE executor_bindings; PRAGMA user_version=14;",
        )
        .unwrap();
    }
    match SqliteStore::open(&db, 1) {
        Err(StoreError::UnsupportedFormat { .. }) => {}
        Err(e) => panic!("expected UnsupportedFormat, got {e:?}"),
        Ok(_) => panic!("a v14 file missing executor tables must not upgrade"),
    }
    let _ = std::fs::remove_dir_all(&root);
}
