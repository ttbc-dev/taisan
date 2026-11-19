//! Capacity-slot configuration: the configured capacity is the
//! effective claim bound — reopening a store with a smaller value
//! trims free rows and bounds new claims, while a slot held beyond
//! the shrink point is honored until released and never re-claimed.

use std::path::PathBuf;

use workloop_core::domain::ExecutionId;
use workloop_core::store::{Effect, Store, StoreError};
use workloop_store::sqlite::SqliteStore;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-capacity-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn eid(n: u64) -> ExecutionId {
    ExecutionId(format!("e-{n}"))
}

fn claim(store: &SqliteStore, n: u64) -> Result<(), StoreError> {
    store.transact(|tx| {
        tx.apply(&Effect::ClaimSlot {
            execution_id: eid(n),
        })
    })
}

fn release(store: &SqliteStore, n: u64) {
    store
        .transact(|tx| {
            tx.apply(&Effect::ReleaseSlot {
                execution_id: eid(n),
            })
        })
        .unwrap();
}

#[test]
fn a_shrunk_capacity_bounds_new_claims_and_trims_free_slots() {
    let db = temp_root("shrink").join("store.db");
    {
        let store = SqliteStore::open(&db, 3).unwrap();
        claim(&store, 1).unwrap();
        claim(&store, 2).unwrap();
    }
    {
        // Reopening with a smaller capacity drops the limit
        // immediately: the two held slots survive, but no free slot
        // below the new bound remains.
        let store = SqliteStore::open(&db, 1).unwrap();
        assert_eq!(
            store.transact(|tx| tx.slot_limit()).unwrap(),
            1,
            "the last configured capacity is the effective limit"
        );
        assert!(matches!(claim(&store, 3), Err(StoreError::NoFreeSlot)));
        // Releasing a held slot frees a claimable row below the bound.
        release(&store, 1);
        claim(&store, 3).unwrap();
        assert!(matches!(claim(&store, 4), Err(StoreError::NoFreeSlot)));
    }
    {
        // Growing again reseeds the trimmed row: two slots held, one
        // free — exactly one more claim fits.
        let store = SqliteStore::open(&db, 3).unwrap();
        claim(&store, 4).unwrap();
        assert!(matches!(claim(&store, 5), Err(StoreError::NoFreeSlot)));
    }
}

#[test]
fn a_held_slot_past_the_shrink_point_is_honored_but_never_reclaimed() {
    let db = temp_root("shrink-held").join("store.db");
    {
        let store = SqliteStore::open(&db, 3).unwrap();
        claim(&store, 1).unwrap();
        claim(&store, 2).unwrap();
        claim(&store, 3).unwrap();
    }
    {
        // All three slots held: the shrink keeps the held rows —
        // claims stay refused while they drain.
        let store = SqliteStore::open(&db, 1).unwrap();
        assert_eq!(store.transact(|tx| tx.slot_limit()).unwrap(), 1);
        assert!(matches!(claim(&store, 4), Err(StoreError::NoFreeSlot)));
        // The excess slots (ids 1, 2) drain first: freed rows beyond
        // the bound stay unreachable — claims are still refused.
        release(&store, 2);
        release(&store, 3);
        assert!(matches!(claim(&store, 4), Err(StoreError::NoFreeSlot)));
        // Once the in-bound slot drains it is re-claimable.
        release(&store, 1);
        claim(&store, 4).unwrap();
        assert!(matches!(claim(&store, 5), Err(StoreError::NoFreeSlot)));
    }
}
