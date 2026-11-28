//! Regression coverage for the fixture Workers' parent-death watchdog:
//! a bounded test may leave a waiting Worker behind, but the Worker must
//! never outlive the process that launched it — a pipe-holding orphan
//! would defeat the caller's timeouts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn fixture() -> &'static str {
    env!("CARGO_BIN_EXE_workloop-fixture-worker")
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-fixture-orphan-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn harness(script: &str, root: &Path) -> Child {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .env("FIXTURE", fixture())
        .env("ROOT", root)
        .env("WORKLOOP_ORPHAN_GUARD_READY_FILE", root.join("guard-armed"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test harness")
}

fn wait_for(mut pred: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn read_pid(path: &Path) -> i32 {
    // The harness writes with `> "$ROOT/child.pid"`, which creates and
    // truncates the file before `printf` fills it. Wait for non-empty,
    // parseable content instead of mere existence, and return that value
    // rather than re-reading a file that could still be mid-write.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for pid file {}", path.display());
}

fn last_beat_pid(path: &Path) -> i32 {
    wait_for(
        || std::fs::read_to_string(path).is_ok_and(|text| !text.trim().is_empty()),
        "heartbeat",
    );
    std::fs::read_to_string(path)
        .expect("read heartbeat")
        .lines()
        .filter_map(|line| line.strip_prefix("beat "))
        .next_back()
        .and_then(|pid| pid.trim().parse().ok())
        .expect("heartbeat names its writer")
}

fn proc_state(pid: i32) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = text.rfind(')')?;
    text[close + 1..].split_whitespace().next()?.chars().next()
}

fn running(pid: i32) -> bool {
    match proc_state(pid) {
        Some(state) => state != 'Z' && state != 'X',
        None => (unsafe { kill(pid, 0) }) == 0,
    }
}

fn wait_until_running(pid: i32, what: &str) {
    wait_for(|| running(pid), what);
}

fn wait_until_dead(pid: i32, what: &str) {
    wait_for(
        || !running(pid),
        &format!("{what} (pid {pid}) to exit after its launcher died"),
    );
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[test]
fn hang_exits_when_its_launching_process_dies() {
    let root = tmp_root("hang");
    let script = r#"
        "$FIXTURE" hang &
        child=$!
        printf '%s\n' "$child" > "$ROOT/child.pid"
        wait "$child"
    "#;
    let mut harness = harness(script, &root);
    let worker = read_pid(&root.join("child.pid"));
    wait_until_running(worker, "hang fixture");
    // Kill only after the watchdog is observing the harness: arming and
    // the kill otherwise race under load, and a guard that starts after
    // the reparent cannot see that the launcher is gone.
    wait_for(|| root.join("guard-armed").exists(), "orphan guard to arm");

    // Kill only the harness, not the Worker: the fixture has no supervisor
    // left and must observe the lost parent instead of waiting forever.
    harness.kill().expect("kill harness");
    let _ = harness.wait();
    wait_until_dead(worker, "hang fixture");
}

#[test]
fn escaped_writer_exits_when_the_harness_dies() {
    let root = tmp_root("escape");
    let script = r#"
        "$FIXTURE" orphan-writer --interval-ms 50 &
        leader=$!
        wait "$leader"
        : > "$ROOT/harness-ready"
        while [ ! -e "$ROOT/harness-stop" ]; do sleep 0.05; done
    "#;
    let mut harness = harness(script, &root);
    wait_for(
        || root.join("harness-ready").exists(),
        "fixture leader to exit",
    );
    // The escaped writer deliberately outlives its leader, so its direct
    // parent is already gone; it must instead watch the harness above the
    // leader and end when that harness dies.
    let worker = last_beat_pid(&root.join("heartbeat.log"));
    wait_until_running(worker, "escaped writer");

    harness.kill().expect("kill harness");
    let _ = harness.wait();
    wait_until_dead(worker, "escaped writer");
}
