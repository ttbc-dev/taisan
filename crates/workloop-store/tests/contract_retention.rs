//! Retention-sweep contract: bounded, cadence-gated reclamation of
//! catalogued bodies. Metadata survives every pass, pins are never
//! deleted, unattributed or foreign paths are refused, and each pass
//! bound stops before the cost is paid.

use std::path::{Path, PathBuf};

use workloop_store::retention::{CatalogBody, SweepParams};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-retention-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn params() -> SweepParams {
    SweepParams {
        interval_seconds: 10,
        max_bodies: 256,
        max_bytes: 1 << 30,
        delete_error_limit: 3,
    }
}

/// A registered body whose file exists under `root`.
fn register(
    store: &SqliteStore,
    root: &Path,
    body_id: &str,
    registered_at: i64,
    eligible_after: Option<i64>,
    pinned: bool,
    bytes: u64,
) -> PathBuf {
    let path = root.join(format!("{body_id}.blob"));
    std::fs::write(&path, vec![0u8; bytes.min(64) as usize]).unwrap();
    store
        .register_body(&CatalogBody {
            body_id: body_id.into(),
            category: "cas".into(),
            path: path.to_string_lossy().into_owned(),
            bytes,
            registered_unix_seconds: registered_at,
            eligible_after_unix_seconds: eligible_after,
            pinned,
        })
        .unwrap();
    path
}

#[test]
fn an_eligible_unpinned_body_is_reclaimed_and_metadata_survives() {
    let dir = root("reclaim");
    let store = SqliteStore::in_memory(1).unwrap();
    let path = register(&store, &dir, "b1", NOW - 100, Some(NOW - 10), false, 8);
    assert!(path.exists());

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert!(report.ran);
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(report.reclaimed[0].reason, "age_expired");
    assert!(!path.exists(), "the body file is gone");
    // Durable metadata is never reclaimed with the body.
    let (state, reason, _) = store.body_state("b1").unwrap().unwrap();
    assert_eq!(state, "reclaimed");
    assert_eq!(reason.as_deref(), Some("age_expired"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_cadence_gate_holds_a_second_pass_within_the_interval() {
    let dir = root("cadence");
    let store = SqliteStore::in_memory(1).unwrap();
    let path = register(&store, &dir, "b1", NOW - 100, Some(NOW - 10), false, 8);

    let first = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert!(first.ran);
    assert_eq!(first.reclaimed.len(), 1);

    let second = store.retention_sweep(&params(), &dir, NOW + 5).unwrap();
    assert!(!second.ran);
    assert_eq!(second.defer_reason.as_deref(), Some("cadence"));

    // After the interval a new pass runs again (nothing left to do).
    let third = store.retention_sweep(&params(), &dir, NOW + 20).unwrap();
    assert!(third.ran);
    assert!(third.reclaimed.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = path;
}

#[test]
fn a_pinned_body_is_never_reclaimed() {
    let dir = root("pin");
    let store = SqliteStore::in_memory(1).unwrap();
    let path = register(&store, &dir, "b1", NOW - 100, Some(NOW - 10), true, 8);

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert!(report.reclaimed.is_empty());
    assert!(
        report
            .held
            .iter()
            .any(|h| h.body_id == "b1" && h.reason == "pinned")
    );
    assert!(path.exists(), "protective pin blocks the delete");
    assert_eq!(store.body_state("b1").unwrap().unwrap().0, "present");

    // Once the pin is discharged the same body becomes reclaimable.
    store.set_body_pin("b1", false).unwrap();
    let after = store.retention_sweep(&params(), &dir, NOW + 20).unwrap();
    assert_eq!(after.reclaimed.len(), 1);
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn young_and_unattributed_bodies_are_not_eligible() {
    let dir = root("young");
    let store = SqliteStore::in_memory(1).unwrap();
    let young = register(&store, &dir, "young", NOW - 5, Some(NOW + 1000), false, 8);
    let unattributed = register(&store, &dir, "unattributed", NOW - 5, None, false, 8);

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert!(report.ran);
    assert!(report.reclaimed.is_empty());
    assert!(young.exists());
    assert!(
        unattributed.exists(),
        "unattributable bodies are never swept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_body_bound_stops_the_pass_and_reports_the_remainder() {
    let dir = root("bound-bodies");
    let store = SqliteStore::in_memory(1).unwrap();
    let a = register(&store, &dir, "a", NOW - 300, Some(NOW - 10), false, 8);
    let b = register(&store, &dir, "b", NOW - 200, Some(NOW - 10), false, 8);
    let c = register(&store, &dir, "c", NOW - 100, Some(NOW - 10), false, 8);

    let mut p = params();
    p.max_bodies = 2;
    let report = store.retention_sweep(&p, &dir, NOW).unwrap();
    assert_eq!(report.reclaimed.len(), 2, "exactly the bound is reclaimed");
    assert_eq!(report.eligible_remaining, 1);
    // Oldest-first: a and b are gone, c waits for the next pass.
    assert!(!a.exists() && !b.exists());
    assert!(c.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_byte_bound_stops_before_paying_for_the_next_body() {
    let dir = root("bound-bytes");
    let store = SqliteStore::in_memory(1).unwrap();
    let a = register(&store, &dir, "a", NOW - 300, Some(NOW - 10), false, 60);
    let b = register(&store, &dir, "b", NOW - 200, Some(NOW - 10), false, 60);

    let mut p = params();
    p.max_bytes = 100;
    let report = store.retention_sweep(&p, &dir, NOW).unwrap();
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(report.bytes_reclaimed, 60);
    assert_eq!(report.eligible_remaining, 1);
    assert!(!a.exists());
    assert!(
        b.exists(),
        "the bound is checked before the next body's cost is paid"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_byte_bound_stops_the_pass_without_reordering_to_a_smaller_body() {
    let dir = root("bound-bytes-order");
    let store = SqliteStore::in_memory(1).unwrap();
    let oldest = register(&store, &dir, "oldest", NOW - 300, Some(NOW - 10), false, 60);
    let blocked = register(
        &store,
        &dir,
        "blocked",
        NOW - 200,
        Some(NOW - 10),
        false,
        60,
    );
    let smaller = register(
        &store,
        &dir,
        "smaller",
        NOW - 100,
        Some(NOW - 10),
        false,
        10,
    );

    let mut p = params();
    p.max_bytes = 100;
    let report = store.retention_sweep(&p, &dir, NOW).unwrap();
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(report.bytes_reclaimed, 60);
    assert_eq!(
        report.eligible_remaining, 2,
        "the blocked body and every later body wait for the next pass"
    );
    assert!(!oldest.exists());
    assert!(
        blocked.exists(),
        "the bound stops the pass rather than reordering around the blocked body"
    );
    assert!(
        smaller.exists(),
        "a newer smaller body is not reclaimed ahead of an older blocked one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_body_is_committed_as_reclaimed_missing() {
    let dir = root("missing");
    let store = SqliteStore::in_memory(1).unwrap();
    let path = register(&store, &dir, "b1", NOW - 100, Some(NOW - 10), false, 8);
    std::fs::remove_file(&path).unwrap();

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(report.reclaimed[0].reason, "missing");
    let (state, reason, _) = store.body_state("b1").unwrap().unwrap();
    assert_eq!(state, "reclaimed");
    assert_eq!(reason.as_deref(), Some("missing"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn foreign_and_traversing_paths_are_refused_untouched() {
    let dir = root("foreign");
    let outside = std::env::temp_dir().join(format!("workloop-ret-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    let foreign_file = outside.join("precious.txt");
    std::fs::write(&foreign_file, "keep me").unwrap();

    let store = SqliteStore::in_memory(1).unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "foreign".into(),
            category: "cas".into(),
            path: foreign_file.to_string_lossy().into_owned(),
            bytes: 7,
            registered_unix_seconds: NOW - 100,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "traversal".into(),
            category: "cas".into(),
            path: dir.join("../escape.blob").to_string_lossy().into_owned(),
            bytes: 7,
            registered_unix_seconds: NOW - 90,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert!(report.reclaimed.is_empty());
    assert!(
        report
            .held
            .iter()
            .any(|h| h.body_id == "foreign" && h.reason == "outside_root")
    );
    assert!(
        report
            .held
            .iter()
            .any(|h| h.body_id == "traversal" && h.reason == "unsafe_path")
    );
    assert!(foreign_file.exists(), "a foreign body is never touched");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_managed_directory_body_is_removed_with_its_contents() {
    let dir = root("dirbody");
    let store = SqliteStore::in_memory(1).unwrap();
    let body_dir = dir.join("ws-body");
    std::fs::create_dir_all(body_dir.join("nested")).unwrap();
    std::fs::write(body_dir.join("nested/file.txt"), "x").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "ws".into(),
            category: "workspace".into(),
            path: body_dir.to_string_lossy().into_owned(),
            bytes: 1024,
            registered_unix_seconds: NOW - 100,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let report = store.retention_sweep(&params(), &dir, NOW).unwrap();
    assert_eq!(report.reclaimed.len(), 1);
    assert!(!body_dir.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete_failures_retry_then_stop_at_the_limit_without_forcing() {
    let dir = root("delete-fail");
    let store = SqliteStore::in_memory(1).unwrap();
    // A path whose parent is a regular file: the delete always fails.
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, "block").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "stuck".into(),
            category: "cas".into(),
            path: blocker.join("child.blob").to_string_lossy().into_owned(),
            bytes: 8,
            registered_unix_seconds: NOW - 100,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let mut p = params();
    p.interval_seconds = 1;
    for (i, at) in [NOW, NOW + 10, NOW + 20, NOW + 30].into_iter().enumerate() {
        let report = store.retention_sweep(&p, &dir, at).unwrap();
        if i < 3 {
            assert_eq!(report.failed.len(), 1, "pass {i} records the failure");
            let (_, _, failures) = store.body_state("stuck").unwrap().unwrap();
            assert_eq!(failures, i as i64 + 1);
        } else {
            assert!(
                report
                    .held
                    .iter()
                    .any(|h| h.body_id == "stuck" && h.reason == "delete_error_limit"),
                "after the limit the automatic sweep stops attempting"
            );
            assert_eq!(report.failed.len(), 0);
        }
        assert!(blocker.exists(), "a failing delete is never forced");
    }
    let (state, _, failures) = store.body_state("stuck").unwrap().unwrap();
    assert_eq!(state, "reclaiming", "failure leaves the row retryable");
    assert_eq!(failures, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zero_bounds_are_refused_before_any_work() {
    let dir = root("zero");
    let store = SqliteStore::in_memory(1).unwrap();
    let mut p = params();
    p.max_bodies = 0;
    assert!(store.retention_sweep(&p, &dir, NOW).is_err());
    let mut p = params();
    p.max_bytes = 0;
    assert!(store.retention_sweep(&p, &dir, NOW).is_err());
    let mut p = params();
    p.interval_seconds = 0;
    assert!(store.retention_sweep(&p, &dir, NOW).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
