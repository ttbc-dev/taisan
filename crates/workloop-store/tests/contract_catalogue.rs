//! Body-catalogue contract: the five persisted fields, the exact
//! eligibility matrix under per-category age/byte settings, explicit
//! reporting of over-budget protected bodies, and metadata survival
//! across reclamation.

use std::collections::BTreeMap;
use std::path::PathBuf;

use workloop_store::retention::{CategoryLimits, CategorySettings, PinState, RetainedBody};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn settings(max_age: Option<i64>, max_bytes: Option<u64>) -> CategorySettings {
    BTreeMap::from([("cas".to_string(), CategoryLimits::new(max_age, max_bytes))])
}

fn body(id: &str, created_at: i64, size_bytes: u64, pin_state: PinState) -> RetainedBody {
    RetainedBody {
        body_id: id.into(),
        category: "cas".into(),
        size_bytes,
        created_at,
        pin_state,
        path: format!("/managed/{id}.blob"),
    }
}

fn register_all(store: &SqliteStore, bodies: &[RetainedBody]) {
    for body in bodies {
        store.register_retained_body(body).unwrap();
    }
}

#[test]
fn catalogue_registration_persists_all_five_fields() {
    let store = SqliteStore::in_memory(1).unwrap();
    let registered = body("body-1", NOW - 50, 128, PinState::Paused);
    store.register_retained_body(&registered).unwrap();

    let stored = store.catalogue_record("body-1").unwrap().unwrap();
    assert_eq!(stored.body_id, "body-1");
    assert_eq!(stored.category, "cas");
    assert_eq!(stored.size_bytes, 128);
    assert_eq!(stored.created_at, NOW - 50);
    assert_eq!(stored.pin_state, PinState::Paused);
    assert_eq!(store.body_state("body-1").unwrap().unwrap().0, "present");
}

/// The complete matrix: old+unpinned is eligible; young+unpinned within
/// the byte cap is retained; young+unpinned over the cap is eligible;
/// every declared pin state is retained regardless of age or bytes.
#[test]
fn eligibility_matrix_is_exact_for_every_age_byte_and_pin_combination() {
    let pins = [
        PinState::None,
        PinState::Active,
        PinState::Paused,
        PinState::Recovering,
        PinState::UnknownEffect,
    ];
    for pin_state in pins {
        for old in [false, true] {
            for over_bytes in [false, true] {
                let store = SqliteStore::in_memory(1).unwrap();
                let created_at = if old { NOW - 200 } else { NOW - 10 };
                let mut bodies = vec![body("target", created_at, 10, pin_state)];
                let max_bytes = if over_bytes {
                    bodies.push(body("sibling", NOW - 10, 10, PinState::None));
                    15
                } else {
                    100
                };
                register_all(&store, &bodies);

                let report = store
                    .catalogue_eligibility(&settings(Some(100), Some(max_bytes)), NOW)
                    .unwrap();
                let target = report.entry("target").unwrap();
                let expected = !pin_state.is_pin() && (old || over_bytes);
                assert_eq!(
                    target.eligible, expected,
                    "pin={pin_state:?} old={old} over_bytes={over_bytes}"
                );
                assert_eq!(target.pin_state, pin_state);
                assert_eq!(target.age_expired, old);
                assert_eq!(target.category_over_max_bytes, over_bytes);
            }
        }
    }
}

/// The rule is exactly `pin_state = none AND (old OR over max_bytes)`:
/// the same young over-budget body flips only when pinned, and no pin
/// state can ever be eligible.
#[test]
fn only_the_exact_rule_makes_a_young_over_budget_body_eligible() {
    for pin_state in [
        PinState::None,
        PinState::Active,
        PinState::Paused,
        PinState::Recovering,
        PinState::UnknownEffect,
    ] {
        let store = SqliteStore::in_memory(1).unwrap();
        register_all(
            &store,
            &[
                body("target", NOW - 10, 10, pin_state),
                body("sibling", NOW - 10, 10, PinState::None),
            ],
        );
        let report = store
            .catalogue_eligibility(&settings(Some(10_000), Some(15)), NOW)
            .unwrap();
        let target = report.entry("target").unwrap();
        assert!(target.category_over_max_bytes);
        assert!(!target.age_expired);
        if pin_state == PinState::None {
            assert!(
                target.eligible,
                "young unpinned over-budget body is eligible"
            );
        } else {
            assert!(
                !target.eligible,
                "{pin_state:?} is a pin and must be retained regardless of bytes"
            );
            assert_eq!(target.reason, "pinned");
        }
    }
}

/// Category settings are authoritative: varying either axis changes
/// the decision for the same catalogue row.
#[test]
fn category_max_age_and_max_bytes_settings_drive_eligibility() {
    let store = SqliteStore::in_memory(1).unwrap();
    register_all(&store, &[body("target", NOW - 10, 20, PinState::None)]);

    let retained = store
        .catalogue_eligibility(&settings(Some(100), Some(100)), NOW)
        .unwrap();
    assert!(!retained.is_eligible("target"));
    assert_eq!(retained.entry("target").unwrap().reason, "within_limits");

    let younger_limit = store
        .catalogue_eligibility(&settings(Some(5), Some(100)), NOW)
        .unwrap();
    assert!(younger_limit.is_eligible("target"));
    assert_eq!(younger_limit.entry("target").unwrap().reason, "age_expired");

    let tighter_cap = store
        .catalogue_eligibility(&settings(Some(1000), Some(19)), NOW)
        .unwrap();
    assert!(tighter_cap.is_eligible("target"));
    assert_eq!(tighter_cap.entry("target").unwrap().reason, "size_pressure");

    let unlimited = store
        .catalogue_eligibility(&settings(None, None), NOW)
        .unwrap();
    assert!(!unlimited.is_eligible("target"));
}

/// `age > max_age` and `bytes > max_bytes` are strict: equality keeps
/// the body.
#[test]
fn age_and_byte_limits_are_strict_at_the_boundary() {
    let store = SqliteStore::in_memory(1).unwrap();
    register_all(&store, &[body("target", NOW - 100, 10, PinState::None)]);
    let report = store
        .catalogue_eligibility(&settings(Some(100), Some(10)), NOW)
        .unwrap();
    let target = report.entry("target").unwrap();
    assert!(!target.age_expired);
    assert!(!target.category_over_max_bytes);
    assert!(!target.eligible);
}

/// A pinned body over the cap is retained and named in an explicit
/// category report entry; it is never silently dropped from accounting.
#[test]
fn over_budget_protected_bodies_are_retained_and_reported() {
    let dir = temp_root("catalogue-protected");
    let path = dir.join("pinned.blob");
    std::fs::write(&path, b"0123456789").unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    store
        .register_retained_body(&RetainedBody {
            body_id: "pinned".into(),
            category: "cas".into(),
            size_bytes: 20,
            created_at: NOW - 10,
            pin_state: PinState::Paused,
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();

    let report = store
        .catalogue_eligibility(&settings(Some(1000), Some(5)), NOW)
        .unwrap();
    let target = report.entry("pinned").unwrap();
    assert!(!target.eligible);
    let category = report.category("cas").unwrap();
    assert!(category.over_max_bytes);
    assert!(category.protected_over_limit);
    assert_eq!(category.protected_bytes, 20);
    assert!(
        category
            .held
            .iter()
            .any(|held| held.body_id == "pinned" && held.reason == "protected_over_limit"),
        "over-budget protection has an explicit report entry: {:?}",
        category.held
    );

    // Reclamation refuses the protected body and it remains on disk.
    let outcome = store
        .reclaim_catalogue_body("pinned", &settings(Some(1000), Some(5)), &dir, &[], NOW)
        .unwrap();
    assert!(!outcome.reclaimed);
    assert_eq!(outcome.reason, "pinned");
    assert!(
        path.exists(),
        "a pinned body is never reclaimed by the catalogue"
    );
    assert_eq!(store.body_state("pinned").unwrap().unwrap().0, "present");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reclaiming a body removes only the physical body. The five catalogue
/// fields remain readable and the availability reason is recorded.
#[test]
fn reclaiming_a_body_preserves_its_catalogue_metadata() {
    let dir = temp_root("catalogue-reclaim");
    let path = dir.join("old.blob");
    std::fs::write(&path, b"evidence").unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let registered = RetainedBody {
        body_id: "old".into(),
        category: "cas".into(),
        size_bytes: 8,
        created_at: NOW - 200,
        pin_state: PinState::None,
        path: path.to_string_lossy().into_owned(),
    };
    store.register_retained_body(&registered).unwrap();
    assert!(
        store
            .catalogue_eligibility(&settings(Some(100), Some(1000)), NOW)
            .unwrap()
            .is_eligible("old")
    );

    let outcome = store
        .reclaim_catalogue_body("old", &settings(Some(100), Some(1000)), &dir, &[], NOW)
        .unwrap();
    assert!(outcome.reclaimed);
    assert_eq!(outcome.reason, "age_expired");
    assert!(!path.exists(), "the physical body is gone");

    let after = store.catalogue_record("old").unwrap().unwrap();
    assert_eq!(after.body_id, "old");
    assert_eq!(after.category, "cas");
    assert_eq!(after.size_bytes, 8);
    assert_eq!(after.created_at, NOW - 200);
    assert_eq!(after.pin_state, PinState::None);
    let (state, reason, _) = store.body_state("old").unwrap().unwrap();
    assert_eq!(state, "reclaimed");
    assert_eq!(reason.as_deref(), Some("age_expired"));

    // A reclaimed body is unavailable: the catalogue refuses a new pin.
    let err = store.set_pin_state("old", PinState::Active).unwrap_err();
    assert!(format!("{err}").contains("unavailable"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Changing the catalogue pin state is authoritative for eligibility:
/// a young body becomes protected, then eligible again when released.
#[test]
fn pin_state_changes_protect_and_release_the_same_body() {
    let store = SqliteStore::in_memory(1).unwrap();
    register_all(&store, &[body("target", NOW - 10, 10, PinState::None)]);
    let over = settings(Some(1000), Some(5));

    assert!(
        store
            .catalogue_eligibility(&over, NOW)
            .unwrap()
            .is_eligible("target")
    );
    store.set_pin_state("target", PinState::Recovering).unwrap();
    assert_eq!(
        store.catalogue_record("target").unwrap().unwrap().pin_state,
        PinState::Recovering
    );
    assert!(
        !store
            .catalogue_eligibility(&over, NOW)
            .unwrap()
            .is_eligible("target")
    );
    store.set_pin_state("target", PinState::None).unwrap();
    assert!(
        store
            .catalogue_eligibility(&over, NOW)
            .unwrap()
            .is_eligible("target")
    );
}

/// Reclamation failure paths keep the catalogue readable: an absent
/// body is recorded `missing`, an unsafe path is held before deletion,
/// and a delete error leaves the row retryable without losing metadata.
#[test]
fn catalogue_reclaim_reports_missing_unsafe_and_failed_deletes() {
    let dir = temp_root("catalogue-reclaim-failures");
    let store = SqliteStore::in_memory(1).unwrap();
    let missing = RetainedBody {
        body_id: "missing".into(),
        category: "cas".into(),
        size_bytes: 8,
        created_at: NOW - 200,
        pin_state: PinState::None,
        path: dir.join("absent.blob").to_string_lossy().into_owned(),
    };
    let unsafe_path = RetainedBody {
        body_id: "unsafe".into(),
        category: "cas".into(),
        size_bytes: 8,
        created_at: NOW - 200,
        pin_state: PinState::None,
        path: dir.join("../escape.blob").to_string_lossy().into_owned(),
    };
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, "block").unwrap();
    let failed = RetainedBody {
        body_id: "failed".into(),
        category: "cas".into(),
        size_bytes: 8,
        created_at: NOW - 200,
        pin_state: PinState::None,
        path: blocker.join("child.blob").to_string_lossy().into_owned(),
    };
    register_all(&store, &[missing, unsafe_path, failed]);

    let reclaim_settings = settings(Some(100), Some(1000));
    let absent = store
        .reclaim_catalogue_body("missing", &reclaim_settings, &dir, &[], NOW)
        .unwrap();
    assert!(absent.reclaimed);
    assert_eq!(absent.reason, "missing");

    let unsafe_result = store
        .reclaim_catalogue_body("unsafe", &reclaim_settings, &dir, &[], NOW)
        .unwrap();
    assert!(!unsafe_result.reclaimed);
    assert_eq!(unsafe_result.reason, "unsafe_path");
    assert_eq!(store.body_state("unsafe").unwrap().unwrap().0, "present");

    let failure = store
        .reclaim_catalogue_body("failed", &reclaim_settings, &dir, &[], NOW)
        .unwrap();
    assert!(!failure.reclaimed);
    assert!(
        failure.reason.starts_with("delete_failed(1)"),
        "{}",
        failure.reason
    );
    let (state, _, failures) = store.body_state("failed").unwrap().unwrap();
    assert_eq!(state, "reclaiming", "a failed delete stays retryable");
    assert_eq!(failures, 1);
    // The five catalogue fields remain readable on every path.
    for id in ["missing", "unsafe", "failed"] {
        let record = store.catalogue_record(id).unwrap().unwrap();
        assert_eq!(record.body_id, id);
        assert_eq!(record.size_bytes, 8);
        assert_eq!(record.created_at, NOW - 200);
        assert_eq!(record.pin_state, PinState::None);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A v2 catalogue has no `pin_state` column. Opening it upgrades
/// additively and materializes the pre-existing boolean protection as
/// `active`, so eligibility can never silently treat a pinned old row
/// as unpinned.
#[test]
fn a_v2_catalogue_without_pin_state_upgrades_and_materializes_existing_pins() {
    use workloop_store::retention::CatalogBody;

    let dir = temp_root("catalogue-v2-upgrade");
    let db = dir.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        store
            .register_body(&CatalogBody {
                body_id: "pinned".into(),
                category: "cas".into(),
                path: dir.join("pinned.blob").to_string_lossy().into_owned(),
                bytes: 10,
                registered_unix_seconds: NOW - 10,
                eligible_after_unix_seconds: Some(NOW - 5),
                pinned: true,
            })
            .unwrap();
        store
            .register_body(&CatalogBody {
                body_id: "free".into(),
                category: "cas".into(),
                path: dir.join("free.blob").to_string_lossy().into_owned(),
                bytes: 10,
                registered_unix_seconds: NOW - 10,
                eligible_after_unix_seconds: Some(NOW - 5),
                pinned: false,
            })
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE body_catalog DROP COLUMN pin_state; PRAGMA user_version=2;",
        )
        .unwrap();
    }
    let store = SqliteStore::open(&db, 1).unwrap();
    assert_eq!(
        store.catalogue_record("pinned").unwrap().unwrap().pin_state,
        PinState::Active
    );
    assert_eq!(
        store.catalogue_record("free").unwrap().unwrap().pin_state,
        PinState::None
    );
    let report = store
        .catalogue_eligibility(&settings(Some(1000), Some(1)), NOW)
        .unwrap();
    assert!(!report.is_eligible("pinned"));
    assert!(report.is_eligible("free"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-catalogue-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A young unpinned body the policy retains must not be deletable
/// through the catalogue reclamation API: eligibility is recomputed from
/// the same authoritative state the report uses, inside the marking
/// transaction.
#[test]
fn reclaim_refuses_a_body_the_policy_would_retain() {
    let dir = temp_root("catalogue-ineligible-reclaim");
    let path = dir.join("young.blob");
    std::fs::write(&path, b"0123456789").unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    store
        .register_retained_body(&RetainedBody {
            body_id: "young".into(),
            category: "cas".into(),
            size_bytes: 10,
            created_at: NOW - 10,
            pin_state: PinState::None,
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
    let limits = settings(Some(2_592_000), Some(1_073_741_824));
    assert!(
        !store
            .catalogue_eligibility(&limits, NOW)
            .unwrap()
            .is_eligible("young"),
        "the policy retains a young body within its byte cap"
    );

    let outcome = store
        .reclaim_catalogue_body("young", &limits, &dir, &[], NOW)
        .unwrap();
    assert!(!outcome.reclaimed);
    assert_eq!(outcome.reason, "within_limits");
    assert!(path.exists(), "an ineligible body's file remains");
    assert_eq!(store.body_state("young").unwrap().unwrap().0, "present");

    // Tightening the byte cap makes the same young body eligible by
    // size pressure; the reclaim records that reason, not an age one.
    let tight = settings(Some(2_592_000), Some(9));
    assert!(
        store
            .catalogue_eligibility(&tight, NOW)
            .unwrap()
            .is_eligible("young")
    );
    let outcome = store
        .reclaim_catalogue_body("young", &tight, &dir, &[], NOW)
        .unwrap();
    assert!(outcome.reclaimed);
    assert_eq!(outcome.reason, "size_pressure");
    assert!(!path.exists());
    let after = store.catalogue_record("young").unwrap().unwrap();
    assert_eq!(after.size_bytes, 10);
    assert_eq!(after.category, "cas");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A catalogue row pointing at one of the installation's own files is
/// held, never reclaimed: containment under the state root does not
/// authorize deleting the live store or the config.
#[test]
fn reclaim_holds_a_catalogue_row_pointing_at_an_installation_file() {
    let dir = temp_root("catalogue-protected-reclaim");
    let live = dir.join("store.db");
    let config = dir.join("config.toml");
    std::fs::write(&live, b"live store").unwrap();
    std::fs::write(&config, b"config").unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    for (body_id, path) in [("live", &live), ("config", &config)] {
        store
            .register_retained_body(&RetainedBody {
                body_id: body_id.into(),
                category: "cas".into(),
                size_bytes: 8,
                created_at: NOW - 200,
                pin_state: PinState::None,
                path: path.to_string_lossy().into_owned(),
            })
            .unwrap();
    }
    let limits = settings(Some(100), Some(1000));
    let protected = vec![live.clone(), config.clone()];
    for body_id in ["live", "config"] {
        let outcome = store
            .reclaim_catalogue_body(body_id, &limits, &dir, &protected, NOW)
            .unwrap();
        assert!(!outcome.reclaimed, "{body_id} is held");
        assert_eq!(outcome.reason, "protected_path");
        assert_eq!(store.body_state(body_id).unwrap().unwrap().0, "present");
    }
    assert!(live.exists(), "the live store survives");
    assert!(config.exists(), "the config survives");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An omitted category key applies the documented bounded default, not
/// unlimited: `cas` keeps 90 days / 40 GiB, so the same rows become
/// eligible on age or byte pressure without an explicit settings entry.
#[test]
fn omitted_known_categories_use_the_documented_bounded_defaults() {
    let age_store = SqliteStore::in_memory(1).unwrap();
    register_all(
        &age_store,
        &[
            body("old", NOW - 7_776_001, 1, PinState::None),
            body("young", NOW - 7_775_999, 1, PinState::None),
        ],
    );
    let empty = CategorySettings::new();
    let aged = age_store.catalogue_eligibility(&empty, NOW).unwrap();
    assert!(aged.entry("old").unwrap().age_expired);
    assert!(aged.is_eligible("old"));
    assert!(!aged.entry("young").unwrap().age_expired);
    assert!(!aged.is_eligible("young"));

    let byte_store = SqliteStore::in_memory(1).unwrap();
    register_all(
        &byte_store,
        &[body("over", NOW - 10, 42_949_672_961, PinState::None)],
    );
    let pressured = byte_store.catalogue_eligibility(&empty, NOW).unwrap();
    assert!(pressured.category("cas").unwrap().over_max_bytes);
    assert!(pressured.is_eligible("over"));
    assert_eq!(pressured.entry("over").unwrap().reason, "size_pressure");

    assert_eq!(
        CategoryLimits::documented_default("workspace"),
        Some(CategoryLimits::new(Some(2_592_000), Some(21_474_836_480)))
    );
    assert_eq!(
        CategoryLimits::documented_default("archive"),
        Some(CategoryLimits::unlimited())
    );
}

/// A category outside the closed set is a loud error, not a silent
/// never-reclaim policy: when no explicit setting exists for it,
/// eligibility fails closed instead of resolving to unlimited.
#[test]
fn unknown_catalogue_categories_are_rejected_instead_of_becoming_unlimited() {
    use workloop_store::retention::CatalogBody;

    let store = SqliteStore::in_memory(1).unwrap();
    let mut typo = body("typo", NOW - 10, 10, PinState::None);
    typo.category = "CAS".into();
    store.register_retained_body(&typo).unwrap();
    let err = store
        .catalogue_eligibility(&CategorySettings::new(), NOW)
        .unwrap_err();
    assert!(
        format!("{err}").contains("unknown retention category"),
        "{err}"
    );

    // A row written through the legacy API also cannot resolve to an
    // unbounded category during eligibility.
    let legacy = SqliteStore::in_memory(1).unwrap();
    legacy
        .register_body(&CatalogBody {
            body_id: "legacy".into(),
            category: "checkpoint".into(),
            path: "/managed/legacy.blob".into(),
            bytes: 10,
            registered_unix_seconds: NOW - 10,
            eligible_after_unix_seconds: None,
            pinned: false,
        })
        .unwrap();
    let err = legacy
        .catalogue_eligibility(&CategorySettings::new(), NOW)
        .unwrap_err();
    assert!(
        format!("{err}").contains("unknown retention category"),
        "{err}"
    );

    // An explicit setting for the same name is authoritative, so a
    // producer can still manage a non-default category deliberately.
    let explicit = BTreeMap::from([("CAS".to_string(), CategoryLimits::new(Some(100), Some(100)))]);
    assert!(
        !store
            .catalogue_eligibility(&explicit, NOW)
            .unwrap()
            .is_eligible("typo")
    );
}
