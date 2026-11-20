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
    }
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

/// The v2 `run --request <path> --out <dir>` transport: read the
/// immutable request file, write `report.json`/`runtime.log` into the
/// Invocation-owned out dir, then apply the base mode's workspace
/// behavior. An unreadable or non-JSON request is a worker-side
/// failure — exit non-zero, never a silent pass.
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
    let log_path = out.join("runtime.log");
    std::fs::write(&log_path, format!("fixture worker run {request_path}\n"))
        .expect("write runtime.log");
    let log_bytes = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let log_digest = workloop_core::domain::sha256_hex(
        "worker-runtime-log-v1",
        &std::fs::read(&log_path).unwrap_or_default(),
    );
    // The report echoes the exact request binding; `report_digest`
    // covers the canonical body minus the member, computed with the
    // same helpers the supervisor's classifier uses.
    let mut report = serde_json::json!({
        "format_version": "taisan.worker.report/2",
        "request_id": request.get("request_id").cloned().unwrap_or(serde_json::Value::Null),
        "invocation_id": request.get("invocation_id").cloned().unwrap_or(serde_json::Value::Null),
        "request_digest": request.get("request_digest").cloned().unwrap_or(serde_json::Value::Null),
        "role": request.get("role").cloned().unwrap_or(serde_json::Value::Null),
        "status": "success",
        "reason": {"code": "completed"},
        "claimed_head": null,
        "claimed_checks": [],
        "findings": [],
        "invariant_items": [],
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
    let digest = workloop_core::worker_v2::report_v2_digest(
        &workloop_core::worker_v2::report_canonical_body(&report),
    );
    report["report_digest"] = serde_json::Value::String(digest);
    std::fs::write(
        out.join("report.json"),
        workloop_core::domain::canonical_json(&report),
    )
    .expect("write report.json");
    // The base mode's workspace behavior still applies: `clean-exit`
    // produces the artifact, `exit-no-artifact` does not, `hang` keeps
    // the process alive for the stop paths.
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
        "exit-no-artifact" => {}
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        other => {
            eprintln!("unknown run behavior {other}\n{USAGE}");
            std::process::exit(2);
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
