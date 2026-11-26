//! A second, independently implemented in-memory `ProcessControlPort`
//! for the provider-parameterized Worker contract suite.
//!
//! `PosixProcessControl` proves the contract against real processes.
//! `ScriptedProcessControl` replays a scripted process table —
//! deterministic spawn outcomes, liveness reads, snapshot contents, and
//! signal delivery — so the same suite scenarios run unchanged against a
//! provider that can model outcomes no argv can produce (unknown
//! identity capture, uninspectable liveness, snapshot failure).
//!
//! Artifact production is deliberately *not* scripted inside the port:
//! a scripted "exit after write" performs real filesystem writes into
//! the workspace `cwd`, so artifact presence is read back through the
//! same path the supervisor uses for real workers.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use workloop_core::domain::ProcessIdentity;
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, ProcessMatch, SignalOutcome, SpawnError,
    WorkerEnv,
};

/// What the next `spawn` produces.
#[derive(Debug, Clone)]
pub enum SpawnPlan {
    /// A writer that stays alive until a verified signal or `exit`.
    Resident,
    /// A writer that writes `artifacts` (`(name, content)`) into its
    /// cwd at spawn, then exits with `exit_code`.
    ExitAfterWrite {
        artifacts: Vec<(String, String)>,
        exit_code: i32,
    },
    /// Definitely no process exists — `SpawnError::Refused`.
    Refused { reason: String },
    /// Identity capture failed; the process may exist — in the model it
    /// does (alive), matching a real spawn-then-crash.
    Unknown { reason: String },
}

/// One recorded signal delivery — the suite's signal audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalRecord {
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
    pub signal: i32,
}

#[derive(Debug)]
struct Proc {
    started_at: String,
    pgid: Option<i32>,
    /// The cwd the spawn ran under — the evidence `matching_processes`
    /// reports for launch-outcome attribution.
    cwd: PathBuf,
    /// The argv the spawn was given, whitespace-joined like a process
    /// table's command column.
    command: String,
    alive: bool,
    exit_code: Option<i32>,
    /// When set, `inspect` reports `Unknown` — an uninspectable process,
    /// never silently dead.
    inspect_unknown: Option<String>,
    /// When set, a delivered signal is recorded but the process keeps
    /// writing — models the realizable gap between signal delivery and
    /// the process actually dying.
    survives_signal: bool,
    /// When set, `matching_processes` reports `cwd: None` — a process
    /// whose working directory the platform cannot prove (denied
    /// lsof, minimal image). It can be neither bound nor ruled out,
    /// and the directory scans err on the incomplete table.
    cwd_unreadable: bool,
}

#[derive(Debug, Default)]
struct State {
    next_pid: i32,
    /// Process table: pid → modeled process. `reap` removes rows.
    procs: BTreeMap<i32, Proc>,
    /// Spawned leader identities, in spawn order.
    spawned: Vec<ProcessIdentity>,
    /// Every `spawn` call — including attempts that returned an error.
    /// This is the suite's no-second-writer counter: an unknown start
    /// outcome still consumed a start attempt.
    spawn_calls: usize,
    /// Queued outcomes consumed FIFO by `spawn`; the default applies
    /// when the queue is empty.
    plans: VecDeque<SpawnPlan>,
    default_plan: Option<SpawnPlan>,
    /// Write-capable descendants a leader may have (scripted tree for
    /// `snapshot_descendants`).
    children: BTreeMap<i32, Vec<i32>>,
    /// Queued snapshot failures consumed FIFO.
    snapshot_errors: VecDeque<String>,
    /// Queued `matching_processes` failures consumed FIFO — an
    /// enumeration that cannot complete must never authorize an
    /// absence claim.
    scan_errors: VecDeque<String>,
    /// Queued directory-scan failures consumed FIFO by either
    /// directory query — a separate tool from the signature scan
    /// (`lsof` vs `ps`), so its failures are queued separately.
    dir_scan_errors: VecDeque<String>,
    /// Every accepted signal, in order — the write history a scenario
    /// audits without pattern-matching internals.
    signals: Vec<SignalRecord>,
    /// Queued outcomes consumed FIFO by `run_probe`; the default applies
    /// when the queue is empty.
    probes: VecDeque<Result<ProbeOutcome, SpawnError>>,
    default_probe: Option<Result<ProbeOutcome, SpawnError>>,
    /// The environment each `spawn` was given, in call order — the
    /// suite's audit of what the worker would have inherited.
    spawn_envs: Vec<WorkerEnv>,
    /// The environment each `run_probe` was given, in call order.
    probe_envs: Vec<WorkerEnv>,
    /// The argv each `run_probe` was given, in call order — the full
    /// outer argv a contained probe runs, so a suite can read the
    /// mount boundary a probe carried.
    probe_commands: Vec<Vec<String>>,
}

/// An in-memory `ProcessControlPort`. Clone the handle to steer the
/// model between supervisor passes; the port and the handle share one
/// process table.
#[derive(Debug, Clone, Default)]
pub struct ScriptedProcessControl {
    state: Arc<Mutex<State>>,
}

impl ScriptedProcessControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the outcome of the next `spawn`.
    pub fn push_plan(&self, plan: SpawnPlan) {
        self.state.lock().unwrap().plans.push_back(plan);
    }

    /// The outcome `spawn` produces when the queue is empty.
    pub fn set_default_plan(&self, plan: SpawnPlan) {
        self.state.lock().unwrap().default_plan = Some(plan);
    }

    /// Every process the model spawned, in spawn order — the
    /// process-table view, not the "identity the caller received"
    /// view: a `SpawnPlan::Unknown` outcome still records its leader
    /// because the process exists even though the spawn response was
    /// lost. `spawn_calls` remains the no-second-writer counter.
    /// The command lines the spawn calls recorded, in spawn order —
    /// the full argv a containment launch wraps, so launch-shape
    /// assertions can read the mount boundary the worker got.
    pub fn spawned_commands(&self) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.spawned
            .iter()
            .filter_map(|id| s.procs.get(&id.pid))
            .map(|p| p.command.clone())
            .collect()
    }

    pub fn spawned(&self) -> Vec<ProcessIdentity> {
        self.state.lock().unwrap().spawned.clone()
    }

    /// How many times `spawn` was invoked — the suite's
    /// no-second-writer counter.
    pub fn spawn_calls(&self) -> usize {
        self.state.lock().unwrap().spawn_calls
    }

    /// The command line each spawn was given, in spawn order — the
    /// argv a mount-plan assertion reads the bound flags from.
    pub fn commands(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .procs
            .values()
            .map(|proc| proc.command.clone())
            .collect()
    }

    /// Every recorded signal, in order.
    pub fn signals(&self) -> Vec<SignalRecord> {
        self.state.lock().unwrap().signals.clone()
    }

    /// Mark every tracked process stopped.
    pub fn exit_all(&self, exit_code: i32) {
        let mut s = self.state.lock().unwrap();
        for p in s.procs.values_mut() {
            p.alive = false;
            p.exit_code = Some(exit_code);
        }
    }

    /// Mark one process stopped.
    pub fn exit(&self, pid: i32, exit_code: i32) {
        let mut s = self.state.lock().unwrap();
        if let Some(p) = s.procs.get_mut(&pid) {
            p.alive = false;
            p.exit_code = Some(exit_code);
        }
    }

    /// Make `inspect` report `Unknown` for the pid until cleared — an
    /// uninspectable writer that is neither proven alive nor dead.
    pub fn set_inspect_unknown(&self, pid: i32, reason: Option<String>) {
        let mut s = self.state.lock().unwrap();
        if let Some(p) = s.procs.get_mut(&pid) {
            p.inspect_unknown = reason;
        }
    }

    /// Make the pid keep running after a delivered signal until
    /// cleared — the signal is recorded (delivery is auditable) but
    /// does not constitute stop proof.
    pub fn set_survives_signal(&self, pid: i32, on: bool) {
        let mut s = self.state.lock().unwrap();
        if let Some(p) = s.procs.get_mut(&pid) {
            p.survives_signal = on;
        }
    }

    /// Make `matching_processes` report `cwd: None` for the pid until
    /// cleared — an lsof-denied or minimal-image process whose launch
    /// attribution cannot be proven either way. While it is live it
    /// also makes the directory scans fail: an unmappable cwd is
    /// an enumeration gap, never a silent absence.
    pub fn set_cwd_unreadable(&self, pid: i32, on: bool) {
        let mut s = self.state.lock().unwrap();
        if let Some(p) = s.procs.get_mut(&pid) {
            p.cwd_unreadable = on;
        }
    }

    /// Report a different cwd for `pid` — a contained worker's cwd
    /// reads as its sandbox mount (`/workspace`), not the host launch
    /// directory the spawn ran under.
    pub fn set_cwd(&self, pid: i32, cwd: &std::path::Path) {
        let mut s = self.state.lock().unwrap();
        if let Some(p) = s.procs.get_mut(&pid) {
            p.cwd = cwd.to_path_buf();
        }
    }

    /// Register a write-capable descendant of `leader` (e.g. a
    /// setsid-escaped writer) so `snapshot_descendants` reports it.
    pub fn adopt_child(&self, leader: i32) -> i32 {
        let command = self
            .state
            .lock()
            .unwrap()
            .procs
            .get(&leader)
            .map(|p| p.command.clone());
        self.adopt_child_with(leader, command.as_deref().unwrap_or_default())
    }

    /// Register a live process that shares `leader`'s working
    /// directory but carries its own command line — the
    /// escaped-descendant shape the launch signature cannot see but
    /// the directory scan still finds.
    pub fn adopt_child_with(&self, leader: i32, command: &str) -> i32 {
        let mut s = self.state.lock().unwrap();
        let pid = s.next_pid;
        s.next_pid += 1;
        // A descendant shares its leader's launch context in the model.
        let cwd = s
            .procs
            .get(&leader)
            .map(|p| p.cwd.clone())
            .unwrap_or_default();
        s.procs.insert(
            pid,
            Proc {
                started_at: format!("scripted-lstart-{pid}"),
                pgid: Some(pid), // escaped its leader's group
                cwd,
                command: command.to_string(),
                alive: true,
                exit_code: None,
                inspect_unknown: None,
                survives_signal: false,
                cwd_unreadable: false,
            },
        );
        s.children.entry(leader).or_default().push(pid);
        pid
    }

    /// Register a live descendant that kept `leader`'s process group —
    /// the chdir/argv escape the launch-group check exists to catch:
    /// `group_members(leader's pgid)` still reports it even though
    /// neither the signature nor the directory scan can see it.
    pub fn adopt_child_in_group(&self, leader: i32, command: &str) -> i32 {
        let pid = self.adopt_child_with(leader, command);
        if let Some(p) = self.state.lock().unwrap().procs.get_mut(&pid) {
            p.pgid = Some(leader);
        }
        pid
    }

    /// Recycle `pid` as a foreign process: the row is replaced by a
    /// live one at the same pid with a fresh start time — the pid-reuse
    /// shape the real table produces when an unrelated process lands on
    /// a freed number. The replacement is its own group leader, so a
    /// recorded pgid equal to the pid enumerates a foreign family whose
    /// owner start differs from the launch's recorded leader.
    pub fn recycle_as(&self, pid: i32, command: &str, cwd: &std::path::Path) {
        let mut s = self.state.lock().unwrap();
        s.procs.insert(
            pid,
            Proc {
                started_at: format!("scripted-lstart-recycled-{pid}"),
                pgid: Some(pid),
                cwd: cwd.to_path_buf(),
                command: command.to_string(),
                alive: true,
                exit_code: None,
                inspect_unknown: None,
                survives_signal: false,
                cwd_unreadable: false,
            },
        );
    }

    /// The next `snapshot_descendants` fails with this reason — an
    /// incomplete process-table read that must not authorize signals.
    pub fn fail_next_snapshot(&self, reason: &str) {
        self.state
            .lock()
            .unwrap()
            .snapshot_errors
            .push_back(reason.to_string());
    }

    /// The next `matching_processes` scan fails with this reason —
    /// an unreadable process table that must never prove a writer
    /// absent.
    pub fn fail_next_process_scan(&self, reason: &str) {
        self.state
            .lock()
            .unwrap()
            .scan_errors
            .push_back(reason.to_string());
    }

    /// The next `processes_in_directory` scan fails with this reason —
    /// an unreadable directory enumeration that must never prove a
    /// vanished launch's tree is gone.
    pub fn fail_next_directory_scan(&self, reason: &str) {
        self.state
            .lock()
            .unwrap()
            .dir_scan_errors
            .push_back(reason.to_string());
    }

    /// Queue the outcome of the next `run_probe` call — a scripted
    /// `describe` answer (or its failure), consumed FIFO.
    pub fn push_probe(&self, outcome: Result<ProbeOutcome, SpawnError>) {
        self.state.lock().unwrap().probes.push_back(outcome);
    }

    /// The outcome `run_probe` produces when the queue is empty.
    /// Replaces the built-in default declaration.
    pub fn set_default_probe(&self, outcome: Result<ProbeOutcome, SpawnError>) {
        self.state.lock().unwrap().default_probe = Some(outcome);
    }

    /// The environment every `spawn` was given, in call order — the
    /// suite audits that no host credential name ever reaches a launch.
    pub fn spawn_envs(&self) -> Vec<WorkerEnv> {
        self.state.lock().unwrap().spawn_envs.clone()
    }

    /// The environment every `run_probe` was given, in call order.
    pub fn probe_envs(&self) -> Vec<WorkerEnv> {
        self.state.lock().unwrap().probe_envs.clone()
    }

    /// The argv every `run_probe` was given, in call order.
    pub fn probe_commands(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().probe_commands.clone()
    }

    /// The shared scan behind both directory queries — the model's
    /// cwd is ground truth: `cwd_within` canonicalizes host-backed
    /// spellings, and a path no host inode backs (a contained
    /// worker's `/workspace`) compares literally.
    /// `report_unresolvable` selects the contract: the genuine scan
    /// fails on such a spelling inside `dir` exactly like
    /// `PosixProcessControl`; the namespace query reports it as
    /// occupant data for the launch-scope filter instead.
    fn directory_scan(
        &self,
        dir: &Path,
        report_unresolvable: bool,
    ) -> Result<Vec<ProcessMatch>, String> {
        let mut s = self.state.lock().unwrap();
        if let Some(reason) = s.dir_scan_errors.pop_front() {
            return Err(reason);
        }
        // The model's cwd is provable unless flagged; a live process
        // the scan cannot map makes the whole enumeration incomplete —
        // like a real table read that cannot prove every row, the
        // scan errs rather than return a truncated set an absence
        // proof would read as clear.
        if s.procs.values().any(|p| p.alive && p.cwd_unreadable) {
            return Err("directory scan incomplete: process cwd unreadable".into());
        }
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        // A live process whose modeled cwd has no host inode yet is
        // spelled inside `dir` — a contained worker's `/workspace`
        // under the sandbox-root scan — is the real port's
        // fail-closed edge: the genuine scan errs on the same table
        // rather than silently drop the pid; only the namespace
        // query surfaces it as data. A ` (deleted)`-marked cwd is
        // not ambiguous that way: the marker reports the exact
        // directory the process was in, so the model counts a
        // marked spelling inside `dir` as an occupant — the same
        // table the real port produces — under either contract.
        if !report_unresolvable {
            for (pid, p) in &s.procs {
                if !p.alive {
                    continue;
                }
                let (cwd, deleted) = crate::supervisor::strip_deleted_marker(&p.cwd);
                if !deleted && std::fs::canonicalize(&cwd).is_err() && cwd.starts_with(&dir) {
                    return Err(format!(
                        "directory scan incomplete: process {pid} cwd {} is not resolvable",
                        cwd.display()
                    ));
                }
            }
        }
        Ok(s.procs
            .iter()
            // The contract is "cwd inside `dir`" — never widened to
            // the launch scope; the supervisor layers
            // `SANDBOX_WORKSPACE` on at its own call sites. The
            // namespace query additionally counts an unresolvable
            // spelling inside `dir`, matching the real port; both
            // queries count a ` (deleted)`-marked spelling inside
            // `dir` as an occupant.
            .filter_map(|(pid, p)| {
                if !p.alive {
                    return None;
                }
                let (cwd, deleted) = crate::supervisor::strip_deleted_marker(&p.cwd);
                let inside = (deleted && cwd.starts_with(&dir))
                    || crate::supervisor::cwd_within(Some(&cwd), &dir)
                    || (report_unresolvable
                        && !deleted
                        && std::fs::canonicalize(&cwd).is_err()
                        && cwd.starts_with(&dir));
                inside.then(|| ProcessMatch {
                    identity: ProcessIdentity {
                        pid: *pid,
                        started_at: p.started_at.clone(),
                        pgid: p.pgid,
                    },
                    cwd: Some(cwd),
                    command: Some(p.command.clone()),
                })
            })
            .collect())
    }
}

impl ProcessControlPort for ScriptedProcessControl {
    fn spawn(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError> {
        let mut s = self.state.lock().unwrap();
        s.spawn_calls += 1;
        s.spawn_envs.push(env.clone());
        let command = argv.join(" ");
        let plan = s
            .plans
            .pop_front()
            .or_else(|| s.default_plan.clone())
            .unwrap_or(SpawnPlan::Resident);
        match plan {
            SpawnPlan::Refused { reason } => Err(SpawnError::Refused { reason }),
            SpawnPlan::Unknown { reason } => {
                // The process exists in the model — a real spawn that
                // crashed before identity capture leaves a live writer.
                // Its leader identity is still recorded: the process
                // was spawned even though the response was lost.
                let pid = s.next_pid;
                s.next_pid += 1;
                let started_at = format!("scripted-lstart-{pid}");
                let identity = ProcessIdentity {
                    pid,
                    started_at: started_at.clone(),
                    pgid: Some(pid),
                };
                s.procs.insert(
                    pid,
                    Proc {
                        started_at,
                        pgid: identity.pgid,
                        cwd: cwd.to_path_buf(),
                        command,
                        alive: true,
                        exit_code: None,
                        inspect_unknown: None,
                        survives_signal: false,
                        cwd_unreadable: false,
                    },
                );
                s.spawned.push(identity);
                Err(SpawnError::Unknown {
                    reason,
                    pgid: Some(pid),
                })
            }
            SpawnPlan::Resident | SpawnPlan::ExitAfterWrite { .. } => {
                let pid = s.next_pid;
                s.next_pid += 1;
                let started_at = format!("scripted-lstart-{pid}");
                let (alive, exit_code) = match &plan {
                    SpawnPlan::Resident => (true, None),
                    SpawnPlan::ExitAfterWrite {
                        artifacts,
                        exit_code,
                    } => {
                        // Real filesystem writes: the supervisor's
                        // artifact read sees exactly what a real
                        // worker's write would leave.
                        for (name, content) in artifacts {
                            let path: PathBuf = cwd.join(name);
                            if let Some(parent) = path.parent() {
                                std::fs::create_dir_all(parent).map_err(|e| {
                                    SpawnError::Refused {
                                        reason: format!("mkdir {parent:?}: {e}"),
                                    }
                                })?;
                            }
                            std::fs::write(&path, content).map_err(|e| SpawnError::Refused {
                                reason: format!("write {path:?}: {e}"),
                            })?;
                        }
                        (false, Some(*exit_code))
                    }
                    _ => unreachable!(),
                };
                let identity = ProcessIdentity {
                    pid,
                    started_at,
                    pgid: Some(pid),
                };
                s.procs.insert(
                    pid,
                    Proc {
                        started_at: identity.started_at.clone(),
                        pgid: identity.pgid,
                        cwd: cwd.to_path_buf(),
                        command,
                        alive,
                        exit_code,
                        inspect_unknown: None,
                        survives_signal: false,
                        cwd_unreadable: false,
                    },
                );
                s.spawned.push(identity.clone());
                Ok(identity)
            }
        }
    }

    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {
        // Check-and-pop is one critical section: a cloned handle racing
        // this caller must not observe or consume the same queued error
        // twice — "the next snapshot_descendants fails" is exactly one
        // call.
        let mut s = self.state.lock().unwrap();
        if let Some(reason) = s.snapshot_errors.pop_front() {
            return Err(reason);
        }
        let mut out = Vec::new();
        let mut stack = vec![leader_pid];
        let mut seen = std::collections::HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(p) = s.procs.get(&pid) {
                out.push(ProcessIdentity {
                    pid,
                    started_at: p.started_at.clone(),
                    pgid: p.pgid,
                });
            }
            if let Some(children) = s.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        Ok(out)
    }

    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String> {
        let s = self.state.lock().unwrap();
        // Every process in the group — orphans that reparented out of
        // the ppid tree keep the recorded group, exactly the coverage
        // `snapshot_descendants` cannot give once the leader is gone.
        Ok(s.procs
            .iter()
            .filter(|(_, p)| p.pgid == Some(pgid))
            .map(|(pid, p)| ProcessIdentity {
                pid: *pid,
                started_at: p.started_at.clone(),
                pgid: p.pgid,
            })
            .collect())
    }

    fn matching_processes(&self, command: &str) -> Result<Vec<ProcessMatch>, String> {
        let mut s = self.state.lock().unwrap();
        if let Some(reason) = s.scan_errors.pop_front() {
            return Err(reason);
        }
        let expected = crate::supervisor::normalize_lstart(command);
        // The model's process table is system-wide like the real one:
        // every live process whose command matches is a candidate,
        // parentage irrelevant — a reparented orphan still matches.
        // Dead rows are not matches — `alive` is the model's liveness
        // read, and an uninspectable row still counts as live.
        Ok(s.procs
            .iter()
            .filter(|(_, p)| p.alive && crate::supervisor::normalize_lstart(&p.command) == expected)
            .map(|(pid, p)| ProcessMatch {
                identity: ProcessIdentity {
                    pid: *pid,
                    started_at: p.started_at.clone(),
                    pgid: p.pgid,
                },
                cwd: (!p.cwd_unreadable).then(|| p.cwd.clone()),
                command: Some(p.command.clone()),
            })
            .collect())
    }

    fn processes_in_directory(&self, dir: &Path) -> Result<Vec<ProcessMatch>, String> {
        self.directory_scan(dir, false)
    }

    fn processes_in_namespace_dir(&self, dir: &Path) -> Result<Vec<ProcessMatch>, String> {
        self.directory_scan(dir, true)
    }

    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        let s = self.state.lock().unwrap();
        let Some(p) = s.procs.get(&identity.pid) else {
            // Absent from the process table is definitive.
            return ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: None,
            };
        };
        if let Some(reason) = &p.inspect_unknown {
            // An incomplete liveness read is never a silent Stopped.
            return ProcessEvidence::Unknown {
                reason: reason.clone(),
            };
        }
        if p.started_at != identity.started_at {
            // Reused pid: the recorded process is gone.
            return ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: None,
            };
        }
        if p.alive {
            ProcessEvidence::Alive {
                identity: identity.clone(),
            }
        } else {
            ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: p.exit_code,
            }
        }
    }

    fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome {
        let mut s = self.state.lock().unwrap();
        let Some(p) = s.procs.get_mut(&identity.pid) else {
            return SignalOutcome::AlreadyGone;
        };
        // Re-verification is impossible while the liveness read is
        // unavailable — a conforming port fails the signal rather than
        // signalling an unverified process.
        if let Some(reason) = &p.inspect_unknown {
            return SignalOutcome::Failed {
                reason: reason.clone(),
            };
        }
        if p.started_at != identity.started_at {
            return SignalOutcome::SkippedStale;
        }
        if !p.alive {
            return SignalOutcome::AlreadyGone;
        }
        if !p.survives_signal {
            p.alive = false;
        }
        s.signals.push(SignalRecord {
            pid: Some(identity.pid),
            pgid: None,
            signal,
        });
        SignalOutcome::Sent
    }

    fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome {
        let mut s = self.state.lock().unwrap();
        let mut hit = false;
        for p in s.procs.values_mut() {
            if p.pgid == Some(pgid) && p.alive {
                if !p.survives_signal {
                    p.alive = false;
                }
                hit = true;
            }
        }
        if hit {
            s.signals.push(SignalRecord {
                pid: None,
                pgid: Some(pgid),
                signal,
            });
            SignalOutcome::Sent
        } else {
            SignalOutcome::AlreadyGone
        }
    }

    fn reap(&self, pid: i32) -> Option<i32> {
        // The modeled exit status is collected with the reap, like a
        // real waitpid: a process that ended by signal or was never
        // observed exiting carries no proven code.
        self.state
            .lock()
            .unwrap()
            .procs
            .remove(&pid)
            .and_then(|p| p.exit_code)
    }

    fn run_probe(
        &self,
        argv: &[String],
        _cwd: &Path,
        env: &WorkerEnv,
        _timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        let outcome = {
            let mut s = self.state.lock().unwrap();
            s.probe_envs.push(env.clone());
            s.probe_commands.push(argv.to_vec());
            s.probes.pop_front().or_else(|| s.default_probe.clone())
        };
        match outcome {
            Some(outcome) => outcome,
            // Default: the scripted adapter does implement `describe`,
            // declaring an argv-derived identity so a suite's requests
            // bind deterministically.
            None => {
                let adapter = argv
                    .first()
                    .and_then(|program| Path::new(program).file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("scripted-worker");
                Ok(ProbeOutcome::Exited {
                    stdout: serde_json::json!({
                        "adapter": adapter,
                        "adapter_version": "1.0.0",
                        "runtime_version": "scripted-1.0.0",
                        "protocol_versions": ["taisan.worker/1"],
                        "roles": ["coding", "fix", "invariant_check", "review"],
                        "capabilities": ["workspace_write", "workspace_read_only", "artifact_json", "process_control"],
                    })
                    .to_string()
                    .into_bytes(),
                    success: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "The next `snapshot_descendants` fails with this reason" is
    /// exactly one call: two cloned handles racing on one queued error
    /// must not both observe it, and the unconsumed caller gets a normal
    /// snapshot.
    #[test]
    fn queued_snapshot_error_is_consumed_by_exactly_one_caller() {
        let a = ScriptedProcessControl::new();
        let b = a.clone();
        a.fail_next_snapshot("incomplete process-table read");

        let barrier = std::sync::Barrier::new(2);
        let results: Vec<Result<Vec<ProcessIdentity>, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = [a, b]
                .into_iter()
                .map(|handle| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        handle.snapshot_descendants(0)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let errors: Vec<_> = results.iter().filter(|r| r.is_err()).collect();
        assert_eq!(errors.len(), 1, "queued error consumed exactly once");
        assert_eq!(
            errors[0].as_ref().unwrap_err(),
            "incomplete process-table read"
        );
        assert!(
            results.iter().any(|r| r.is_ok()),
            "the other caller gets a normal snapshot"
        );
    }

    /// Sequentially: one queued error fails one call, the next succeeds.
    #[test]
    fn queued_snapshot_error_is_consumed_fifo() {
        let port = ScriptedProcessControl::new();
        port.fail_next_snapshot("first");
        port.fail_next_snapshot("second");
        assert_eq!(port.snapshot_descendants(0).unwrap_err(), "first");
        assert_eq!(port.snapshot_descendants(0).unwrap_err(), "second");
        assert!(port.snapshot_descendants(0).unwrap().is_empty());
    }
}
