//! CLI surface tests for intake (#209/#210): `intake`,
//! `intake-runs`, and `intake-detail` — profile resolution through
//! the flag/env/XDG precedence, the bounded config refusals, the
//! end-to-end manual invocation, and the run projections.
//!
//! `--config` cannot point at the TOML profile: store-backed
//! commands read it as the JSON installation record first. The
//! profile therefore arrives through `TAISAN_CONFIG`, exactly the
//! precedence `resolve_paths_from_env` documents.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use workloop_core::identity::{
    AdapterId, HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef,
};
use workloop_selector::protocol;
use workloop_selector::{Proposal, Provenance, SourceBinding, TaskInput, proposal_key};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("workloop-intake-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `taisan` against `root`'s store with the TOML profile supplied
/// through `TAISAN_CONFIG` (or deliberately absent). `--config` is
/// never passed: it would be read as the JSON installation record.
fn intake_cli(root: &Path, profile: Option<&Path>, args: &[&str]) -> Output {
    let mut full = vec![
        "--db".to_string(),
        root.join("workloop.db").to_string_lossy().into_owned(),
        "--state-dir".to_string(),
        root.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    let mut command = Command::new(bin());
    command.args(&full);
    // The profile path is an absolute override; XDG is pinned empty so
    // an ambient user config can never leak into a test.
    command.env("XDG_CONFIG_HOME", root.join("xdg-config"));
    command.env("XDG_STATE_HOME", root.join("xdg-state"));
    match profile {
        Some(p) => {
            command.env("TAISAN_CONFIG", p);
        }
        None => {
            command.env_remove("TAISAN_CONFIG");
        }
    }
    command.output().expect("taisan runs")
}

fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

fn run_ok(root: &Path, profile: Option<&Path>, args: &[&str]) -> serde_json::Value {
    let out = intake_cli(root, profile, args);
    assert!(
        out.status.success(),
        "taisan {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    envelope(&out)
}

/// A minimal valid profile: one publication repository in scope and
/// an enabled intake bound to the fixture selector script.
fn profile_toml(selector_argv: &[String]) -> String {
    let argv = selector_argv
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"schema_version = 1

[hosts.h1]
adapter = "github"
endpoint = "https://api.github.com"
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"
use = "publication"

[roles.publication]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_TOKEN"

[intake]
enabled = true
interval_minutes = 30
timeout_seconds = 60
pending_queue_limit = 10

[intake.selector]
adapter = "test-sel"
selector_version = 1
policy_revision = 7
argv = [{argv}]
"#
    )
}

/// The one proposal the fixture selector returns: a single source in
/// the profile's repository scope, empty task defaults matching the
/// unset `[intake.selector]` task fields.
fn proposal() -> Proposal {
    let source = SourceBinding {
        issue: IssueRef {
            repository: RepositoryRef::new(
                HostInstanceId::trusted("https://api.github.com"),
                RepositoryId::trusted("0123456789"),
            ),
            issue: IssueId::trusted("issue-1"),
            display_number: None,
        },
        revision: "rev-1".to_string(),
        content_digest: "digest-1".to_string(),
    };
    let task = TaskInput {
        intent: "cli intake work".to_string(),
        constraints: vec![],
        verification_argv: vec![],
    };
    let permissions = vec![];
    let sources = vec![source];
    Proposal {
        key: proposal_key("test-sel", 1, 7, &sources, &task, &permissions),
        selector: AdapterId::trusted("test-sel"),
        selector_version: 1,
        policy_revision: 7,
        sources,
        task,
        required_permissions: permissions,
        provenance: Provenance::default(),
    }
}

/// Write the selector script + response template; returns the argv
/// the profile binds.
fn selector_fixture(root: &Path) -> Vec<String> {
    let template = String::from_utf8(protocol::success_response("__RID__", &[proposal()]))
        .expect("template is UTF-8");
    let template_path = root.join("response.template");
    std::fs::write(&template_path, template).unwrap();
    let capture = root.join("request.json");
    let script_path = root.join("sel.sh");
    let script = format!(
        "#!/bin/sh\nreq=$(cat)\nprintf '%s' \"$req\" > '{}'\nrid=$(printf '%s' \"$req\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\nsed \"s/__RID__/$rid/g\" '{}'\n",
        capture.display(),
        template_path.display()
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    vec![
        "/bin/sh".to_string(),
        script_path.to_string_lossy().into_owned(),
    ]
}

fn write_profile(root: &Path, argv: &[String]) -> PathBuf {
    let path = root.join("profile.toml");
    std::fs::write(&path, profile_toml(argv)).unwrap();
    path
}

#[test]
fn intake_without_a_profile_is_a_bounded_config_refusal() {
    let dir = root("noprofile");
    let out = intake_cli(&dir, None, &["intake"]);
    assert!(!out.status.success());
    let env = envelope(&out);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    assert!(
        env["reason"]["message"]
            .as_str()
            .unwrap()
            .contains("no installation profile"),
        "{env}"
    );
}

#[test]
fn intake_runs_reports_unconfigured_without_a_profile() {
    let dir = root("runs-noprofile");
    let env = run_ok(&dir, None, &["intake-runs"]);
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "intake_runs");
    assert_eq!(env["result"]["status"]["configured"], false);
    assert_eq!(env["result"]["items"].as_array().unwrap().len(), 0);
}

#[test]
fn intake_with_an_invalid_profile_is_a_bounded_refusal() {
    let dir = root("badprofile");
    let profile = dir.join("profile.toml");
    std::fs::write(&profile, "this is not = valid toml [[[").unwrap();
    let out = intake_cli(&dir, Some(&profile), &["intake"]);
    assert!(!out.status.success());
    let env = envelope(&out);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
}

#[test]
fn intake_runs_reports_the_enabled_profile_state() {
    let dir = root("runs-profile");
    let argv = selector_fixture(&dir);
    let profile = write_profile(&dir, &argv);
    let env = run_ok(&dir, Some(&profile), &["intake-runs"]);
    assert_eq!(env["status"], "accepted");
    let status = &env["result"]["status"];
    assert_eq!(status["configured"], true);
    assert_eq!(status["enabled"], true);
    assert_eq!(status["interval_minutes"], 30);
    assert_eq!(status["pending_queue_limit"], 10);
    assert!(status["settings_revision"].as_str().unwrap().len() == 64);
}

#[test]
fn intake_detail_for_an_unknown_run_is_a_completed_not_found() {
    let dir = root("detail-missing");
    let argv = selector_fixture(&dir);
    let profile = write_profile(&dir, &argv);
    let env = run_ok(
        &dir,
        Some(&profile),
        &["intake-detail", "--run", "intake-nope"],
    );
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "intake_detail");
    // NotFound is a completed query: accepted envelope, null result.
    assert!(env["result"].is_null(), "{env}");
}

#[test]
fn intake_runs_the_coordinator_and_projects_the_run() {
    let dir = root("roundtrip");
    let argv = selector_fixture(&dir);
    let profile = write_profile(&dir, &argv);

    // The one-shot manual invocation: completed with one accepted
    // candidate carrying its decision links.
    let env = run_ok(&dir, Some(&profile), &["intake"]);
    assert_eq!(env["status"], "accepted", "{env}");
    assert_eq!(env["command"]["kind"], "intake");
    let result = &env["result"];
    assert_eq!(result["state"], "completed", "{result}");
    assert_eq!(result["launched"], true);
    let candidates = result["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["status"], "accepted");
    assert!(candidates[0]["work_id"].as_str().is_some());
    assert!(candidates[0]["command_id"].as_str().is_some());
    assert!(
        candidates[0]["replay_key"]
            .as_str()
            .unwrap()
            .starts_with("proposal/test-sel:")
    );
    let run_id = result["run_id"].as_str().unwrap().to_string();

    // The run list carries the durable status facts.
    let env = run_ok(&dir, Some(&profile), &["intake-runs"]);
    let items = env["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["run_id"], run_id);
    assert_eq!(items[0]["state"], "completed");
    assert_eq!(items[0]["trigger"], "manual");
    assert_eq!(env["result"]["last_completed_run"], run_id);
    assert_eq!(env["result"]["active_run"], serde_json::Value::Null);

    // The detail projection resolves the candidate's links.
    let env = run_ok(&dir, Some(&profile), &["intake-detail", "--run", &run_id]);
    let detail = &env["result"];
    assert_eq!(detail["run"]["run_id"], run_id);
    let candidates = detail["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["status"], "accepted");
    assert!(candidates[0]["work_id"].as_str().is_some());
    assert!(candidates[0]["command_id"].as_str().is_some());
    assert!(candidates[0]["committed_seq"].is_number());
    assert_eq!(candidates[0]["proposal_key"].as_str().unwrap().len(), 64);
}

#[test]
fn run_once_drives_a_scheduled_startup_intake_without_blocking() {
    let dir = root("run-scheduled");
    let argv = selector_fixture(&dir);
    let profile = write_profile(&dir, &argv);

    // `run --once` evaluates the driver's first settings-check tick
    // after the pass: an enabled profile is due at startup, the
    // invocation runs on its own thread, and `finish()` joins it
    // before the run's terminal envelope — the selector never blocks
    // a phase.
    let out = intake_cli(&dir, Some(&profile), &["run", "--once"]);
    assert!(
        out.status.success(),
        "run --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("intake invocation finished"),
        "expected the bounded intake diagnostic on stderr: {stderr}"
    );

    // The scheduled run is durable and projected like a manual one.
    let env = run_ok(&dir, Some(&profile), &["intake-runs"]);
    let items = env["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{env}");
    assert_eq!(items[0]["trigger"], "scheduled_startup");
    assert_eq!(items[0]["state"], "completed");
    assert_eq!(items[0]["candidates"]["accepted"], 1);
}
