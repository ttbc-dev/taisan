//! Run-lease contract: one installation, one loop driver. Contention
//! fails closed, a provably gone owner's lease is reclaimable (restart),
//! an uninspectable owner never yields the lease, and a stale holder's
//! release cannot clear a reclaimed lease.

use workloop_core::domain::ProcessIdentity;
use workloop_core::ports::ProcessEvidence;
use workloop_store::sqlite::{RunLeaseError, RunLeaseGrant, RunLeaseOwner, SqliteStore};

const NOW: i64 = 1_700_000_000;

fn owner(pid: i32, started_at: &str, nonce: &str) -> RunLeaseOwner {
    RunLeaseOwner {
        pid,
        started_at: started_at.into(),
        nonce: nonce.into(),
    }
}

fn alive(id: &ProcessIdentity) -> ProcessEvidence {
    ProcessEvidence::Alive {
        identity: id.clone(),
    }
}

fn stopped(id: &ProcessIdentity) -> ProcessEvidence {
    ProcessEvidence::Stopped {
        identity: id.clone(),
        exit_code: None,
    }
}

fn uninspectable(id: &ProcessIdentity) -> ProcessEvidence {
    let _ = id;
    ProcessEvidence::Unknown {
        reason: "process table unreadable".into(),
    }
}

#[test]
fn fresh_lease_acquires_and_releases() {
    let store = SqliteStore::in_memory(1).unwrap();
    let a = owner(101, "Mon Jan  1 00:00:00 2024", "nonce-a");
    assert_eq!(
        store.acquire_run_lease(&a, alive, NOW).unwrap(),
        RunLeaseGrant::Acquired
    );
    // A second driver with a live recorded owner is refused.
    let b = owner(102, "Mon Jan  1 00:00:00 2024", "nonce-b");
    match store.acquire_run_lease(&b, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 101),
        other => panic!("expected lock.held, got {other:?}"),
    }
    store.release_run_lease(&a).unwrap();
    assert_eq!(
        store.acquire_run_lease(&b, alive, NOW).unwrap(),
        RunLeaseGrant::Acquired
    );
}

#[test]
fn restart_reclaims_a_proven_gone_owners_lease() {
    let store = SqliteStore::in_memory(1).unwrap();
    let crashed = owner(201, "Mon Jan  1 00:00:00 2024", "nonce-crashed");
    store.acquire_run_lease(&crashed, alive, NOW).unwrap();
    // The next driver inspects the recorded identity: gone (or its pid
    // reused under a different start time) — the lease is reclaimable.
    let restarted = owner(202, "Tue Jan  2 00:00:00 2024", "nonce-restart");
    assert_eq!(
        store
            .acquire_run_lease(&restarted, stopped, NOW + 60)
            .unwrap(),
        RunLeaseGrant::Reclaimed { previous_pid: 201 }
    );
    // The reclaimed row now belongs to the restarted driver.
    let again = owner(202, "Tue Jan  2 00:00:00 2024", "nonce-restart");
    assert_eq!(
        store.acquire_run_lease(&again, alive, NOW + 61).unwrap(),
        RunLeaseGrant::Acquired
    );
}

#[test]
fn uninspectable_owner_fails_closed() {
    let store = SqliteStore::in_memory(1).unwrap();
    let a = owner(301, "Mon Jan  1 00:00:00 2024", "nonce-a");
    store.acquire_run_lease(&a, alive, NOW).unwrap();
    let b = owner(302, "Mon Jan  1 00:00:00 2024", "nonce-b");
    match store.acquire_run_lease(&b, uninspectable, NOW) {
        Err(RunLeaseError::Unavailable { reason }) => {
            assert!(reason.contains("unreadable"), "{reason}");
        }
        other => panic!("expected lock.unavailable, got {other:?}"),
    }
    // The failed attempt never disturbed the recorded owner.
    match store.acquire_run_lease(&b, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 301),
        other => panic!("expected lock.held after the failed probe, got {other:?}"),
    }
}

#[test]
fn stale_holders_release_cannot_clear_a_reclaimed_lease() {
    let store = SqliteStore::in_memory(1).unwrap();
    let a = owner(401, "Mon Jan  1 00:00:00 2024", "nonce-a");
    store.acquire_run_lease(&a, alive, NOW).unwrap();
    let b = owner(402, "Tue Jan  2 00:00:00 2024", "nonce-b");
    store.acquire_run_lease(&b, stopped, NOW + 1).unwrap();
    // The old holder returns and releases: only its own nonce clears.
    store.release_run_lease(&a).unwrap();
    // b still owns the lease (same-owner renewal path proves the row).
    assert_eq!(
        store.acquire_run_lease(&b, alive, NOW + 2).unwrap(),
        RunLeaseGrant::Acquired
    );
}

#[test]
fn release_requires_the_matching_owner_nonce() {
    let store = SqliteStore::in_memory(1).unwrap();
    let a = owner(501, "Mon Jan  1 00:00:00 2024", "nonce-a");
    store.acquire_run_lease(&a, alive, NOW).unwrap();
    let impostor = owner(501, "Mon Jan  1 00:00:00 2024", "nonce-other");
    store.release_run_lease(&impostor).unwrap();
    match store.acquire_run_lease(&impostor, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 501),
        other => panic!("release by a foreign nonce must not clear the lease: {other:?}"),
    }
}

#[test]
fn two_connections_share_one_single_flight_lease_row() {
    let root = std::env::temp_dir().join(format!("workloop-lease-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");

    let driver_a = SqliteStore::open(&db, 1).unwrap();
    let driver_b = SqliteStore::open(&db, 1).unwrap();

    let a = owner(601, "Mon Jan  1 00:00:00 2024", "nonce-a");
    driver_a.acquire_run_lease(&a, alive, NOW).unwrap();

    // Contention across connections: the recorded owner is alive.
    let b = owner(602, "Tue Jan  2 00:00:00 2024", "nonce-b");
    match driver_b.acquire_run_lease(&b, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 601),
        other => panic!("expected lock.held across connections, got {other:?}"),
    }

    // After the first driver crashes, the second reclaims on the same
    // file — still exactly one holder.
    assert_eq!(
        driver_b.acquire_run_lease(&b, stopped, NOW + 1).unwrap(),
        RunLeaseGrant::Reclaimed { previous_pid: 601 }
    );
    let a_stale = owner(601, "Mon Jan  1 00:00:00 2024", "nonce-a");
    driver_a.release_run_lease(&a_stale).unwrap();
    assert_eq!(
        driver_b.acquire_run_lease(&b, alive, NOW + 2).unwrap(),
        RunLeaseGrant::Acquired,
        "the crashed driver's late release must not free the reclaimed lease"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tick_report_events_round_trip_through_the_audit_log() {
    let store = SqliteStore::in_memory(1).unwrap();
    assert_eq!(store.count_events("tick_report").unwrap(), 0);
    let seq = store
        .append_event("tick_report", "{\"schema_version\":\"1\"}")
        .unwrap();
    assert!(seq > 0);
    assert_eq!(store.count_events("tick_report").unwrap(), 1);
    assert_eq!(store.count_events("effect").unwrap(), 0);
}

#[test]
fn maintenance_lease_is_independent_of_the_driver_lease() {
    let store = SqliteStore::in_memory(1).unwrap();
    let driver = owner(201, "Mon Jan  1 00:00:00 2024", "nonce-driver");
    let maint_a = owner(202, "Mon Jan  1 00:00:00 2024", "nonce-maint-a");
    let maint_b = owner(203, "Mon Jan  1 00:00:00 2024", "nonce-maint-b");

    store.acquire_run_lease(&driver, alive, NOW).unwrap();
    // The maintenance lease is granted while the driver lease is held.
    store
        .acquire_maintenance_lease(&maint_a, alive, NOW)
        .unwrap();
    // A second maintenance writer is refused; the driver lease is untouched.
    match store.acquire_maintenance_lease(&maint_b, alive, NOW) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 202),
        other => panic!("expected lock.held, got {other:?}"),
    }
    store.release_maintenance_lease(&maint_a).unwrap();
    store
        .acquire_maintenance_lease(&maint_b, alive, NOW)
        .unwrap();
    // A second driver is still refused while the driver lease is held.
    match store.acquire_run_lease(
        &owner(204, "Mon Jan  1 00:00:00 2024", "nonce-driver2"),
        alive,
        NOW,
    ) {
        Err(RunLeaseError::Held { owner_pid }) => assert_eq!(owner_pid, 201),
        other => panic!("expected lock.held, got {other:?}"),
    }
    // Maintenance release does not disturb the driver lease either.
    store.release_maintenance_lease(&maint_b).unwrap();
    store.release_run_lease(&driver).unwrap();
    store
        .acquire_run_lease(
            &owner(204, "Mon Jan  1 00:00:00 2024", "nonce-driver2"),
            alive,
            NOW,
        )
        .unwrap();
}
