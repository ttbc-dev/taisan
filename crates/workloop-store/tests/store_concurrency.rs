//! Concurrency test: two `SqliteStore` handles on one database file — the
//! in-process stand-in for a CLI process and the run loop sharing the DB.
//! The second writer must wait on the busy timeout and then commit, not fail
//! immediately with SQLITE_BUSY.

use std::time::Duration;

use workloop_core::store::Store;
use workloop_store::sqlite::SqliteStore;

#[test]
fn second_connection_waits_for_write_lock() {
    let root = std::env::temp_dir().join(format!("workloop-store-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let a = SqliteStore::open(&db, 1).unwrap();
    let b = SqliteStore::open(&db, 1).unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        a.transact(|tx| -> Result<(), workloop_core::store::StoreError> {
            tx.alloc_seq()?;
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
            Ok(())
        })
    });

    ready_rx.recv().unwrap();
    // A holds the write lock; B's insert must wait inside the busy handler
    // for A's commit instead of surfacing SQLITE_BUSY as an error.
    b.transact(|tx| tx.alloc_seq())
        .expect("second writer should wait for the lock, not fail immediately");
    let last = b.transact(|tx| tx.last_seq()).unwrap();
    assert_eq!(last, 2);

    drop(b);
    writer.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_then_write_waits_before_observing_a_snapshot() {
    let root = std::env::temp_dir().join(format!("workloop-store-snapshot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    let a = SqliteStore::open(&db, 1).unwrap();
    let b = SqliteStore::open(&db, 1).unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    let (committed_tx, committed_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        a.transact(|tx| {
            tx.alloc_seq()?;
            locked_tx.send(()).unwrap();
            let _ = read_rx.recv_timeout(Duration::from_millis(250));
            Ok(())
        })
        .unwrap();
        committed_tx.send(()).unwrap();
    });
    locked_rx.recv().unwrap();
    let result = b.transact(|tx| {
        let observed = tx.last_seq()?;
        let _ = read_tx.send(());
        committed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let next = tx.alloc_seq()?;
        Ok((observed, next))
    });
    writer.join().unwrap();
    assert_eq!(
        result.expect("a read-modify-write transaction must not upgrade a stale WAL snapshot"),
        (1, 2)
    );
    assert_eq!(b.transact(|tx| tx.last_seq()).unwrap(), 2);
    drop(b);
    let _ = std::fs::remove_dir_all(&root);
}

/// A deferred transaction holds no writer lock while it only reads: a
/// Query through `transact_deferred` observes the last committed
/// snapshot concurrently with a held write lock instead of
/// busy-waiting behind the writer.
#[test]
fn deferred_read_does_not_wait_on_write_lock() {
    let root = std::env::temp_dir().join(format!("workloop-store-deferred-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let a = SqliteStore::open(&db, 1).unwrap();
    let b = SqliteStore::open(&db, 1).unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        a.transact(|tx| {
            tx.alloc_seq()?;
            ready_tx.send(()).unwrap();
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
            Ok(())
        })
    });

    ready_rx.recv().unwrap();
    // A holds the write lock; B's deferred read sees the committed
    // snapshot immediately rather than waiting for A's commit.
    let observed = b
        .transact_deferred(|tx| tx.last_seq())
        .expect("a deferred read must not wait on the write lock");
    assert_eq!(observed, 0, "read observed the uncommitted write");
    done_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
    assert_eq!(b.transact(|tx| tx.last_seq()).unwrap(), 1);
    drop(b);
    let _ = std::fs::remove_dir_all(&root);
}

/// `preview` begins deferred: while a writer holds the lock, the
/// simulated effect application cannot upgrade the snapshot, so the
/// preview returns `Err` promptly — well inside the 5 s busy timeout
/// an immediate transaction would spend waiting on the lock.
#[test]
fn preview_fails_fast_under_held_write_lock() {
    use std::collections::BTreeSet;

    use workloop_core::command::{CommandBody, CommandRequest};
    use workloop_core::domain::{
        Actor, ActorKind, AllowedEffects, ExecutionBudget, LanePolicy, Origin, WorkPolicy,
    };
    use workloop_core::identity::{
        ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability,
        HostInstanceId, RepositoryId, RepositoryRef, RoleBinding,
    };
    use workloop_core::work::{HostTemplate, WorkloopConfig, preview};

    let root = std::env::temp_dir().join(format!(
        "workloop-store-preview-busy-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let a = SqliteStore::open(&db, 1).unwrap();
    let b = SqliteStore::open(&db, 1).unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        a.transact(|tx| {
            tx.alloc_seq()?;
            ready_tx.send(()).unwrap();
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
            Ok(())
        })
    });
    ready_rx.recv().unwrap();

    let host = HostInstanceId::trusted("test-host");
    let config = WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: RepositoryRef::new(host.clone(), RepositoryId::trusted("local-repo")),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host,
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
    };
    // `Submit` allocates sequence and IDs inside the pass — the first
    // write the deferred transaction attempts cannot upgrade while A
    // holds the lock.
    let req = CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
        origin: Origin("tui".into()),
        idempotency_key: "pv-busy".into(),
        body: CommandBody::Submit {
            intent: "intent".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let started = std::time::Instant::now();
    let result = preview(&b, &config, &req, 1_700_000_000);
    let elapsed = started.elapsed();
    assert!(
        result.is_err(),
        "preview under a held write lock must fail, not freeze: {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "preview waited on the write lock ({elapsed:?}) — the deferred \
         upgrade must fail promptly, not exhaust the 5 s busy timeout"
    );
    done_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
    drop(b);
    let _ = std::fs::remove_dir_all(&root);
}

/// Regression: two first-opens of a not-yet-created store must not
/// collide on the bootstrap `store_id` row. Both can read the version-0
/// / empty-table gate before either commits; the loser must wait on the
/// winner's bootstrap transaction and then reuse its store identity
/// instead of surfacing the `store_meta.key` UNIQUE violation.
///
/// A blocker transaction holds the writer lock on the empty file so both
/// openers pass the format gate before either can bootstrap.
#[test]
fn concurrent_first_open_bootstraps_once_without_a_unique_failure() {
    use std::sync::{Arc, Barrier};

    use workloop_core::store::StoreError;

    let root = std::env::temp_dir().join(format!(
        "workloop-store-bootstrap-race-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    // Put the empty file in WAL mode first: the openers' own WAL pragma
    // is then a no-op, so the blocker below stalls them at BEGIN
    // IMMEDIATE (after each has read the version-0 / empty-table gate)
    // rather than on the journal-mode conversion.
    {
        let init = rusqlite::Connection::open(&db).unwrap();
        let mode: String = init
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    let blocker = rusqlite::Connection::open(&db).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            SqliteStore::open(&db, 1)
        }));
    }
    // Both openers have read the gate and are waiting on the writer
    // lock by now; release it so the race reaches the bootstrap.
    std::thread::sleep(Duration::from_millis(300));
    blocker.execute_batch("COMMIT;").unwrap();
    drop(blocker);

    let mut stores = Vec::new();
    for handle in handles {
        match handle.join().unwrap() {
            Ok(store) => stores.push(store),
            Err(StoreError::ConstraintConflict { detail }) => {
                panic!("first-open collided on bootstrap: {detail}")
            }
            Err(e) => panic!("concurrent first-open failed instead of waiting: {e:?}"),
        }
    }

    let ids: Vec<_> = stores
        .iter()
        .map(|store| store.transact(|tx| tx.store_id()).unwrap())
        .collect();
    assert_eq!(
        ids[0], ids[1],
        "both connections must see the winner's store identity"
    );
    // Both handles are usable writers on the one bootstrapped store.
    stores[0].transact(|tx| tx.alloc_seq()).unwrap();
    stores[1].transact(|tx| tx.alloc_seq()).unwrap();
    assert_eq!(stores[0].transact(|tx| tx.last_seq()).unwrap(), 2);
    drop(stores);

    let reopened = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(reopened.transact(|tx| tx.store_id()).unwrap(), ids[0]);
    drop(reopened);
    let _ = std::fs::remove_dir_all(&root);
}
