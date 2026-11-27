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
/// A guardian that cannot be identified at launch is already gone; such a
/// fixture has nothing left to supervise it and exits immediately. A
/// process without an observable guardian inside its PID namespace (the
/// `--unshare-pid` sandbox init) keeps waiting instead: containment owns
/// its lifetime, and treating the absent parent as an orphan would end
/// every contained unbounded run at startup.
pub fn exit_when_orphaned(guardian: Guardian) {
    let this_parent = parent_pid();
    let contained = contained_in_namespace();
    let guardian_pid = match resolve_guardian(guardian, this_parent, parent_of, contained) {
        Resolution::Watch(pid) => pid,
        Resolution::Unobservable => return,
        Resolution::Orphaned => std::process::exit(0),
    };
    let Some(guardian) = identity(guardian_pid) else {
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

/// What the watchdog can prove about the guardian at launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolution {
    /// Supervise this process; exit when it disappears.
    Watch(i32),
    /// No guardian exists inside this PID namespace that the watchdog
    /// could observe. This is either the namespace init itself (parent
    /// pid 0) or, for a contained launch, the namespace init/reaper that
    /// the sandbox ties to the launcher. The process keeps running;
    /// namespace teardown stops it.
    Unobservable,
    /// The guardian is known to be gone: exit now.
    Orphaned,
}

fn resolve_guardian<F>(
    guardian: Guardian,
    this_parent: i32,
    parent_of: F,
    contained: bool,
) -> Resolution
where
    F: FnOnce(i32) -> Option<i32>,
{
    match this_parent {
        // `--unshare-pid` makes the sandboxed process the PID-namespace
        // init, and a PID namespace init has no parent it can observe.
        0 => Resolution::Unobservable,
        // Reparenting to pid 1 means the launcher is gone, unless pid 1 is
        // the init/reaper of a contained namespace that outlives it.
        1 if contained => Resolution::Unobservable,
        1 => Resolution::Orphaned,
        pid if pid > 1 => match guardian {
            Guardian::Parent => Resolution::Watch(pid),
            Guardian::Grandparent => match parent_of(pid) {
                Some(0) => Resolution::Unobservable,
                Some(1) if contained => Resolution::Unobservable,
                Some(grandparent) if grandparent > 1 => Resolution::Watch(grandparent),
                Some(_) | None => Resolution::Orphaned,
            },
        },
        _ => Resolution::Orphaned,
    }
}

/// Whether this process runs in a non-initial user namespace — the marker
/// of a bwrap containment launch (the reference profile always passes
/// `--unshare-user`). It disambiguates pid 1: inside containment it is the
/// namespace init/reaper; outside it is the host init a launcher's death
/// reparents us to.
fn contained_in_namespace() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .ok()
        .is_some_and(|map| !uid_map_is_initial(&map))
}

fn uid_map_is_initial(map: &str) -> bool {
    let mut lines = map.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let mut fields = line.split_whitespace();
    matches!(
        (fields.next(), fields.next(), fields.next(), fields.next()),
        (Some(_), Some(_), Some(count), None)
            if count
                .parse::<u64>()
                .is_ok_and(|count| count == u32::MAX as u64)
    )
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
///
/// A parent pid of 0 is preserved: it identifies a PID-namespace init,
/// which must not be mistaken for an unreadable parent.
fn parent_of(pid: i32) -> Option<i32> {
    if let Some((_, ppid, _)) = read_stat(pid) {
        return Some(ppid);
    }
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_guardian_is_the_direct_parent() {
        assert_eq!(
            resolve_guardian(Guardian::Parent, 42, |_| unreachable!(), false),
            Resolution::Watch(42)
        );
    }

    #[test]
    fn parent_guardian_for_namespace_init_stays_alive() {
        assert_eq!(
            resolve_guardian(Guardian::Parent, 0, |_| unreachable!(), false),
            Resolution::Unobservable
        );
        // Even outside a user namespace a parent pid of 0 cannot be a
        // reparent-to-init signal; there is no init to observe.
        assert_eq!(
            resolve_guardian(Guardian::Parent, 0, |_| unreachable!(), true),
            Resolution::Unobservable
        );
    }

    #[test]
    fn parent_guardian_rejects_reparented_init() {
        assert_eq!(
            resolve_guardian(Guardian::Parent, 1, |_| unreachable!(), false),
            Resolution::Orphaned
        );
    }

    #[test]
    fn contained_parent_that_is_namespace_init_stays_alive() {
        assert_eq!(
            resolve_guardian(Guardian::Parent, 1, |_| unreachable!(), true),
            Resolution::Unobservable
        );
    }

    #[test]
    fn grandparent_guardian_is_the_harness_above_the_leader() {
        assert_eq!(
            resolve_guardian(
                Guardian::Grandparent,
                42,
                |pid| {
                    assert_eq!(pid, 42);
                    Some(7)
                },
                false,
            ),
            Resolution::Watch(7)
        );
    }

    #[test]
    fn grandparent_guardian_for_namespace_init_stays_alive() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 0, |_| unreachable!(), false),
            Resolution::Unobservable
        );
    }

    #[test]
    fn grandparent_guardian_rejects_reparented_init_child() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 1, |_| unreachable!(), false),
            Resolution::Orphaned
        );
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 1, |_| unreachable!(), true),
            Resolution::Unobservable
        );
    }

    #[test]
    fn grandparent_guardian_rejects_init_as_the_harness() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| Some(1), false),
            Resolution::Orphaned
        );
    }

    #[test]
    fn grandparent_guardian_accepts_namespace_init_as_the_harness_when_contained() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| Some(1), true),
            Resolution::Unobservable
        );
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| Some(0), true),
            Resolution::Unobservable
        );
    }

    #[test]
    fn grandparent_guardian_distinguishes_unset_from_init() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| Some(0), false),
            Resolution::Unobservable
        );
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| None, false),
            Resolution::Orphaned
        );
    }

    #[test]
    fn grandparent_guardian_does_not_fall_back_to_the_leader() {
        assert_eq!(
            resolve_guardian(Guardian::Grandparent, 42, |_| None, false),
            Resolution::Orphaned
        );
    }

    #[test]
    fn full_uid_map_is_the_initial_namespace() {
        assert!(uid_map_is_initial("         0          0 4294967295\n"));
    }

    #[test]
    fn restricted_uid_map_marks_containment() {
        assert!(!uid_map_is_initial("         0       1000          1\n"));
        assert!(!uid_map_is_initial(
            "         0       1000          1\n         1       1001          1\n"
        ));
        assert!(!uid_map_is_initial(""));
    }
}
