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
