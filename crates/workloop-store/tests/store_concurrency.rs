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
/// preview fails fast with `Err` instead of freezing on the busy
/// timeout behind the writer.
#[test]
fn preview_fails_fast_under_held_write_lock() {
    use workloop_core::command::{CommandBody, CommandRequest};
    use workloop_core::domain::{Actor, ActorKind, AllowedEffects, ExecutionBudget, Origin};
    use workloop_core::work::{WorkloopConfig, preview};

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

    let config = WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
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
            issue_refs: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let result = preview(&b, &config, &req, 1_700_000_000);
    assert!(
        result.is_err(),
        "preview under a held write lock must fail, not freeze: {result:?}"
    );
    done_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
    drop(b);
    let _ = std::fs::remove_dir_all(&root);
}
