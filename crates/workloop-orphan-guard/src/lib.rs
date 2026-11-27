//! Parent-death watchdog shared by the test-only fixture Workers.
//!
//! The fixture and contract Workers run only as children of an in-process
//! supervisor inside a test binary. When that binary exits, is killed, or
//! panics, nothing remains to stop an unbounded Worker: the orphan keeps
//! the caller's inherited output pipes open and can wedge an agent-driven
//! run long after the test finished. The watchdog below ends such a Worker
//! when the process that launched it (or, for a deliberately escaping
//! child, the harness above its leader) is gone.
//!
//! A guardian is identified by pid *and* process start time, not just the
//! current parent pid: reparenting to a long-lived subreaper changes the
//! parent, and a recycled pid must never keep an orphan alive.

use std::process::Command;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The process that must remain alive for the fixture to keep running.
#[derive(Clone, Copy)]
pub enum Guardian {
    /// The direct parent — the supervisor/test process for a top-level
    /// fixture mode.
    Parent,
    /// The parent of the direct parent — the test harness for an internal
    /// child that deliberately outlives its fixture leader
    /// (`escaped-writer`).
    Grandparent,
}

/// Exit this process as soon as its guardian disappears.
///
/// Called by each unbounded fixture mode at the point it starts waiting.
/// A guardian that cannot be identified at launch is already gone (or a
/// direct init child); such a fixture has nothing left to supervise it and
/// exits immediately.
pub fn exit_when_orphaned(guardian: Guardian) {
    let this_parent = parent_pid();
    let guardian = match guardian {
        Guardian::Parent => {
            // A fixture launched directly by init has no harness above it.
            // Treating pid 1 as "alive" would also let a spawn/death race
            // (already reparented to init) leak forever.
            (!(0..=1).contains(&this_parent))
                .then(|| identity(this_parent))
                .flatten()
        }
        Guardian::Grandparent => parent_of(this_parent)
            .and_then(identity)
            .or_else(|| identity(this_parent)),
    };
    let Some(guardian) = guardian else {
        std::process::exit(0);
    };
    std::thread::spawn(move || {
        loop {
            if !guardian.is_alive() {
                std::process::exit(0);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

#[derive(Clone, Copy)]
struct Identity {
    pid: i32,
    /// Linux `/proc/<pid>/stat` start time; `None` when `/proc` is not
    /// available and liveness is only provable with `kill(pid, 0)`.
    start_time: Option<u64>,
}

impl Identity {
    fn is_alive(&self) -> bool {
        match read_stat(self.pid) {
            Some((state, _, start_time)) => {
                if state == 'Z' || state == 'X' {
                    return false;
                }
                self.start_time
                    .is_none_or(|expected| expected == start_time)
            }
            // Once a start time was captured, an unreadable process is
            // gone — never fall back to a possibly recycled pid.
            None => self.start_time.is_none() && process_exists(self.pid),
        }
    }
}

fn identity(pid: i32) -> Option<Identity> {
    if pid <= 0 {
        return None;
    }
    if let Some((_, _, start_time)) = read_stat(pid) {
        return Some(Identity {
            pid,
            start_time: Some(start_time),
        });
    }
    process_exists(pid).then_some(Identity {
        pid,
        start_time: None,
    })
}

fn parent_pid() -> i32 {
    unsafe extern "C" {
        fn getppid() -> i32;
    }
    unsafe { getppid() }
}

/// Parent pid from `/proc`, falling back to `ps` where `/proc` is absent.
fn parent_of(pid: i32) -> Option<i32> {
    if let Some((_, ppid, _)) = read_stat(pid) {
        return (ppid > 0).then_some(ppid);
    }
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ppid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (ppid > 0).then_some(ppid)
}

/// `(state, ppid, starttime)` from `/proc/<pid>/stat`.
///
/// The command name may contain spaces and parentheses, so parse from the
/// last `)`; the remaining fields are fixed. `starttime` is field 22
/// overall (index 19 after the state field).
fn read_stat(pid: i32) -> Option<(char, i32, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = text.rfind(')')?;
    let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, ppid, start_time))
}

fn process_exists(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    pid > 0 && unsafe { kill(pid, 0) } == 0
}
