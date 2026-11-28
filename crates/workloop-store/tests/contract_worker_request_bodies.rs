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
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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
                placement: None,
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

/// Seal a raw request value into the durable row shape: recompute the
/// self-referential `request_digest` member and the recorded
/// `body_digest` over the canonical bytes minus that member. This is
/// deliberately usable with an incomplete envelope so tests can prove
/// the store validates more than a self-consistent digest.
fn seal(request_id: &str, mut value: serde_json::Value) -> WorkerRequestBody {
    value.as_object_mut().unwrap().remove("request_digest");
    let digest = workloop_core::worker_v2::request_v2_digest(&canonical_json(&value));
    value.as_object_mut().unwrap().insert(
        "request_digest".into(),
        serde_json::Value::String(digest.clone()),
    );
    WorkerRequestBody {
        request_id: request_id.into(),
        format_version: "taisan.worker.request/2".into(),
        canonical_body: canonical_json(&value),
        body_digest: digest,
    }
}

/// A complete, valid `taisan.worker.request/2` body — every required
/// envelope member, a coding task, and a genuine digest. The store
/// contract requires the whole executable request, not a digest-shaped
/// fragment.
fn body(request_id: &str, goal: &str) -> WorkerRequestBody {
    let value = serde_json::json!({
        "format_version": "taisan.worker.request/2",
        "request_id": request_id,
        "invocation_id": format!("inv-{request_id}"),
        "store_id": "store-1",
        "work_id": "work-1",
        "execution_id": "execution-1",
        "role": "coding",
        "repository": {"host": "test-host", "repository": "local-repo"},
        "base": {
            "repository": {"host": "test-host", "repository": "local-repo"},
            "object": "0".repeat(40),
        },
        "spec": {"revision": 1, "digest": "a".repeat(64)},
        "workspace": {
            "workspace_id": "ws-1",
            "mount": "/workspace",
            "read_only": false,
        },
        "budget": {
            "deadline_unix_seconds": NOW + 3600,
            "max_runtime_seconds": 1800,
            "max_turns": 200,
            "max_report_bytes": 1_048_576,
            "max_log_bytes": 33_554_432,
            "max_verification_commands": 8,
            "max_continuation_refs": 4,
        },
        "required_capabilities": {
            "role": "coding",
            "protocol_versions": ["taisan.worker.request/2"],
            "capabilities": [],
            "resume": "unsupported",
            "verification_egress_hosts": [],
        },
        "continuation": [],
        "task": {
            "kind": "coding",
            "goal": {
                "ref": format!("goal-{request_id}"),
                "media_type": "text/markdown",
                "bytes": goal.len() as u64,
                "digest": "b".repeat(64),
            },
            "issues": [],
            "constraints": [goal],
            "verification_argv": [],
        },
    });
    seal(request_id, value)
}

/// Start from a complete body and mutate it, then reseal the digest so
/// only the intended envelope rule can refuse the result.
fn reshaped(request_id: &str, mutate: impl FnOnce(&mut serde_json::Value)) -> WorkerRequestBody {
    let mut value: serde_json::Value =
        serde_json::from_slice(&body(request_id, "goal").canonical_body).unwrap();
    mutate(&mut value);
    seal(request_id, value)
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

fn expect_refused_then_recovered(store: &SqliteStore, request_id: &str, bad: WorkerRequestBody) {
    let err = store
        .transact(|tx| tx.apply(&Effect::InsertWorkerRequestBody { body: bad }))
        .expect_err("an unverifiable body is refused at the write boundary");
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "the refusal is a typed conflict: {err:?}"
    );
    assert!(
        store
            .transact(|tx| tx.worker_request_body(request_id))
            .unwrap()
            .is_none(),
        "no row is left behind by the refused write"
    );
    let corrected = body(request_id, "corrected");
    apply_effects(
        store,
        &[Effect::InsertWorkerRequestBody {
            body: corrected.clone(),
        }],
    );
    assert_eq!(
        store
            .transact(|tx| tx.worker_request_body(request_id))
            .unwrap()
            .expect("the corrected body records"),
        corrected
    );
}

#[test]
fn an_invalid_json_body_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-badjson");
    let mut bad = body(&request_id, "goal");
    bad.canonical_body = b"{not json".to_vec();
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn a_bad_body_digest_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-baddigest");
    let mut bad = body(&request_id, "goal");
    bad.body_digest = "0".repeat(64);
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn an_unsupported_format_version_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-badver");
    let mut bad = body(&request_id, "goal");
    bad.format_version = "taisan.worker.request/9".into();
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn a_mismatched_request_id_member_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-badid");
    let mut bad = body("other:inv:9", "goal");
    bad.request_id = request_id.clone();
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn a_mismatched_request_digest_member_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-badrdigest");
    let mut bad = body(&request_id, "goal");
    let mut value: serde_json::Value = serde_json::from_slice(&bad.canonical_body).unwrap();
    value["request_digest"] = serde_json::Value::String("0".repeat(64));
    bad.canonical_body = canonical_json(&value);
    expect_refused_then_recovered(&store, &request_id, bad);
}

/// A digest-consistent body missing the required `task` member is not a
/// complete request — the write boundary parses the whole envelope, so
/// the row is refused even though every recorded digest matches.
#[test]
fn a_digest_consistent_incomplete_body_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-incomplete");
    let bad = reshaped(&request_id, |value| {
        value.as_object_mut().unwrap().remove("task");
    });
    assert!(
        workloop_core::worker_v2::WorkerRequestV2::parse(&bad.canonical_body).is_err(),
        "the fixture is digest-consistent but not a complete request"
    );
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn an_unknown_envelope_member_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-unknown");
    let bad = reshaped(&request_id, |value| {
        value["operator_hint"] = serde_json::Value::String("skip checks".into());
    });
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn a_role_task_disagreement_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-role");
    let bad = reshaped(&request_id, |value| {
        value["role"] = serde_json::Value::String("review".into());
        value["required_capabilities"]["role"] = serde_json::Value::String("review".into());
        value["workspace"]["read_only"] = serde_json::Value::Bool(true);
    });
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn an_over_budget_body_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-bounds");
    let bad = reshaped(&request_id, |value| {
        value["budget"]["max_verification_commands"] = serde_json::Value::from(0);
        value["task"]["verification_argv"] = serde_json::json!([["true"]]);
    });
    expect_refused_then_recovered(&store, &request_id, bad);
}

#[test]
fn an_invalid_reinsert_leaves_the_original_body() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "wrb-badreinsert");
    let recorded = body(&request_id, "first");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: recorded.clone(),
        }],
    );
    let mut bad = body(&request_id, "second");
    bad.body_digest = "0".repeat(64);
    let err = store
        .transact(|tx| tx.apply(&Effect::InsertWorkerRequestBody { body: bad }))
        .expect_err("an unverifiable replacement is refused");
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "the refusal is a typed conflict: {err:?}"
    );
    let read = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .unwrap()
        .expect("the original body survives the refused write");
    assert_eq!(read, recorded);
    assert_eq!(read.canonical_body, recorded.canonical_body);
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
    assert_eq!(version, 18, "the upgrade lands on the current format");
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

/// A tampered or foreign-version body row reads as corrupt — the
/// verbatim replay path is never handed bytes that fail verification,
/// and the row is not silently reported absent (which would downgrade
/// the dispatch onto the legacy v1 path).
#[test]
fn a_tampered_body_reads_as_corrupt() {
    let root = temp_root("tampered-body");
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let request_id = seeded_request(&store, "body-tamper");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "verifiable"),
        }],
    );
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE worker_request_bodies SET canonical_body=?1 WHERE request_id=?2",
            rusqlite::params![b"{}".to_vec(), request_id],
        )
        .unwrap();
    }
    let err = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .expect_err("a tampered body is corrupt, not replayable");
    assert!(
        matches!(err, StoreError::Internal { .. }),
        "the refusal names corruption: {err:?}"
    );

    // A foreign format version fails the same gate.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE worker_request_bodies SET format_version='taisan.worker.request/9', \
             canonical_body=?1, body_digest=?2 WHERE request_id=?3",
            rusqlite::params![
                body(&request_id, "verifiable").canonical_body,
                body(&request_id, "verifiable").body_digest,
                request_id
            ],
        )
        .unwrap();
    }
    assert!(
        store
            .transact(|tx| tx.worker_request_body(&request_id))
            .is_err(),
        "a non-v2 body row is corrupt"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// `request_digest` is excluded from its own digest, so altering that
/// member alone leaves `body_digest` consistent — the read gate
/// re-checks the embedded member explicitly and the row still reads
/// as corrupt.
#[test]
fn a_tampered_digest_member_reads_as_corrupt() {
    let root = temp_root("tampered-digest-member");
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let request_id = seeded_request(&store, "body-digest-tamper");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "verifiable"),
        }],
    );
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&body(&request_id, "verifiable").canonical_body).unwrap();
        value["request_digest"] = serde_json::Value::String("0".repeat(64));
        conn.execute(
            "UPDATE worker_request_bodies SET canonical_body=?1 WHERE request_id=?2",
            rusqlite::params![canonical_json(&value), request_id],
        )
        .unwrap();
    }
    let err = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .expect_err("a tampered digest member is corrupt, not replayable");
    assert!(
        matches!(err, StoreError::Internal { .. }),
        "the refusal names corruption: {err:?}"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// A row corrupted in place into a digest-consistent but incomplete
/// envelope must fail closed on read — it is never returned as
/// executable and never silently reported absent.
#[test]
fn a_digest_consistent_incomplete_body_reads_as_corrupt() {
    let root = temp_root("incomplete-body");
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let request_id = seeded_request(&store, "body-incomplete-tamper");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "verifiable"),
        }],
    );
    let bad = reshaped(&request_id, |value| {
        value.as_object_mut().unwrap().remove("task");
    });
    let mut stripped: serde_json::Value = serde_json::from_slice(&bad.canonical_body).unwrap();
    stripped.as_object_mut().unwrap().remove("request_digest");
    assert_eq!(
        bad.body_digest,
        workloop_core::worker_v2::request_v2_digest(&canonical_json(&stripped)),
        "the corrupted row is digest-consistent, so only envelope validation can refuse it"
    );
    assert!(
        workloop_core::worker_v2::WorkerRequestV2::parse(&bad.canonical_body).is_err(),
        "the corrupted row is not a complete request"
    );
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE worker_request_bodies SET canonical_body=?1, body_digest=?2 \
             WHERE request_id=?3",
            rusqlite::params![bad.canonical_body, bad.body_digest, request_id],
        )
        .unwrap();
    }
    let err = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .expect_err("an incomplete body is corrupt, not replayable");
    assert!(
        matches!(err, StoreError::Internal { .. }),
        "the refusal names corruption, not absence or a legacy row: {err:?}"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// The embedded `request_id` is covered by `body_digest`, so a row
/// written with a key/member mismatch verifies consistently — the
/// read gate re-checks the embedded identity against the row key and
/// the row still reads as corrupt.
#[test]
fn a_mismatched_request_id_member_reads_as_corrupt() {
    let root = temp_root("mismatched-request-id");
    let db = root.join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let request_id = seeded_request(&store, "body-id-mismatch");
    apply_effects(
        &store,
        &[Effect::InsertWorkerRequestBody {
            body: body(&request_id, "verifiable"),
        }],
    );
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&body(&request_id, "verifiable").canonical_body).unwrap();
        value["request_id"] = serde_json::Value::String("other:inv:9".into());
        // Keep the row self-consistent: recompute the digest over the
        // mismatched body — embedded member and column alike — so only
        // the identity check can refuse it.
        let mut stripped = value.clone();
        stripped.as_object_mut().unwrap().remove("request_digest");
        let digest = workloop_core::worker_v2::request_v2_digest(&canonical_json(&stripped));
        value["request_digest"] = serde_json::Value::String(digest.clone());
        conn.execute(
            "UPDATE worker_request_bodies SET canonical_body=?1, body_digest=?2              WHERE request_id=?3",
            rusqlite::params![canonical_json(&value), digest, request_id],
        )
        .unwrap();
    }
    let err = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .expect_err("a mismatched request_id is corrupt, not replayable");
    assert!(
        matches!(err, StoreError::Internal { .. }),
        "the refusal names corruption: {err:?}"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// Regression: a store stamped 11 before the `summary` column existed
/// gains `finding_occurrences.summary` on open — the upgrade is
/// additive, idempotent, and lands on the current format.
#[test]
fn a_v11_store_gains_the_occurrence_summary_column() {
    let root = temp_root("upgrade-v11");
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE finding_occurrences DROP COLUMN summary; \
             PRAGMA user_version=11;",
        )
        .unwrap();
        let has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('finding_occurrences') \
                 WHERE name='summary')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has, "the reshaped file is a real pre-summary v11 store");
    }
    let store = SqliteStore::open(&db, 1).expect("a v11 store upgrades in place");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 18, "the upgrade lands on the current format");
    // The occurrences read the column the upgrade restored — it would
    // have failed `no such column: summary` before the bump.
    assert!(
        store
            .transact(|tx| tx.occurrences("no-such-round"))
            .unwrap()
            .is_empty(),
        "the upgraded file answers occurrence reads"
    );
    drop(store);
    // Re-opening after the upgrade is a no-op, not a second migration.
    let _ = SqliteStore::open(&db, 1).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
