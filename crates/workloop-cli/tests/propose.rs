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

/// A configured `repo:number` exclusion holds even when the adapter's
/// opaque IssueId is not that number: the fact is skipped as
/// `excluded`, never proposed, while other eligible work still
/// admits through the normal path.
#[test]
fn excluded_entries_match_by_display_number_for_opaque_issue_ids() {
    let root = root("propose-excluded-display");
    configure(
        &root,
        serde_json::json!({
            "adapter": "issue-scan",
            "allowlist": ["ops"],
            "excluded": ["ops:9"],
        }),
    );
    let opaque = serde_json::json!({
        "issue": {
            "repository": {"host": "host-local", "repository": "ops"},
            "issue": "I_kwDOPaqeNumber",
            "display_number": 9,
        },
        "state": "open",
        "revision": "r9",
        "content_digest": "d9",
        "title": "Excluded by display number",
    });
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([opaque, issue("ops", 10, "open", "Kept")]),
    );
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = result["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{result}");
    assert_eq!(proposals[0]["status"], "accepted", "{result}");
    let skipped = result["result"]["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{result}");
    assert_eq!(skipped[0]["reason"], "excluded", "{result}");
    assert_eq!(
        skipped[0]["issue"], "host-local/ops/I_kwDOPaqeNumber",
        "{result}"
    );
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
    // Both semantic identity errors (validated at the boundary) and
    // structural shape errors (parsed at the boundary, never at config
    // load) are refused the same way.
    for selector in [
        serde_json::json!({"adapter": "", "allowlist": ["ops"]}),
        serde_json::json!({"adapter": "issue-scan", "excluded": ["ops:notanumber"]}),
        // Structurally malformed: missing field, wrong field type,
        // non-array list, out-of-range number, section not an object.
        serde_json::json!({"allowlist": ["ops"]}),
        serde_json::json!({"adapter": 7}),
        serde_json::json!({"adapter": "issue-scan", "allowlist": "ops"}),
        serde_json::json!({"adapter": "issue-scan", "policy_revision": -1}),
        serde_json::json!("not an object"),
        // Unknown keys are refused, never defaulted past: a misspelled
        // `allowlist` must surface as the bounded config error instead
        // of silently degrading to an accepted zero-proposal run, and
        // a typo beside valid fields must not be ignored either.
        serde_json::json!({"adapter": "issue-scan", "alowlist": ["ops"]}),
        serde_json::json!({
            "adapter": "issue-scan",
            "allowlist": ["ops"],
            "max_proposalsx": 5
        }),
    ] {
        configure(&root, selector);
        // Loading the installation never fails on the selector
        // section: every command reaches its own path.
        run_at(&root, &["work-list"]);
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
fn a_mid_batch_store_fault_lists_replay_keys_for_recovery() {
    let root = root("propose-fault");
    configure(
        &root,
        serde_json::json!({"adapter": "issue-scan", "allowlist": ["ops"]}),
    );
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([
            issue("ops", 1, "open", "First"),
            issue("ops", 2, "open", "Second"),
        ]),
    );
    inject_mid_batch_fault(&root);

    let (code, env) = fail_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    // The interrupted proposal's transaction rolled back, so its fate
    // is definitely absent — a store-class error (exit 4), never a
    // guess — and the envelope lists the committed proposal's replay
    // key next to the interrupted one's.
    assert_eq!(code, 4, "{env}");
    assert_eq!(env["status"], "error", "{env}");
    assert_eq!(env["reason"]["code"], "store", "{env}");
    let refs = env["evidence_refs"]
        .as_array()
        .unwrap_or_else(|| panic!("evidence_refs is an array: {env}"));
    assert_eq!(refs.len(), 2, "{env}");
    assert!(
        refs.iter()
            .all(|r| r.as_str().unwrap().starts_with("proposal/issue-scan:")),
        "{env}"
    );

    // Each listed key is operator-visible through the documented
    // recovery query: the committed one yields its recorded decision,
    // the interrupted one a durable not-found — before any re-run.
    let committed_key = refs[0].as_str().unwrap().to_string();
    let interrupted_key = refs[1].as_str().unwrap().to_string();
    let recovered = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "selector",
            "--replay-key",
            &committed_key,
        ],
    );
    let committed_command_id = recovered["result"]["command_id"]
        .as_str()
        .unwrap_or_else(|| panic!("committed decision recovered: {recovered}"))
        .to_string();
    let interrupted = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "selector",
            "--replay-key",
            &interrupted_key,
        ],
    );
    assert!(
        interrupted["result"].is_null(),
        "interrupted decision is durably absent: {interrupted}"
    );

    // Only the committed proposal became Work.
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 1);

    // Recovery: clear the fault and re-run — the committed proposal
    // replays its recorded decision (same command id), the interrupted
    // one admits normally. Nothing is duplicated.
    clear_mid_batch_fault(&root);
    let result = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = result["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2, "{result}");
    assert!(
        proposals.iter().all(|p| p["status"] == "accepted"),
        "{result}"
    );
    let replayed = proposals
        .iter()
        .find(|p| p["replay_key"] == committed_key)
        .unwrap_or_else(|| panic!("committed proposal is in the re-run: {result}"));
    assert_eq!(replayed["command_id"], committed_command_id, "{result}");
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 2);
}

/// Install a deterministic mid-batch store fault on the live store: the
/// first selector decision save commits, the second is aborted by a
/// trigger, so its transaction rolls back. The counter row is seeded —
/// an `UPDATE` of an empty table changes nothing and would never trip
/// the abort.
fn inject_mid_batch_fault(root: &Path) {
    let conn = rusqlite::Connection::open(root.join("workloop.db")).expect("store opens");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS fault_counter (n INTEGER NOT NULL DEFAULT 0);
         INSERT INTO fault_counter (n) VALUES (0);
         CREATE TRIGGER mid_batch_fault BEFORE INSERT ON commands
         WHEN NEW.origin = 'selector'
         BEGIN
           UPDATE fault_counter SET n = n + 1;
           SELECT RAISE(ABORT, 'injected mid-batch fault')
             WHERE (SELECT n FROM fault_counter) >= 2;
         END;",
    )
    .expect("fault trigger installs");
}

fn clear_mid_batch_fault(root: &Path) {
    let conn = rusqlite::Connection::open(root.join("workloop.db")).expect("store opens");
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS mid_batch_fault;
         DROP TABLE IF EXISTS fault_counter;",
    )
    .expect("fault trigger clears");
}

/// Raise the installation's pending-queue limit directly in the store:
/// the envelope-bound test needs more admitted Works than the default
/// limit of 10, and the guarded settings writer is core API the CLI
/// surface does not expose yet.
fn raise_pending_limit(root: &Path, limit: i64) {
    let conn = rusqlite::Connection::open(root.join("workloop.db")).expect("store opens");
    conn.execute(
        "INSERT OR REPLACE INTO intake_settings \
         (singleton, settings_revision, pending_queue_limit, recorded_seq) \
         VALUES (1, 'test-limit', ?1, 0)",
        [limit],
    )
    .expect("pending limit writes");
}

/// Overwrite a recorded decision's payload so every later read of that
/// replay key fails to deserialize: both `handle_command`'s idempotency
/// read and the recovery lookup then error, which is an *undetermined*
/// batch fate — distinct from the mid-batch fault's definitely-absent
/// one.
fn corrupt_recorded_decision(root: &Path, replay_key: &str) {
    let conn = rusqlite::Connection::open(root.join("workloop.db")).expect("store opens");
    let updated = conn
        .execute(
            "UPDATE commands SET decision = 'not-json' \
             WHERE origin = 'selector' AND idempotency_key = ?1",
            [replay_key],
        )
        .expect("recorded decision corrupts");
    assert_eq!(updated, 1, "one recorded decision matches {replay_key}");
}

/// An undetermined batch fate emits `unknown` carrying the interrupted
/// proposal's own mutation identity — kind `submit`, origin `selector`,
/// its replay key — exactly like a single mutation's lost response, so
/// `command-result` recovery keys on the right `(origin, actor, key)`
/// instead of the propose run's query identity.
#[test]
fn an_undetermined_batch_fate_reports_the_interrupted_proposal_identity() {
    let root = root("propose-unknown");
    configure(
        &root,
        serde_json::json!({"adapter": "issue-scan", "allowlist": ["ops"]}),
    );
    let snapshot = write_snapshot(
        &root,
        "snapshot.json",
        serde_json::json!([
            issue("ops", 1, "open", "First"),
            issue("ops", 2, "open", "Second"),
        ]),
    );
    let first = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = first["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2, "{first}");
    let committed_key = proposals[0]["replay_key"].as_str().unwrap().to_string();
    let interrupted_key = proposals[1]["replay_key"].as_str().unwrap().to_string();
    // The actor label is the installation's, identical to a manual
    // submit's — the interrupted identity is not a fresh one.
    let manual = run_at(
        &root,
        &[
            "submit",
            "--intent",
            "actor probe",
            "--idempotency-key",
            "k-actor",
        ],
    );
    let actor = manual["command"]["actor"].as_str().unwrap().to_string();

    corrupt_recorded_decision(&root, &interrupted_key);
    let (code, env) = fail_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    assert_eq!(code, 6, "{env}");
    assert_eq!(env["status"], "unknown", "{env}");
    assert_eq!(env["reason"]["code"], "unknown", "{env}");
    let cmd = &env["command"];
    assert_eq!(cmd["kind"], "submit", "{env}");
    assert_eq!(cmd["idempotency_key"], interrupted_key.as_str(), "{env}");
    assert_eq!(cmd["actor"], actor.as_str(), "{env}");

    // Both replay keys ride the envelope: the committed decision still
    // recovers, the interrupted one is named by the mutation identity
    // above.
    let refs = env["evidence_refs"].as_array().unwrap();
    let refs: Vec<&str> = refs.iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(
        refs,
        vec![committed_key.as_str(), interrupted_key.as_str()],
        "{env}"
    );
    let recovered = run_at(
        &root,
        &[
            "command-result",
            "--origin",
            "selector",
            "--replay-key",
            &committed_key,
        ],
    );
    assert_eq!(
        recovered["result"]["command_id"], proposals[0]["command_id"],
        "{recovered}"
    );
    // Nothing new was admitted: the interrupted proposal recorded
    // nothing, the committed one replayed.
    let list = run_at(&root, &["work-list"]);
    assert_eq!(list["result"]["items"].as_array().unwrap().len(), 3);
}

/// The batch's terminal `unknown` envelope lists replay keys under the
/// envelope's own evidence bounds: with far more recoverable keys than
/// the item bound, the list is capped and every surviving entry stays
/// within the per-item limit — the bounded contract surface holds even
/// for an unbounded `max_proposals`.
#[test]
fn batch_evidence_refs_stay_within_the_envelope_bounds() {
    let root = root("propose-evibound");
    configure(
        &root,
        serde_json::json!({
            "adapter": "issue-scan",
            "allowlist": ["ops"],
            "max_proposals": 32,
        }),
    );
    // Twenty admitted Works exceed the default pending limit of 10;
    // capacity-deferred proposals record no decision and cannot fill
    // the recoverable-key list this test bounds.
    raise_pending_limit(&root, 64);
    let issues: Vec<serde_json::Value> = (1u64..=20)
        .map(|n| issue("ops", n, "open", "Issue"))
        .collect();
    let snapshot = write_snapshot(&root, "snapshot.json", serde_json::json!(issues));
    let first = run_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    let proposals = first["result"]["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 20, "{first}");
    let last_key = proposals[19]["replay_key"].as_str().unwrap().to_string();
    corrupt_recorded_decision(&root, &last_key);

    let (code, env) = fail_at(
        &root,
        &["propose", "--snapshot", &snapshot.to_string_lossy()],
    );
    assert_eq!(code, 6, "{env}");
    assert_eq!(env["status"], "unknown", "{env}");
    // 19 replayed + 1 interrupted = 20 recoverable keys, capped at the
    // envelope's evidence bound; each surviving entry is within the
    // per-item bound.
    let refs = env["evidence_refs"].as_array().unwrap();
    assert_eq!(refs.len(), 16, "{env}");
    for r in refs {
        assert!(r.as_str().unwrap().chars().count() <= 240, "{env}");
    }
    // The interrupted proposal's identity — the key recovery needs —
    // survives in the structured command field even when the list
    // bound trimmed it.
    assert_eq!(
        env["command"]["idempotency_key"],
        last_key.as_str(),
        "{env}"
    );
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

    // Outcome semantics survive the switch too: cancelling the
    // pre-existing Work stores its terminal Outcome, and the later
    // switch back to manual-only neither rewrites nor drops it.
    let version = items
        .iter()
        .find(|i| i["work_id"].as_str() == Some(existing.as_str()))
        .and_then(|i| i["version"].as_u64())
        .expect("the pre-existing Work is listed");
    run_at(
        &root,
        &[
            "cancel",
            "--work",
            &existing,
            "--expected-version",
            &version.to_string(),
        ],
    );
    let after_cancel = run_at(&root, &["work-list"]);
    let cancelled = work_item(&after_cancel, &existing);
    assert_eq!(cancelled["phase"], "stopped");
    assert_eq!(cancelled["outcome"]["state"], "cancelled");

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
    let list = run_at(&root, &["work-list"]);
    let items = list["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let cancelled = work_item(&list, &existing);
    assert_eq!(
        cancelled["outcome"]["state"], "cancelled",
        "the Outcome recorded under one admission path survives the switch"
    );
    let queued = items.iter().filter(|i| i["phase"] == "queued").count();
    assert_eq!(
        queued, 2,
        "the selector Work and the new manual Work share the queue"
    );
}

/// One `work-list` item by Work id.
fn work_item<'a>(list: &'a serde_json::Value, work_id: &str) -> &'a serde_json::Value {
    list["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["work_id"].as_str() == Some(work_id))
        .unwrap_or_else(|| panic!("Work {work_id} is listed: {list}"))
}
