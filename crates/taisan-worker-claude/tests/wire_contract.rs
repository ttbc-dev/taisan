//! Wire-contract tests for `taisan-worker-claude`: the reference
//! adapter driving a real coding-agent runtime, exercised here
//! against a scriptable fake `claude` executable. These tests assert
//! the exact `taisan.worker.declaration/1` + `request/2` + `report/2`
//! behavior a production launch depends on — no model calls, no
//! network, no credentials.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};
use workloop_core::{
    domain::{InvocationRole, canonical_json},
    worker_v2::{
        ReportBinding, ReportClassification, WorkerDeclarationV1, WorkerRequestV2, classify_report,
        request_v2_digest,
    },
};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn shim() -> &'static str {
    env!("CARGO_BIN_EXE_taisan-worker-claude")
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "twc-test-{}-{}-{}",
        name,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A scriptable fake `claude` runtime. Behavior is driven entirely by
/// env vars so each test scripts exactly one scenario:
///   FAKE_EVENTS     file of NDJSON stream events to emit on stdout
///   FAKE_EXIT       process exit code (default 0)
///   FAKE_HANG       sleep N seconds before doing anything else
///   FAKE_FLOOD      bytes of stdout noise to emit before events
///   FAKE_WRITE      relative file path to create inside the workspace
///   FAKE_COMMIT     when set, `git add -A && git commit` inside cwd
///   FAKE_PROMPT_CAPTURE  file that receives the `-p` prompt text
fn fake_runtime(dir: &Path) -> PathBuf {
    let path = dir.join("fake-claude");
    fs::write(
        &path,
        r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = "--version" ]; then
    echo "fake-claude 9.9.9"
    exit 0
  fi
done
prev=""
for a in "$@"; do
  if [ "$prev" = "-p" ] && [ -n "$FAKE_PROMPT_CAPTURE" ]; then
    printf '%s' "$a" > "$FAKE_PROMPT_CAPTURE"
  fi
  prev="$a"
done
if [ -n "$FAKE_PGID_FILE" ]; then echo $$ > "$FAKE_PGID_FILE"; fi
if [ -n "$FAKE_GRANDCHILD" ]; then sleep "$FAKE_GRANDCHILD" & fi
if [ -n "$FAKE_HANG" ]; then sleep "$FAKE_HANG"; fi
if [ -n "$FAKE_FLOOD" ]; then yes 'flood line' | head -c "$FAKE_FLOOD"; fi
if [ -n "$FAKE_WRITE" ]; then printf 'fake output\n' > "$FAKE_WRITE"; fi
if [ -n "$FAKE_COMMIT" ]; then
  git add -A >/dev/null 2>&1
  git -c user.email=fake@test -c user.name=fake commit -qm fake >/dev/null 2>&1
fi
if [ -n "$FAKE_STDERR" ]; then printf '%s\n' "$FAKE_STDERR" >&2; fi
if [ -n "$FAKE_EVENTS" ]; then cat "$FAKE_EVENTS"; fi
exit "${FAKE_EXIT:-0}"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs([
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A workspace the runtime may treat as its checkout: a real git repo
/// so `claimed_head` and review `git diff` resolve.
fn workspace(dir: &Path) -> PathBuf {
    let ws = dir.join("workspace");
    fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    fs::write(ws.join("file.txt"), "base\n").unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-qm", "init"]);
    ws
}

fn goal_ref() -> Value {
    json!({"ref": "goal-1", "media_type": "text/markdown", "bytes": 64, "digest": "a".repeat(64)})
}

fn hex(byte: u8) -> String {
    format!("{:02x}", byte).repeat(32)
}

fn budget() -> Value {
    json!({
        "deadline_unix_seconds": 4_000_000_000i64,
        "max_runtime_seconds": 120,
        "max_turns": 10,
        "max_report_bytes": 1_048_576,
        "max_log_bytes": 4_194_304,
        "max_verification_commands": 8,
        "max_continuation_refs": 4,
    })
}

/// Build a syntactically complete `taisan.worker.request/2` body for
/// one role, with a real `request_digest` the adapter verifies.
fn request_json(role: &str, task: Value, mount: &Path, read_only: bool) -> Value {
    let write_cap = if read_only {
        "workspace_read_only"
    } else {
        "workspace_write"
    };
    let mut request = json!({
        "format_version": "taisan.worker.request/2",
        "request_id": "e-1:inv:1",
        "invocation_id": "inv-1",
        "store_id": "store-1",
        "work_id": "w-1",
        "execution_id": "e-1",
        "role": role,
        "repository": {"host": "github.com", "repository": "public/example"},
        "base": {"repository": {"host": "github.com", "repository": "public/example"},
                 "object": hex(0x0a)},
        "spec": {"digest": hex(0x0b), "revision": 1},
        "workspace": {"workspace_id": "ws-1", "mount": mount.to_string_lossy(),
                      "read_only": read_only},
        "budget": budget(),
        "required_capabilities": {
            "role": role,
            "protocol_versions": ["taisan.worker.request/2", "taisan.worker.report/2"],
            "capabilities": ["invocation_transport.argv-v2", "report_json", "process_control", write_cap],
            "resume": "unsupported",
            "verification_egress_hosts": [],
        },
        "continuation": [],
        "task": task,
    });
    let digest = request_v2_digest(&canonical_json(&request));
    request["request_digest"] = json!(digest);
    // Sanity: the adapter's own parser must accept what tests mint.
    WorkerRequestV2::parse(&canonical_json(&request)).expect("test request must parse");
    request
}

/// Materialize one dispatch directory: `request.json`, `input/`, `out/`.
fn dispatch(name: &str, request: &Value, inputs: &[(&str, &str)]) -> (PathBuf, PathBuf, PathBuf) {
    let dir = tmp(name);
    let request_path = dir.join("request.json");
    fs::write(&request_path, canonical_json(request)).unwrap();
    let input = dir.join("input");
    fs::create_dir_all(&input).unwrap();
    for (name, body) in inputs {
        fs::write(input.join(name), body).unwrap();
    }
    let out = dir.join("out");
    (request_path, input, out)
}

fn issue_snapshot() -> String {
    String::from_utf8(canonical_json(&json!({
        "format": "taisan.issue-snapshot/1",
        "issue": {"host": "github.com", "repository": "public/example", "issue": "42"},
        "source_revision": "7",
        "title": "Example issue title",
        "body": "Issue body text.",
        "content_digest": hex(0x0c),
    })))
    .unwrap()
}

fn events_file(dir: &Path, result_text: &str, extra: Value) -> PathBuf {
    let path = dir.join("events.ndjson");
    let mut event = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": result_text,
        "num_turns": 3,
        "duration_ms": 42,
    });
    if let (Some(dst), Some(src)) = (event.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    path
}

fn run_shim(
    fake: &Path,
    request: &Path,
    out: &Path,
    cwd: &Path,
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new(shim());
    cmd.args(["--runtime"])
        .arg(fake)
        .args(["run", "--request"])
        .arg(request)
        .args(["--out"])
        .arg(out)
        .current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn report(out: &Path) -> Value {
    let bytes = fs::read(out.join("report.json")).expect("report.json must exist");
    serde_json::from_slice(&bytes).expect("report.json must parse")
}

/// The supervisor-side classification: the report must validate as a
/// real `taisan.worker.report/2` for this exact request.
fn classified(out: &Path, request: &Value) -> ReportClassification {
    let bytes = fs::read(out.join("report.json")).expect("report.json must exist");
    let binding = ReportBinding {
        request_id: request["request_id"].as_str().unwrap(),
        invocation_id: request["invocation_id"].as_str().unwrap(),
        request_digest: request["request_digest"].as_str().unwrap(),
        role: serde_json::from_value::<InvocationRole>(request["role"].clone()).unwrap(),
    };
    classify_report(
        &bytes,
        &binding,
        request["budget"]["max_report_bytes"].as_u64().unwrap(),
    )
    .classification
}

// ----------------------------------------------------------------------

#[test]
fn describe_emits_valid_declaration() {
    let dir = tmp("describe");
    let fake = fake_runtime(&dir);
    let out = Command::new(shim())
        .args(["--runtime"])
        .arg(&fake)
        .arg("describe")
        .output()
        .unwrap();
    assert!(out.status.success(), "describe failed: {:?}", out.stderr);
    let decl = WorkerDeclarationV1::parse(&out.stdout).expect("declaration must parse");
    assert_eq!(decl.adapter, "taisan-worker-claude");
    assert_eq!(decl.runtime.product, "claude-code");
    assert_eq!(decl.runtime.version, "fake-claude 9.9.9");
    assert_eq!(decl.resume, "unsupported");
    assert_eq!(decl.checkpoints, "workspace_commits");
    assert!(decl.runtime_artifact_store.is_null());
    for role in ["coding", "fix", "invariant_check", "review"] {
        assert!(decl.roles.contains(&role.to_string()), "missing {role}");
    }

    // The declaration must negotiate against a real v2 request.
    let ws = workspace(&dir);
    let req = request_json(
        "coding",
        json!({
            "kind": "coding",
            "goal": goal_ref(),
            "issues": [],
            "constraints": [],
            "verification_argv": [],
        }),
        &ws,
        false,
    );
    let typed = WorkerRequestV2::parse(&canonical_json(&req)).unwrap();
    decl.negotiate_v2(&typed).expect("negotiation must pass");
}

#[test]
fn describe_fails_without_runtime() {
    let out = Command::new(shim())
        .args(["--runtime", "/no/such/runtime-binary", "describe"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "no declaration on probe failure");
}

#[test]
fn version_probe_reports_runtime_version() {
    let dir = tmp("version");
    let fake = fake_runtime(&dir);
    let out = Command::new(shim())
        .args(["--runtime"])
        .arg(&fake)
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("fake-claude 9.9.9"), "{text}");
}

#[test]
fn run_rejects_malformed_request_without_report() {
    let dir = tmp("malformed");
    let fake = fake_runtime(&dir);
    let request_path = dir.join("request.json");
    fs::write(&request_path, b"{not json").unwrap();
    let out_dir = dir.join("out");
    let out = run_shim(&fake, &request_path, &out_dir, &dir, &[]);
    assert!(!out.status.success());
    assert!(!out_dir.join("report.json").exists(), "absent report");
}

#[test]
fn run_rejects_digest_mismatch_without_report() {
    let dir = tmp("bad-digest");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let mut req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    req["request_digest"] = json!("0".repeat(64));
    let (request_path, _, out_dir) = dispatch("bad-digest", &req, &[]);
    let out = run_shim(&fake, &request_path, &out_dir, &ws, &[]);
    assert!(!out.status.success());
    assert!(!out_dir.join("report.json").exists(), "absent report");
}

#[test]
fn coding_success_reports_head_and_checks() {
    let dir = tmp("coding-ok");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(
        &dir,
        "implemented\n\n```taisan-result\n{\"notes\":\"done\"}\n```",
        json!({}),
    );
    let capture = dir.join("prompt.txt");
    let req = request_json(
        "coding",
        json!({
            "kind": "coding",
            "goal": goal_ref(),
            "issues": [{
                "order": 0,
                "issue": {"host": "github.com", "repository": "public/example", "issue": "42"},
                "source_revision": "7",
                "snapshot_digest": hex(0x0d),
                "body": {"ref": "issue-1", "bytes": 128, "digest": hex(0x0e)},
            }],
            "constraints": ["keep it small"],
            "verification_argv": [["sh", "-c", "exit 0"]],
        }),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch(
        "coding-ok",
        &req,
        &[
            ("goal.md", "Implement the feature.\n"),
            ("issue-0.json", &issue_snapshot()),
        ],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_EVENTS", events.to_str().unwrap()),
            ("FAKE_WRITE", "produced.txt"),
            ("FAKE_COMMIT", "1"),
            ("FAKE_PROMPT_CAPTURE", capture.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "run failed: {:?}", out.stderr);

    let r = report(&out_dir);
    assert_eq!(r["status"], "success");
    assert_eq!(r["reason"]["code"], "completed");
    let head = git(&ws, &["rev-parse", "HEAD"]);
    assert_eq!(r["claimed_head"].as_str().unwrap(), head);
    assert_eq!(
        r["continuation"]["workspace_commit"].as_str().unwrap(),
        head
    );
    assert_eq!(r["claimed_checks"][0]["status"], "passed");
    assert_eq!(r["claimed_checks"][0]["exit_code"], 0);
    assert_eq!(r["runtime_usage"]["turns"], 3);
    assert_eq!(r["runtime_usage"]["runtime_product"], "claude-code");
    assert_eq!(r["log"]["truncated"], false);
    assert!(out_dir.join("runtime.log").exists());
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Success
    ));

    // The shim translated the request into a prompt containing the
    // goal, the issue snapshot, and the declared check.
    let prompt = fs::read_to_string(&capture).unwrap();
    assert!(prompt.contains("Implement the feature"));
    assert!(prompt.contains("Example issue title"));
    assert!(prompt.contains("keep it small"));
    // Workspace holds the produced change.
    assert_eq!(
        fs::read_to_string(ws.join("produced.txt")).unwrap(),
        "fake output\n"
    );
}

#[test]
fn coding_failed_check_reports_negative() {
    let dir = tmp("coding-fail");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(&dir, "done", json!({}));
    let req = request_json(
        "coding",
        json!({
            "kind": "coding",
            "goal": goal_ref(),
            "issues": [],
            "constraints": [],
            "verification_argv": [["sh", "-c", "exit 3"]],
        }),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("coding-fail", &req, &[("goal.md", "goal\n")]);
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "negative");
    assert_eq!(r["reason"]["code"], "checks_failed");
    assert_eq!(r["claimed_checks"][0]["exit_code"], 3);
    assert_eq!(r["claimed_checks"][0]["status"], "failed");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Negative
    ));
}

#[test]
fn runtime_error_reports_unavailable() {
    let dir = tmp("rt-error");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(
        &dir,
        "provider exploded",
        json!({"subtype": "error_during_execution", "is_error": true}),
    );
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("rt-error", &req, &[("goal.md", "goal\n")]);
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_EVENTS", events.to_str().unwrap()),
            ("FAKE_EXIT", "1"),
        ],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "unavailable");
    assert_eq!(r["reason"]["code"], "provider_unavailable");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Unavailable
    ));
}

#[test]
fn missing_runtime_binary_reports_unavailable() {
    let dir = tmp("no-runtime");
    let ws = workspace(&dir);
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("no-runtime", &req, &[("goal.md", "goal\n")]);
    let out = run_shim(
        Path::new("/no/such/runtime-binary"),
        &request_path,
        &out_dir,
        &ws,
        &[],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "unavailable");
    assert_eq!(r["reason"]["code"], "provider_unavailable");
}

#[test]
fn deadline_reports_limit_reached_and_kills_tree() {
    let dir = tmp("deadline");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let mut req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    req["budget"]["max_runtime_seconds"] = json!(1);
    let digest = request_v2_digest(&canonical_json(&{
        let mut v = req.clone();
        v.as_object_mut().unwrap().remove("request_digest");
        v
    }));
    req["request_digest"] = json!(digest);
    WorkerRequestV2::parse(&canonical_json(&req)).unwrap();
    let (request_path, _, out_dir) = dispatch("deadline", &req, &[("goal.md", "goal\n")]);
    let pgid_file = dir.join("pgid");
    let start = std::time::Instant::now();
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_HANG", "60"),
            // A grandchild inside the runtime's own group — the deadline
            // kill must reap the whole tree, not just the leader.
            ("FAKE_GRANDCHILD", "60"),
            ("FAKE_PGID_FILE", pgid_file.to_str().unwrap()),
        ],
    );
    assert!(out.status.success());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "hung runtime must be killed, not awaited"
    );
    let r = report(&out_dir);
    assert_eq!(r["status"], "limit_reached");
    assert_eq!(r["reason"]["code"], "deadline_reached");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Failure { .. }
    ));
    // The runtime ran in its own process group (`$$` == pgid); after
    // the kill, no member of that group may still be *alive*. Killed
    // members can linger in the table indefinitely as zombies until
    // their new parent reaps them — CI's PID 1 does not — so a bare
    // `kill -0 -pgid` probe is not the right check. Scan the table and
    // require every remaining member to be a zombie.
    let pgid: i32 = fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let table = Command::new("ps")
            .args(["-eo", "pgid=,stat="])
            .output()
            .unwrap();
        let live: Vec<String> = String::from_utf8_lossy(&table.stdout)
            .lines()
            .filter_map(|line| {
                let mut cols = line.split_whitespace();
                let p: i64 = cols.next()?.parse().ok()?;
                let stat = cols.next()?;
                (p == pgid as i64 && !stat.starts_with('Z')).then(|| line.trim().to_string())
            })
            .collect();
        if live.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "runtime process group {pgid} survived: {live:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn runtime_max_turns_reports_limit_reached() {
    let dir = tmp("max-turns");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(
        &dir,
        "stopped",
        json!({"subtype": "error_max_turns", "is_error": true}),
    );
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("max-turns", &req, &[("goal.md", "goal\n")]);
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_EVENTS", events.to_str().unwrap()),
            ("FAKE_EXIT", "1"),
        ],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "limit_reached");
    assert_eq!(r["reason"]["code"], "max_turns_reached");
}

#[test]
fn log_flood_reports_output_limit_reached() {
    let dir = tmp("flood");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let mut req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    req["budget"]["max_log_bytes"] = json!(2048);
    let digest = request_v2_digest(&canonical_json(&{
        let mut v = req.clone();
        v.as_object_mut().unwrap().remove("request_digest");
        v
    }));
    req["request_digest"] = json!(digest);
    WorkerRequestV2::parse(&canonical_json(&req)).unwrap();
    let events = events_file(&dir, "done", json!({}));
    let (request_path, _, out_dir) = dispatch("flood", &req, &[("goal.md", "goal\n")]);
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_FLOOD", "1048576"),
            ("FAKE_EVENTS", events.to_str().unwrap()),
        ],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "limit_reached");
    assert_eq!(r["reason"]["code"], "output_limit_reached");
    assert_eq!(r["log"]["truncated"], true);
    assert!(fs::metadata(out_dir.join("runtime.log")).unwrap().len() <= 4096);
}

#[test]
fn review_reports_findings() {
    let dir = tmp("review");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    git(&ws, &["commit", "--allow-empty", "-qm", "head-commit"]);
    let base = git(&ws, &["rev-parse", "HEAD~1"]);
    let head = git(&ws, &["rev-parse", "HEAD"]);
    let block = "```taisan-result\n{\"findings\":[{\"key\":\"f-1\",\"summary\":\"off-by-one in loop\",\"severity\":\"medium\",\"position\":\"file.txt:1\"}]}\n```";
    let events = events_file(&dir, &format!("reviewed\n\n{block}"), json!({}));
    let req = request_json(
        "review",
        json!({
            "kind": "code_review",
            "target": {"change": "chg-1", "base": base, "head": head, "diff_digest": hex(0x0f)},
            "review_source": {"source": "gh-review", "bytes": 256, "digest": hex(0x10), "source_revision": "1"},
        }),
        &ws,
        true,
    );
    let review_source = canonical_json(&json!({
        "format": "taisan.review-source/1",
        "work_id": "w-1",
        "target": req["task"]["target"],
        "prior_round": null,
    }));
    let (request_path, _, out_dir) = dispatch(
        "review",
        &req,
        &[(
            "review-source.json",
            std::str::from_utf8(&review_source).unwrap(),
        )],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success(), "{:?}", out.stderr);
    let r = report(&out_dir);
    assert_eq!(r["status"], "negative");
    assert_eq!(r["reason"]["code"], "findings_reported");
    assert_eq!(r["findings"][0]["key"], "f-1");
    assert_eq!(r["findings"][0]["summary"], "off-by-one in loop");
    assert!(r["claimed_head"].is_null(), "read-only role claims no head");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Negative
    ));
}

#[test]
fn review_empty_findings_is_success() {
    let dir = tmp("review-clean");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let head = git(&ws, &["rev-parse", "HEAD"]);
    let block = "```taisan-result\n{\"findings\":[]}\n```";
    let events = events_file(&dir, &format!("clean\n\n{block}"), json!({}));
    let req = request_json(
        "review",
        json!({
            "kind": "code_review",
            "target": {"change": "chg-1", "base": head, "head": head, "diff_digest": hex(0x0f)},
            "review_source": {"source": "gh-review", "bytes": 64, "digest": hex(0x10), "source_revision": "1"},
        }),
        &ws,
        true,
    );
    let review_source = canonical_json(&json!({
        "format": "taisan.review-source/1",
        "work_id": "w-1",
        "target": req["task"]["target"],
        "prior_round": null,
    }));
    let (request_path, _, out_dir) = dispatch(
        "review-clean",
        &req,
        &[(
            "review-source.json",
            std::str::from_utf8(&review_source).unwrap(),
        )],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "success");
    assert_eq!(r["reason"]["code"], "completed");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Success
    ));
}

#[test]
fn invariant_check_violation_reports_negative() {
    let dir = tmp("inv-check");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let block = "```taisan-result\n{\"invariant_items\":[{\"id\":\"inv-a\",\"verdict\":\"conforms\"},{\"id\":\"inv-b\",\"verdict\":\"violates\"}]}\n```";
    let events = events_file(&dir, &format!("assessed\n\n{block}"), json!({}));
    let req = request_json(
        "invariant_check",
        json!({
            "kind": "invariant_check",
            "artifact": {"artifact_id": "art-1", "manifest_digest": hex(0x11), "produced": hex(0x12)},
            "invariant_set": {"id": "set-1", "revision": 2, "digest": hex(0x13)},
        }),
        &ws,
        true,
    );
    let set = canonical_json(&json!({
        "format": "taisan.invariant-set/1",
        "id": "set-1",
        "revision": 2,
        "items": ["inv-a", "inv-b"],
    }));
    let (request_path, _, out_dir) = dispatch(
        "inv-check",
        &req,
        &[("invariant-set.json", std::str::from_utf8(&set).unwrap())],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "negative");
    assert_eq!(r["reason"]["code"], "invariant_violated");
    assert_eq!(r["invariant_items"][1]["verdict"], "violates");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Negative
    ));
}

#[test]
fn invariant_check_conforms_is_success() {
    let dir = tmp("inv-ok");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let block = "```taisan-result\n{\"invariant_items\":[{\"id\":\"inv-a\",\"verdict\":\"conforms\"}]}\n```";
    let events = events_file(&dir, &format!("assessed\n\n{block}"), json!({}));
    let req = request_json(
        "invariant_check",
        json!({
            "kind": "invariant_check",
            "artifact": {"artifact_id": "art-1", "manifest_digest": hex(0x11), "produced": hex(0x12)},
            "invariant_set": {"id": "set-1", "revision": 2, "digest": hex(0x13)},
        }),
        &ws,
        true,
    );
    let set = canonical_json(&json!({
        "format": "taisan.invariant-set/1",
        "id": "set-1",
        "revision": 2,
        "items": ["inv-a"],
    }));
    let (request_path, _, out_dir) = dispatch(
        "inv-ok",
        &req,
        &[("invariant-set.json", std::str::from_utf8(&set).unwrap())],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "success");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Success
    ));
}

#[test]
fn reader_role_without_result_block_is_typed_failure() {
    let dir = tmp("no-block");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let head = git(&ws, &["rev-parse", "HEAD"]);
    // Runtime exits cleanly but never emits the result block — its
    // output cannot be promoted into an evidence claim.
    let events = events_file(&dir, "some prose with no block", json!({}));
    let req = request_json(
        "review",
        json!({
            "kind": "code_review",
            "target": {"change": "chg-1", "base": head, "head": head, "diff_digest": hex(0x0f)},
            "review_source": {"source": "gh-review", "bytes": 64, "digest": hex(0x10), "source_revision": "1"},
        }),
        &ws,
        true,
    );
    let review_source = canonical_json(&json!({
        "format": "taisan.review-source/1",
        "work_id": "w-1",
        "target": req["task"]["target"],
        "prior_round": null,
    }));
    let (request_path, _, out_dir) = dispatch(
        "no-block",
        &req,
        &[(
            "review-source.json",
            std::str::from_utf8(&review_source).unwrap(),
        )],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["reason"]["code"], "internal_error");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Failure { .. }
    ));
}

#[test]
fn fix_request_translates_review_findings_into_prompt() {
    let dir = tmp("fix");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(&dir, "fixed", json!({}));
    let capture = dir.join("prompt.txt");
    let req = request_json(
        "fix",
        json!({
            "kind": "fix",
            "goal": goal_ref(),
            "issues": [],
            "predecessor": {"artifact_id": "art-9", "manifest_digest": hex(0x14), "produced": hex(0x15)},
            "origin": {
                "kind": "review",
                "target": {"change": "chg-1", "base": hex(0x16), "head": hex(0x17), "diff_digest": hex(0x18)},
                "round": "round-1",
                "judgment": "judgment-1",
                "findings": [{
                    "occurrence": "occ-1",
                    "source": "gh-review",
                    "digest": hex(0x19),
                    "order": 0,
                    "summary": "rename the helper before merging",
                }],
            },
            "constraints": [],
            "verification_argv": [["sh", "-c", "exit 0"]],
        }),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("fix", &req, &[("goal.md", "Repair it.\n")]);
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_EVENTS", events.to_str().unwrap()),
            ("FAKE_COMMIT", "1"),
            ("FAKE_PROMPT_CAPTURE", capture.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{:?}", out.stderr);
    let r = report(&out_dir);
    assert_eq!(r["status"], "success");
    let prompt = fs::read_to_string(&capture).unwrap();
    assert!(prompt.contains("Repair it."));
    assert!(prompt.contains("rename the helper before merging"));
    assert!(prompt.contains("art-9"));
}

#[test]
fn missing_required_input_reports_unavailable() {
    let dir = tmp("no-input");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    // No input/goal.md materialized.
    let (request_path, _, out_dir) = dispatch("no-input", &req, &[]);
    let out = run_shim(&fake, &request_path, &out_dir, &ws, &[]);
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "unavailable");
    assert_eq!(r["reason"]["code"], "missing_required_input");
    assert!(matches!(
        classified(&out_dir, &req),
        ReportClassification::Unavailable
    ));
}

#[test]
fn malformed_assessment_member_fails_the_report() {
    let dir = tmp("bad-member");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let head = git(&ws, &["rev-parse", "HEAD"]);
    // A finding with an empty key violates the closed member shape —
    // the shim must fail the report, never ship the member or drop it.
    let block =
        "```taisan-result\n{\"findings\":[{\"key\":\"\",\"summary\":\"unbound finding\"}]}\n```";
    let events = events_file(&dir, &format!("reviewed\n\n{block}"), json!({}));
    let req = request_json(
        "review",
        json!({
            "kind": "code_review",
            "target": {"change": "chg-1", "base": head, "head": head, "diff_digest": hex(0x0f)},
            "review_source": {"source": "gh-review", "bytes": 64, "digest": hex(0x10), "source_revision": "1"},
        }),
        &ws,
        true,
    );
    let review_source = canonical_json(&json!({
        "format": "taisan.review-source/1",
        "work_id": "w-1",
        "target": req["task"]["target"],
        "prior_round": null,
    }));
    let (request_path, _, out_dir) = dispatch(
        "bad-member",
        &req,
        &[(
            "review-source.json",
            std::str::from_utf8(&review_source).unwrap(),
        )],
    );
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[("FAKE_EVENTS", events.to_str().unwrap())],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["reason"]["code"], "internal_error");
    assert!(r["findings"].as_array().unwrap().is_empty());
}

#[test]
fn read_only_role_claims_no_head_and_keeps_head_fixed() {
    let dir = tmp("ro-ws");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let block = "```taisan-result\n{\"invariant_items\":[{\"id\":\"inv-a\",\"verdict\":\"conforms\"}]}\n```";
    let events = events_file(&dir, &format!("assessed\n\n{block}"), json!({}));
    let req = request_json(
        "invariant_check",
        json!({
            "kind": "invariant_check",
            "artifact": {"artifact_id": "art-1", "manifest_digest": hex(0x11), "produced": hex(0x12)},
            "invariant_set": {"id": "set-1", "revision": 2, "digest": hex(0x13)},
        }),
        &ws,
        true,
    );
    let set = canonical_json(&json!({
        "format": "taisan.invariant-set/1",
        "id": "set-1",
        "revision": 2,
        "items": ["inv-a"],
    }));
    let before = git(&ws, &["rev-parse", "HEAD"]);
    let (request_path, _, out_dir) = dispatch(
        "ro-ws",
        &req,
        &[("invariant-set.json", std::str::from_utf8(&set).unwrap())],
    );
    // A runtime that tries to write anyway must not corrupt the
    // workspace claim surface: the shim reports no head for a
    // read-only role regardless of what the runtime touched.
    let out = run_shim(
        &fake,
        &request_path,
        &out_dir,
        &ws,
        &[
            ("FAKE_EVENTS", events.to_str().unwrap()),
            ("FAKE_WRITE", "should-not-matter.txt"),
        ],
    );
    assert!(out.status.success());
    let r = report(&out_dir);
    assert_eq!(r["status"], "success");
    assert!(r["claimed_head"].is_null());
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), before);
}

#[test]
fn replayed_request_produces_valid_reports() {
    let dir = tmp("replay");
    let fake = fake_runtime(&dir);
    let ws = workspace(&dir);
    let events = events_file(&dir, "done", json!({}));
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": []}),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("replay", &req, &[("goal.md", "goal\n")]);
    for i in 0..2 {
        let out_i = out_dir.with_file_name(format!("out-{i}"));
        let out = run_shim(
            &fake,
            &request_path,
            &out_i,
            &ws,
            &[("FAKE_EVENTS", events.to_str().unwrap())],
        );
        assert!(out.status.success());
        assert!(matches!(
            classified(&out_i, &req),
            ReportClassification::Success
        ));
    }
}

#[test]
fn auth_token_reaches_runtime_but_not_verification() {
    let dir = tmp("auth");
    let ws = workspace(&dir);
    let auth = dir.join("runtime-auth");
    fs::write(&auth, "sk-test-secret-token\n").unwrap();
    // Fake captures its env for assertion.
    let env_capture = dir.join("env.txt");
    let fake2 = dir.join("fake-env-claude");
    fs::write(
        &fake2,
        format!(
            "#!/bin/sh\nenv | grep -E '^(ANTHROPIC_API_KEY|GH_TOKEN)=' > {}\ncat {}\nexit 0\n",
            env_capture.display(),
            events_file(&dir, "done", json!({})).display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake2, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let req = request_json(
        "coding",
        json!({"kind": "coding", "goal": goal_ref(), "issues": [],
               "constraints": [], "verification_argv": [["sh", "-c", "env > verify-env.txt; exit 0"]]}),
        &ws,
        false,
    );
    let (request_path, _, out_dir) = dispatch("auth", &req, &[("goal.md", "goal\n")]);
    let mut cmd = Command::new(shim());
    cmd.args(["--runtime"])
        .arg(&fake2)
        .args(["--runtime-auth"])
        .arg(&auth)
        .args(["run", "--request"])
        .arg(&request_path)
        .args(["--out"])
        .arg(&out_dir)
        .current_dir(&ws)
        .env("GH_TOKEN", "gh-secret-should-strip");
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{:?}", out.stderr);
    let r = report(&out_dir);
    assert_eq!(r["status"], "success");

    // The runtime saw the staged token and never the publication token.
    let runtime_env = fs::read_to_string(&env_capture).unwrap();
    assert!(runtime_env.contains("ANTHROPIC_API_KEY=sk-test-secret-token"));
    assert!(!runtime_env.contains("GH_TOKEN"));
    // Verification claims ran without either credential.
    let verify_env = fs::read_to_string(ws.join("verify-env.txt")).unwrap();
    assert!(!verify_env.contains("sk-test-secret-token"));
    assert!(!verify_env.contains("gh-secret-should-strip"));
}
