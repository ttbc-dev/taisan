//! CLI surface smoke tests: every command goes through
//! `work::handle_command`, every read through the projection Queries, and
//! the actor identity comes from the installation config, stable across
//! invocations.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-cli-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let db = root.join("workloop.db");
    let config = root.join("config.json");
    let mut full = vec![
        "--db".into(),
        db.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    let output = Command::new(bin())
        .args(&full)
        .output()
        .expect("workloop runs");
    assert!(
        output.status.success(),
        "workloop {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn command_and_query_surface_round_trip() {
    let root = root("surface");
    let decision = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "do the thing",
            "--issue",
            "local/repo:42",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    assert_eq!(decision["status"], "accepted", "{decision}");
    let work_id = decision["work_id"].as_str().unwrap().to_string();
    let command_id = decision["command_id"].as_str().unwrap().to_string();

    // work_list derives the phase; nothing else was touched.
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["kind"], "found");
    let items = list["value"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["work_id"], work_id);
    assert_eq!(items[0]["phase"], "queued");

    // run-once admits the queued execution (capacity 1).
    run_at(&root, &["run-once"]);
    let detail = run_at(&root, &["work-detail", "--work", &work_id]);
    assert_eq!(
        detail["value"]["executions"][0]["state"], "ready",
        "dispatch admitted the queued execution"
    );

    // command_result recovers the response by command id and replay key.
    for args in [
        vec!["command-result", "--command-id", &command_id],
        vec![
            "command-result",
            "--origin",
            "cli",
            "--replay-key",
            "cli-k1",
        ],
    ] {
        let r = run_at(&root, &args);
        assert_eq!(r["kind"], "found", "{args:?}: {r}");
        assert_eq!(r["value"]["command_id"], command_id);
    }

    // capacity explains the slot holder and held reservations.
    let capacity = run_at(&root, &["capacity"]);
    assert_eq!(capacity["value"]["used"], 1);
    assert!(
        !capacity["value"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // cancel goes through handle_command with an expected version.
    let decision = run_at(
        &root,
        &[
            "cancel",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "cli-k2",
        ],
    );
    assert_eq!(decision["status"], "accepted", "{decision}");

    // A stale expected version is rejected, not silently applied.
    let decision = run_at(
        &root,
        &[
            "resume",
            "--work",
            &work_id,
            "--expected-version",
            "99",
            "--execution",
            "e-1",
            "--idempotency-key",
            "cli-k3",
        ],
    );
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"], "stale_work_version");

    // attention is a well-formed empty result.
    let attention = run_at(&root, &["attention"]);
    assert_eq!(attention["kind"], "found");

    // Idempotent replay: the same key returns the recorded decision.
    let replay = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "do the thing",
            "--issue",
            "local/repo:42",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    assert_eq!(replay["status"], "accepted");
    assert_eq!(
        replay["command_id"], command_id,
        "replay returns the stored decision"
    );
}

#[test]
fn actor_identity_is_stable_across_invocations() {
    let root = root("actor");
    run_at(
        &root,
        &["submit", "--intent", "x", "--idempotency-key", "k-a1"],
    );
    let config_path = root.join("config.json");
    let first: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    run_at(&root, &["work-list"]);
    let second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(first["actor_id"], second["actor_id"]);
    assert!(first["actor_id"].as_str().unwrap().starts_with("actor-"));
}
