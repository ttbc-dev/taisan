//! `run` driver integration: the single-flight lease refuses a second
//! driver, reclaims after a crash, `--once` emits the terminal envelope
//! while the eight-phase tick report is persisted durably, and `--work`
//! scopes a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::ports::ProcessControlPort;
use workloop_core::store::*;
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

fn primitive_on_path(name: &str) -> bool {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin", "/usr/sbin", "/sbin"] {
        if Path::new(dir).join(name).is_file() {
            return true;
        }
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
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
    // The run's stdout is its terminal envelope; the tick report
    // itself is durable in the audit log.
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");

    let store = store(&dir);
    assert_eq!(store.count_events("tick_report").unwrap(), 1);
    let report: serde_json::Value =
        serde_json::from_str(&store.events("tick_report").unwrap()[0]).unwrap();
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
}

#[test]
fn the_retention_boundary_is_the_bodies_root_not_the_callers_cwd() {
    let state = root("retention-state");
    let elsewhere = root("retention-cwd");
    let out = run_cli(&state, &["work-list"]);
    assert!(out.status.success());

    let store = store(&state);
    // Every retain catalogues a `bodies/…` path under the state root —
    // that root is the containment boundary, not the caller's cwd.
    let state_body = state.join("bodies").join("case-1.blob");
    std::fs::create_dir_all(state_body.parent().unwrap()).unwrap();
    std::fs::write(&state_body, b"body").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "state-body".into(),
            category: "cas".into(),
            path: "bodies/case-1.blob".into(),
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
        "a body under <state_root>/bodies is reclaimed regardless of the caller's cwd"
    );
    assert!(
        foreign_body.exists(),
        "a body under the caller's cwd is outside the bodies root"
    );
    let report: serde_json::Value =
        serde_json::from_str(&store.events("tick_report").unwrap()[0]).unwrap();
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
    let report: serde_json::Value =
        serde_json::from_str(&store(&dir).events("tick_report").unwrap()[0]).unwrap();
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
    v["result"]["executions"][0]["state"]
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
    // stdout carries the run's one terminal envelope; each pass's
    // report is durable in the audit log instead.
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one envelope");
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");
    let store = store(&dir);
    let reports = store.events("tick_report").unwrap();
    assert_eq!(reports.len(), 2, "one durable report per pass");
    for payload in &reports {
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(value["kind"], "tick_report");
        assert_eq!(value["phases"].as_array().unwrap().len(), 8);
        assert_eq!(value["aborted"], false);
    }
    assert_eq!(store.count_events("tick_report").unwrap(), 2);
}

#[test]
fn the_inter_pass_gap_is_the_interval_in_seconds() {
    let dir = root("interval");
    // `--interval` documents seconds (`loop.interval_seconds` is the
    // same unit): with `--ticks 2` the one gap between the two passes
    // must sleep the whole second — sleeping it as milliseconds would
    // finish the run in a few milliseconds instead.
    let started = std::time::Instant::now();
    let out = run_cli(&dir, &["run", "--ticks", "2", "--interval", "1"]);
    let elapsed = started.elapsed();
    assert!(
        out.status.success(),
        "bounded run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one envelope");
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "--interval 1 must space the two passes one second apart, \
         the run took {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_sweep_never_reclaims_the_store_or_the_config() {
    let dir = root("protected");
    let out = run_cli(&dir, &["work-list"]);
    assert!(out.status.success());

    let store = store(&dir);
    // A catalog row pointing at each of the installation's own files:
    // the live store, its WAL sidecar and the config. They resolve
    // inside the state root, so the protected-paths list — not mere
    // containment — is what must hold them against the sweep.
    for (body_id, path) in [
        ("store-file", dir.join("workloop.db")),
        ("store-wal", dir.join("workloop.db-wal")),
        ("config-file", dir.join("config.json")),
    ] {
        store
            .register_body(&CatalogBody {
                body_id: body_id.into(),
                category: "cas".into(),
                path: path.to_string_lossy().into_owned(),
                bytes: 4,
                registered_unix_seconds: NOW - 100,
                eligible_after_unix_seconds: Some(NOW - 10),
                pinned: false,
            })
            .unwrap();
    }
    // A managed body under the bodies root is still reclaimed: the
    // exclusion is the installation's own files, not every catalogued
    // path.
    let body = dir.join("bodies").join("case-1.blob");
    std::fs::create_dir_all(body.parent().unwrap()).unwrap();
    std::fs::write(&body, b"body").unwrap();
    store
        .register_body(&CatalogBody {
            body_id: "reclaimable".into(),
            category: "cas".into(),
            path: "bodies/case-1.blob".into(),
            bytes: 4,
            registered_unix_seconds: NOW - 90,
            eligible_after_unix_seconds: Some(NOW - 10),
            pinned: false,
        })
        .unwrap();

    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "run --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&store.events("tick_report").unwrap()[0]).unwrap();
    let retention = &report["phases"][6];
    assert_eq!(retention["phase"], "retention_sweep");
    let held: Vec<&str> = retention["held"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for id in ["store-file", "store-wal", "config-file"] {
        let entry = format!("{id}:protected_path");
        assert!(
            held.contains(&entry.as_str()),
            "expected {entry} held, got {held:?}"
        );
    }

    assert!(
        dir.join("workloop.db").exists(),
        "the live store survives the sweep"
    );
    assert!(
        dir.join("config.json").exists(),
        "the config survives the sweep"
    );
    for id in ["store-file", "store-wal", "config-file"] {
        assert_eq!(
            store.body_state(id).unwrap().unwrap().0,
            "present",
            "{id} is held, not reclaimed"
        );
    }
    assert_eq!(
        store.body_state("reclaimable").unwrap().unwrap().0,
        "reclaimed",
        "an ordinary body beside them is still reclaimed"
    );
    assert!(!body.exists(), "the ordinary body is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sigint_finishes_the_phase_boundary_and_exits_gracefully() {
    use std::io::{BufRead, BufReader};
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
    let stderr = child.stderr.take().expect("piped stderr");

    // Consume the first pass's stderr diagnostic — stdout stays silent
    // until the run ends, so per-pass progress is the only live signal.
    // The signal then arrives while the driver is between passes.
    let mut stderr_lines = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        let n = stderr_lines.read_line(&mut line).expect("stderr readable");
        assert!(n > 0, "driver exited before its first pass: {line}");
        if line.contains("pass 1") {
            break;
        }
    }

    // Without the handler the default disposition kills the driver
    // mid-run; with it the process exits at the phase boundary with 0.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().expect("the driver exits");
    assert!(
        status.success(),
        "SIGINT finishes the phase boundary and exits 0: {status}"
    );
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().expect("piped stdout"), &mut stdout)
        .unwrap();
    let env: serde_json::Value = serde_json::from_str(&stdout).expect("one terminal envelope");
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");

    // The graceful exit released the run lease: the next driver runs.
    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "after shutdown the next driver proceeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pending outbox intent the drain can never attempt — its Work has
/// no Execution — must be visible in the phase-4 report's held list
/// instead of silently stalling every pass.
#[test]
fn the_outbox_phase_surfaces_unattemptable_intents_as_held() {
    let dir = root("outbox-held");
    // The default installation denies `host_write`, which stops the
    // drain before any item — grant it so the intent reaches attempt.
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "actor_id": "op",
            "actor_kind": "human",
            "host_instance": "host-local",
            "repository": "local-repo",
            "adapter": "fixture",
            "contract_version": 1,
            "capabilities": ["push_ref"],
            "host_actors": { "publication": "fixture-agent" },
            "host_credentials": {},
            "required_checks": [],
            "capacity": 1,
            "worker_argv": [],
            "workspace_root": dir.join("workspaces"),
            "allowed_effects": {
                "workspace_write": true,
                "host_write": true,
                "network": false
            }
        }))
        .unwrap(),
    )
    .unwrap();
    {
        let store = store(&dir);
        store
            .transact(|tx| {
                let repository = RepositoryRef::new(
                    HostInstanceId::trusted("github.test"),
                    RepositoryId::trusted("owner/repo"),
                );
                let action_id = tx.alloc_id("act")?;
                let created_seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertHostActionIntent {
                    intent: HostActionIntent {
                        action_id,
                        work_id: WorkId("w-ghost".into()),
                        host: HostInstanceId::trusted("github.test"),
                        repository: repository.clone(),
                        role: ActorRole::Publication,
                        actor: HostActorRef {
                            host: HostInstanceId::trusted("github.test"),
                            actor: HostActorId::trusted("actor-1"),
                        },
                        action: HostAction::PushRef {
                            branch: "work/w-ghost".into(),
                            produced: CommitRevision::new(repository.clone(), "deadbeef").unwrap(),
                            expected_prior: None,
                        },
                        payload_digest: "test".into(),
                        status: HostActionStatus::Pending,
                        authenticated_actor: None,
                        effects: vec![],
                        created_seq,
                    },
                })?;
                Ok(())
            })
            .unwrap();
    }
    let out = run_cli(&dir, &["run", "--once"]);
    assert!(
        out.status.success(),
        "run --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let store = store(&dir);
    let report: serde_json::Value =
        serde_json::from_str(&store.events("tick_report").unwrap()[0]).unwrap();
    let outbox = &report["phases"][3];
    assert_eq!(outbox["phase"], "host_outbox");
    let held = outbox["held"].as_array().unwrap();
    assert!(
        held.iter()
            .any(|h| h.as_str().unwrap().ends_with(":missing_execution")),
        "the unroutable intent names its reason: {outbox}"
    );
    assert!(
        outbox["reason"].as_str().unwrap().contains("skipped=1"),
        "{outbox}"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The complete v2 wire through the normal driver: a `worker_dispatch`
/// installation section composes the bound-request transport, so a
/// plain `submit` + bounded `run` admits the Work, probes `describe`,
/// records the adapter binding, materializes the immutable
/// `taisan.worker.request/2` body, launches `run --request/--out`,
/// collects and classifies the report as validated evidence, and
/// drives the produced commit through durable handoff, declared
/// verification, and the agreeing-claim verdict — every step a durable
/// fact inside the eight-phase tick, never a fixture shortcut. The
/// launch runs under the approved reference profile, which declares
/// the `bwrap` primitive: on a host without it the contract refuses
/// dispatch (OPS-01), so the evidence is gathered where the primitive
/// exists — CI installs it.
#[test]
fn a_v2_worker_runs_the_complete_wire_through_the_driver() {
    if !primitive_on_path("bwrap") {
        eprintln!("skipping: bwrap is absent on this host; the launch gate would refuse (OPS-01)");
        return;
    }
    let dir = root("v2-driver");
    // The supervisor-side source repository the Coding request pins at
    // consume time — a real checkout the workspace is built from.
    let src = dir.join("source-repo");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-b", "main"]);
    git(&src, &["config", "user.email", "t@t"]);
    git(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("seed.txt"), "base").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "base"]);
    let source_path = src.canonicalize().unwrap();

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .canonicalize()
        .expect("build the fixture worker first: cargo build --workspace");
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "actor_id": "op",
            "actor_kind": "human",
            "host_instance": "host-local",
            "repository": "local-repo",
            "adapter": "fixture",
            "contract_version": 1,
            "capabilities": [
                "repository_read",
                "push_ref",
                "change_request_create",
                "change_request_read",
                "change_request_update",
                "comment_write",
                "checks_read",
                "review_findings_read",
                "issue_read"
            ],
            "host_actors": { "publication": "fixture-agent" },
            "host_credentials": {},
            "required_checks": [],
            "capacity": 1,
            "worker_argv": [
                fixture.to_string_lossy(),
                "clean-exit",
                "--wire",
                "v2",
                "--git-commit",
                "--claim-head",
                "HEAD"
            ],
            "worker_isolation_profile": workloop_core::worker::DEFAULT_ISOLATION_PROFILE_ID,
            "worker_dispatch": {
                "source_path": source_path.to_string_lossy(),
                "budget": {
                    "max_turns": 8,
                    "max_report_bytes": 1048576,
                    "max_log_bytes": 33554432,
                    "max_verification_commands": 4,
                    "max_continuation_refs": 4
                },
                "invariant_set": {
                    "id": "invariants-1",
                    "revision": 1,
                    "items": ["no-todo", "tests-pass"]
                },
                "capabilities": ["artifact_json", "process_control"]
            },
            "workspace_root": dir.join("workspaces"),
            "allowed_effects": {
                "workspace_write": true,
                "host_write": false,
                "network": false
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // Submit declares one verification command; the produced commit
    // must pass it before the handoff acknowledges.
    let out = run_cli(
        &dir,
        &[
            "submit",
            "--intent",
            "v2-driver",
            "--idempotency-key",
            "v2-driver",
            "--verification",
            "true",
        ],
    );
    assert!(
        out.status.success(),
        "submit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let submitted: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(submitted["status"], "accepted", "{submitted}");
    let work_id = submitted["work_id"].as_str().unwrap().to_string();

    // Bounded passes: admission + declaration probe + binding on the
    // first, consume + launch the same pass, report collection and
    // handoff on the next.
    let out = run_cli(&dir, &["run", "--ticks", "5", "--interval", "1"]);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let store = store(&dir);
    let reports = store.events("tick_report").unwrap();
    assert_eq!(reports.len(), 5, "one durable report per pass");
    for payload in &reports {
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(value["aborted"], false, "{value}");
        assert!(
            value["phases"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| { p["status"] == "completed" || p["status"] == "skipped" }),
            "every phase completed or cadence-skipped: {value}"
        );
    }

    let executions = store
        .transact(|tx| tx.executions_for_work(&WorkId(work_id.clone())))
        .unwrap();
    assert_eq!(executions.len(), 1, "one Execution admitted");
    let execution = &executions[0];
    // The wire boundary completed: the Coding writer and the
    // pre-publication InvariantCheck both resolved Done. The
    // Execution itself waits in `reconciliation` — no
    // CapabilityReport reaches this driver, so the capability gate
    // records no publish intent and publication holds by design.
    assert_eq!(
        execution.state,
        ExecutionState::Waiting,
        "{:?}",
        execution.state
    );
    assert_eq!(
        execution.waiting_reason,
        Some(WaitingReason::Reconciliation),
        "{:?}",
        execution.waiting_reason
    );

    let invocations = store
        .transact(|tx| tx.invocations_for_execution(&execution.id))
        .unwrap();
    assert_eq!(
        invocations.len(),
        2,
        "the Coding writer and the pre-publication check both launched: {invocations:?}"
    );
    assert!(
        invocations
            .iter()
            .all(|i| i.state == InvocationState::Done && i.process_identity.is_some()),
        "every launched Invocation resolved Done with a recorded process identity"
    );
    let invocation = invocations
        .iter()
        .find(|i| i.role == InvocationRole::Coding)
        .expect("the Coding invocation");

    // The durable v2 request body verifies and binds this exact
    // request — the launch record the worker consumed.
    let request_id = invocation.request_id.clone();
    let body = store
        .transact(|tx| tx.worker_request_body(&request_id))
        .unwrap()
        .expect("the v2 request body is retained");
    assert!(body.verifies(), "the stored body verifies");
    let wire = workloop_core::worker_v2::WorkerRequestV2::parse(&body.canonical_body)
        .expect("the stored body parses as taisan.worker.request/2");
    assert_eq!(wire.request_id, request_id);
    assert_eq!(wire.work_id, work_id);
    assert_eq!(wire.execution_id, execution.id.0);
    assert_eq!(wire.role, InvocationRole::Coding);

    // The adapter binding the `describe` probe recorded — configuration
    // provenance, never worker-authored.
    let request = store
        .transact(|tx| tx.request(&request_id))
        .unwrap()
        .expect("request row");
    let binding = request
        .adapter_binding
        .expect("the declaration probe recorded a binding");
    assert_eq!(binding.adapter.as_str(), "workloop-fixture-worker");

    // Every launched Invocation's report classified as validated
    // evidence — the Coding writer's bound to its own request id.
    let facts = store
        .transact(|tx| tx.observations_for_execution(&execution.id))
        .unwrap();
    let reports_facts: Vec<&ObservationFact> = facts
        .iter()
        .map(|o| &o.fact)
        .filter(|f| matches!(f, ObservationFact::WorkerReport { .. }))
        .collect();
    assert_eq!(
        reports_facts.len(),
        2,
        "one collected report per launched Invocation"
    );
    for fact in &reports_facts {
        let ObservationFact::WorkerReport { classification, .. } = *fact else {
            unreachable!()
        };
        assert_eq!(
            classification,
            &workloop_core::worker_v2::ReportClassification::Success
        );
    }
    let coding_report = reports_facts
        .iter()
        .find(|f| {
            matches!(f, ObservationFact::WorkerReport { request_id: r, .. } if *r == request_id)
        })
        .expect("the Coding report fact");
    let ObservationFact::WorkerReport { classification, .. } = coding_report else {
        unreachable!()
    };
    assert_eq!(
        *classification,
        workloop_core::worker_v2::ReportClassification::Success
    );

    // The produced commit handed off: acknowledged against the bare
    // repository, verified by the declared command on the exact
    // produced commit, and the agreeing claim recorded as evidence.
    let handoff = store
        .transact(|tx| tx.artifact_handoff(&format!("{}/artifact.json", invocation.id.0)))
        .unwrap()
        .expect("handoff row recorded");
    assert_eq!(handoff.state, HandoffState::Acknowledged, "{handoff:?}");
    assert!(handoff.produced.is_some(), "produced commit recorded");
    let results = store
        .transact(|tx| tx.verification_results(&handoff.artifact_id))
        .unwrap();
    assert_eq!(results.len(), 1, "one declared verification ran");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    let claim_facts: Vec<&ObservationFact> = facts
        .iter()
        .map(|o| &o.fact)
        .filter(|f| matches!(f, ObservationFact::ClaimedHeadCheck { .. }))
        .collect();
    assert_eq!(claim_facts.len(), 1, "one claim verdict recorded");
    let ObservationFact::ClaimedHeadCheck { agrees, .. } = claim_facts[0] else {
        unreachable!()
    };
    assert!(
        agrees,
        "the worker's claimed head agreed with the promoted commit"
    );
}
