//! CLI surface tests: every result — accepted, rejected, error, or
//! unknown — is exactly one JSON envelope on stdout carrying
//! `schema_version: "1"`, the closed status vocabulary, and the caller's
//! replay identity; exit codes follow the documented classes; human
//! prose stays on stderr.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-cli"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-cli-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn output_at(root: &Path, args: &[&str]) -> Output {
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
        .expect("workloop runs")
}

/// Run a command that must succeed and return its parsed envelope.
fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = output_at(root, args);
    assert!(
        output.status.success(),
        "workloop {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(&output)
}

/// The whole stdout must be exactly one JSON object — no prose, no
/// trailing diagnostics.
fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

/// Run a command expected to fail and return (exit code, envelope).
fn fail_at(root: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let output = output_at(root, args);
    let code = output.status.code().unwrap_or(-1);
    let env = envelope(&output);
    (code, env)
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
            "--idempotency-key",
            "cli-k1",
        ],
    );
    // The envelope contract: versioned, closed status, replay identity.
    assert_eq!(decision["schema_version"], "1");
    assert_eq!(decision["status"], "accepted", "{decision}");
    assert_eq!(decision["command"]["kind"], "submit");
    assert_eq!(decision["command"]["idempotency_key"], "cli-k1");
    assert!(decision["command"]["actor"].as_str().unwrap().contains(':'));
    assert!(decision["reason"].is_null());
    assert!(decision["evidence_refs"].is_array());
    let work_id = decision["work_id"].as_str().unwrap().to_string();
    let command_id = decision["command"]["id"].as_str().unwrap().to_string();
    let execution_id = decision["execution_id"].as_str().unwrap().to_string();
    assert_eq!(
        decision["result"]["kind"], "work_created",
        "the effect rides in result"
    );

    // work_list derives the phase; nothing else was touched.
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["status"], "accepted");
    assert_eq!(list["command"]["kind"], "work_list");
    let items = list["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["work_id"], work_id);
    assert_eq!(items[0]["phase"], "queued");
    assert!(list["as_of"].is_number());
    assert!(list["sources"].is_array());

    // `run --once` admits the queued execution (capacity 1) through
    // the single run verb.
    run_at(&root, &["run", "--once"]);
    let detail = run_at(&root, &["work-detail", "--work", &work_id]);
    assert_eq!(detail["status"], "accepted");
    assert_eq!(detail["work_id"], work_id);
    assert_eq!(
        detail["result"]["executions"][0]["state"], "ready",
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
        assert_eq!(r["status"], "accepted", "{args:?}: {r}");
        assert_eq!(r["result"]["command_id"], command_id);
    }

    // A command-result for an unknown key is accepted with a null
    // result — a completed query, not an error.
    let r = run_at(&root, &["command-result", "--command-id", "cmd-nope"]);
    assert_eq!(r["status"], "accepted");
    assert!(r["result"].is_null());

    // capacity explains the slot holder and held reservations.
    let capacity = run_at(&root, &["capacity"]);
    assert_eq!(capacity["result"]["used"], 1);
    assert!(
        !capacity["result"]["reservations"]
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

    // A stale expected version is rejected, not silently applied —
    // exit 1 with the closed rejection vocabulary in reason.code.
    let (code, decision) = fail_at(
        &root,
        &[
            "resume",
            "--work",
            &work_id,
            "--expected-version",
            "99",
            "--execution",
            &execution_id,
            "--idempotency-key",
            "cli-k3",
        ],
    );
    assert_eq!(code, 1, "{decision}");
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"]["code"], "stale_work_version");

    // attention is a well-formed empty result.
    let attention = run_at(&root, &["attention"]);
    assert_eq!(attention["status"], "accepted");

    // Idempotent replay: the same key returns the recorded decision.
    let replay = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "do the thing",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    assert_eq!(replay["status"], "accepted");
    assert_eq!(
        replay["command"]["id"], command_id,
        "replay returns the stored decision"
    );

    // The same key with a changed payload is a recorded conflict —
    // exit 1, never a second mutation.
    let (code, conflict) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "a different thing",
            "--idempotency-key",
            "cli-k1",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(conflict["status"], "rejected");
    assert_eq!(conflict["reason"]["code"], "idempotency_conflict");
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

#[test]
fn malformed_input_is_usage_error() {
    let root = root("usage");

    // clap-level failure: unknown subcommand.
    let (code, env) = fail_at(&root, &["frobnicate"]);
    assert_eq!(code, 2, "{env}");
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "usage");
    assert_eq!(env["schema_version"], "1");

    // Flag-level failure: unparseable argument value.
    let (code, env) = fail_at(&root, &["work-detail", "--work"]);
    assert_eq!(code, 2);
    assert_eq!(env["reason"]["code"], "usage");

    // CLI-level validation: unpaired --issue triples.
    let (code, env) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "x",
            "--issue",
            "repo:1",
            "--issue-revision",
            "r1",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "usage");
    assert_eq!(env["command"]["kind"], "submit");

    // Bad --issue grammar.
    let (code, env) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "x",
            "--issue",
            "not-a-repo-issue",
            "--issue-revision",
            "r1",
            "--issue-digest",
            "d1",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(env["reason"]["code"], "usage");

    // command-result with no lookup identity.
    let (code, env) = fail_at(&root, &["command-result"]);
    assert_eq!(code, 2);
    assert_eq!(env["reason"]["code"], "usage");

    // Nothing was recorded behind the usage errors.
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["status"], "accepted");
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 0);
}

#[test]
fn config_failure_is_exit_3() {
    let root = root("config");
    std::fs::write(root.join("config.json"), "{ not json").unwrap();
    let (code, env) = fail_at(&root, &["submit", "--intent", "x"]);
    assert_eq!(code, 3, "{env}");
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    // A config failure can never appear accepted.
    assert_ne!(env["status"], "accepted");
}

#[test]
fn store_failure_is_exit_4() {
    let root = root("store");
    // A foreign file at the db path is a store-class refusal, not a
    // config or usage error.
    std::fs::write(root.join("workloop.db"), "not a database").unwrap();
    let (code, env) = fail_at(&root, &["submit", "--intent", "x"]);
    assert_eq!(code, 4, "{env}");
    assert_eq!(env["status"], "error");
    assert!(
        matches!(env["reason"]["code"].as_str(), Some("store" | "config")),
        "{env}"
    );
}

#[test]
fn run_emits_one_envelope_across_ticks() {
    let root = root("run");
    run_at(
        &root,
        &["submit", "--intent", "x", "--idempotency-key", "k-r1"],
    );
    // A bounded run is one command result: exactly one envelope on
    // stdout no matter how many ticks it drove; per-tick progress is
    // a stderr diagnostic.
    let output = output_at(&root, &["run", "--ticks", "2", "--interval", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env = envelope(&output);
    assert_eq!(env["status"], "accepted");
    assert_eq!(env["command"]["kind"], "run");
}

#[test]
fn source_unavailable_rejection_exits_5() {
    let root = root("unavailable");
    let decision = run_at(
        &root,
        &["submit", "--intent", "x", "--idempotency-key", "k-s1"],
    );
    let work_id = decision["work_id"].as_str().unwrap().to_string();

    // judge-findings on a Work with no observed change target is a
    // domain rejection whose class is an unavailable source.
    let (code, env) = fail_at(
        &root,
        &[
            "judge-findings",
            "--work",
            &work_id,
            "--expected-version",
            "1",
            "--head",
            "a".repeat(40).as_str(),
            "--decision",
            "occ-1:fix",
        ],
    );
    assert_eq!(code, 5, "{env}");
    assert_eq!(env["status"], "rejected");
    assert_eq!(env["reason"]["code"], "source_unavailable");
}

/// The pending-queue limit refuses the eleventh queued submit with a
/// structured `capacity_exhausted` rejection — transient, so the
/// envelope carries no recorded command id and `command-result` under
/// the same key finds nothing; an identical resubmit is decided fresh.
#[test]
fn a_full_pending_queue_rejects_without_a_recorded_decision() {
    let root = root("capacity");
    for i in 0..10 {
        run_at(
            &root,
            &[
                "submit",
                "--intent",
                &format!("fill-{i}"),
                "--idempotency-key",
                &format!("fill-k{i}"),
            ],
        );
    }

    let (code, env) = fail_at(
        &root,
        &[
            "submit",
            "--intent",
            "one too many",
            "--idempotency-key",
            "overflow-k",
        ],
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["status"], "rejected", "{env}");
    assert_eq!(env["reason"]["code"], "capacity_exhausted", "{env}");
    assert!(
        env["command"]["id"].is_null(),
        "no phantom command id for an unpersisted refusal: {env}"
    );
    assert_eq!(env["command"]["idempotency_key"], "overflow-k");

    // Nothing was recorded under the refused key.
    let lookup = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "cli",
            "--replay-key",
            "overflow-k",
        ],
    );
    assert!(lookup["result"].is_null(), "{lookup}");
}
