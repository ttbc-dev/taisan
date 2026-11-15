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

/// The complete environment a spawned worker or probe runs under — an
/// allowlist constructed by the isolation profile, never the
/// supervisor's inherited environment. The port applies it verbatim:
/// a variable not listed here does not exist for the child, so a host
/// credential (`GH_TOKEN`, `SSH_AUTH_SOCK`, `HOME` pointing at the
/// owner's home, a git credential helper config) cannot leak in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerEnv {
    /// The child's whole environment as `name=value` pairs.
    pub vars: Vec<(String, String)>,
}

/// One bound role actor whose remote identity `check --probe` reads.
/// The context is pinned to the configuration the preflight already
/// validated — the probe never picks its own host or repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRead {
    /// Bound role name (`publication` / `review` / `discussion`).
    pub role: String,
    /// The bound actor identity the observation must match.
    pub actor: String,
    /// Host instance identity (the stable endpoint), from config.
    pub host_instance: String,
    /// Repository stable id the observation is pinned to, from config.
    pub repository: String,
}

/// What a remote identity read observed for one bound actor: the actor
/// login or application slug plus the host instance and repository the
/// read was pinned to. Only these observed identifiers cross the port —
/// never a credential value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedIdentity {
    /// Observed actor login or application slug.
    pub login: String,
    /// Host instance the adapter reported for this read.
    pub host_instance: String,
    /// Repository stable id the adapter reported for this read.
    pub repository: String,
}

/// One capability as a preflight probe reports it. Support and grant
/// are evaluated separately per the repository-host contract, so
/// `unsupported` never masquerades as `denied` and an unreadable answer
/// stays `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Unknown,
}

/// Whether the bound actor may perform the capability's operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityGrant {
    Allowed,
    Denied,
    Unknown,
}

/// One requested capability's reported support and grant for a bound
/// actor on the pinned repository context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityStatus {
    /// Capability name as declared in configuration.
    pub capability: String,
    pub support: CapabilitySupport,
    pub grant: CapabilityGrant,
}

/// The complete answer to one capability read: one status per requested
/// capability, in request order. A missing entry is treated by the
/// caller as `unknown`, never as an empty pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityObservation {
    pub statuses: Vec<CapabilityStatus>,
}

/// The read-only adapter surface `check --probe` opts into. The trait
/// exposes identity and capability *reads* only — there is no write
/// method to call, so a probe run is read-only by construction: it can
/// never create, mutate, or delete anything remotely. Dispatch-time
/// revalidation stays mandatory; a passing probe is per-run evidence,
/// never a standing grant.
pub trait ProbePort {
    /// Read the remote identity (actor login or app slug) for one
    /// bound actor in the pinned host/repository context.
    fn read_actor_identity(&mut self, read: &IdentityRead) -> Result<ObservedIdentity, String>;

    /// Read the support/grant report for the requested capabilities of
    /// one bound actor in the pinned repository context.
    fn read_capabilities(&mut self, read: &CapabilityRead)
    -> Result<CapabilityObservation, String>;
}

/// The capability set one bound actor's probe read covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRead {
    /// Bound role name.
    pub role: String,
    /// The bound actor identity being evaluated.
    pub actor: String,
    /// Host instance identity (the stable endpoint), from config.
    pub host_instance: String,
    /// Repository stable id the read is pinned to, from config.
    pub repository: String,
    /// Required capability names for this role; the answer must carry
    /// one status per name.
    pub capabilities: Vec<String>,
}

/// How `execution` supervises real worker processes. Implementations must
/// satisfy the crash rules in `docs/lifecycle.md`: record `pid + start
/// time`, snapshot write-capable descendants with the stop intent, signal
/// only re-verified identities, read state not presence, and reap spawned
/// children.
pub trait ProcessControlPort {
    /// Spawn `argv` with `cwd` in its own process group under `env` — the
    /// allowlisted environment the isolation profile built; the caller's
    /// environment is never inherited. Returns the leader's
    /// host-observable identity on success.
    fn spawn(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError>;

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
    /// child past the bound. The probe runs under `env`, the same
    /// allowlisted environment a writer launch gets — an untrusted
    /// adapter binary must not see more during declaration than it does
    /// during execution. `SpawnError` means no probe ran (`Refused`) or
    /// its fate is unknown (`Unknown`); a caller must treat either as
    /// "the adapter did not answer".
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError>;
}
