//! `run` driver integration: the single-flight lease refuses a second
//! driver, reclaims after a crash, `--once` emits one eight-phase tick
//! report that is also persisted durably, and `--work` scopes a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::ports::ProcessControlPort;
use workloop_exec::supervisor::PosixProcessControl;
use workloop_store::sqlite::{RunLeaseOwner, SqliteStore};

const NOW: i64 = 1_700_000_000;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_cli(root: &Path, args: &[&str]) -> std::process::Output {
    let db = root.join("workloop.db");
    let config = root.join("config.json");
    let mut full = vec![
        "--db".into(),
        db.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    Command::new(bin())
        .args(&full)
        .output()
        .expect("taisan runs")
}

fn store(root: &Path) -> SqliteStore {
    SqliteStore::open(&root.join("workloop.db"), 1).unwrap()
}

/// This test process as a lease owner — the CLI's real probe reads the
/// recorded identity and sees a live driver.
fn own_owner(nonce: &str) -> RunLeaseOwner {
    let identity =
        PosixProcessControl::identity_of(std::process::id() as i32).expect("own identity");
    RunLeaseOwner {
        pid: identity.pid,
        started_at: identity.started_at,
        nonce: nonce.into(),
    }
}

#[test]
fn run_once_emits_one_eight_phase_tick_report_and_persists_it() {
    let dir = root("report");
    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "run --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(report["schema_version"], "1");
    assert_eq!(report["kind"], "tick_report");
    assert_eq!(report["aborted"], false);
    assert_eq!(report["deadline_exceeded"], false);
    let phases = report["phases"].as_array().unwrap();
    let names: Vec<&str> = phases
        .iter()
        .map(|p| p["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "source_refresh",
            "dispatch_admission",
            "in_flight_supervision",
            "host_outbox",
            "timed_sweep",
            "recovery_scheduling",
            "retention_sweep",
            "attention_derivation",
        ],
        "the approved eight-phase order"
    );
    assert!(
        phases.iter().all(|p| p["status"] == "completed"),
        "{report}"
    );

    // The same pass is durable in the audit log.
    let store = store(&dir);
    assert_eq!(store.count_events("tick_report").unwrap(), 1);
}

#[test]
fn a_second_run_fails_closed_while_the_lease_is_held() {
    let dir = root("contention");
    // Create the store and config first.
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());

    let store = store(&dir);
    let held = own_owner("test-holder");
    store
        .acquire_run_lease(&held, |id| PosixProcessControl.inspect(id), NOW)
        .unwrap();

    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        !out.status.success(),
        "a second driver must not start while the lease is held"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("lock.held"),
        "expected lock.held, got: {stderr}"
    );

    // Release and the next driver proceeds — the refusal was the live
    // holder, not a wedged lock.
    store.release_run_lease(&held).unwrap();
    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "after release the driver proceeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_crashed_drivers_lease_is_reclaimed_on_restart() {
    let dir = root("restart");
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());

    // Simulate a driver that crashed without releasing: a lease row
    // whose recorded process is already gone.
    let mut child = Command::new("sleep").arg("0.05").spawn().unwrap();
    let pid = child.id() as i32;
    child.wait().unwrap();
    let store = store(&dir);
    store
        .acquire_run_lease(
            &RunLeaseOwner {
                pid,
                started_at: "crashed-driver".into(),
                nonce: "crashed".into(),
            },
            |id| PosixProcessControl.inspect(id),
            NOW,
        )
        .unwrap();

    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "a restart reclaims a provably gone driver's lease: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The reclaimed run released its own lease on exit.
    assert_eq!(
        store
            .acquire_run_lease(
                &own_owner("after-restart"),
                |id| { PosixProcessControl.inspect(id) },
                NOW
            )
            .unwrap(),
        workloop_store::sqlite::RunLeaseGrant::Acquired
    );
}

#[test]
fn a_scoped_run_admits_only_the_named_work() {
    let dir = root("scope");
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());
    // Two slots, so the scope — not capacity — decides which Work is
    // admitted: queue order alone would pick the first submission.
    let config_path = dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["capacity"] = serde_json::json!(2);
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    let w1 = submit(&dir, "scope-1");
    let w2 = submit(&dir, "scope-2");

    let out = run_cli(&dir, &["run", "--once", "--work", &w2]);
    assert!(
        out.status.success(),
        "scoped run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["scope"], w2.as_str());

    assert_eq!(state_of(&dir, &w2), "ready", "the named Work was admitted");
    assert_eq!(
        state_of(&dir, &w1),
        "queued",
        "the other Work was not touched even though queue order favored it"
    );
}

#[test]
fn commands_do_not_take_the_run_lock() {
    let dir = root("no-lock");
    let held = own_owner("commands-holder");
    // Hold the lease directly; ordinary Commands/Queries must still work.
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());
    let store = store(&dir);
    store
        .acquire_run_lease(&held, |id| PosixProcessControl.inspect(id), NOW)
        .unwrap();

    let out = run_cli(&dir, &["work-list"]);
    assert!(
        out.status.success(),
        "Queries never take the run lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run_cli(&dir, &["submit", "--intent", "while-locked"]);
    assert!(
        out.status.success(),
        "Commands never take the run lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    store.release_run_lease(&held).unwrap();
}

fn submit(dir: &Path, key: &str) -> String {
    let out = run_cli(dir, &["submit", "--intent", key, "--idempotency-key", key]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "accepted", "{v}");
    v["work_id"].as_str().unwrap().to_string()
}

fn state_of(dir: &Path, work_id: &str) -> String {
    let out = run_cli(dir, &["work-detail", "--work", work_id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["value"]["executions"][0]["state"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_bounded_multi_pass_run_reports_each_pass_and_stays_single_flight() {
    let dir = root("multi");
    // Two passes with the minimum interval: the lease spans the whole
    // run (no second driver appears between passes) and each pass
    // leaves its own durable tick report.
    let out = run_cli(&dir, &["run", "--ticks", "2", "--interval", "1"]);
    assert!(
        out.status.success(),
        "bounded run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut reports = 0;
    let stream = serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    for item in stream {
        let value = item.unwrap_or_else(|e| panic!("pass report {reports} is not JSON: {e}"));
        assert_eq!(value["kind"], "tick_report");
        assert_eq!(value["phases"].as_array().unwrap().len(), 8);
        assert_eq!(value["aborted"], false);
        reports += 1;
    }
    assert_eq!(reports, 2, "one report per pass: {stdout}");
    let store = store(&dir);
    assert_eq!(store.count_events("tick_report").unwrap(), 2);
}
