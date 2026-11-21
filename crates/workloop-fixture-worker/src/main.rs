//! Fixture worker: a real process the contract tests spawn. Behavior is
//! driven by argv so tests can model every writer shape the supervisor
//! must handle. The managed workspace is the process's cwd.
// Unreaped children are the point of this binary: `zombie`, `setsid-escape`,
// and `orphan-writer` deliberately never wait on their spawned children.
#![allow(clippy::zombie_processes)]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const USAGE: &str = "usage: workloop-fixture-worker <mode> [--interval-ms <n>] \
    [--file <name>] [--runtime-version <v>] [--adapter-version <v>] \
    [--wire v1|v2] [--decl-file <path>] [--request <path>] [--out <dir>]\nmodes: \
    clean-exit | exit-no-artifact | hang | setsid-escape | orphan-writer | \
    zombie | path-reopen | makedirs-writer | describe | run | env-report\ninternal: \
    escaped-writer | exit-now";

struct Args {
    mode: String,
    /// The protocol subcommand a trailing positional selects:
    /// `describe` prints the declaration, `run` executes the v2
    /// `--request/--out` transport. `None` runs the base mode directly.
    subcommand: Option<String>,
    interval: Duration,
    file: String,
    runtime_version: Option<String>,
    adapter_version: Option<String>,
    wire_v2: bool,
    decl_file: Option<String>,
    request: Option<String>,
    out: Option<String>,
    /// Test-only report shaping for the v2 `run` transport: the worker
    /// status/reason to emit, or a supervisor-classification scenario
    /// (`skip`, `malformed`, `oversize`, `symlink`, `unreadable`,
    /// `wrong_identity`).
    /// `None` emits the default success report. Declaration probes
    /// ignore it — it never changes the declared surface.
    report_mode: Option<String>,
    /// Test-only runtime.log size pad for truncation scenarios.
    log_bytes: Option<u64>,
    /// Test-only: commit the workspace's dirty state on the managed
    /// branch before writing the report, so `claimed_head` can name
    /// the produced commit. v2 `run` transport only.
    git_commit: bool,
    /// Test-only `claimed_head` value: a git revision resolved in the
    /// workspace at report time (`HEAD` names the commit
    /// `--git-commit` just made), or a literal string claim.
    claim_head: Option<String>,
    /// Test-only verbatim `findings[]`/`invariant_items[]` arrays for
    /// the v2 report — JSON array text injected member-for-member so
    /// tests shape claims (valid, malformed, coverage-breaking)
    /// without re-deriving the envelope digest. The `-file` variants
    /// read the array from a path at report time, so a test can change
    /// claims between sequential Invocations under one argv.
    findings: Option<serde_json::Value>,
    invariant_items: Option<serde_json::Value>,
    findings_file: Option<String>,
    invariant_items_file: Option<String>,
}

fn parse() -> Args {
    let mut positionals: Vec<String> = Vec::new();
    let mut interval = Duration::from_millis(100);
    let mut file = "artifact.json".to_string();
    let mut runtime_version = None;
    let mut adapter_version = None;
    let mut wire_v2 = false;
    let mut decl_file = None;
    let mut request = None;
    let mut out = None;
    let mut report_mode = None;
    let mut log_bytes = None;
    let mut git_commit = false;
    let mut claim_head = None;
    let mut findings = None;
    let mut invariant_items = None;
    let mut findings_file = None;
    let mut invariant_items_file = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval-ms" => {
                interval =
                    Duration::from_millis(it.next().and_then(|v| v.parse().ok()).unwrap_or(100));
            }
            "--file" => {
                if let Some(v) = it.next() {
                    file = v;
                }
            }
            // Declaration-only values used by the effect-free `describe`
            // operation. They never change the writing mode.
            "--runtime-version" => runtime_version = it.next(),
            "--adapter-version" => adapter_version = it.next(),
            "--wire" => wire_v2 = it.next().as_deref() == Some("v2"),
            "--decl-file" => decl_file = it.next(),
            "--request" => request = it.next(),
            "--out" => out = it.next(),
            "--report-mode" => report_mode = it.next(),
            "--log-bytes" => {
                log_bytes = it.next().and_then(|v| v.parse().ok());
            }
            "--git-commit" => git_commit = true,
            "--claim-head" => claim_head = it.next(),
            "--findings" => findings = Some(member_array(it.next(), "--findings")),
            "--invariant-items" => {
                invariant_items = Some(member_array(it.next(), "--invariant-items"));
            }
            "--findings-file" => findings_file = it.next(),
            "--invariant-items-file" => invariant_items_file = it.next(),
            other => positionals.push(other.to_string()),
        }
    }
    // A trailing `describe`/`run` positional is the protocol
    // subcommand; the first positional stays the base mode. A single
    // positional is the mode itself, exactly as before.
    let (mode, subcommand) = match positionals.as_slice() {
        [] => (String::new(), None),
        [single] => (single.clone(), None),
        [first, .., last] if last == "describe" || last == "run" => {
            (first.clone(), Some(last.clone()))
        }
        _ => (positionals.last().cloned().unwrap_or_default(), None),
    };
    if mode.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    Args {
        mode,
        subcommand,
        interval,
        file,
        runtime_version,
        adapter_version,
        wire_v2,
        decl_file,
        request,
        out,
        report_mode,
        log_bytes,
        git_commit,
        claim_head,
        findings,
        invariant_items,
        findings_file,
        invariant_items_file,
    }
}

/// A `--findings`/`--invariant-items` value must be a JSON array —
/// anything else is a test-authoring error, refused before launch.
fn member_array(raw: Option<String>, flag: &str) -> serde_json::Value {
    let text = raw.unwrap_or_else(|| {
        eprintln!("{flag} requires a JSON array argument");
        std::process::exit(2);
    });
    member_array_text(&text, flag)
}

fn member_array_text(text: &str, flag: &str) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|e| {
        eprintln!("{flag} is not valid JSON: {e}");
        std::process::exit(2);
    });
    if !value.is_array() {
        eprintln!("{flag} must be a JSON array");
        std::process::exit(2);
    }
    value
}

/// Resolve one member-claim array: the `-file` variant reads at report
/// time (so sequential Invocations under one argv can carry different
/// claims); the inline variant was already parsed at launch.
fn member_claims(
    inline: &Option<serde_json::Value>,
    file: &Option<String>,
    flag: &str,
) -> Option<serde_json::Value> {
    if let Some(path) = file {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("{flag} file {path}: {e}");
            std::process::exit(1);
        });
        return Some(member_array_text(&text, flag));
    }
    inline.clone()
}

fn workspace() -> PathBuf {
    std::env::current_dir().expect("fixture worker has a cwd")
}

fn write_artifact(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = std::fs::File::create(path).expect("create artifact");
    f.write_all(body.as_bytes()).expect("write artifact");
}

fn self_exe() -> String {
    std::env::current_exe()
        .expect("current exe")
        .to_string_lossy()
        .into_owned()
}

/// The effect-free `describe` operation. `--decl-file` prints a
/// caller-supplied declaration verbatim (drift and malformed-body
/// tests); `--wire v2` selects the `taisan.worker.declaration/1`
/// envelope; the default stays the legacy shape.
fn declare(args: &Args) {
    if let Some(path) = &args.decl_file {
        let bytes = std::fs::read(path).expect("read declaration file");
        print!("{}", String::from_utf8_lossy(&bytes));
        return;
    }
    if args.wire_v2 {
        println!(
            "{}",
            serde_json::json!({
                "format_version": "taisan.worker.declaration/1",
                "adapter": "workloop-fixture-worker",
                "adapter_version": args.adapter_version.as_deref().unwrap_or("1.0.0"),
                "runtime": {
                    "product": "fixture-runtime",
                    "version": args.runtime_version.as_deref().unwrap_or("fixture-1.0.0"),
                },
                "protocol_versions": [
                    "taisan.worker.request/2",
                    "taisan.worker.report/2",
                ],
                "roles": ["coding", "fix", "invariant_check", "review"],
                "capabilities": [
                    "workspace_write",
                    "workspace_read_only",
                    "artifact_json",
                    "process_control",
                ],
                "resume": "unsupported",
                "checkpoints": "workspace_commits",
                "runtime_artifact_store": null,
                "local_retries": "runtime_internal",
            })
        );
        return;
    }
    println!(
        "{}",
        serde_json::json!({
            "adapter": "workloop-fixture-worker",
            "adapter_version": args.adapter_version.as_deref().unwrap_or("1.0.0"),
            "runtime_version": args.runtime_version.as_deref().unwrap_or("fixture-1.0.0"),
            "protocol_versions": ["taisan.worker/1"],
            "roles": ["coding", "fix", "invariant_check", "review"],
            "capabilities": ["workspace_write", "artifact_json", "process_control"],
        })
    );
}

/// Resolve `rev` inside the workspace to a commit id — `HEAD`,
/// `HEAD^`, a branch, or an already-literal sha all normalize to the
/// object id git reports. `None` outside a repository or for a rev
/// git cannot resolve.
fn git_rev_parse(ws: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["rev-parse", "--verify", rev])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Stage everything and commit on the managed branch — the produced
/// commit a real Worker leaves for handoff. A no-op outside a git
/// worktree; inside one a failed add/commit is a worker-side failure.
fn git_commit_produced(ws: &Path) {
    let is_repo = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["rev-parse", "--git-dir"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !is_repo {
        return;
    }
    for argv in [
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "user.name=fixture-worker",
            "commit",
            "--allow-empty",
            "-m",
            "worker produced",
        ],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(ws)
            .args(&argv)
            .status()
            .unwrap_or_else(|e| {
                eprintln!("git {argv:?} failed to spawn: {e}");
                std::process::exit(1);
            });
        if !status.success() {
            eprintln!("git {argv:?} failed: {status}");
            std::process::exit(1);
        }
    }
}

/// The v2 `run --request <path> --out <dir>` transport: read the
/// immutable request file, apply the base mode's workspace behavior
/// (plus the optional produced commit), then write
/// `report.json`/`runtime.log` into the Invocation-owned out dir —
/// the report is written after the workspace effect so
/// `--claim-head HEAD` names the commit the Worker just made. An
/// unreadable or non-JSON request is a worker-side failure — exit
/// non-zero, never a silent pass.
fn run_request(args: &Args, ws: &Path) {
    let request_path = args.request.clone().unwrap_or_else(|| {
        eprintln!("run requires --request");
        std::process::exit(2);
    });
    let out_dir = args.out.clone().unwrap_or_else(|| {
        eprintln!("run requires --out");
        std::process::exit(2);
    });
    let request_bytes = std::fs::read(&request_path).unwrap_or_else(|e| {
        eprintln!("cannot read request {request_path}: {e}");
        std::process::exit(1);
    });
    let request: serde_json::Value = serde_json::from_slice(&request_bytes).unwrap_or_else(|e| {
        eprintln!("request is not JSON: {e}");
        std::process::exit(1);
    });
    let out = PathBuf::from(&out_dir);
    std::fs::create_dir_all(&out).expect("create out dir");
    // The base mode's workspace behavior applies first: `clean-exit`
    // produces the artifact, `exit-no-artifact` does not, `hang`
    // leaves the tree untouched and keeps the process alive — its
    // report is still written below before the loop, exactly as the
    // stop-path tests require.
    let behavior = if args.mode == "run" {
        "clean-exit"
    } else {
        args.mode.as_str()
    };
    match behavior {
        "clean-exit" => {
            write_artifact(
                &ws.join(&args.file),
                "{\"status\":\"done\",\"worker\":\"fixture\"}\n",
            );
        }
        "exit-no-artifact" | "hang" => {}
        other => {
            eprintln!("unknown run behavior {other}\n{USAGE}");
            std::process::exit(2);
        }
    }
    if args.git_commit {
        git_commit_produced(ws);
    }
    let log_path = out.join("runtime.log");
    let mut log_content = format!("fixture worker run {request_path}\n");
    if let Some(target) = args.log_bytes {
        let pad = target.saturating_sub(log_content.len() as u64) as usize;
        log_content.push_str(&"x".repeat(pad));
    }
    std::fs::write(&log_path, &log_content).expect("write runtime.log");
    let log_bytes = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let log_digest = workloop_core::domain::sha256_hex(
        "worker-runtime-log-v1",
        &std::fs::read(&log_path).unwrap_or_default(),
    );
    let report_path = out.join("report.json");
    match args.report_mode.as_deref().unwrap_or("success") {
        // No report at all — the supervisor's `absent` class.
        "skip" => {}
        // Bytes that are not the envelope — `invalid(report.malformed)`.
        "malformed" => {
            std::fs::write(&report_path, b"{not json").expect("write malformed report");
        }
        // A link where the contract wants a regular file — the
        // supervisor must classify `invalid(report.unsafe_path)`
        // without ever following it.
        "symlink" => {
            let _ = std::fs::remove_file(&report_path);
            std::os::unix::fs::symlink(&request_path, &report_path).expect("symlink report.json");
        }
        // A regular file no reader may open — the supervisor must
        // classify `invalid(report.unreadable)` and still finish the
        // Invocation rather than wedging on the read error.
        "unreadable" => {
            std::fs::write(&report_path, b"{}").expect("write unreadable report");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&report_path, std::fs::Permissions::from_mode(0o000))
                .expect("chmod 000 report.json");
        }
        mode => {
            // The report echoes the exact request binding;
            // `report_digest` covers the canonical body minus the
            // member, computed with the same helpers the supervisor's
            // classifier uses.
            let (status, reason_code) = match mode {
                "negative" => ("negative", "checks_failed"),
                "findings" => ("negative", "findings_reported"),
                "violated" => ("negative", "invariant_violated"),
                "unavailable" => ("unavailable", "missing_required_input"),
                "refused" => ("refused", "provider_refused"),
                "limit_reached" => ("limit_reached", "deadline_reached"),
                "failed" => ("failed", "internal_error"),
                "oversize" => ("success", "completed"),
                "wrong_identity" => ("success", "completed"),
                _ => ("success", "completed"),
            };
            // `claimed_head` resolves at report time — after the
            // optional produced commit — so `HEAD` names exactly what
            // handoff will export. An unresolvable revision stays a
            // literal claim, which is how disagreement scenarios are
            // spelled.
            let claimed_head = args
                .claim_head
                .as_deref()
                .map(|claim| git_rev_parse(ws, claim).unwrap_or_else(|| claim.to_string()));
            let mut report = serde_json::json!({
                "format_version": "taisan.worker.report/2",
                "request_id": request.get("request_id").cloned().unwrap_or(serde_json::Value::Null),
                "invocation_id": request.get("invocation_id").cloned().unwrap_or(serde_json::Value::Null),
                "request_digest": request.get("request_digest").cloned().unwrap_or(serde_json::Value::Null),
                "role": request.get("role").cloned().unwrap_or(serde_json::Value::Null),
                "status": status,
                "reason": {"code": reason_code},
                "claimed_head": claimed_head,
                "claimed_checks": [],
                "findings": member_claims(&args.findings, &args.findings_file, "--findings")
                    .unwrap_or_else(|| serde_json::json!([])),
                "invariant_items": member_claims(
                    &args.invariant_items,
                    &args.invariant_items_file,
                    "--invariant-items",
                )
                .unwrap_or_else(|| serde_json::json!([])),
                "continuation": {"workspace_commit": null, "report_refs": []},
                "runtime_usage": {
                    "turns": 1,
                    "elapsed_ms": 1,
                    "runtime_product": "fixture-runtime",
                },
                "log": {
                    "path": "runtime.log",
                    "bytes": log_bytes,
                    "truncated": false,
                    "digest": log_digest,
                },
            });
            if mode == "wrong_identity" {
                report["invocation_id"] = serde_json::Value::String("inv-not-this-one".to_string());
            }
            if mode == "oversize" {
                // Pad past the request's own `max_report_bytes` — the
                // supervisor must classify `invalid(report.oversize)`
                // before parsing.
                let bound = request
                    .pointer("/budget/max_report_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1 << 20);
                report["findings"] = serde_json::json!([
                    {"pad": "x".repeat((bound as usize).min(64 << 20))}
                ]);
            }
            let digest = workloop_core::worker_v2::report_v2_digest(
                &workloop_core::worker_v2::report_canonical_body(&report),
            );
            report["report_digest"] = serde_json::Value::String(digest);
            std::fs::write(&report_path, workloop_core::domain::canonical_json(&report))
                .expect("write report.json");
        }
    }
    // `hang` is the only behavior left: the report is already on disk
    // — the stop paths collect it after the proven stop.
    if behavior == "hang" {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

fn main() {
    let args = parse();
    let ws = workspace();
    match args.subcommand.as_deref() {
        Some("describe") => {
            declare(&args);
            return;
        }
        Some("run") => {
            run_request(&args, &ws);
            return;
        }
        _ => {}
    }
    match args.mode.as_str() {
        // Write the required artifact, report evidence on stdout, exit 0.
        "clean-exit" => {
            write_artifact(
                &ws.join(&args.file),
                "{\"status\":\"done\",\"worker\":\"fixture\"}\n",
            );
            println!(
                "{}",
                serde_json::json!({
                    "evidence": "clean-exit",
                    "artifact": args.file,
                })
            );
        }
        // Exit 0 without the artifact — triggers bounded artifact recovery.
        "exit-no-artifact" => {
            println!("{}", serde_json::json!({"evidence": "exit-no-artifact"}));
        }
        // Never exits on its own; the stop target. Default signal
        // disposition — SIGTERM/SIGKILL end it.
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        // A `setsid` child keeps writing after the leader exits: it
        // survives a process-group signal, so only the descendant snapshot
        // recorded at stop-intent time covers it.
        "setsid-escape" => {
            let mut cmd = Command::new(self_exe());
            cmd.args(["escaped-writer", "--file", "heartbeat.log"]);
            unsafe {
                cmd.pre_exec(|| {
                    libc_setsid();
                    Ok(())
                });
            }
            cmd.spawn().expect("spawn escaped writer");
            // Bounded leader lifetime: long enough for a stop-intent
            // snapshot to catch the escapee while it is still a child,
            // then the leader exits and the escapee keeps writing.
            std::thread::sleep(args.interval * 20);
        }
        // Leader exits while a same-group child keeps writing — the
        // reparenting case the snapshot exists for.
        "orphan-writer" => {
            Command::new(self_exe())
                .args(["escaped-writer", "--file", "heartbeat.log"])
                .spawn()
                .expect("spawn orphan writer");
            std::thread::sleep(args.interval * 20);
        }
        // Exits while holding an unreaped child in Z state.
        "zombie" => {
            Command::new(self_exe())
                .arg("exit-now")
                .spawn()
                .expect("spawn short-lived child");
            // Deliberately never wait: the child stays a zombie of this
            // process while it hangs.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        // Reopen the workspace path on an interval; `makedirs-writer`
        // also recreates it — documenting why rename is not a fence.
        "path-reopen" | "makedirs-writer" => {
            let recreate = args.mode == "makedirs-writer";
            loop {
                let path = ws.join("heartbeat.log");
                if recreate {
                    let _ = std::fs::create_dir_all(&ws);
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(f, "beat {}", std::process::id());
                }
                std::thread::sleep(args.interval);
            }
        }
        // Internal modes used by the parent modes above.
        "escaped-writer" => {
            let path = ws.join(&args.file);
            loop {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(f, "beat {}", std::process::id());
                    let _ = f.flush();
                }
                std::thread::sleep(args.interval);
            }
        }
        "exit-now" => std::process::exit(0),
        // Dump the environment this process was launched under into
        // `env.json` in the workspace — the contract suite's audit of
        // what the launch environment actually contained.
        "env-report" => {
            let vars: serde_json::Map<String, serde_json::Value> = std::env::vars()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            write_artifact(
                &ws.join(&args.file),
                &serde_json::json!({"vars": vars}).to_string(),
            );
        }
        // Effect-free capability declaration. One adapter identity may be
        // probed with different runtime versions to prove that a version
        // change is a configuration repair rather than a silent upgrade.
        "describe" => declare(&args),
        "run" => run_request(&args, &ws),
        other => {
            eprintln!("unknown mode {other}\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn libc_setsid() {
    // Calling setsid() directly — we are in a pre_exec context in a
    // freshly forked child.
    unsafe {
        // extern without libc dependency churn: link the libc symbol.
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        setsid();
    }
}
