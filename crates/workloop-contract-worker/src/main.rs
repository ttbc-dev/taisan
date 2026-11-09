//! Contract worker: the independent deterministic Worker implementation
//! the shared U4.1 contract suite runs against. It is not the reference
//! runtime and shares no provider-specific behavior: payloads are a
//! pure function of the request bytes, never time-, pid-, or
//! environment-derived, so identical inputs reproduce identical
//! artifacts. It produces only workspace-local changes and evidence —
//! it has no host-write, publish, or outer-loop capability at all.
//!
//! argv contract (shared with the suite): `<mode> [--interval-ms <n>]
//! [--file <name>]`; the managed workspace is the process's cwd and the
//! only place it may write.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

const USAGE: &str = "usage: workloop-contract-worker <mode> \
    [--interval-ms <n>] [--file <name>]\nmodes: clean-exit | \
    exit-no-artifact | hang | path-reopen";

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

fn main() {
    let mut mode = String::new();
    let mut interval = Duration::from_millis(100);
    let mut file_arg: Option<String> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval-ms" => {
                interval =
                    Duration::from_millis(it.next().and_then(|v| v.parse().ok()).unwrap_or(100));
            }
            "--file" => file_arg = it.next(),
            other => mode = other.to_string(),
        }
    }
    if mode.is_empty() {
        usage_exit();
    }
    let ws = workspace();
    match mode.as_str() {
        // Produce the declared artifact and exit — the payload is a
        // deterministic function of the request bytes.
        "clean-exit" => match declared_file(file_arg) {
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
        // Never exits on its own — the suite's stop-proof target.
        "hang" => loop {
            std::thread::park();
        },
        // Reopen the workspace path on an interval — a live writer the
        // stop proof must cover. The shared suite expects the fixed
        // heartbeat file.
        "path-reopen" => {
            let file = declared_file(file_arg.or_else(|| Some("heartbeat.log".to_string())))
                .unwrap_or_else(|_| {
                    report(serde_json::json!({"evidence": "denied-host-write"}));
                    std::process::exit(3);
                });
            let Some(path) = resolve_inside(&ws, &file) else {
                report(serde_json::json!({"evidence": "denied-host-write"}));
                std::process::exit(3);
            };
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
        // Periodic heartbeat, fixed name — workspace-local only.
        "reopen" => heartbeat_forever(&ws, interval),
        _ => usage_exit(),
    }
}
