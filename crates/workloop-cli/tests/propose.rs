//! The `propose` command is the alternate admission path: the
//! configured selector runs over a snapshot file and every proposal is
//! admitted through the normal submit command. Its absence is a
//! bounded refusal that blocks nothing — manual submission and
//! existing Work are unaffected — and replaying a run replays the
//! recorded decisions rather than duplicating Work.

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

fn run_at(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = output_at(root, args);
    assert!(
        output.status.success(),
        "workloop {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(&output)
}

fn envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout}"))
}

fn fail_at(root: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let output = output_at(root, args);
    let code = output.status.code().unwrap_or(-1);
    (code, envelope(&output))
}

/// Materialize the default installation config, then overlay the given
/// selector section.
fn configure(root: &Path, selector: serde_json::Value) {
    run_at(root, &["work-list"]); // creates config.json on first use
    let path = root.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["selector"] = selector;
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
}

fn write_snapshot_at(
    root: &Path,
    name: &str,
    issues: serde_json::Value,
    cursor: serde_json::Value,
) -> PathBuf {
    let path = root.join(name);
    let snapshot = serde_json::json!({
        "issues": issues,
        "cursor": cursor,
        "fetched_unix_seconds": 1_700_000_000,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();
    path
}

fn write_snapshot(root: &Path, name: &str, issues: serde_json::Value) -> PathBuf {
    write_snapshot_at(root, name, issues, serde_json::Value::Null)
}

fn issue(repo: &str, number: u64, state: &str, title: &str) -> serde_json::Value {
    serde_json::json!({
        "issue": {
            "repository": {"host": "host-local", "repository": repo},
            "issue": number.to_string(),
            "display_number": number,
        },
        "state": state,
        "revision": format!("r{number}"),
        "content_digest": format!("d{number}"),
        "title": title,
    })
}

#[test]
fn proposals_are_admitted_through_the_normal_submit_path() {
    let root = root("propose-ok");
    configure(
        &root,
        serde_json::json!({
            "adapter": "issue-scan",
            "policy_revision": 3,
            "allowlist": ["ops"],
            "excluded": ["ops:9"],
            "max_proposals": 5,
            "task_constraints": ["keep it small"],
            "verification_argv": ["cargo", "test"],
            "required_permissions": [],
        }),
    );
    let conflict_a = {
        let mut f = issue("ops", 4, "open", "Torn read");
        f["revision"] = serde_json::json!("r4a");
        f
    };
    let conflict_b = {
        let mut f = issue("ops", 4, "open", "Torn read");
        f["revision"] = serde_json::json!("r4b");
        f
    };
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([
            issue("ops", 1, "open", "Fix the widget"),
            issue("ops", 2, "closed", "Already done"),
            issue("other", 3, "open", "Not ours"),
            issue("ops", 9, "open", "Excluded by policy"),
            conflict_a,
            conflict_b,
        ]),
    );
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    // One batch envelope; per-proposal decisions and skip reasons are
    // data inside it.
    assert_eq!(result["status"], "accepted", "{result}");
    assert_eq!(result["command"]["kind"], "propose");
    assert_eq!(result["result"]["selector"], "issue-scan");
    assert_eq!(result["result"]["policy_revision"], 3);
    let proposals = result["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{result}");
    assert_eq!(proposals[0]["status"], "accepted");
    let work_id = proposals[0]["work_id"].as_str().unwrap().to_string();

    let skipped = result["result"]["skipped"].as_array().unwrap();
    let reasons: Vec<_> = skipped
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    assert_eq!(skipped.len(), 4, "{result}");
    assert!(reasons.contains(&"not_open"));
    assert!(reasons.contains(&"not_allowlisted"));
    assert!(reasons.contains(&"excluded"));
    // A torn snapshot read — two facts for one Issue at different
    // revisions — is refused once, never silently picked between.
    assert_eq!(
        reasons
            .iter()
            .filter(|r| **r == "conflicting_facts")
            .count(),
        1
    );

    // The proposal became ordinary queued Work.
    let list = run_at(&root, &["work-list"]);
    let items = list["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["work_id"], work_id);
    assert_eq!(items[0]["phase"], "queued");

    // Snapshot bookkeeping instability — a moved cursor, a different
    // fetch time — never changes proposal identity: the retry replays
    // the recorded decision, the same command and Work, never a
    // duplicate.
    let unstable = write_snapshot_at(
        &root,
        "snapshot-2.json",
        serde_json::json!([
            issue("ops", 1, "open", "Fix the widget"),
            issue("ops", 2, "closed", "Already done"),
            issue("other", 3, "open", "Not ours"),
            issue("ops", 9, "open", "Excluded by policy"),
        ]),
        serde_json::json!("cursor-moved"),
    );
    let replay = run_at(
        &root,
        &["propose", "--snapshot", &unstable.to_string_lossy()],
    );
    let replayed = replay["result"]["proposals"].as_array().unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0]["status"], "accepted");
    assert_eq!(replayed[0]["work_id"], work_id);
    assert_eq!(replayed[0]["command_id"], proposals[0]["command_id"]);
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 1);

    // Replay identity and provenance are operator-visible: the envelope
    // carries the derived replay key, and the recorded decision is
    // recoverable under the selector origin by that key or by command
    // id.
    let command_id = proposals[0]["command_id"].as_str().unwrap();
    let replay_key = proposals[0]["replay_key"].as_str().unwrap();
    assert!(
        replay_key.starts_with("proposal/issue-scan:"),
        "replay key names the admitting selector: {replay_key}"
    );
    let recovered = run_at(&root, &["command-result", "--command-id", command_id]);
    assert_eq!(recovered["status"], "accepted");
    assert_eq!(recovered["result"]["command_id"], command_id);
    let recovered = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "selector",
            "--replay-key",
            replay_key,
        ],
    );
    assert_eq!(recovered["status"], "accepted");
    assert_eq!(recovered["result"]["command_id"], command_id);
}

#[test]
fn a_missing_selector_section_blocks_nothing_else() {
    let root = root("propose-none");
    // Manual admission on a selector-less installation stays fine.
    run_at(
        &root,
        &["submit", "--intent", "manual", "--idempotency-key", "k-m1"],
    );
    let snapshot = write_snapshot(&root, "snap.json", serde_json::json!([]));
    let (code, env) = fail_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    assert_eq!(code, 3, "{env}");
    assert_eq!(env["status"], "error");
    assert_eq!(env["reason"]["code"], "config");
    // The outage changed nothing: manual submit still admits, the
    // existing Work is untouched.
    run_at(
        &root,
        &[
            "submit",
            "--intent",
            "still fine",
            "--idempotency-key",
            "k-m2",
        ],
    );
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 2);
}

#[test]
fn a_malformed_snapshot_is_a_usage_error() {
    let root = root("propose-bad");
    configure(
        &root,
        serde_json::json!({"adapter": "issue-scan", "allowlist": ["ops"]}),
    );
    let (code, env) = fail_at(&root, &["propose", "--snapshot", "no-such.json"]);
    assert_eq!(code, 2, "{env}");
    assert_eq!(env["reason"]["code"], "usage");

    let bad = root.join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    let (code, env) = fail_at(&root, &["propose", "--snapshot", &bad.to_string_lossy()]);
    assert_eq!(code, 2, "{env}");
    assert_eq!(env["reason"]["code"], "usage");

    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 0);
}

#[test]
fn an_unparseable_selector_section_is_refused_at_the_propose_boundary() {
    let root = root("propose-invalid");
    // A malformed selector section is a bounded config error for the
    // selector path only — it can never take manual submission down.
    for selector in [
        serde_json::json!({"adapter": "", "allowlist": ["ops"]}),
        serde_json::json!({"adapter": "issue-scan", "excluded": ["ops:notanumber"]}),
    ] {
        configure(&root, selector);
        let snapshot = write_snapshot(&root, "snap.json", serde_json::json!([]));
        let (code, env) = fail_at(
            &root,
            &["propose", "--snapshot", &snapshot.to_string_lossy()],
        );
        assert_eq!(code, 3, "{env}");
        assert_eq!(env["status"], "error");
        assert_eq!(env["reason"]["code"], "config");
        run_at(
            &root,
            &[
                "submit",
                "--intent",
                "manual still fine",
                "--idempotency-key",
                "k-i",
            ],
        );
    }
}

#[test]
fn a_rejected_proposal_does_not_abort_the_batch() {
    let root = root("propose-partial");
    // Manual submission claims ops:1 first.
    run_at(
        &root,
        &[
            "submit",
            "--intent",
            "claimed",
            "--issue",
            "ops:1",
            "--issue-revision",
            "r1",
            "--issue-digest",
            "d1",
            "--idempotency-key",
            "k-c1",
        ],
    );
    configure(
        &root,
        serde_json::json!({"adapter": "issue-scan", "allowlist": ["ops"]}),
    );
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([
            issue("ops", 1, "open", "Claimed issue"),
            issue("ops", 2, "open", "Fresh issue"),
        ]),
    );
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = result["result"]["proposals"].as_array().unwrap();
    // One item's rejection is data; later proposals are still admitted.
    assert_eq!(proposals.len(), 2, "{result}");
    assert_eq!(proposals[0]["status"], "rejected");
    assert_eq!(proposals[0]["reason_code"], "ineligible");
    assert_eq!(proposals[1]["status"], "accepted");
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 2);
}

#[test]
fn truncation_at_the_proposal_bound_is_reported() {
    let root = root("propose-bound");
    configure(
        &root,
        serde_json::json!({
            "adapter": "issue-scan",
            "allowlist": ["ops"],
            "max_proposals": 1,
        }),
    );
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([
            issue("ops", 1, "open", "First"),
            issue("ops", 2, "open", "Second"),
        ]),
    );
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    assert_eq!(result["result"]["proposals"].as_array().unwrap().len(), 1);
    // "Hit the bound" is distinguishable from "no more eligible work".
    assert_eq!(result["result"]["truncated"], 1, "{result}");
}

#[test]
fn switching_admission_configuration_preserves_existing_work() {
    let root = root("propose-swap");
    // Work accepted under manual admission.
    let first = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "existing",
            "--idempotency-key",
            "k-e1",
        ],
    );
    let existing = first["work_id"].as_str().unwrap().to_string();
    // Admission configuration switches to the selector path.
    configure(
        &root,
        serde_json::json!({"adapter": "issue-scan", "allowlist": ["ops"]}),
    );
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([issue("ops", 5, "open", "New work")]),
    );
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = result["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals[0]["status"], "accepted");
    // The pre-existing Work kept its identity and phase; the new Work
    // joins the same queue.
    let list = run_at(&root, &["work-list"]);
    let items = list["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let phases: Vec<_> = items
        .iter()
        .map(|i| (i["work_id"].as_str().unwrap(), i["phase"].as_str().unwrap()))
        .collect();
    assert!(phases.contains(&(existing.as_str(), "queued")));
    // Switching back to manual-only admission is equally inert.
    configure(&root, serde_json::Value::Null);
    run_at(
        &root,
        &[
            "submit",
            "--intent",
            "back to manual",
            "--idempotency-key",
            "k-e2",
        ],
    );
    assert_eq!(
        run_at(&root, &["work-list"])["result"]["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}
