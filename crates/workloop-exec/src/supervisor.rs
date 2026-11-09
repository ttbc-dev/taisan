//! POSIX process supervision. Requirements verified by the macOS spike:
//! record the descendant snapshot with the stop intent (orphans reparent
//! and leave the process tree once the leader exits), re-verify
//! `pid + start time` before signaling, read process state rather than
//! presence (a zombie still lists but cannot write), and reap children the
//! supervisor spawned.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use workloop_core::domain::*;
use workloop_core::observation::{
    ArtifactFact, DeliveryId, DeliveryScope, Observation, ObservationFact, ProcessStateFact,
};
use workloop_core::ports::{
    ProcessControlPort, ProcessEvidence, SignalOutcome, SpawnError, StopReceipt,
};
use workloop_core::store::{Effect, Store, StoreError};

/// The POSIX `ProcessControlPort`. Identities are `pid + lstart` strings
/// captured from `ps`; the recorded string is compared verbatim before any
/// signal so a reused pid is never signaled.
pub struct PosixProcessControl;

fn ps_output(args: &[&str]) -> std::io::Result<String> {
    let out = Command::new("ps").args(args).output()?;
    checked_ps_output(out, args.contains(&"-p"))
}

fn checked_ps_output(out: Output, allows_absence: bool) -> std::io::Result<String> {
    if out.status.success()
        || (allows_absence
            && out.status.code() == Some(1)
            && out.stdout.is_empty()
            && out.stderr.is_empty())
    {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(std::io::Error::other(format!(
            "ps failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// `ps` lstart prints five fields (`Sat Oct 24 06:14:00 2026`). The ctime
/// day field is space-padded for single-digit days (`Oct  4`), so the
/// stored verifier is whitespace-normalized — every identity path
/// (spawn, snapshot, re-verify) must produce the same string.
fn normalize_lstart(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One `ps`-based lstart lookup, keeping "the process is gone" separate
/// from "the lookup could not be completed". Only `Absent` (and a
/// verified start-time mismatch) may prove a stop; `Failed` proves
/// nothing and must surface as `Unknown`/`Failed` to callers.
#[derive(Debug, PartialEq, Eq)]
enum LstartLookup {
    Present(String),
    Absent,
    Failed(String),
}

fn classify_lstart_lookup(out: std::io::Result<String>) -> LstartLookup {
    match out {
        Err(e) => LstartLookup::Failed(e.to_string()),
        Ok(out) => {
            let s = out.trim();
            if s.is_empty() {
                // `ps` ran and listed nothing: the pid is not in the
                // process table.
                LstartLookup::Absent
            } else {
                LstartLookup::Present(normalize_lstart(s))
            }
        }
    }
}

fn lstart_of(pid: i32) -> LstartLookup {
    classify_lstart_lookup(ps_output(&["-o", "lstart=", "-p", &pid.to_string()]))
}

fn stat_of(pid: i32) -> Option<String> {
    let out = ps_output(&["-o", "stat=", "-p", &pid.to_string()]).ok()?;
    let s = out.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

impl ProcessControlPort for PosixProcessControl {
    fn spawn(&self, argv: &[String], cwd: &Path) -> Result<ProcessIdentity, SpawnError> {
        let Some(program) = argv.first() else {
            return Err(SpawnError::Refused {
                reason: "empty argv".into(),
            });
        };
        // Own process group so a group signal reaches the whole tree the
        // leader did not let escape.
        let mut cmd = Command::new(program);
        cmd.args(&argv[1..]).current_dir(cwd).process_group(0);
        let child = cmd.spawn().map_err(|e| SpawnError::Refused {
            reason: format!("spawn {program}: {e}"),
        })?;
        let pid = child.id() as i32;
        // Dropping the handle never kills; reap() later calls waitpid on
        // the recorded pid, which is still our child.
        drop(child);
        let started_at = match lstart_of(pid) {
            LstartLookup::Present(s) => s,
            LstartLookup::Absent => {
                return Err(SpawnError::Unknown {
                    reason: format!("spawned pid {pid} vanished before identity capture"),
                });
            }
            LstartLookup::Failed(reason) => return Err(SpawnError::Unknown { reason }),
        };
        let pgid = unsafe { libc::getpgid(pid) };
        Ok(ProcessIdentity {
            pid,
            started_at,
            pgid: if pgid > 0 { Some(pgid) } else { None },
        })
    }

    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {
        // pid, ppid, pgid, lstart(5 fields). Collected while the leader is
        // still alive — after it exits, orphans reparent and leave the tree.
        let table = ps_output(&["-Ao", "pid=,ppid=,pgid=,lstart="])
            .map_err(|e| format!("descendant snapshot failed: {e}"))?;
        let mut by_ppid: HashMap<i32, Vec<(i32, i32, String)>> = HashMap::new();
        let mut all = HashMap::new();
        for line in table.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.is_empty() {
                continue;
            }
            if f.len() != 8 {
                return Err(format!("incomplete process-table row: {line}"));
            }
            let (Ok(pid), Ok(ppid), Ok(pgid)) = (
                f[0].parse::<i32>(),
                f[1].parse::<i32>(),
                f[2].parse::<i32>(),
            ) else {
                return Err(format!("invalid process-table row: {line}"));
            };
            let lstart = normalize_lstart(&f[3..].join(" "));
            by_ppid
                .entry(ppid)
                .or_default()
                .push((pid, pgid, lstart.clone()));
            all.insert(pid, (pgid, lstart));
        }
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![leader_pid];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some((pgid, lstart)) = all.get(&pid) {
                out.push(ProcessIdentity {
                    pid,
                    started_at: lstart.clone(),
                    pgid: Some(*pgid).filter(|g| *g != 0),
                });
            }
            if let Some(children) = by_ppid.get(&pid) {
                stack.extend(children.iter().map(|(c, _, _)| *c));
            }
        }
        Ok(out)
    }

    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        let lstart = match lstart_of(identity.pid) {
            // The pid is gone; this identity no longer exists.
            LstartLookup::Absent => {
                return ProcessEvidence::Stopped {
                    identity: identity.clone(),
                    exit_code: None,
                };
            }
            // The lookup itself failed: an incomplete process-table read
            // is never stop proof — the port contract requires Unknown.
            LstartLookup::Failed(reason) => return ProcessEvidence::Unknown { reason },
            LstartLookup::Present(lstart) => lstart,
        };
        if normalize_lstart(&lstart) != normalize_lstart(&identity.started_at) {
            // Reused pid: the recorded process is gone.
            return ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: None,
            };
        }
        match stat_of(identity.pid) {
            None => ProcessEvidence::Unknown {
                reason: format!("pid {} present at lstart but stat unread", identity.pid),
            },
            Some(stat) if stat.contains('Z') => ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: None,
            },
            Some(_) => ProcessEvidence::Alive {
                identity: identity.clone(),
            },
        }
    }

    fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome {
        match lstart_of(identity.pid) {
            // The identity cannot be re-verified: delivery cannot be
            // confirmed, and absence must never be assumed from a failed
            // lookup.
            LstartLookup::Failed(reason) => SignalOutcome::Failed { reason },
            LstartLookup::Absent => SignalOutcome::AlreadyGone,
            LstartLookup::Present(now) => {
                if normalize_lstart(&now) != normalize_lstart(&identity.started_at) {
                    SignalOutcome::SkippedStale
                } else {
                    let rc = unsafe { libc::kill(identity.pid, signal) };
                    if rc == 0 {
                        SignalOutcome::Sent
                    } else {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::ESRCH) {
                            SignalOutcome::AlreadyGone
                        } else {
                            SignalOutcome::Failed {
                                reason: format!("kill({}, {signal}): {e}", identity.pid),
                            }
                        }
                    }
                }
            }
        }
    }

    fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome {
        let rc = unsafe { libc::kill(-pgid, signal) };
        if rc == 0 {
            SignalOutcome::Sent
        } else {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                SignalOutcome::AlreadyGone
            } else {
                SignalOutcome::Failed {
                    reason: format!("killpg({pgid}, {signal}): {e}"),
                }
            }
        }
    }

    fn reap(&self, pid: i32) {
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
    }
}

/// Anything `execution` can fail doing; store errors propagate verbatim so
/// callers can tell a durable-write failure from a process-layer one.
#[derive(Debug)]
pub enum ExecError {
    Store(StoreError),
    Io(std::io::Error),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ExecError {}
impl From<StoreError> for ExecError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<std::io::Error> for ExecError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The outcome of the durable-intent half of the stop path
/// (`Supervisor::record_stop_intent`).
#[derive(Debug)]
pub enum StopRecord {
    /// The stop intent — descendant snapshot included — is durable; its
    /// recorded identities are the only ones a later signal pass may
    /// target.
    Recorded(StopIntent),
    /// The snapshot or the durable write could not be completed: nothing
    /// was persisted, so no signal may be sent.
    Unknown { reason: String },
}

/// The execution supervisor: consumes approved requests, spawns workers,
/// drives stop intents to stop proof, and runs bounded artifact recovery.
/// Start and stop for one request are serialized through a per-request
/// lock. It never decides lifecycle state — it reports Observations.
pub struct Supervisor<P: ProcessControlPort> {
    pub port: P,
    /// Managed root; each Work's workspace is `<root>/ws-<work_id>`.
    pub workspace_root: PathBuf,
    /// The worker command line the composition root supplies.
    pub worker_argv: Vec<String>,
    /// The installation config `work` decisions are made against.
    pub config: workloop_core::work::WorkloopConfig,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl<P: ProcessControlPort> Supervisor<P> {
    pub fn new(
        port: P,
        workspace_root: PathBuf,
        worker_argv: Vec<String>,
        config: workloop_core::work::WorkloopConfig,
    ) -> Self {
        Self {
            port,
            workspace_root,
            worker_argv,
            config,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    #[doc(hidden)]
    pub fn locks_len(&self) -> usize {
        self.locks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn workspace_of(&self, work_id: &WorkId) -> PathBuf {
        self.workspace_root.join(format!("ws-{}", work_id.0))
    }

    /// One supervisor pass: consume approved requests, inspect live
    /// invocations, drive stop intents, and run one recovery attempt for
    /// each Execution waiting on artifact recovery. Reconciles by scanning
    /// the store — a wakeup is an at-least-once signal, not evidence.
    pub fn run_once(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {
        self.consume_requests(store, now_unix_seconds)?;
        self.poll_invocations(store, now_unix_seconds)?;
        self.drive_stops(store, now_unix_seconds)?;
        self.run_recovery(store, now_unix_seconds)?;
        Ok(())
    }

    /// Replay the durable request log: for each unconsumed request either
    /// reconcile the recorded Invocation (never a second start) or record
    /// one and launch.
    pub fn consume_requests(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        for request in store.transact(|tx| tx.pending_requests())? {
            self.reconcile_or_start(store, &request, now_unix_seconds)?;
        }
        Ok(())
    }

    fn reconcile_or_start(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let lock = self.lock_for(&request.request_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(invocation) =
            store.transact(|tx| tx.invocation_for_request(&request.request_id))?
        {
            // The request already produced an Invocation — reconcile it,
            // never start another writer for the same request.
            self.mark_consumed(store, &request.request_id)?;
            match (&invocation.state, &invocation.process_identity) {
                (InvocationState::Prepared, None) => {
                    // Recorded before launch, no identity: the spawn outcome
                    // is unknown — report it; `work` keeps the Execution
                    // uncertain and blocks a replacement.
                    self.report(
                        store,
                        request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: String::new(),
                            state: ProcessStateFact::Unknown,
                        },
                        now_unix_seconds,
                    )?;
                }
                (_, Some(identity)) => {
                    let ev = self.port.inspect(identity);
                    self.report_evidence(store, request, &invocation, ev, now_unix_seconds)?;
                }
                _ => {
                    self.mark_done_if_resolved(store, request, &invocation)?;
                }
            }
            return Ok(());
        }

        // First consumption: persist the Invocation inside the same commit
        // that consumes the request — crash order is identity before launch.
        // Recording the Invocation flips `consumed` itself; the explicit
        // mark records the supervisor's handoff and is idempotent.
        let invocation = Invocation {
            id: InvocationId(store.transact(|tx| tx.alloc_id("inv"))?),
            request_id: request.request_id.clone(),
            execution_id: request.execution_id.clone(),
            role: request.role.clone(),
            state: InvocationState::Prepared,
            process_identity: None,
            required_artifacts: required_artifacts(request),
            evidence_refs: vec![],
        };
        store.transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: invocation.clone(),
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })?;

        // Launch only for a freshly admitted Execution. A request consumed
        // while the Execution wants stop (or is uncertain, waiting, or
        // terminal) must not produce a writer — the invocation is
        // abandoned so `drive_stops` can settle the Execution instead of
        // leaving it blocked on a writer that never existed.
        let exec_state = store
            .transact(|tx| tx.execution(&request.execution_id))?
            .map(|e| e.state);
        if exec_state != Some(ExecutionState::Ready) {
            store.transact(|tx| {
                let mut next = invocation.clone();
                next.state = InvocationState::Abandoned;
                tx.apply(&Effect::UpdateInvocation {
                    invocation: next,
                    expected_state: InvocationState::Prepared,
                })?;
                Ok(())
            })?;
            return Ok(());
        }

        let work_id = request.work_id.clone();
        let workspace = self.workspace_of(&work_id);
        std::fs::create_dir_all(&workspace)?;
        let argv = self.worker_argv.clone();
        match self.port.spawn(&argv, &workspace) {
            Ok(identity) => {
                store.transact(|tx| {
                    let mut next = invocation.clone();
                    next.state = InvocationState::Running;
                    next.process_identity = Some(identity.clone());
                    tx.apply(&Effect::UpdateInvocation {
                        invocation: next,
                        expected_state: InvocationState::Prepared,
                    })?;
                    Ok(())
                })?;
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: format!("{}@{}", identity.pid, identity.started_at),
                        state: ProcessStateFact::Running,
                    },
                    now_unix_seconds,
                )?;
            }
            Err(SpawnError::Refused { reason }) => {
                store.transact(|tx| {
                    let mut next = invocation.clone();
                    next.state = InvocationState::Abandoned;
                    tx.apply(&Effect::UpdateInvocation {
                        invocation: next,
                        expected_state: InvocationState::Prepared,
                    })?;
                    Ok(())
                })?;
                self.report(
                    store,
                    request,
                    ObservationFact::LaunchRefused {
                        invocation_id: invocation.id.clone(),
                        reason,
                    },
                    now_unix_seconds,
                )?;
            }
            Err(SpawnError::Unknown { .. }) => {
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: String::new(),
                        state: ProcessStateFact::Unknown,
                    },
                    now_unix_seconds,
                )?;
            }
        }
        Ok(())
    }

    fn mark_consumed(&self, store: &impl Store, request_id: &str) -> Result<(), ExecError> {
        store
            .transact(|tx| {
                tx.apply(&Effect::MarkRequestConsumed {
                    request_id: request_id.to_string(),
                })
            })
            .or_else(|e| match e {
                // Already consumed is the at-least-once delivery case.
                StoreError::ConstraintConflict { .. } => Ok(()),
                other => Err(other),
            })?;
        Ok(())
    }

    fn mark_done_if_resolved(
        &self,
        _store: &impl Store,
        _request: &InvocationRequest,
        _invocation: &Invocation,
    ) -> Result<(), ExecError> {
        Ok(())
    }

    /// Inspect every unresolved Invocation with a recorded identity:
    /// report exits (with artifact results), liveness losses, and reap the
    /// children this supervisor spawned.
    pub fn poll_invocations(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let unresolved = store.transact(|tx| tx.unresolved_invocations())?;
        for invocation in unresolved {
            let Some(identity) = invocation.process_identity.clone() else {
                // Prepared but never identified: a crash near spawn left no
                // provable answer — report unknown; `work` keeps the
                // Execution uncertain rather than starting a second writer.
                if invocation.state == InvocationState::Prepared
                    && let Some(request) =
                        store.transact(|tx| tx.request(&invocation.request_id))?
                {
                    self.report(
                        store,
                        &request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: String::new(),
                            state: ProcessStateFact::Unknown,
                        },
                        now_unix_seconds,
                    )?;
                }
                continue;
            };
            // Stopping Executions are driven by drive_stops, not here.
            let exec_state = store.transact(|tx| {
                Ok::<_, StoreError>(tx.execution(&invocation.execution_id)?.map(|e| e.state))
            })?;
            if matches!(exec_state, Some(ExecutionState::Stopping)) {
                continue;
            }
            match self.port.inspect(&identity) {
                ProcessEvidence::Stopped { .. } => {
                    self.port.reap(identity.pid);
                    self.finish_invocation(store, &invocation, now_unix_seconds)?;
                }
                ProcessEvidence::Alive { .. } => {
                    // Re-assert liveness. A crash (or failed observation
                    // commit) between persisting the running identity and
                    // reporting ProcessState::Running would otherwise
                    // leave the Execution in Ready forever while the
                    // writer lives; the deterministic delivery id makes a
                    // repeated identical report a store-level duplicate.
                    let request = store
                        .transact(|tx| tx.request(&invocation.request_id))?
                        .ok_or(ExecError::Store(StoreError::Internal {
                            detail: "invocation without request".into(),
                        }))?;
                    self.report(
                        store,
                        &request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: format!("{}@{}", identity.pid, identity.started_at),
                            state: ProcessStateFact::Running,
                        },
                        now_unix_seconds,
                    )?;
                }
                ProcessEvidence::Unknown { .. } => {
                    let request = store
                        .transact(|tx| tx.request(&invocation.request_id))?
                        .ok_or(ExecError::Store(StoreError::Internal {
                            detail: "invocation without request".into(),
                        }))?;
                    self.report(
                        store,
                        &request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: format!("{}@{}", identity.pid, identity.started_at),
                            state: ProcessStateFact::Unknown,
                        },
                        now_unix_seconds,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Save every artifact fact before the exit fact resolves the writer.
    /// Interrupted reporting leaves the Invocation in the next scan.
    fn finish_invocation(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let request = store
            .transact(|tx| tx.request(&invocation.request_id))?
            .ok_or(ExecError::Store(StoreError::Internal {
                detail: "invocation without request".into(),
            }))?;
        let workspace = self.workspace_of(&request.work_id);
        for required in &invocation.required_artifacts {
            let digest = artifact_digest(&workspace, &required.path);
            // A present artifact must carry its exact payload digest; an
            // unexpected payload is corrupt evidence, never present.
            let state = match digest {
                Some(digest)
                    if required
                        .expected_digest
                        .as_ref()
                        .is_none_or(|expected| expected == &digest) =>
                {
                    ArtifactFact::Present { digest }
                }
                Some(_) => ArtifactFact::Corrupt {
                    reason: Some("payload digest does not match the declared expectation".into()),
                },
                None => ArtifactFact::Missing,
            };
            self.report(
                store,
                &request,
                ObservationFact::Artifact {
                    invocation_id: invocation.id.clone(),
                    required_artifact: required.kind.clone(),
                    state,
                },
                now_unix_seconds,
            )?;
        }
        self.report(
            store,
            &request,
            ObservationFact::ProcessState {
                invocation_id: invocation.id.clone(),
                process_identity: invocation
                    .process_identity
                    .as_ref()
                    .map(|i| format!("{}@{}", i.pid, i.started_at))
                    .unwrap_or_default(),
                state: ProcessStateFact::Exited,
            },
            now_unix_seconds,
        )?;
        Ok(())
    }

    /// Executions in `stopping` (and `uncertain` wanting stop) need the
    /// durable-intent → signal → inspect loop. A `requested` stop is never
    /// proof; only inspecting every recorded identity settles the state.
    pub fn drive_stops(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {
        let executions = store.transact(|tx| tx.nonterminal_executions())?;
        for execution in executions {
            let wants_stop = match execution.state {
                ExecutionState::Stopping => true,
                ExecutionState::Uncertain => matches!(
                    execution.desired_action,
                    Some(DesiredAction::Cancel)
                        | Some(DesiredAction::Fail)
                        | Some(DesiredAction::Pause)
                ),
                _ => false,
            };
            if !wants_stop {
                continue;
            }
            let invocations = store.transact(|tx| tx.invocations_for_execution(&execution.id))?;
            let mut unresolved: Vec<Invocation> = invocations
                .iter()
                .filter(|i| i.state.is_unresolved_writer())
                .cloned()
                .collect();
            // A resolved record is not stop proof on its own: an
            // unverified `Exited` report (e.g. a worker's own done claim)
            // resolves the Invocation while the process may still run,
            // and settling on that record alone would release the slot
            // and the reservation under a live writer. Re-enter it in the
            // signal/inspect loop unless every recorded process — the
            // stop intent's snapshot when one exists — verifies stopped.
            for invocation in &invocations {
                if invocation.state.is_unresolved_writer() {
                    continue;
                }
                let identities: Vec<ProcessIdentity> =
                    match store.transact(|tx| tx.stop_intent(&invocation.id))? {
                        Some(intent) => intent.identities,
                        None => invocation.process_identity.iter().cloned().collect(),
                    };
                let unproven = identities
                    .iter()
                    .any(|id| !matches!(self.port.inspect(id), ProcessEvidence::Stopped { .. }));
                if unproven {
                    unresolved.push(invocation.clone());
                }
            }
            if unresolved.is_empty() {
                // Every recorded writer is resolved and every recorded
                // process is verified stopped (or none ever ran):
                // reporting the exit fact lets `work` settle the
                // stopping/uncertain Execution — without this it would
                // wait forever for a process-exit observation that can
                // never arrive.
                if let Some(latest) = invocations.last()
                    && let Some(request) = store.transact(|tx| tx.request(&latest.request_id))?
                {
                    self.report_tagged(
                        store,
                        &request,
                        "stop-settle",
                        ObservationFact::ProcessState {
                            invocation_id: latest.id.clone(),
                            process_identity: latest
                                .process_identity
                                .as_ref()
                                .map(|i| format!("{}@{}", i.pid, i.started_at))
                                .unwrap_or_default(),
                            state: ProcessStateFact::Exited,
                        },
                        now_unix_seconds,
                    )?;
                }
                continue;
            }
            for invocation in unresolved {
                let Some(identity) = invocation.process_identity.clone() else {
                    // Unresolved but unidentifiable: nothing can be
                    // signalled or proven — report it; `work` surfaces
                    // uncertainty rather than declaring a stop it cannot
                    // verify.
                    if let Some(request) =
                        store.transact(|tx| tx.request(&invocation.request_id))?
                    {
                        self.report(
                            store,
                            &request,
                            ObservationFact::ProcessState {
                                invocation_id: invocation.id.clone(),
                                process_identity: String::new(),
                                state: ProcessStateFact::Unknown,
                            },
                            now_unix_seconds,
                        )?;
                    }
                    continue;
                };
                let intent = store.transact(|tx| tx.stop_intent(&invocation.id))?;
                let identities = match intent {
                    Some(recorded) => recorded.identities,
                    None => match self.request_stop(store, &invocation, &identity)? {
                        StopReceipt::Requested | StopReceipt::AlreadyStopped => store
                            .transact(|tx| tx.stop_intent(&invocation.id))?
                            .map(|i| i.identities)
                            .unwrap_or_default(),
                        StopReceipt::Unknown { .. } => continue,
                    },
                };
                // Stop proof = inspecting every recorded identity.
                let mut all_stopped = true;
                let mut any_unknown = false;
                for id in &identities {
                    match self.port.inspect(id) {
                        ProcessEvidence::Stopped { .. } => {}
                        ProcessEvidence::Alive { .. } => {
                            all_stopped = false;
                            // Still writing: re-signal under re-verification.
                            self.port.signal_verified(id, libc::SIGKILL);
                        }
                        ProcessEvidence::Unknown { .. } => {
                            all_stopped = false;
                            any_unknown = true;
                        }
                    }
                }
                if all_stopped {
                    // The leader is our child: reap it so a killed writer
                    // does not linger as a zombie of this supervisor.
                    self.port.reap(identity.pid);
                    let request = store
                        .transact(|tx| tx.request(&invocation.request_id))?
                        .ok_or(ExecError::Store(StoreError::Internal {
                            detail: "invocation without request".into(),
                        }))?;
                    self.report(
                        store,
                        &request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: format!("{}@{}", identity.pid, identity.started_at),
                            state: ProcessStateFact::Exited,
                        },
                        now_unix_seconds,
                    )?;
                } else if any_unknown && execution.state == ExecutionState::Stopping {
                    let request = store
                        .transact(|tx| tx.request(&invocation.request_id))?
                        .ok_or(ExecError::Store(StoreError::Internal {
                            detail: "invocation without request".into(),
                        }))?;
                    self.report(
                        store,
                        &request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: format!("{}@{}", identity.pid, identity.started_at),
                            state: ProcessStateFact::Unknown,
                        },
                        now_unix_seconds,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Persist the stop intent — with the descendant snapshot — before any
    /// signal. If persistence fails, no signal is sent.
    pub fn request_stop(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        leader: &ProcessIdentity,
    ) -> Result<StopReceipt, ExecError> {
        self.stop_unless_persisted(invocation, leader, |intent| {
            persist_intent_row(store, intent).map(|_| ())
        })
    }

    /// The durable-intent half of `request_stop`: the same descendant
    /// snapshot, leader checks, and store commit — without the signal
    /// pass. A crash-window test holds the boundary open here; the
    /// production stop path always continues into the signals in the
    /// same call.
    pub fn record_stop_intent(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        leader: &ProcessIdentity,
    ) -> Result<StopRecord, ExecError> {
        let mut recorded = None;
        let persisted = self.persist_stop_snapshot(invocation, leader, |intent| {
            recorded = Some(persist_intent_row(store, intent)?);
            Ok(())
        })?;
        Ok(match persisted {
            Ok(_) => StopRecord::Recorded(
                recorded.expect("the intent is recorded before the boundary closes"),
            ),
            Err(reason) => StopRecord::Unknown { reason },
        })
    }

    /// The stop path, split at its persistence boundary: snapshot the
    /// writer's descendants, hand the intent to `persist`, and signal only
    /// if it was durably recorded. Exposed so tests can prove a failed
    /// persist sends no signal.
    pub fn stop_unless_persisted(
        &self,
        invocation: &Invocation,
        leader: &ProcessIdentity,
        persist: impl FnOnce(StopIntent) -> Result<(), StoreError>,
    ) -> Result<StopReceipt, ExecError> {
        let identities = match self.persist_stop_snapshot(invocation, leader, persist)? {
            Ok(identities) => identities,
            Err(reason) => return Ok(StopReceipt::Unknown { reason }),
        };
        // A stale leader cannot authorize a signal to a reused group.
        if let Some(pgid) = leader.pgid
            && matches!(self.port.inspect(leader), ProcessEvidence::Alive { identity } if identity == *leader)
        {
            self.port.signal_group(pgid, libc::SIGKILL);
        }
        let mut all_gone = true;
        for id in &identities {
            match self.port.signal_verified(id, libc::SIGKILL) {
                SignalOutcome::AlreadyGone => {}
                SignalOutcome::Sent | SignalOutcome::SkippedStale => all_gone = false,
                SignalOutcome::Failed { reason } => return Ok(StopReceipt::Unknown { reason }),
            }
        }
        Ok(if all_gone {
            StopReceipt::AlreadyStopped
        } else {
            StopReceipt::Requested
        })
    }

    /// Snapshot the writer's descendants and hand the stop intent to
    /// `persist` under the per-request lock — the half of the stop path
    /// that must complete before any signal. `Err` carries why nothing
    /// was persisted; it means no signal may be sent.
    fn persist_stop_snapshot(
        &self,
        invocation: &Invocation,
        leader: &ProcessIdentity,
        persist: impl FnOnce(StopIntent) -> Result<(), StoreError>,
    ) -> Result<Result<Vec<ProcessIdentity>, String>, ExecError> {
        // Same key as the start path: start and stop for one request are
        // serialized through a single per-request lock.
        let lock = self.lock_for(&invocation.request_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut identities = match self.port.snapshot_descendants(leader.pid) {
            Ok(identities) => identities,
            Err(reason) => return Ok(Err(reason)),
        };
        if identities
            .iter()
            .any(|identity| identity.pid == leader.pid && identity.started_at != leader.started_at)
        {
            return Ok(Err(
                "leader pid was reused before the descendant snapshot".into()
            ));
        }
        if !identities.iter().any(|i| i.pid == leader.pid) {
            identities.insert(0, leader.clone());
        }
        match persist(StopIntent {
            invocation_id: invocation.id.clone(),
            identities: identities.clone(),
            recorded_seq: 0,
        }) {
            Ok(()) => Ok(Ok(identities)),
            // Durably stopping is already recorded on the Execution; with
            // no durable intent the supervisor must not signal.
            Err(e) => Ok(Err(format!(
                "stop intent not persisted, no signal sent: {e}"
            ))),
        }
    }

    /// One bounded recovery attempt per waiting Execution per pass. Each
    /// attempt is durable; exhaustion is decided by `work`'s sweep against
    /// the stored bound, not by this loop.
    pub fn run_recovery(
        &self,
        store: &impl Store,
        _now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let executions = store.transact(|tx| tx.nonterminal_executions())?;
        for execution in executions {
            if execution.state != ExecutionState::Waiting
                || execution.waiting_reason != Some(WaitingReason::ArtifactRecovery)
            {
                continue;
            }
            if execution.recovery_attempts_used >= execution.budget.artifact_recovery_attempts {
                continue;
            }
            // Recover from workspace + checkpoints + worker-runtime store.
            let workspace = self.workspace_of(&execution.work_id);
            let mut recovered: Vec<(RequiredArtifact, PathBuf)> = Vec::new();
            for invocation in store.transact(|tx| tx.invocations_for_execution(&execution.id))? {
                for required in &invocation.required_artifacts {
                    if let Some(dir) = find_artifact(&workspace, &required.path) {
                        recovered.push((required.clone(), dir));
                    }
                }
            }
            // Record the attempt durably, then report any found artifacts.
            let mut next = execution.clone();
            next.recovery_attempts_used += 1;
            store.transact(|tx| {
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: execution.state.clone(),
                })?;
                Ok(())
            })?;
            for invocation in store.transact(|tx| tx.invocations_for_execution(&execution.id))? {
                let request = match store.transact(|tx| tx.request(&invocation.request_id))? {
                    Some(r) => r,
                    None => continue,
                };
                for (required, dir) in &recovered {
                    if invocation.required_artifacts.contains(required) {
                        // Promote into the workspace root so downstream
                        // evidence paths are uniform. A hit already in
                        // the root needs no copy (copying a file onto
                        // itself truncates it), and only a verified
                        // artifact may be reported present.
                        if dir != &workspace
                            && std::fs::copy(
                                dir.join(&required.path),
                                workspace.join(&required.path),
                            )
                            .is_err()
                        {
                            continue;
                        }
                        let digest = match artifact_digest(&workspace, &required.path) {
                            Some(digest)
                                if required
                                    .expected_digest
                                    .as_ref()
                                    .is_none_or(|expected| expected == &digest) =>
                            {
                                digest
                            }
                            _ => continue,
                        };
                        self.report(
                            store,
                            &request,
                            ObservationFact::Artifact {
                                invocation_id: invocation.id.clone(),
                                required_artifact: required.kind.clone(),
                                state: ArtifactFact::Present { digest },
                            },
                            _now_unix_seconds,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Emit an Observation to `work`. A deterministic delivery id derived
    /// from the request and the fact makes a repeated identical report a
    /// store-level duplicate instead of unbounded growth.
    fn report(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        fact: ObservationFact,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        self.report_tagged(store, request, "auto", fact, now_unix_seconds)
    }

    /// `tag` distinguishes delivery contexts that legitimately re-report
    /// the same fact (e.g. a stop-settle proof repeating an earlier
    /// process exit) from plain duplicate deliveries.
    fn report_tagged(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        tag: &str,
        fact: ObservationFact,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        request.request_id.hash(&mut h);
        tag.hash(&mut h);
        serde_json::to_string(&fact)
            .unwrap_or_default()
            .hash(&mut h);
        let mut event_id = format!("sup:{:x}", h.finish());
        if let ObservationFact::ProcessState { invocation_id, .. } = &fact {
            let (generation, repeated) = store.transact(|tx| {
                let observations = tx.observations_for_execution(&request.execution_id)?;
                let process_facts: Vec<_> = observations
                    .iter()
                    .filter(|observation| {
                        matches!(&observation.fact,
                        ObservationFact::ProcessState { invocation_id: recorded, .. }
                            if recorded == invocation_id)
                    })
                    .collect();
                let repeated = process_facts.last().is_some_and(|last| {
                    last.delivery.as_ref().is_some_and(|d| {
                        d.event_id == event_id || d.event_id.starts_with(&format!("{event_id}:"))
                    })
                });
                Ok((process_facts.len(), repeated))
            })?;
            if repeated {
                return Ok(());
            }
            event_id = format!("{event_id}:{generation}");
        }
        let obs = Observation {
            delivery: Some(DeliveryId {
                scope: DeliveryScope::Local,
                source: "supervisor".into(),
                event_id,
            }),
            subject_work_id: Some(request.work_id.clone()),
            subject_execution_id: Some(request.execution_id.clone()),
            fact,
            observed_unix_seconds: now_unix_seconds,
        };
        workloop_core::work::handle_observation(store, &self.config, &obs, now_unix_seconds)?;
        Ok(())
    }

    fn report_evidence(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        invocation: &Invocation,
        ev: ProcessEvidence,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        match ev {
            ProcessEvidence::Stopped { .. } => {
                if let Some(id) = &invocation.process_identity {
                    self.port.reap(id.pid);
                }
                self.finish_invocation(store, invocation, now_unix_seconds)
            }
            ProcessEvidence::Alive { .. } => {
                // Same re-assertion as the poll path: a reconciled
                // request whose Running report was lost must still move
                // the Execution Ready→Running while the writer lives.
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: invocation
                            .process_identity
                            .as_ref()
                            .map(|i| format!("{}@{}", i.pid, i.started_at))
                            .unwrap_or_default(),
                        state: ProcessStateFact::Running,
                    },
                    now_unix_seconds,
                )
            }
            ProcessEvidence::Unknown { .. } => {
                let id = invocation
                    .process_identity
                    .clone()
                    .unwrap_or(ProcessIdentity {
                        pid: -1,
                        started_at: String::new(),
                        pgid: None,
                    });
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: format!("{}@{}", id.pid, id.started_at),
                        state: ProcessStateFact::Unknown,
                    },
                    now_unix_seconds,
                )
            }
        }
    }
}

/// The durable write `request_stop` and `record_stop_intent` share: the
/// intent row `drive_stops` resumes from after a restart. Returns the
/// intent with its recorded sequence filled in.
fn persist_intent_row(
    store: &impl Store,
    mut intent: StopIntent,
) -> Result<StopIntent, StoreError> {
    store.transact(|tx| {
        intent.recorded_seq = tx.alloc_seq()?;
        tx.apply(&Effect::InsertStopIntent {
            intent: intent.clone(),
        })?;
        Ok(())
    })?;
    Ok(intent)
}

fn required_artifacts(request: &InvocationRequest) -> Vec<RequiredArtifact> {
    request
        .payload
        .get("required_artifacts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    if let Some(name) = v.as_str() {
                        return Some(RequiredArtifact {
                            kind: name.to_string(),
                            path: name.to_string(),
                            expected_digest: None,
                        });
                    }
                    let kind = v.get("kind").and_then(|k| k.as_str())?;
                    let path = v.get("path").and_then(|p| p.as_str()).unwrap_or(kind);
                    Some(RequiredArtifact {
                        kind: kind.to_string(),
                        path: path.to_string(),
                        expected_digest: v
                            .get("expected_digest")
                            .and_then(|d| d.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .filter(|v: &Vec<RequiredArtifact>| !v.is_empty())
        .unwrap_or_else(|| {
            vec![RequiredArtifact {
                kind: "artifact.json".to_string(),
                path: "artifact.json".to_string(),
                expected_digest: None,
            }]
        })
}

/// Whether `name` exists in the workspace root, checkpoints, or the
/// worker-runtime artifact store. Returns the directory holding it.
fn find_artifact(workspace: &Path, name: &str) -> Option<PathBuf> {
    for dir in [
        workspace.to_path_buf(),
        workspace.join(".checkpoints"),
        workspace.join(".artifact-store"),
    ] {
        let candidate = dir.join(name);
        if candidate.is_file()
            && std::fs::metadata(&candidate)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            return Some(dir);
        }
    }
    None
}

/// The payload digest of an artifact in the workspace root; `None` when
/// it is absent or unreadable. Presence is never reported without a
/// digest — the fact's `Present` state carries it.
fn artifact_digest(workspace: &Path, name: &str) -> Option<String> {
    let candidate = workspace.join(name);
    if !candidate.is_file() {
        return None;
    }
    let bytes = std::fs::read(&candidate).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(sha256_hex("artifact-v1", &bytes))
}

/// Test hook: write a line into a workspace artifact (used by harness
/// tests that emulate the worker-runtime store).
pub fn write_artifact(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_ps_status_cannot_prove_absence() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code, stderr: &[u8]| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: vec![],
            stderr: stderr.to_vec(),
        };
        assert_eq!(checked_ps_output(output(1, b""), true).unwrap(), "");
        assert!(checked_ps_output(output(1, b"process table unavailable"), true).is_err());
        assert!(checked_ps_output(output(2, b""), true).is_err());
        assert!(checked_ps_output(output(1, b""), false).is_err());
    }

    struct SnapshotFailure;

    impl ProcessControlPort for SnapshotFailure {
        fn spawn(&self, _: &[String], _: &Path) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn expected")
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Err("process table unavailable".into())
        }
        fn inspect(&self, _: &ProcessIdentity) -> ProcessEvidence {
            panic!("no inspect expected")
        }
        fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {
            panic!("incomplete snapshot must not authorize signaling")
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            panic!("incomplete snapshot must not authorize signaling")
        }
        fn reap(&self, _: i32) {}
    }

    #[test]
    fn failed_snapshot_neither_persists_partial_stop_intent_nor_signals() {
        let sup = Supervisor::new(
            SnapshotFailure,
            PathBuf::from("/unused"),
            vec![],
            workloop_core::work::WorkloopConfig {
                host: workloop_core::work::HostTemplate {
                    adapter: workloop_core::identity::AdapterId::trusted("fixture"),
                    repository: workloop_core::identity::RepositoryRef::new(
                        workloop_core::identity::HostInstanceId::trusted("host-local"),
                        workloop_core::identity::RepositoryId::trusted("local-repo"),
                    ),
                    contract_version: 0,
                    actors: vec![],
                },
                declared_capabilities: Default::default(),
                lanes: Default::default(),
                required_checks: vec![],
                allowed_effects: AllowedEffects {
                    workspace_write: true,
                    host_write: false,
                    network: false,
                },
                budget: ExecutionBudget::default(),
                policy: WorkPolicy::default(),
            },
        );
        let identity = ProcessIdentity {
            pid: 42,
            started_at: "recorded start".into(),
            pgid: Some(42),
        };
        let invocation = Invocation {
            id: InvocationId("inv-1".into()),
            request_id: "req-1".into(),
            execution_id: ExecutionId("exec-1".into()),
            role: InvocationRole::Coding,
            state: InvocationState::Running,
            process_identity: Some(identity.clone()),
            required_artifacts: vec![],
            evidence_refs: vec![],
        };
        let receipt = sup
            .stop_unless_persisted(&invocation, &identity, |_| {
                panic!("a partial snapshot must not be persisted as complete")
            })
            .unwrap();
        assert_eq!(
            receipt,
            StopReceipt::Unknown {
                reason: "process table unavailable".into()
            }
        );
    }

    #[test]
    fn lstart_lookup_keeps_failure_distinct_from_absence() {
        // A `ps` spawn/io failure is an incomplete lookup, never absence.
        let failed = classify_lstart_lookup(Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "ps unavailable",
        )));
        assert!(
            matches!(&failed, LstartLookup::Failed(r) if r.contains("ps unavailable")),
            "spawn failure classifies as Failed: {failed:?}"
        );
        // `ps` running and listing nothing is definitive absence.
        assert_eq!(
            classify_lstart_lookup(Ok(String::new())),
            LstartLookup::Absent
        );
        assert_eq!(
            classify_lstart_lookup(Ok("\n".into())),
            LstartLookup::Absent
        );
        // A listed pid yields the whitespace-normalized verifier.
        match classify_lstart_lookup(Ok("Sat Oct  4 06:14:00 2026\n".into())) {
            LstartLookup::Present(s) => assert_eq!(s, "Sat Oct 4 06:14:00 2026"),
            other => panic!("expected Present, got {other:?}"),
        }
    }
}
