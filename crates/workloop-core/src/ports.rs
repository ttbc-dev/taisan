//! Behavioral ports the core declares and adapters implement. Slice 0:
//! `ProcessControlPort` (POSIX supervisor in `workloop-exec`) and the
//! GitHub-integration port (fixture in `workloop-github`).
//!
//! The GitHub-integration port also carries the event-filter contract:
//! a filter that drops bot-authored or bot-sent deliveries must exempt
//! lane-produced events, or the lane starves its own loop — see
//! [`admit_bot_filtered_event`].

use std::path::Path;

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
}

/// Whether an inbound delivery was produced by the lane itself — e.g.
/// authored or sent by the lane's GitHub App identity. Bot-produced PRs
/// and heads are first-class inputs: an adapter derives provenance from
/// lane-managed identity (the PRs and heads the lane opened), never from
/// a sender-name heuristic, and must not starve its own loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventProvenance {
    /// Produced by the lane itself: a managed PR or head.
    LaneManaged,
    /// Any other delivery, including third-party bot traffic.
    External,
}

/// The adapter event-filter contract. A filter that drops bot-authored or
/// bot-sent deliveries may apply that drop only to `External` events —
/// lane-produced PRs and heads are always admitted, whatever the sender
/// kind, or the lane's own review loop silently never fires.
pub fn admit_bot_filtered_event(
    provenance: EventProvenance,
    sender_or_author_is_bot: bool,
) -> bool {
    match provenance {
        EventProvenance::LaneManaged => true,
        EventProvenance::External => !sender_or_author_is_bot,
    }
}
