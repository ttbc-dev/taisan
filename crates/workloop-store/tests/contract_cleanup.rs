//! U2.6 contract tests: two-phase bounded cleanup (`retention cleanup`,
//! dry-run default, `--execute` executes) and `retention report`
//! pressure metrics. These are the named tests the U2.6 acceptance
//! criteria pin down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_store::retention::{
    CategoryLimits, CategorySettings, PinState, PlanBounds, RetainedBody,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn settings(max_age: Option<i64>, max_bytes: Option<u64>) -> CategorySettings {
    BTreeMap::from([("cas".to_string(), CategoryLimits::new(max_age, max_bytes))])
}

fn bounds() -> PlanBounds {
    PlanBounds {
        max_bodies: 256,
        max_bytes: 1 << 30,
    }
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let unique = format!(
            "taisan-cleanup-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write_body(&self, name: &str, bytes: usize) -> String {
        let rel = format!("bodies/{name}");
        let path = self.root.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0xab; bytes]).unwrap();
        rel
    }

    fn disk_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let mut total = 0;
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    total += walk(&entry.path());
                } else {
                    total += meta.len();
                }
            }
            total
        }
        walk(&self.root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn register(
    store: &SqliteStore,
    id: &str,
    created_at: i64,
    size_bytes: u64,
    pin: PinState,
    path: &str,
) {
    store
        .register_retained_body(&RetainedBody {
            body_id: id.into(),
            category: "cas".into(),
            size_bytes,
            created_at,
            pin_state: pin,
            path: path.into(),
        })
        .unwrap();
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: RepositoryRef::new(
                HostInstanceId::trusted("test-host"),
                RepositoryId::trusted("local-repo"),
            ),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: HostInstanceId::trusted("test-host"),
                    actor: HostActorId::trusted("pub-agent"),
                },
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: std::collections::BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::ChecksRead,
        ]),
        lanes: workloop_core::domain::LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: workloop_core::domain::AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: workloop_core::domain::ExecutionBudget::default(),
        policy: workloop_core::domain::WorkPolicy::default(),
    }
}

fn submit(key: &str) -> CommandRequest {
    CommandRequest {
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
    }
}

#[test]
fn dry_run_changes_nothing() {
    let fixture = Fixture::new("dry-run");
    let store = SqliteStore::in_memory(1).unwrap();
    let rel = fixture.write_body("body-a", 64);
    register(&store, "body-a", NOW - 10_000, 64, PinState::None, &rel);
    let before_bytes = fixture.disk_bytes();
    let before_present: u64 = store
        .catalogue_eligibility(&settings(Some(100), None), NOW)
        .unwrap()
        .categories
        .iter()
        .map(|c| c.present_bytes)
        .sum();

    let preview = store
        .reclaim_plan_candidates(
            &settings(Some(100), None),
            &bounds(),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    assert_eq!(preview.status, "preview");
    assert_eq!(preview.entries.len(), 1);

    assert_eq!(
        fixture.disk_bytes(),
        before_bytes,
        "dry-run must not delete bytes"
    );
    let after_present: u64 = store
        .catalogue_eligibility(&settings(Some(100), None), NOW)
        .unwrap()
        .categories
        .iter()
        .map(|c| c.present_bytes)
        .sum();
    assert_eq!(
        after_present, before_present,
        "dry-run must not change the catalogue"
    );
    assert!(
        store.reclaim_plan_open().unwrap().is_none(),
        "dry-run must not persist a plan"
    );
    assert_eq!(store.body_state("body-a").unwrap().unwrap().0, "present");
}

#[test]
fn plan_is_bounded_oldest_first() {
    let fixture = Fixture::new("bounded");
    let store = SqliteStore::in_memory(1).unwrap();
    for (i, name) in ["b1", "b2", "b3", "b4"].iter().enumerate() {
        let rel = fixture.write_body(name, 10);
        register(
            &store,
            name,
            NOW - 10_000 + i as i64,
            10,
            PinState::None,
            &rel,
        );
    }
    let plan = store
        .reclaim_plan_create(
            &settings(Some(100), None),
            &PlanBounds {
                max_bodies: 2,
                max_bytes: 1 << 30,
            },
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    let ids: Vec<&str> = plan.entries.iter().map(|e| e.body_id.as_str()).collect();
    assert_eq!(ids, ["b1", "b2"], "plan keeps the two oldest, in order");
    assert_eq!(plan.total_bytes, 20);
}

#[test]
fn execute_reclaims_planned_bodies_and_records_plan_and_execution() {
    let fixture = Fixture::new("execute");
    let store = SqliteStore::in_memory(1).unwrap();
    let rel_a = fixture.write_body("body-a", 32);
    let rel_b = fixture.write_body("body-b", 48);
    register(&store, "body-a", NOW - 10_000, 32, PinState::None, &rel_a);
    register(&store, "body-b", NOW - 9_000, 48, PinState::None, &rel_b);

    let plan = store
        .reclaim_plan_create(
            &settings(Some(100), None),
            &bounds(),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    let plan_id = plan.plan_id.clone();
    let execution = store
        .reclaim_plan_execute(&fixture.root, &[], 3, NOW)
        .unwrap()
        .expect("an open plan exists");

    assert_eq!(execution.reclaimed.len(), 2);
    assert_eq!(execution.bytes_reclaimed, 80);
    assert!(execution.completed);
    assert!(!fixture.root.join("bodies/body-a").exists());
    assert!(!fixture.root.join("bodies/body-b").exists());

    let recorded = store.reclaim_plan(&plan_id).unwrap().unwrap();
    assert_eq!(recorded.status, "completed");
    assert_eq!(recorded.entries.len(), 2);
    for entry in &recorded.entries {
        assert_eq!(entry.outcome.as_deref(), Some("reclaimed"));
        assert!(entry.outcome_reason.is_some());
    }
    for id in ["body-a", "body-b"] {
        let (state, reason, _) = store.body_state(id).unwrap().unwrap();
        assert_eq!(state, "reclaimed");
        assert_eq!(reason.as_deref(), Some("age_expired"));
    }
}

#[test]
fn interrupted_phase_two_resumes_and_skips_reclaimed_bodies() {
    let fixture = Fixture::new("resume");
    let store = SqliteStore::in_memory(1).unwrap();
    for (i, name) in ["body-a", "body-b", "body-c"].iter().enumerate() {
        let rel = fixture.write_body(name, 16);
        register(
            &store,
            name,
            NOW - 10_000 + i as i64,
            16,
            PinState::None,
            &rel,
        );
    }
    let plan = store
        .reclaim_plan_create(
            &settings(Some(100), None),
            &bounds(),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    let plan_id = plan.plan_id.clone();

    // Simulate a process that dies after reclaiming exactly one body:
    // phase 2 for `body-a` completes, the process stops before the rest.
    let first = store
        .reclaim_plan_entry(&plan_id, "body-a", &fixture.root, &[], 3, NOW)
        .unwrap();
    assert_eq!(first.reclaimed.len(), 1);
    assert_eq!(store.body_state("body-a").unwrap().unwrap().0, "reclaimed");
    assert!(
        store
            .reclaim_plan(&plan_id)
            .unwrap()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.body_id == "body-b" && e.outcome.is_none())
    );

    // The rerun skips the already-reclaimed body and completes the plan.
    let execution = store
        .reclaim_plan_execute(&fixture.root, &[], 3, NOW)
        .unwrap()
        .unwrap();
    assert_eq!(
        execution.already_recorded, 1,
        "the reclaimed body is skipped, not re-processed"
    );
    assert_eq!(execution.reclaimed.len(), 2);
    assert!(execution.completed);
    assert!(execution.failed.is_empty());
    let recorded = store.reclaim_plan(&plan_id).unwrap().unwrap();
    assert_eq!(recorded.status, "completed");
    assert!(recorded.entries.iter().all(|e| e.outcome.is_some()));
}

#[test]
fn missing_and_already_reclaimed_bodies_are_unavailable_not_errors() {
    let fixture = Fixture::new("unavailable");
    let store = SqliteStore::in_memory(1).unwrap();
    let rel_a = fixture.write_body("body-a", 16);
    let rel_b = fixture.write_body("body-b", 16);
    register(&store, "body-a", NOW - 10_000, 16, PinState::None, &rel_a);
    register(&store, "body-b", NOW - 9_000, 16, PinState::None, &rel_b);

    let plan = store
        .reclaim_plan_create(
            &settings(Some(100), None),
            &bounds(),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    let plan_id = plan.plan_id.clone();

    // (a) body-a is reclaimed by another pass after this plan was
    // computed but before it executes.
    store
        .reclaim_catalogue_body(
            "body-a",
            &settings(Some(100), None),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();

    // (b) body-b's file vanishes after planning, before execution.
    std::fs::remove_file(fixture.root.join("bodies/body-b")).unwrap();

    let execution = store
        .reclaim_plan_execute(&fixture.root, &[], 3, NOW)
        .unwrap()
        .unwrap();

    // (a) already-reclaimed: reported unavailable, not an error.
    assert!(
        execution
            .unavailable
            .iter()
            .any(|h| h.body_id == "body-a" && h.reason == "already_reclaimed"),
        "already-reclaimed body must be reported unavailable: {execution:?}"
    );
    // (b) file absent at delete time: recorded missing, not an error.
    assert!(
        execution
            .reclaimed
            .iter()
            .any(|r| r.body_id == "body-b" && r.reason == "missing"),
        "absent file must be recorded as missing: {execution:?}"
    );
    assert!(execution.failed.is_empty());
    assert!(execution.completed);
    assert_eq!(store.body_state("body-a").unwrap().unwrap().0, "reclaimed");
    assert_eq!(store.body_state("body-b").unwrap().unwrap().0, "reclaimed");
    let _ = plan_id;
}

#[test]
fn protected_over_limit_body_is_retained_and_admission_proceeds() {
    let fixture = Fixture::new("protected");
    let store = SqliteStore::in_memory(1).unwrap();
    // A pinned body alone exceeds the 15-byte cap.
    let rel = fixture.write_body("pinned", 20);
    register(&store, "pinned", NOW - 10_000, 20, PinState::Active, &rel);

    let report = store
        .retention_report(&settings(Some(100), Some(15)), &fixture.root, NOW)
        .unwrap();
    let cas = report
        .categories
        .iter()
        .find(|c| c.category == "cas")
        .unwrap();
    assert!(
        cas.protected_over_limit,
        "pinned bytes alone exceed the cap"
    );
    assert_eq!(cas.pressure, "protected_over_limit");
    assert_eq!(cas.pinned_bytes, 20);

    // The plan refuses the pinned body; nothing is reclaimed.
    let plan = store
        .reclaim_plan_create(
            &settings(Some(100), Some(15)),
            &bounds(),
            &fixture.root,
            &[],
            NOW,
        )
        .unwrap();
    assert!(plan.entries.is_empty(), "pinned bodies are never planned");
    let execution = store
        .reclaim_plan_execute(&fixture.root, &[], 3, NOW)
        .unwrap()
        .unwrap();
    assert!(execution.reclaimed.is_empty());
    assert!(fixture.root.join("bodies/pinned").exists());
    assert_eq!(store.body_state("pinned").unwrap().unwrap().0, "present");

    // Pressure does not block admission.
    let decision =
        handle_command(&store, &config(), &submit("admission-under-pressure"), NOW).unwrap();
    match decision {
        CommandDecision::Accepted { .. } => {}
        other => panic!("admission must proceed under pressure, got {other:?}"),
    }
}

/// Regression (shadow review): a store stamped with the format this
/// slice bumps from (3), created before this slice — full base schema,
/// no reclaim-plan tables — must open and upgrade in place. Refusing
/// it as "not the recognized schema" would lock existing installations
/// out of their own stores.
#[test]
fn a_previous_format_store_missing_reclaim_tables_is_upgraded_in_place() {
    let unique = format!(
        "taisan-cleanup-upgrade-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("store.db");
    {
        let store = SqliteStore::open(&db, 1).unwrap();
        let d = handle_command(&store, &config(), &submit("upgrade-prev"), NOW).unwrap();
        assert!(matches!(d, CommandDecision::Accepted { .. }));
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE reclaim_plan_entries; DROP TABLE reclaim_plans; PRAGMA user_version=3;",
        )
        .unwrap();
    }
    let store = SqliteStore::open(&db, 1).expect("a previous-format store opens");
    assert!(
        store.reclaim_plan_open().unwrap().is_none(),
        "the reclaim tables are installed and usable"
    );
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4, "the upgrade lands on the current format");
    }
    drop(store);
    let again = SqliteStore::open(&db, 1).expect("reopen after upgrade is a no-op");
    assert!(again.reclaim_plan_open().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// Accounting fixture bodies whose path does not exist on disk must
/// not be counted as present usage: the report lists them under
/// `unknown` and excludes them from the pressure computation.
#[test]
fn report_counts_unstatable_bodies_as_unknown_not_present() {
    let fixture = Fixture::new("unknown");
    let store = SqliteStore::in_memory(1).unwrap();
    let rel_real = fixture.write_body("real", 10);
    register(&store, "real", NOW - 10, 10, PinState::None, &rel_real);
    register(
        &store,
        "ghost",
        NOW - 20,
        20,
        PinState::None,
        "bodies/ghost-never-written.blob",
    );

    let report = store
        .retention_report(&settings(Some(100), Some(15)), &fixture.root, NOW)
        .unwrap();
    let cas = report
        .categories
        .iter()
        .find(|c| c.category == "cas")
        .unwrap();
    assert_eq!(cas.unknown_count, 1);
    assert_eq!(cas.unknown_bytes, 20);
    assert_eq!(cas.present_bytes, 10, "ghost bytes are not present usage");
    assert_eq!(cas.present_count, 1);
    assert!(
        !cas.over_max_bytes,
        "verified usage (10 B) is under the 15 B cap even though the catalogue holds 30 B"
    );
    assert!(!report.unknown_scan_truncated);
}

#[test]
fn report_exposes_pressure_metrics_and_unavailable_list() {
    let fixture = Fixture::new("report");
    let store = SqliteStore::in_memory(1).unwrap();
    let rel_pinned = fixture.write_body("pinned", 20);
    let rel_old = fixture.write_body("old", 10);
    register(
        &store,
        "pinned",
        NOW - 50,
        20,
        PinState::Active,
        &rel_pinned,
    );
    register(&store, "old", NOW - 10_000, 10, PinState::None, &rel_old);
    // Reclaim the eligible body so it appears as unavailable evidence.
    store
        .reclaim_catalogue_body("old", &settings(Some(100), None), &fixture.root, &[], NOW)
        .unwrap();

    let report = store
        .retention_report(&settings(Some(100), Some(15)), &fixture.root, NOW)
        .unwrap();
    let cas = report
        .categories
        .iter()
        .find(|c| c.category == "cas")
        .unwrap();
    assert_eq!(cas.max_bytes, Some(15));
    assert_eq!(cas.max_age_seconds, Some(100));
    assert_eq!(cas.present_bytes, 20);
    assert_eq!(cas.pinned_bytes, 20);
    assert_eq!(cas.reclaimed_count, 1);
    assert_eq!(cas.oldest_present_age_seconds, Some(50));
    assert_eq!(cas.pressure, "protected_over_limit");
    assert_eq!(cas.reclaimed_by_reason.get("age_expired"), Some(&1));

    let unavailable = report
        .unavailable
        .iter()
        .find(|u| u.body_id == "old")
        .expect("reclaimed body must be listed as unavailable evidence");
    assert_eq!(unavailable.reclaim_reason.as_deref(), Some("age_expired"));
    assert_eq!(unavailable.size_bytes, 10);
    assert!(!report.unavailable_truncated);
}
