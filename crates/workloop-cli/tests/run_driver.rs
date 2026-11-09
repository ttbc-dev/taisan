//! `run` driver integration: the single-flight lease refuses a second
//! driver, reclaims after a crash, `--once` emits one eight-phase tick
//! report that is also persisted durably, and `--work` scopes a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::ports::ProcessControlPort;
use workloop_exec::supervisor::PosixProcessControl;
use workloop_store::retention::CatalogBody;
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
    run_cli_from(root, None, args)
}

fn run_cli_from(root: &Path, cwd: Option<&Path>, args: &[&str]) -> std::process::Output {
    let db = root.join("workloop.db");
    let config = root.join("config.json");
    let mut full = vec![
        "--db".into(),
        db.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    let mut command = Command::new(bin());
    command.args(&full);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.output().expect("taisan runs")
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
fn the_retention_boundary_is_the_state_root_not_the_callers_cwd() {
    let state = root("retention-state");
    let elsewhere = root("retention-cwd");
    let out = run_cli(&state, &["work-list"]);
    assert!(out.status.success());

    let store = store(&state);
    let state_body = state.join("workspaces").join("case-1.blob");
    std::fs::create_dir_all(state_body.parent().unwrap()).unwrap();
    std::fs::write(&state_body, b"body").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "state-body".into(),
            category: "cas".into(),
            path: state_body.to_string_lossy().into_owned(),
            bytes: 4,
            registered_unix_seconds: NOW - 100,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let foreign_body = elsewhere.join("workspaces").join("case-1.blob");
    std::fs::create_dir_all(foreign_body.parent().unwrap()).unwrap();
    std::fs::write(&foreign_body, b"body").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "foreign-body".into(),
            category: "cas".into(),
            path: foreign_body.to_string_lossy().into_owned(),
            bytes: 4,
            registered_unix_seconds: NOW - 90,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let out = run_cli_from(&state, Some(&elsewhere), &["run", "--once"]);
    assert!(
        out.status.success(),
        "run --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !state_body.exists(),
        "a body under the installation state root is reclaimed regardless of the caller's cwd"
    );
    assert!(
        foreign_body.exists(),
        "a body under the caller's cwd is outside the installation state root"
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let retention = &report["phases"][6];
    assert_eq!(retention["phase"], "retention_sweep");
    assert_eq!(retention["held"][0], "foreign-body:outside_root");

    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&elsewhere);
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

#[test]
fn a_sigint_finishes_the_phase_boundary_and_exits_gracefully() {
    use std::io::BufReader;
    use std::process::Stdio;

    let dir = root("sigint");
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());

    let db = dir.join("workloop.db");
    let config = dir.join("config.json");
    let args = vec![
        "--db".to_string(),
        db.to_string_lossy().into_owned(),
        "--config".to_string(),
        config.to_string_lossy().into_owned(),
        "run".into(),
        "--interval".into(),
        "30".into(),
    ];
    let mut child = Command::new(bin())
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("taisan run starts");
    let stdout = child.stdout.take().expect("piped stdout");

    // Consume the first tick report as soon as the pass that produced
    // it has completed — the signal then arrives while the driver is
    // between passes.
    let mut reports = serde_json::Deserializer::from_reader(BufReader::new(stdout))
        .into_iter::<serde_json::Value>();
    let first = reports
        .next()
        .expect("the first pass reports")
        .expect("one JSON object");
    assert_eq!(first["kind"], "tick_report");
    assert_eq!(first["phases"].as_array().unwrap().len(), 8);

    // Without the handler the default disposition kills the driver
    // mid-run; with it the process exits at the phase boundary with 0.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    for item in reports {
        item.expect("later reports stay JSON");
    }
    let status = child.wait().expect("the driver exits");
    assert!(
        status.success(),
        "SIGINT finishes the phase boundary and exits 0: {status}"
    );

    // The graceful exit released the run lease: the next driver runs.
    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "after shutdown the next driver proceeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
