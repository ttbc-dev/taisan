//! Behavioral ports the core declares and adapters implement. Slice 0:
//! `ProcessControlPort` (POSIX supervisor in `workloop-exec`) and the
//! GitHub-integration port (fixture in `workloop-github`).

use std::path::Path;
use std::time::Duration;

use crate::domain::ProcessIdentity;

/// What `inspect` learned about one recorded process identity. Reads
/// process *state*, not presence: an unreaped zombie still lists in the
/// process table but cannot write, so it reports `Stopped`. A lookup that
/// cannot be completed reports `Unknown` — never a silent `Stopped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvidence {
    Alive {
        identity: ProcessIdentity,
    },
    Stopped {
        identity: ProcessIdentity,
        exit_code: Option<i32>,
    },
    Unknown {
        reason: String,
    },
}

/// Why a spawn could not produce a recorded identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// Definitely no process exists — safe to record `paused`/`failed`.
    Refused { reason: String },
    /// The process may exist — must not start a second writer.
    Unknown { reason: String },
}

/// Result of one signal attempt against a recorded identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOutcome {
    /// The identity re-verified and the signal was delivered.
    Sent,
    /// The pid's current start time no longer matches — a reused pid is
    /// never signaled.
    SkippedStale,
    /// The identity is already gone; counts toward stop proof.
    AlreadyGone,
    /// Delivery could not be confirmed.
    Failed { reason: String },
}

/// A stop attempt's immediate receipt. `Requested` is never stop proof;
/// only inspecting every recorded identity establishes termination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReceipt {
    /// Stop intent was durable and signals were issued to the group and
    /// each snapshotted identity that re-verified.
    Requested,
    /// Every recorded identity already inspected as stopped.
    AlreadyStopped,
    /// The intent could not be persisted or inspection was inconclusive.
    /// No signal was sent if persistence failed.
    Unknown { reason: String },
}

/// Result of one bounded effect-free probe (`<worker_argv...> describe`).
/// The probe is a real supervised process start: it runs in its own
/// process group like a writer, so the bound covers the whole tree —
/// a probe that forks a same-group child does not leave an orphan the
/// stop path cannot reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe exited inside the bound; `success` is the exit status
    /// and `stdout` its captured output.
    Exited { stdout: Vec<u8>, success: bool },
    /// The bound elapsed: the probe's process group and leader were
    /// signalled to stop and the leader was reaped. No partial output
    /// is trusted.
    Bounded,
}

/// How `execution` supervises real worker processes. Implementations must
/// satisfy the crash rules in `docs/lifecycle.md`: record `pid + start
/// time`, snapshot write-capable descendants with the stop intent, signal
/// only re-verified identities, read state not presence, and reap spawned
/// children.
pub trait ProcessControlPort {
    /// Spawn `argv` with `cwd` in its own process group. Returns the
    /// leader's host-observable identity on success.
    fn spawn(&self, argv: &[String], cwd: &Path) -> Result<ProcessIdentity, SpawnError>;

    /// The leader plus every write-capable descendant, captured while the
    /// leader still exists. After the leader exits, orphans reparent and
    /// leave the process tree, so this cannot be reconstructed later.
    /// An incomplete process-table read returns an error and cannot
    /// authorize signaling from a partial descendant snapshot.
    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String>;

    /// Every process currently belonging to `pgid`. Used to cover orphan
    /// writers still in the leader's group after it exits: they reparent
    /// out of the ppid tree `snapshot_descendants` walks but keep the
    /// recorded group. A failed enumeration is an error — never an empty
    /// proof set — so a gap cannot masquerade as "nothing left".
    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String>;

    /// Read the current state of one recorded identity.
    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence;

    /// Signal one identity only when its pid still carries the recorded
    /// start time. Returns `SkippedStale` on a pid-reuse mismatch.
    fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome;

    /// Signal the leader's whole process group. Descendants that escaped
    /// the group (e.g. `setsid`) are covered by `signal_verified` over the
    /// snapshot, not by this call.
    fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome;

    /// Reap a child this supervisor spawned so inspection is not polluted
    /// by zombies. No-op for pids that are not our children.
    fn reap(&self, pid: i32);

    /// Run a bounded effect-free probe (`<worker_argv...> describe`) under
    /// the same containment contract as a writer: the child starts in its
    /// own process group, and when `timeout` elapses the whole group plus
    /// the leader is signalled to stop and the leader reaped — a wedged
    /// probe can never hold a dispatch pass open or leak a same-group
    /// child past the bound. `SpawnError` means no probe ran (`Refused`)
    /// or its fate is unknown (`Unknown`); a caller must treat either as
    /// "the adapter did not answer".
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError>;
}
