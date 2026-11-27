//! Contract worker: the independent deterministic Worker implementation
//! the shared contract suites run against. It is not the reference
//! runtime and shares no provider-specific behavior: payloads are a
//! pure function of the request bytes, never time-, pid-, or
//! environment-derived, so identical inputs reproduce identical
//! artifacts. It produces only workspace-local changes and evidence —
//! it has no host-write, publish, or outer-loop capability at all.
//!
//! argv contract (shared with the suite): `<mode> [flags] [describe|
//! run]` — the managed workspace is the process's cwd and the only
//! place it may write. `describe` is effect-free and prints the
//! declaration (`--wire v2` selects the `taisan.worker.declaration/1`
//! envelope); `run --request <path> --out <dir>` executes the v2
//! request/report transport. The v2 report envelope, its canonical
//! bytes, and its digest are implemented here from the wire contract
//! alone — this crate deliberately does not depend on the host's
//! core library, so a second implementation of the same protocol
//! proves the contract is implementable, not just reusable.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

const USAGE: &str = "usage: workloop-contract-worker <mode> \
    [--interval-ms <n>] [--file <name>] [--runtime-version <v>] \
    [--adapter-version <v>] [--wire v1|v2] [--decl-file <path>] \
    [--request <path>] [--out <dir>] [--report-mode <m>] \
    [--git-commit] [--claim-head <rev>] [describe | run]\nmodes: \
    clean-exit | exit-no-artifact | hang | path-reopen | describe | reopen";

fn usage_exit() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn workspace() -> PathBuf {
    std::env::current_dir().expect("contract worker has a cwd")
}

/// `--file` names a path inside the managed workspace only. An
/// absolute path, a `..` traversal, or a committed symlink that
/// resolves outside the workspace is a denied host write: refuse it
/// outright — a Worker argument must never reach outside the sandbox
/// root.
fn declared_file(arg: Option<String>) -> Result<String, ()> {
    let name = arg.unwrap_or_else(|| "artifact.json".to_string());
    let ok = !Path::new(&name).is_absolute()
        && Path::new(&name)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok { Ok(name) } else { Err(()) }
}

/// Resolve `name` under `ws`, verifying every existing component —
/// a repository can commit symlinks, and a symlinked directory or
/// target file pointing outside the workspace is a host write in
/// disguise. Components that do not exist yet are created by the
/// caller under a verified parent.
fn resolve_inside(ws: &Path, name: &str) -> Option<PathBuf> {
    let root = ws.canonicalize().ok()?;
    let mut p = PathBuf::from(ws);
    for comp in Path::new(name).components() {
        p.push(comp.as_os_str());
        match std::fs::symlink_metadata(&p) {
            Ok(md) if md.file_type().is_symlink() => {
                let resolved = p.canonicalize().ok()?;
                if !resolved.starts_with(&root) {
                    return None;
                }
                p = resolved;
            }
            _ => {}
        }
    }
    Some(p)
}

fn report(fact: serde_json::Value) {
    println!("{fact}");
}

/// The artifact payload is a pure function of the request input bytes
/// and the declared file name — replay produces byte-identical output.
fn artifact_body(request_bytes: &[u8], file: &str) -> String {
    let digest = hex(Sha256::digest(request_bytes));
    serde_json::json!({
        "worker": "contract-v1",
        "request_digest": digest,
        "file": file,
    })
    .to_string()
        + "\n"
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn write_artifact(ws: &Path, name: &str) {
    let request = ws.join("request.json");
    let request_bytes = std::fs::read(&request).unwrap_or_default();
    let body = artifact_body(&request_bytes, name);
    let Some(path) = resolve_inside(ws, name) else {
        // A committed symlink reaching outside the workspace is a
        // host write — reported and refused like an absolute path.
        report(serde_json::json!({"evidence": "denied-host-write"}));
        std::process::exit(3);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("artifact parent");
    }
    std::fs::write(&path, body).expect("write artifact");
}

fn heartbeat_forever(ws: &Path, interval: Duration) -> ! {
    let Some(path) = resolve_inside(ws, "heartbeat.log") else {
        // A committed heartbeat.log symlink reaching outside the
        // workspace is a host write — reported and refused.
        report(serde_json::json!({"evidence": "denied-host-write"}));
        std::process::exit(3);
    };
    workloop_orphan_guard::exit_when_orphaned(workloop_orphan_guard::Guardian::Parent);
    loop {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "beat");
            let _ = f.flush();
        }
        std::thread::sleep(interval);
    }
}

// ---- the v2 wire surface ----------------------------------------------
//
// Everything below implements `taisan.worker.declaration/1`,
// `taisan.worker.request/2` consumption, and `taisan.worker.report/2`
// emission from the protocol contract alone: canonical JSON with
// recursively sorted keys, tagged SHA-256 digests, and the closed
// report member set. No host-library types cross this boundary.

/// Canonical JSON: object keys sorted recursively, UTF-8, no
/// insignificant whitespace — the encoding every wire digest covers.
fn canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        serde_json::Value::Number(_) | serde_json::Value::String(_) => {
            out.extend_from_slice(
                serde_json::to_string(value)
                    .expect("primitive json")
                    .as_bytes(),
            );
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical_json(item, out);
            }
            out.push(b']');
        }
        serde_json::Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(k).expect("key json").as_bytes());
                out.push(b':');
                canonical_json(map.get(*k).expect("key from map.keys()"), out);
            }
            out.push(b'}');
        }
    }
}

/// SHA-256 over `tag:bytes`, hex-encoded — the wire digest rule.
fn tagged_sha256_hex(tag: &str, bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update(b":");
    h.update(bytes);
    hex(h.finalize())
}

/// Canonical bytes of a report value with `report_digest` omitted —
/// the exact input the `worker-report-v2` digest covers.
fn report_canonical_body(value: &serde_json::Value) -> Vec<u8> {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("report_digest");
    }
    let mut out = Vec::new();
    canonical_json(&value, &mut out);
    out
}

struct Args {
    mode: String,
    /// The protocol subcommand a trailing positional selects:
    /// `describe` prints the declaration, `run` executes the v2
    /// `--request/--out` transport. `None` runs the base mode directly.
    subcommand: Option<String>,
    interval: Duration,
    file: Option<String>,
    runtime_version: Option<String>,
    adapter_version: Option<String>,
    wire_v2: bool,
    decl_file: Option<String>,
    request: Option<String>,
    out: Option<String>,
    report_mode: Option<String>,
    git_commit: bool,
    claim_head: Option<String>,
}

fn parse() -> Args {
    let mut positionals: Vec<String> = Vec::new();
    let mut interval = Duration::from_millis(100);
    let mut file = None;
    let mut runtime_version = None;
    let mut adapter_version = None;
    let mut wire_v2 = false;
    let mut decl_file = None;
    let mut request = None;
    let mut out = None;
    let mut report_mode = None;
    let mut git_commit = false;
    let mut claim_head = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval-ms" => {
                interval =
                    Duration::from_millis(it.next().and_then(|v| v.parse().ok()).unwrap_or(100));
            }
            "--file" => file = it.next(),
            "--runtime-version" => runtime_version = it.next(),
            "--adapter-version" => adapter_version = it.next(),
            "--wire" => wire_v2 = it.next().as_deref() == Some("v2"),
            "--decl-file" => decl_file = it.next(),
            "--request" => request = it.next(),
            "--out" => out = it.next(),
            "--report-mode" => report_mode = it.next(),
            "--git-commit" => git_commit = true,
            "--claim-head" => claim_head = it.next(),
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
        usage_exit();
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
        git_commit,
        claim_head,
    }
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
        report(serde_json::json!({
            "format_version": "taisan.worker.declaration/1",
            "adapter": "workloop-contract-worker",
            "adapter_version": args.adapter_version.as_deref().unwrap_or("1.0.0"),
            "runtime": {
                "product": "contract-runtime",
                "version": args.runtime_version.as_deref().unwrap_or("contract-1.0.0"),
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
        }));
        return;
    }
    report(serde_json::json!({
        "adapter": "workloop-contract-worker",
        "adapter_version": args.adapter_version.as_deref().unwrap_or("1.0.0"),
        "runtime_version": args.runtime_version.as_deref().unwrap_or("contract-1.0.0"),
        "protocol_versions": ["taisan.worker/1"],
        "roles": ["coding", "fix", "invariant_check", "review"],
        "capabilities": ["workspace_write", "artifact_json", "process_control"],
    }));
}

/// Resolve `rev` inside the workspace to a commit id — `HEAD`, a
/// branch, or an already-literal sha all normalize to the object id
/// git reports. `None` outside a repository or for a rev git cannot
/// resolve.
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
            "user.email=contract-worker@localhost",
            "-c",
            "user.name=contract-worker",
            "commit",
            "--allow-empty",
            "-m",
            "contract worker produced",
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
/// (plus the optional produced commit), then write `report.json`/
/// `runtime.log` into the Invocation-owned out dir — the report is
/// written after the workspace effect so `--claim-head HEAD` names
/// the commit the Worker just made. An unreadable or non-JSON request
/// is a worker-side failure — exit non-zero, never a silent pass.
fn run_request(args: &Args, ws: &Path) {
    // Arm the parent-death watchdog before touching the request: a hang
    // without a bounded wait must not outlive a launcher that dies while
    // the request is read or the report is written.
    if args.mode == "hang" {
        workloop_orphan_guard::exit_when_orphaned(workloop_orphan_guard::Guardian::Parent);
    }
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
    // The base mode's workspace behavior applies first. A read-only
    // grant (review, invariant check) produces no workspace writes at
    // all — the report is the only output.
    let read_only = request
        .pointer("/workspace/read_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match args.mode.as_str() {
        "clean-exit" | "exit-no-artifact" | "hang" => {}
        other => {
            eprintln!("unknown run behavior {other}\n{USAGE}");
            std::process::exit(2);
        }
    }
    if args.git_commit && !read_only {
        git_commit_produced(ws);
    }
    if args.mode == "clean-exit" && !read_only {
        // The manifest is the Worker's own handoff file: it binds this
        // exact request and invocation, and it lands after the
        // produced commit so a `git add -A` can never capture it into
        // the produced revision.
        let file = declared_file(args.file.clone()).unwrap_or_else(|_| {
            eprintln!("--file must stay inside the workspace");
            std::process::exit(2);
        });
        let Some(path) = resolve_inside(ws, &file) else {
            eprintln!("--file escapes the workspace");
            std::process::exit(2);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("artifact parent");
        }
        let manifest = serde_json::json!({
            "format_version": "taisan.worker.artifact/1",
            "worker": "workloop-contract-worker",
            "request_id": request.get("request_id").cloned().unwrap_or(serde_json::Value::Null),
            "invocation_id": request.get("invocation_id").cloned().unwrap_or(serde_json::Value::Null),
        });
        std::fs::write(&path, manifest.to_string()).expect("write artifact");
    }
    let log_path = out.join("runtime.log");
    std::fs::write(&log_path, format!("contract worker run {request_path}\n"))
        .expect("write runtime.log");
    let log_bytes = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let log_digest = tagged_sha256_hex(
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
        mode => {
            // The report echoes the exact request binding;
            // `report_digest` covers the canonical body minus the
            // member, computed over this implementation's own
            // canonical encoding.
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
                "findings": [],
                "invariant_items": [],
                "continuation": {"workspace_commit": null, "report_refs": []},
                "runtime_usage": {
                    "turns": 1,
                    "elapsed_ms": 1,
                    "runtime_product": "contract-runtime",
                },
                "log": {
                    "path": "runtime.log",
                    "bytes": log_bytes,
                    "truncated": false,
                    "digest": log_digest,
                },
            });
            if mode == "wrong_identity" {
                report["invocation_id"] =
                    serde_json::Value::String("inv-foreign-report".to_string());
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
            let digest = tagged_sha256_hex("worker-report-v2", &report_canonical_body(&report));
            report["report_digest"] = serde_json::Value::String(digest);
            let mut bytes = Vec::new();
            canonical_json(&report, &mut bytes);
            std::fs::write(&report_path, bytes).expect("write report.json");
        }
    }
    // `hang` is the only behavior left: the report is already on disk
    // — the stop paths collect it after the proven stop.
    if args.mode == "hang" {
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
    // The mode itself may be `describe` — the legacy single-positional
    // form the v1 declaration probe uses.
    if args.mode == "describe" && args.subcommand.is_none() {
        declare(&args);
        return;
    }
    match args.mode.as_str() {
        // Produce the declared artifact and exit — the payload is a
        // deterministic function of the request bytes.
        "clean-exit" => match declared_file(args.file.clone()) {
            Ok(file) => {
                write_artifact(&ws, &file);
                report(serde_json::json!({
                    "evidence": "clean-exit",
                    "artifact": file,
                    "worker": "contract-v1",
                }));
            }
            // A denied host write is reported and refused — never
            // executed.
            Err(()) => {
                report(serde_json::json!({"evidence": "denied-host-write"}));
                std::process::exit(3);
            }
        },
        // Clean exit with no artifact — the bounded-recovery shape.
        "exit-no-artifact" => {
            report(serde_json::json!({
                "evidence": "exit-no-artifact",
                "worker": "contract-v1",
            }));
        }
        // Waits without a bound — the suite's stop-proof target — but
        // exits if the launching process dies.
        "hang" => {
            workloop_orphan_guard::exit_when_orphaned(workloop_orphan_guard::Guardian::Parent);
            loop {
                std::thread::park();
            }
        }
        // Reopen the workspace path on an interval — a live writer the
        // stop proof must cover. The shared suite expects the fixed
        // heartbeat file.
        "path-reopen" => {
            let file = declared_file(
                args.file
                    .clone()
                    .or_else(|| Some("heartbeat.log".to_string())),
            )
            .unwrap_or_else(|_| {
                report(serde_json::json!({"evidence": "denied-host-write"}));
                std::process::exit(3);
            });
            let Some(path) = resolve_inside(&ws, &file) else {
                report(serde_json::json!({"evidence": "denied-host-write"}));
                std::process::exit(3);
            };
            workloop_orphan_guard::exit_when_orphaned(workloop_orphan_guard::Guardian::Parent);
            loop {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(f, "beat");
                    let _ = f.flush();
                }
                std::thread::sleep(args.interval);
            }
        }
        // Periodic heartbeat, fixed name — workspace-local only.
        "reopen" => heartbeat_forever(&ws, args.interval),
        _ => usage_exit(),
    }
}
