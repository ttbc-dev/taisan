//! U2.4 CLI surface: `taisan init` scaffolds the profile from observed
//! evidence, `taisan check` is the ordered offline preflight, and
//! `check --probe` is opt-in and fails closed when no probe backend is
//! wired — one JSON envelope per result, exit codes per the CLI contract.

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

struct Install {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

fn install(tag: &str) -> Install {
    let root = std::env::temp_dir().join(format!("taisan-setup-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");
    let state = root.join("state");
    Install {
        root,
        config,
        state,
    }
}

fn command(install: &Install, args: &[&str]) -> Command {
    let mut command = Command::new(bin());
    command
        .args(args)
        .env("TAISAN_CONFIG", &install.config)
        .env("TAISAN_STATE_DIR", &install.state)
        .env("TAISAN_TEST_TOKEN", "seeded-secret-value");
    command
}

fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

fn init_args(install: &Install) -> Vec<String> {
    let _ = install;
    vec![
        "init".into(),
        "--endpoint".into(),
        "https://***.github.com".into(),
        "--repo".into(),
        "0123456789:publication".into(),
        "--role".into(),
        "publication:taisan-publisher:env:TAISAN_TEST_TOKEN".into(),
        "--observe".into(),
        "publication:taisan-publisher@https://***.github.com/0123456789".into(),
        "--egress".into(),
        "crates.io".into(),
    ]
}

fn run(install: &Install, args: &[String]) -> Output {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    command(install, &refs).output().expect("taisan runs")
}

/// `init` then a plain `check`: both succeed, the config carries the
/// allowlist, credential ref, and identities, and the check report is
/// the per-run structured result with stage 8 `not_run`.
#[test]
fn init_then_plain_check_round_trip() {
    let install = install("roundtrip");
    let output = run(&install, &init_args(&install));
    assert!(
        output.status.success(),
        "init: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["schema_version"], "1");
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["result"]["kind"], "init");
    assert!(install.config.exists(), "init published the profile");

    let output = command(&install, &["check"]).output().expect("taisan runs");
    assert!(
        output.status.success(),
        "check: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["result"]["kind"], "check");
    assert_eq!(env["result"]["success"], true, "{env}");
    // Setup commands are not replayable mutations: no actor identity is
    // fabricated for them on the envelope.
    assert!(env["command"].is_null());
    let stages = env["result"]["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 8, "all eight stages are reported");
    for stage in stages.iter().take(7) {
        assert_eq!(stage["status"], "pass", "{stage}");
    }
    assert_eq!(stages[7]["stage"], "probe_capabilities");
    assert_eq!(stages[7]["status"], "not_run", "a plain check never probes");
    assert!(
        env["result"]["probe_operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(env["result"]["remote_writes"], 0);

    let _ = std::fs::remove_dir_all(&install.root);
}

/// An unparseable config fails at stage 1 — later stages reported
/// `not_run` — with exit class 3 (config) and the diagnostic in the
/// per-stage report.
#[test]
fn check_on_unparseable_config_fails_at_the_config_stage() {
    let install = install("unparseable");
    std::fs::write(&install.config, "schema_version = 1\n[hosts.h1\n").unwrap();

    let output = command(&install, &["check"]).output().expect("taisan runs");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let env = envelope(&output);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    assert!(
        env["reason"]["message"]
            .as_str()
            .unwrap()
            .contains("check failed at config_parse"),
        "{env}"
    );
    let stages = env["result"]["stages"].as_array().unwrap();
    assert_eq!(stages[0]["stage"], "config_parse");
    assert_eq!(stages[0]["status"], "fail");
    assert_eq!(
        stages[0]["diagnostics"][0]["code"], "config.invalid_toml",
        "{env}"
    );
    for stage in stages.iter().skip(1) {
        assert_eq!(stage["status"], "not_run", "no later step ran: {stage}");
    }
    assert_eq!(env["result"]["success"], false);

    let _ = std::fs::remove_dir_all(&install.root);
}

/// `--probe` is opt-in and fails closed without a wired backend:
/// stage 8 fails `capability.unknown`, the operation log is empty, and
/// zero remote writes are recorded.
#[test]
fn check_probe_is_opt_in_and_fails_closed_without_a_backend() {
    let install = install("probe-flag");
    let output = run(&install, &init_args(&install));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Plain check never runs the probe (stage 8 `not_run`, above).
    // With --probe, the requested stage fails closed — read-only, no
    // fabricated result, no operation performed.
    let output = command(&install, &["check", "--probe"])
        .output()
        .expect("taisan runs");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let env = envelope(&output);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    let stages = env["result"]["stages"].as_array().unwrap();
    assert_eq!(stages[7]["stage"], "probe_capabilities");
    assert_eq!(stages[7]["status"], "fail");
    assert_eq!(stages[7]["diagnostics"][0]["code"], "capability.unknown");
    for stage in stages.iter().take(7) {
        assert_eq!(stage["status"], "pass", "{stage}");
    }
    assert!(
        env["result"]["probe_operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(env["result"]["remote_writes"], 0);

    let _ = std::fs::remove_dir_all(&install.root);
}

/// `init` is non-destructive and never fabricates identity; malformed
/// flag shapes are usage errors (exit 2).
#[test]
fn init_refuses_existing_configs_and_fabrication_and_bad_shapes() {
    let guarded = install("init-guard");
    let output = run(&guarded, &init_args(&guarded));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = std::fs::read(&guarded.config).unwrap();

    // Re-run over an existing config: refused, operator-set fields and
    // the file bytes untouched.
    let output = run(&guarded, &init_args(&guarded));
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let env = envelope(&output);
    assert_eq!(env["status"], "error");
    assert!(
        env["reason"]["message"]
            .as_str()
            .unwrap()
            .contains("config.exists"),
        "{env}"
    );
    assert_eq!(std::fs::read(&guarded.config).unwrap(), first);

    // A well-formed role with no observed identity: refused at config
    // class, nothing published.
    let unobserved = install("init-no-observe");
    let output = command(
        &unobserved,
        &[
            "init",
            "--endpoint",
            "https://***.github.com",
            "--repo",
            "0123456789:publication",
            "--role",
            "publication:taisan-publisher:env:TAISAN_TEST_TOKEN",
        ],
    )
    .output()
    .expect("taisan runs");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let env = envelope(&output);
    assert!(
        env["reason"]["message"]
            .as_str()
            .unwrap()
            .contains("never fabricates"),
        "{env}"
    );
    assert!(!unobserved.config.exists(), "no partial config published");

    // Malformed flag shapes are usage errors.
    for args in [
        vec![
            "init",
            "--endpoint",
            "https://***.github.com",
            "--role",
            "publication",
        ],
        vec![
            "init",
            "--endpoint",
            "https://***.github.com",
            "--repo",
            "0123456789",
        ],
        vec![
            "init",
            "--endpoint",
            "https://***.github.com",
            "--repo",
            "0123456789:publication",
            "--observe",
            "publication:login-without-context",
        ],
    ] {
        let output = command(&unobserved, &args).output().expect("taisan runs");
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let env = envelope(&output);
        assert_eq!(env["status"], "error");
        assert_eq!(env["reason"]["code"], "usage");
        assert!(!unobserved.config.exists());
    }

    // A relative config override is refused — the ambient working
    // directory is never consulted (flag → env → XDG precedence).
    let output = command(&guarded, &["check", "--config", "relative.toml"])
        .output()
        .expect("taisan runs");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let env = envelope(&output);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    assert!(
        env["reason"]["message"]
            .as_str()
            .unwrap()
            .contains("absolute"),
        "{env}"
    );

    let _ = std::fs::remove_dir_all(&guarded.root);
    let _ = std::fs::remove_dir_all(&unobserved.root);
}
