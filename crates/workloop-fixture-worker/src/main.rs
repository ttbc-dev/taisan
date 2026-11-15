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
    [--file <name>] [--runtime-version <v>] [--adapter-version <v>]\nmodes: \
    clean-exit | exit-no-artifact | hang | setsid-escape | orphan-writer | \
    zombie | path-reopen | makedirs-writer | describe | env-report\ninternal: \
    escaped-writer | exit-now";

struct Args {
    mode: String,
    interval: Duration,
    file: String,
    runtime_version: Option<String>,
    adapter_version: Option<String>,
}

fn parse() -> Args {
    let mut mode = String::new();
    let mut interval = Duration::from_millis(100);
    let mut file = "artifact.json".to_string();
    let mut runtime_version = None;
    let mut adapter_version = None;
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
            other => mode = other.to_string(),
        }
    }
    if mode.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    Args {
        mode,
        interval,
        file,
        runtime_version,
        adapter_version,
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

fn main() {
    let args = parse();
    let ws = workspace();
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
        "describe" => {
            println!(
                "{}",
                serde_json::json!({
                    "adapter": "workloop-fixture-worker",
                    "adapter_version": args.adapter_version.as_deref().unwrap_or("1.0.0"),
                    "runtime_version": args.runtime_version.as_deref().unwrap_or("fixture-1.0.0"),
                    "protocol_versions": ["taisan.worker/1"],
                    "roles": ["coding", "fix", "review"],
                    "capabilities": ["workspace_write", "artifact_json", "process_control"],
                })
            );
        }
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
