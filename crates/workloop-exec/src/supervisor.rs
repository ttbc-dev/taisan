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
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use workloop_core::domain::*;
use workloop_core::observation::{
    ArtifactFact, DeliveryId, DeliveryScope, Observation, ObservationFact, ProcessStateFact,
};
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, ProcessMatch, SignalOutcome, SpawnError,
    StopReceipt, WorkerEnv,
};
use workloop_core::store::{Effect, Store, StoreError, TxView};
use workloop_core::worker::{
    IsolationProfile, WorkerRequest, WorkerRequestExport, WorkerRequestExportState,
    default_isolation_profile, export_request_id,
};
use workloop_core::worker_v2::{
    DispatchRefusal, ReportClassification, WorkerReportV2, WorkerRequestV2,
};

use crate::adapter;
use crate::bodies::{BodyError, BodyStore};
use crate::boundary::{BoundaryGrant, RuntimeBoundary};
use crate::containment::{self, Gate, MountPlan};
use crate::isolation;
use crate::request_v2::V2Dispatch;

/// How long an unidentified launch must stay unexplained — no found
/// writer and no unexplained child — before its absence is reported.
/// The window only has to outlast the fork-exec visibility gap between
/// `spawn` returning `Unknown` and the child appearing in the process
/// table; it is deliberately generous because a false absence is the
/// one outcome that could admit a second writer.
const UNIDENTIFIED_LAUNCH_ABSENT_SECONDS: i64 = 30;

/// The POSIX `ProcessControlPort`. Identities are `pid + lstart` strings
/// captured from `ps`; the recorded string is compared verbatim before any
/// signal so a reused pid is never signaled.
pub struct PosixProcessControl;

pub(crate) fn ps_output(args: &[&str]) -> std::io::Result<String> {
    // The row/lstart parsers below are written for the C locale's ctime
    // format; a localized `ps` renders different field text (e.g. a CJK
    // weekday), which must not read back as an incomplete table row.
    let out = Command::new("ps").args(args).env("LC_ALL", "C").output()?;
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
pub(crate) fn normalize_lstart(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One `ps` table row: `N` integer prefix fields followed by the
/// locale-dependent lstart text. The tail is never field-counted — a
/// localized `ps` renders different field text (e.g. ja_JP prints a CJK
/// weekday and an `M/D` date), and any non-empty tail is still the
/// process start the row carries. Blank lines are skipped (`Ok(None)`);
/// a missing prefix field or an empty lstart tail is an incomplete row
/// and a non-numeric prefix is an invalid row — either aborts the table
/// read so a partial snapshot can never authorize a signal.
fn parse_table_row<const N: usize>(line: &str) -> Result<Option<([i32; N], String)>, String> {
    let mut it = line.split_whitespace();
    let mut tokens = Vec::with_capacity(N);
    for _ in 0..N {
        match it.next() {
            Some(tok) => tokens.push(tok),
            None => {
                return if line.trim().is_empty() {
                    Ok(None)
                } else {
                    Err(format!("incomplete process-table row: {line}"))
                };
            }
        }
    }
    let lstart = it.collect::<Vec<_>>().join(" ");
    if lstart.is_empty() {
        return Err(format!("incomplete process-table row: {line}"));
    }
    let mut fields = [0i32; N];
    for (slot, tok) in fields.iter_mut().zip(tokens) {
        *slot = tok
            .parse()
            .map_err(|_| format!("invalid process-table row: {line}"))?;
    }
    Ok(Some((fields, normalize_lstart(&lstart))))
}

/// One `ps`-based lstart lookup, keeping "the process is gone" separate
/// from "the lookup could not be completed". Only `Absent` (and a
/// verified start-time mismatch) may prove a stop; `Failed` proves
/// nothing and must surface as `Unknown`/`Failed` to callers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LstartLookup {
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

pub(crate) fn lstart_of(pid: i32) -> LstartLookup {
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

/// The working directory `pid` runs in, read through `lsof`. `None` on
/// any failure — a missing `lsof`, a denied read, or a process with no
/// reported cwd — never a guessed path. Only ever used to *attribute* a
/// candidate; `None` makes a child less attributable, never absent-proof.
fn cwd_of(pid: i32) -> Option<PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Whether a child's reported cwd is the launch directory or inside it.
/// Both sides are canonicalized so symlinked temp roots and `.`/`..`
/// spellings compare equal; an unresolvable cwd is never a match.
pub(crate) fn cwd_within(cwd: Option<&Path>, dir: &Path) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    std::fs::canonicalize(cwd).is_ok_and(|canon| canon.starts_with(dir))
}

/// Every process identity the store already accounts for — leaders,
/// descendant snapshots, and stop-intent proofs across all nonterminal
/// executions — so a recorded writer is never re-bound as an
/// unidentified launch's orphan. Called inside the caller's
/// transaction: the bind commit re-verifies against the same snapshot
/// it writes, so a concurrent resolution cannot double-bind one
/// process to two invocations.
fn recorded_process_identities(tx: &dyn TxView) -> Result<HashSet<(i32, String)>, StoreError> {
    let mut set = HashSet::new();
    for exec in tx.nonterminal_executions()? {
        for inv in tx.invocations_for_execution(&exec.id)? {
            if let Some(id) = &inv.process_identity {
                set.insert((id.pid, id.started_at.clone()));
            }
            for d in &inv.descendants {
                set.insert((d.pid, d.started_at.clone()));
            }
            if let Some(intent) = tx.stop_intent(&inv.id)? {
                for id in &intent.identities {
                    set.insert((id.pid, id.started_at.clone()));
                }
            }
        }
    }
    Ok(set)
}

/// The exit status of `pid` without consuming it: `waitid` with
/// `WNOWAIT` reports a terminated child's status while leaving the
/// zombie for `reap`, so the durable exit fact can be committed first
/// and a failed write retried against the same proven status. Only a
/// normal exit counts — a signaled, still-running, already-reaped, or
/// non-child pid proves nothing and reports `None`.
fn peek_exit_status(pid: i32) -> Option<i32> {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        ) != 0
        {
            return None;
        }
        if info.si_pid() == pid && info.si_code == libc::CLD_EXITED {
            Some(info.si_status())
        } else {
            None
        }
    }
}

/// What a timed-out probe may signal once the bound has elapsed. The
/// waiter thread can already have reaped the leader between the bound
/// expiring and the kill, freeing the pid for reuse, so the recorded
/// `pid + lstart` is re-verified first — the same owner check
/// `proof_identities` applies to a recorded group: pgid == the leader's
/// pid (`process_group(0)`), a live process at that pid under a
/// different start is a recycled owner and its group is foreign, while
/// an absent owner leaves only this probe's own stragglers holding the
/// group.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeSignalPlan {
    /// The leader re-verified: it and the group this probe created are
    /// still ours to signal.
    LeaderAndGroup,
    /// The pid is unallocated: a surviving group can only be held by
    /// this probe's own stragglers — a group id is only minted by a
    /// live process of that pid — so the group is still ours to signal.
    GroupOnly,
    /// A live process at the pid carries a different start (recycled,
    /// foreign) or an occupied pid could not be verified: nothing is
    /// signalled. A possibly-leaked straggler beats a signal to an
    /// unrelated process.
    Nothing,
}

pub(crate) fn probe_signal_plan(
    pid: i32,
    recorded_lstart: &str,
    lookup: &LstartLookup,
) -> ProbeSignalPlan {
    match lookup {
        LstartLookup::Present(now) if now == recorded_lstart => ProbeSignalPlan::LeaderAndGroup,
        LstartLookup::Absent => ProbeSignalPlan::GroupOnly,
        LstartLookup::Failed(_) => {
            // The process-table read failed, but a pid probe needs no
            // table: `kill(pid, 0)` reporting ESRCH means the pid is
            // unallocated and no foreign process can be heading a group
            // under it — the group is still ours to signal. An
            // occupied but unverifiable pid could be a recycled foreign
            // owner, so it is left alone.
            let rc = unsafe { libc::kill(pid, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                ProbeSignalPlan::GroupOnly
            } else {
                ProbeSignalPlan::Nothing
            }
        }
        _ => ProbeSignalPlan::Nothing,
    }
}

/// Signal whatever of a bounded probe's leader and process group is
/// still this probe's to stop. `recorded_lstart` is the leader identity
/// captured while the pid was provably ours. Shared by the timeout and
/// the wait-error paths so a wedged probe cannot outlive its bound on
/// either one. Returns the plan applied so the caller can report an
/// unsignalled bound as unknown fate rather than claiming `Bounded`.
pub(crate) fn signal_probe_bound(pid: i32, recorded_lstart: &str) -> ProbeSignalPlan {
    let plan = probe_signal_plan(pid, recorded_lstart, &lstart_of(pid));
    match plan {
        ProbeSignalPlan::LeaderAndGroup => unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        },
        ProbeSignalPlan::GroupOnly => unsafe {
            libc::kill(-pid, libc::SIGKILL);
        },
        ProbeSignalPlan::Nothing => {}
    }
    plan
}

impl PosixProcessControl {
    /// Capture a reuse-resistant identity for an existing process —
    /// the caller's own pid when claiming the run lease. Returns an
    /// error when the identity cannot be verified; a lease is never
    /// claimed under an unverifiable owner record.
    pub fn identity_of(pid: i32) -> Result<ProcessIdentity, String> {
        match lstart_of(pid) {
            LstartLookup::Present(started_at) => {
                let pgid = unsafe { libc::getpgid(pid) };
                Ok(ProcessIdentity {
                    pid,
                    started_at,
                    pgid: if pgid > 0 { Some(pgid) } else { None },
                })
            }
            LstartLookup::Absent => Err(format!("pid {pid} is not in the process table")),
            LstartLookup::Failed(reason) => Err(reason),
        }
    }
}

/// Apply the allowlisted worker environment: `env_clear` so nothing
/// the supervisor inherited survives, then exactly the pairs the
/// profile constructed.
fn apply_worker_env(cmd: &mut Command, env: &WorkerEnv) {
    cmd.env_clear();
    cmd.envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
}

impl ProcessControlPort for PosixProcessControl {
    fn spawn(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError> {
        let Some(program) = argv.first() else {
            return Err(SpawnError::Refused {
                reason: "empty argv".into(),
            });
        };
        // Own process group so a group signal reaches the whole tree the
        // leader did not let escape.
        let mut cmd = Command::new(program);
        cmd.args(&argv[1..]).current_dir(cwd).process_group(0);
        apply_worker_env(&mut cmd, env);
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
            // Numeric prefix fields, then the locale-dependent lstart
            // string — its token count varies (e.g. ja_JP pads days
            // <10 with a space), so it is never field-counted.
            let Some(([pid, ppid, pgid], lstart)) = parse_table_row::<3>(line)? else {
                continue;
            };
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

    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String> {
        // pid, pgid, lstart(5 fields). Orphans keep the leader's group
        // after reparenting, which is exactly the coverage the ppid-tree
        // snapshot cannot give once the leader is gone.
        let table = ps_output(&["-Ao", "pid=,pgid=,lstart="])
            .map_err(|e| format!("group enumeration failed: {e}"))?;
        let mut out = Vec::new();
        for line in table.lines() {
            // Same variable-width lstart handling as
            // `snapshot_descendants`: pid/pgid are numeric prefixes,
            // everything after is the start time.
            let Some(([pid, g], lstart)) = parse_table_row::<2>(line)? else {
                continue;
            };
            if g == pgid {
                out.push(ProcessIdentity {
                    pid,
                    started_at: lstart,
                    pgid: Some(g),
                });
            }
        }
        Ok(out)
    }

    fn matching_processes(&self, command: &str) -> Result<Vec<ProcessMatch>, String> {
        // pid, then the argv tail. `-ww` keeps the command column
        // untruncated — a truncated row could hide the real writer
        // behind a false absence. The scan is system-wide, not limited
        // to this process's children: a supervisor restart reparents an
        // orphaned writer, and only the launch signature still finds it.
        let expected = normalize_lstart(command);
        let table = ps_output(&["-ewwo", "pid=,command="])
            .map_err(|e| format!("process scan failed: {e}"))?;
        let mut out = Vec::new();
        for line in table.lines() {
            let Some(([pid], actual)) = parse_table_row::<1>(line)? else {
                continue;
            };
            if normalize_lstart(&actual) != expected {
                continue;
            }
            // Re-verify after the snapshot: a matched pid that already
            // exited drops out instead of polluting the match set, and
            // a start-time lookup failure fails the whole scan — a
            // partial table can never authorize absence.
            let started_at = match lstart_of(pid) {
                LstartLookup::Present(s) => s,
                LstartLookup::Absent => continue,
                LstartLookup::Failed(reason) => return Err(reason),
            };
            // A zombie is dead: it cannot write. It is not a live
            // match — and the scan must not reap it either: reaping
            // here could consume another Invocation's exit status
            // before its own inspect reads it. Enumeration stays
            // read-only; the owning path reaps.
            if stat_of(pid).is_some_and(|stat| stat.contains('Z')) {
                continue;
            }
            let pgid = unsafe { libc::getpgid(pid) };
            out.push(ProcessMatch {
                identity: ProcessIdentity {
                    pid,
                    started_at,
                    pgid: if pgid > 0 { Some(pgid) } else { None },
                },
                cwd: cwd_of(pid),
                command: Some(actual),
            });
        }
        Ok(out)
    }

    fn processes_in_directory(&self, dir: &Path) -> Result<Vec<ProcessMatch>, String> {
        // Enumerate this user's processes before the `lsof` pass: the
        // absence proof is only as strong as the table's completeness
        // — a live process whose cwd cannot be mapped could be inside
        // `dir`, so the scan is `Err` whenever a same-user live
        // process has no readable cwd. Reading the pid list first
        // means a process born between the two reads is never misread
        // as a gap; one that exits between them drops out on the
        // liveness re-verify below. Another user's unreadable process
        // cannot be this supervisor's spawn and stays the documented
        // residual.
        let euid = unsafe { libc::geteuid() };
        let table =
            ps_output(&["-ewwo", "pid=,uid="]).map_err(|e| format!("process scan failed: {e}"))?;
        let mut same_user_pids = Vec::new();
        for line in table.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(pid), Some(uid)) = (it.next(), it.next()) else {
                return Err(format!("incomplete process-table row: {line}"));
            };
            let pid: i32 = pid
                .parse()
                .map_err(|_| format!("invalid process-table row: {line}"))?;
            let uid: u32 = uid
                .parse()
                .map_err(|_| format!("invalid process-table row: {line}"))?;
            if uid == euid {
                same_user_pids.push(pid);
            }
        }
        // One `lsof -d cwd` pass maps every inspectable pid to its
        // working directory — per-pid lookups would not scale to a
        // system-wide absence proof. Field output (`p`/`n` pairs) is
        // parsed positionally: a process lsof cannot inspect simply
        // has no `n` line and cannot be placed inside `dir`.
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let out = Command::new("lsof")
            .args(["-d", "cwd", "-F", "pn"])
            .output()
            .map_err(|e| format!("directory process scan failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "lsof failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let mut mapped = HashSet::new();
        let mut occupants = Vec::new();
        let mut pid: Option<i32> = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(rest) = line.strip_prefix('p') {
                pid = rest.trim().parse().ok();
            } else if let Some(path) = line.strip_prefix('n')
                && let Some(pid) = pid
            {
                mapped.insert(pid);
                occupants.push((pid, PathBuf::from(path)));
            }
        }
        // Completeness gate before any occupant answer: a live
        // same-user process lsof could not map to a cwd makes the
        // whole enumeration partial — its cwd might be inside `dir`,
        // so the table can never authorize absence.
        for pid in same_user_pids {
            if mapped.contains(&pid) {
                continue;
            }
            match lstart_of(pid) {
                // Exited between the pid-list and lsof reads — not a
                // gap, just a process that was already gone.
                LstartLookup::Absent => continue,
                LstartLookup::Failed(reason) => return Err(reason),
                LstartLookup::Present(_) => {}
            }
            // Gone between the liveness check and the stat read, or a
            // zombie — either way it cannot write inside `dir`.
            let Some(stat) = stat_of(pid) else {
                continue;
            };
            if stat.contains('Z') {
                continue;
            }
            return Err(format!(
                "directory scan incomplete: live process {pid} has no readable cwd"
            ));
        }
        let mut out = Vec::new();
        for (pid, cwd) in occupants {
            // `cwd_within` canonicalizes the reported path and treats
            // an unresolvable cwd as "not inside" — but a reported
            // path spelled inside `dir` cannot be proven outside it
            // either (a live writer whose cwd was deleted). `mapped`
            // already holds the pid, so skipping it here would slip
            // it past the completeness gate: fail the scan instead,
            // matching the same fail-closed contract.
            if std::fs::canonicalize(&cwd).is_err() && cwd.starts_with(&dir) {
                return Err(format!(
                    "directory scan incomplete: process {pid} cwd {} is not resolvable",
                    cwd.display()
                ));
            }
            if !cwd_within(Some(&cwd), &dir) {
                continue;
            }
            // Same re-verification as the signature scan: a pid that
            // exited between the lsof pass and now drops out, and a
            // start-time lookup failure fails the whole scan — a
            // partial table can never authorize absence.
            let started_at = match lstart_of(pid) {
                LstartLookup::Present(s) => s,
                LstartLookup::Absent => continue,
                LstartLookup::Failed(reason) => return Err(reason),
            };
            if stat_of(pid).is_some_and(|stat| stat.contains('Z')) {
                continue;
            }
            let pgid = unsafe { libc::getpgid(pid) };
            out.push(ProcessMatch {
                identity: ProcessIdentity {
                    pid,
                    started_at,
                    pgid: if pgid > 0 { Some(pgid) } else { None },
                },
                cwd: Some(cwd),
                command: None,
            });
        }
        Ok(out)
    }

    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        let lstart = match lstart_of(identity.pid) {
            // The pid is gone from the process table. If this identity
            // is still our unreaped child the table read raced its exit;
            // `waitid(WNOWAIT)` either proves the status non-destructively
            // or reports nothing — it can never invent one.
            LstartLookup::Absent => {
                return ProcessEvidence::Stopped {
                    identity: identity.clone(),
                    exit_code: peek_exit_status(identity.pid),
                };
            }
            // The lookup itself failed: an incomplete process-table read
            // is never stop proof — the port contract requires Unknown.
            LstartLookup::Failed(reason) => return ProcessEvidence::Unknown { reason },
            LstartLookup::Present(lstart) => lstart,
        };
        if normalize_lstart(&lstart) != normalize_lstart(&identity.started_at) {
            // Reused pid: the recorded process is gone. Its status is
            // never peeked — the pid may now name a different child of
            // this supervisor, whose exit must not be attributed here.
            return ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: None,
            };
        }
        match stat_of(identity.pid) {
            None => ProcessEvidence::Unknown {
                reason: format!("pid {} present at lstart but stat unread", identity.pid),
            },
            // A zombie still carries the recorded start time, so the
            // non-destructive peek reports this exact process's exit.
            Some(stat) if stat.contains('Z') => ProcessEvidence::Stopped {
                identity: identity.clone(),
                exit_code: peek_exit_status(identity.pid),
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

    fn reap(&self, pid: i32) -> Option<i32> {
        let mut status = 0;
        // waitpid collects the exit status only when this process is
        // still our unreaped child: `pid` on success, 0 while it runs
        // (WNOHANG), -1 when it is not ours. A normal exit carries its
        // code; a signal death or an unproven reap reports none — the
        // caller records `Exited` with no status rather than inventing
        // one.
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid && libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        }
    }

    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        let Some(program) = argv.first() else {
            return Err(SpawnError::Refused {
                reason: "empty argv".into(),
            });
        };
        // Same containment as `spawn`: own process group so a bound can
        // reach the whole tree the leader did not let escape. stderr is
        // dropped — only the declared stdout payload is evidence.
        let mut cmd = Command::new(program);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_worker_env(&mut cmd, env);
        let mut child = cmd.spawn().map_err(|e| SpawnError::Refused {
            reason: format!("spawn {program}: {e}"),
        })?;
        let pid = child.id() as i32;
        // Record the leader's `pid + lstart` while the pid is provably
        // still ours — the handle is held, so it cannot have been
        // recycled. The waiter below can reap the leader before the
        // bound path signals, and only this identity separates our
        // process from a reused pid there. When the identity cannot be
        // captured no later signal is verifiable: the still-unreaped
        // child is stopped now (signalling it is safe while the handle
        // is held) and the probe's fate is reported unknown.
        let started_at = match Self::identity_of(pid) {
            Ok(identity) => identity.started_at,
            Err(reason) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SpawnError::Unknown { reason });
            }
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(child.wait_with_output());
        });
        match receiver.recv_timeout(timeout) {
            Ok(Ok(output)) => Ok(ProbeOutcome::Exited {
                stdout: output.stdout,
                success: output.status.success(),
            }),
            Ok(Err(e)) => {
                // The wait itself failed and the child's fate is
                // unknown — the bound still applies: signal whatever of
                // the leader and its group is still this probe's before
                // reporting the probe unknown, so a wedged group cannot
                // outlive the bound on a wait-channel error either.
                signal_probe_bound(pid, &started_at);
                let _ = self.reap(pid);
                Err(SpawnError::Unknown {
                    reason: format!("wait {program}: {e}"),
                })
            }
            Err(_) => {
                // The bound elapsed: re-verify the recorded identity
                // before signalling — the waiter may already have reaped
                // the leader, freeing the pid for reuse (#152). The
                // group id is the leader's pid by construction, so the
                // plan decides exactly which of the leader and its
                // group are still ours. A short drain then lets the
                // waiter reap the direct child rather than leaving a
                // zombie. When the plan signalled nothing the outcome
                // is not `Bounded`: that variant promises the group and
                // leader were stopped, so an unsignalled wedged probe
                // reports unknown fate instead.
                let plan = signal_probe_bound(pid, &started_at);
                let _ = receiver.recv_timeout(Duration::from_secs(2));
                match plan {
                    ProbeSignalPlan::Nothing => Err(SpawnError::Unknown {
                        reason: format!(
                            "probe bound elapsed and the leader identity could not \
                             be re-verified at {pid}; nothing was signalled"
                        ),
                    }),
                    _ => Ok(ProbeOutcome::Bounded),
                }
            }
        }
    }
}

/// Anything `execution` can fail doing; store errors propagate verbatim so
/// callers can tell a durable-write failure from a process-layer one.
#[derive(Debug)]
pub enum ExecError {
    Store(StoreError),
    Io(std::io::Error),
    /// An infrastructure fault outside the store/IO channels — the
    /// pass fails and every durable row stays pending for the next
    /// one rather than recording a degraded launch.
    Fault(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Fault(detail) => write!(f, "{detail}"),
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

/// The tick deadline, as every supervision item loop sees it: read the
/// clock only when a deadline is actually set, and stop before starting
/// the next item once it has been reached. `None` means unbounded — the
/// clock is never consulted.
fn deadline_reached(clock: &mut dyn FnMut() -> i64, deadline: Option<i64>) -> bool {
    deadline.is_some_and(|d| clock() >= d)
}

/// Result of proving an exited writer's whole recorded tree is stopped.
#[derive(Debug, PartialEq, Eq)]
enum TreeProof {
    /// Every recorded identity inspected stopped: the exit fact may be
    /// reported.
    Proven,
    /// Live stragglers were re-signaled; the exit fact stays withheld so
    /// capacity can never release while a recorded writer survives.
    Withheld,
    /// Inspection was inconclusive for at least one recorded identity;
    /// the caller reports `Unknown` so the Execution holds ownership.
    Uncertain,
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
    /// The approved isolation profile the WorkerRequest export names and
    /// every worker launch and `describe` probe is contained under —
    /// the environment each spawned process runs in is built from this
    /// profile, never inherited (U2.3). Namespace enforcement remains a
    /// separate slice; the export still records the profile id.
    pub isolation_profile: IsolationProfile,
    /// Durable artifact-handoff layout; `None` keeps the file-artifact
    /// path only (fixture harnesses without git workspaces).
    pub handoff: Option<crate::handoff::HandoffLayout>,
    /// The v2 dispatch profile (`docs/v0.3.md` §4): when set, every
    /// pending request is materialized as an immutable
    /// `taisan.worker.request/2` body and launched through the
    /// `run --request/--out` transport. `None` keeps the legacy v1
    /// consume path — a deployment never mixes the two for one
    /// request because the recorded Invocation decides the replay.
    pub v2: Option<V2Dispatch>,
    /// The runtime credential/egress boundary (`docs/v0.3.md`
    /// §14.3–§14.4): resolved lazily at the first contained launch or
    /// `describe` probe and shared by every launch after. `None`
    /// leaves contained launches with no credential, provider config,
    /// or proxy binds — the v1-compatible empty boundary; the
    /// composition root always installs one built from the approved
    /// installation configuration.
    pub runtime_boundary: Option<RuntimeBoundary>,
    /// The CAS body store worker report evidence is retained into.
    /// Independent of `v2`: report collection follows the *recorded*
    /// request body, so a finished v2 Invocation keeps its evidence
    /// even after the dispatch profile is removed from configuration.
    /// `None` fails report collection for v2 invocations rather than
    /// recording evidence that cannot resolve.
    pub body_store: Option<BodyStore>,
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
            isolation_profile: default_isolation_profile(),
            handoff: None,
            v2: None,
            runtime_boundary: None,
            body_store: None,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Run launches under the approved isolation profile the
    /// WorkerRequest export carries — the profile also builds the
    /// environment every spawn and `describe` probe runs under (U2.3).
    /// Callers resolve the id through `isolation_profile` so an
    /// unapproved name can never reach a launch.
    pub fn with_isolation_profile(mut self, profile: IsolationProfile) -> Self {
        self.isolation_profile = profile;
        self
    }

    /// Configure durable handoff transport (handoff repository + out
    /// root) for Workspaces carrying git objects.
    pub fn with_handoff(mut self, layout: crate::handoff::HandoffLayout) -> Self {
        self.handoff = Some(layout);
        self
    }

    /// Enable the v2 consume path: pending requests materialize as
    /// immutable `taisan.worker.request/2` bodies under this dispatch
    /// profile. Requests already consumed under v1 keep replaying
    /// their recorded Invocation — the format is fixed at consumption,
    /// never upgraded in place.
    pub fn with_v2_dispatch(mut self, dispatch: V2Dispatch) -> Self {
        // Report collection retains into the same CAS root the
        // dispatch profile writes request bodies to, unless the
        // composition root installed a body store explicitly.
        if self.body_store.is_none() {
            self.body_store = Some(dispatch.bodies.clone());
        }
        self.v2 = Some(dispatch);
        self
    }

    /// Install the CAS body store finished v2 Invocations retain their
    /// report evidence into. The composition root sets this whenever a
    /// state root exists — independent of `worker_dispatch`, because
    /// collection follows the recorded request body, not the current
    /// dispatch configuration.
    pub fn with_body_store(mut self, bodies: BodyStore) -> Self {
        self.body_store = Some(bodies);
        self
    }

    /// Install the runtime credential/egress boundary every contained
    /// launch and `describe` probe resolves through. The boundary is
    /// resolved lazily on first contained use; a resolution failure is
    /// a typed launch refusal, never an ambient fallback.
    pub fn with_runtime_boundary(mut self, boundary: RuntimeBoundary) -> Self {
        self.runtime_boundary = Some(boundary);
        self
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

    pub fn prepare_workspace(
        &self,
        store: &impl Store,
        request_id: &str,
        source: &crate::workspace::RepositorySource,
        now_unix_seconds: i64,
    ) -> Result<crate::workspace::PreparedWorkspace, crate::workspace::PreparationError> {
        crate::workspace::prepare(
            store,
            &self.workspace_root,
            self.handoff.as_ref(),
            request_id,
            source,
            now_unix_seconds,
        )
    }

    /// The scratch tree one Invocation's launch environment is built
    /// under — its writer's `HOME`/`TMPDIR`, reclaimed when the
    /// Invocation finishes or the launch is refused.
    fn launch_scratch(&self, invocation_id: &str) -> PathBuf {
        isolation::scratch_root(&self.workspace_root)
            .join(format!("inv-{}", isolation::sanitize_key(invocation_id)))
    }

    /// The disposable workspace snapshot an InvariantCheck Invocation
    /// runs against — a full copy of the producing workspace taken at
    /// consume time, so the check leg reads the produced state but can
    /// never mutate it: its writes land in the copy, reclaimed with
    /// the Invocation's lifecycle like the launch scratch. The
    /// mount-level read-only bind the worker protocol specifies is the
    /// separate namespace slice; until it lands, the snapshot is the
    /// enforcement that keeps the producing workspace — and the
    /// artifact digest recorded against it — untouched.
    fn check_workspace(&self, invocation_id: &str) -> PathBuf {
        isolation::scratch_root(&self.workspace_root)
            .join(format!("check-{}", isolation::sanitize_key(invocation_id)))
    }

    /// The containment decision for one launch argv: the profile's
    /// `sandbox_primitive` resolved through the recorded
    /// negative-probe evidence. `Ok(Err(reason))` is a typed refusal
    /// the caller carries as a launch repair — containment never
    /// degrades to the unwrapped launch; `Err(ExecError)` is an
    /// infrastructure fault that leaves the request pending. Public
    /// so the pass-level prebind probe proves the same boundary the
    /// consume path launches under.
    pub fn containment_gate(
        &self,
        worker_argv: &[String],
        now_unix_seconds: i64,
    ) -> Result<Result<Gate, String>, ExecError> {
        // The boundary identity joins the gate identity only for a
        // contained profile: an unreadable provider config or an
        // unresolvable boundary identity is itself a refusal, and an
        // uncontained profile never resolves the boundary at all.
        let boundary_digest = if self.isolation_profile.sandbox_primitive.is_some() {
            match &self.runtime_boundary {
                Some(boundary) => match boundary.identity_digest() {
                    Ok(digest) => Some(digest),
                    Err(reason) => return Ok(Err(reason)),
                },
                None => None,
            }
        } else {
            None
        };
        match containment::gate(
            &self.port,
            &self.workspace_root,
            &self.isolation_profile,
            worker_argv,
            boundary_digest.as_deref(),
            now_unix_seconds,
        ) {
            Ok(gate) => Ok(Ok(gate)),
            Err(containment::GateError::Refused(reason)) => Ok(Err(reason)),
            Err(containment::GateError::Fault(detail)) => Err(ExecError::Fault(detail)),
        }
    }

    /// Resolve the runtime boundary for one contained launch or probe.
    /// `Ok(None)` when no boundary is installed — the contained launch
    /// then binds no credential, provider config, or proxy (the
    /// v1-compatible empty boundary). `Err` is a typed refusal
    /// (`credential.*`, `provider_config.*`, `egress.*`) the caller
    /// carries as a launch repair — never an ambient fallback.
    fn boundary_grant(&self) -> Result<Option<BoundaryGrant>, String> {
        match &self.runtime_boundary {
            Some(boundary) => boundary.ensure().map(Some),
            None => Ok(None),
        }
    }

    /// Turn a worker-level argv plus a caller-filled mount plan into
    /// the contained outer argv and environment — the single place the
    /// runtime boundary becomes sandbox binds. The plan's boundary
    /// fields (`runtime_auth`, `provider_config`, `proxy_sock`) are
    /// filled from the grant here; a proxy grant also relay-wraps the
    /// inner argv and exports `HTTPS_PROXY`. `Err` is a typed boundary
    /// refusal; there is no unwrapped fallback.
    fn contained_exec(
        &self,
        primitive: &Path,
        mut plan: MountPlan,
        inner_argv: &[String],
        inner_cwd: &str,
        env: &WorkerEnv,
    ) -> Result<(Vec<String>, WorkerEnv), String> {
        let grant = self.boundary_grant()?;
        let proxy_granted = grant
            .as_ref()
            .is_some_and(|grant| grant.proxy_sock.is_some());
        if let Some(grant) = &grant {
            plan.runtime_auth = grant.credential_file.clone();
            plan.provider_config = grant.provider_config.clone();
            plan.proxy_sock = grant.proxy_sock.clone();
        }
        let (exec_argv, egress_proxy) = if proxy_granted {
            // The proxy socket is bound at `/taisan/proxy.sock`; the
            // relay is the sandbox init that serves `HTTPS_PROXY` on
            // loopback and splices to it. The relay executable is an
            // absolute argv path — bound read-only like every other
            // named path.
            let relay_exe = self
                .runtime_boundary
                .as_ref()
                .map(|boundary| boundary.relay_exe().to_path_buf())
                .ok_or_else(|| "egress.relay_unavailable: no boundary installed".to_string())?;
            if relay_exe.is_absolute()
                && relay_exe.exists()
                && !plan.extra_binds.contains(&relay_exe)
            {
                plan.extra_binds.push(relay_exe.clone());
            }
            (
                containment::relay_wrapped(inner_argv, &relay_exe),
                Some(crate::egress::RELAY_PROXY_URL),
            )
        } else {
            (inner_argv.to_vec(), None)
        };
        if let Some(reason) = containment::plan_denial(&plan) {
            return Err(reason);
        }
        Ok((
            containment::bwrap_args(primitive, &plan, &exec_argv, inner_cwd),
            containment::contained_env(env, egress_proxy),
        ))
    }

    /// Wrap a `describe` probe for the gate's answer: a contained
    /// profile produces the bwrap outer argv, the sandbox env, and a
    /// mount plan binding the workspace read-only when it exists;
    /// `Uncontained` passes the plain argv/env through. `probe_argv`
    /// is the worker-level argv — the recorded binding surface never
    /// sees the launcher. Public for the same reason as
    /// [`Self::containment_gate`]. `Err` is a typed boundary refusal —
    /// the caller records it like a gate refusal.
    pub fn contained_probe(
        &self,
        gate: &Gate,
        probe_argv: &[String],
        workspace: Option<&Path>,
        gitconfig: &Path,
        env: &WorkerEnv,
    ) -> Result<(Vec<String>, WorkerEnv), String> {
        let inner_argv = adapter::describe_argv(probe_argv);
        match gate {
            Gate::Uncontained => Ok((inner_argv, env.clone())),
            Gate::Ready { primitive, worker } => {
                let workspace_bound = workspace.filter(|path| path.is_dir());
                let plan = MountPlan {
                    workspace: workspace_bound.map(|path| (path.to_path_buf(), true)),
                    request: None,
                    input: None,
                    out: None,
                    gitconfig: Some(gitconfig.to_path_buf()),
                    runtime_auth: None,
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                };
                let inner_cwd = if workspace_bound.is_some() {
                    containment::SANDBOX_WORKSPACE
                } else {
                    "/tmp"
                };
                self.contained_exec(primitive, plan, &inner_argv, inner_cwd, env)
            }
        }
    }

    /// Copy the producing workspace into the check's scratch tree and
    /// return the snapshot path. A source that cannot be read is a
    /// launch refusal, never a degraded check against partial state.
    fn snapshot_check_workspace(
        &self,
        invocation: &Invocation,
        work_id: &WorkId,
    ) -> Result<PathBuf, ExecError> {
        let source = self.workspace_of(work_id);
        let dest = self.check_workspace(&invocation.id.0);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        copy_workspace_tree(&source, &dest)?;
        Ok(dest)
    }

    /// One supervisor pass: consume approved requests, inspect live
    /// invocations, drive stop intents and artifact handoffs, and run
    /// one recovery attempt for each Execution waiting on artifact
    /// recovery. Reconciles by scanning
    /// the store — a wakeup is an at-least-once signal, not evidence.
    pub fn run_once(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {
        self.run_once_scoped(
            store,
            now_unix_seconds,
            None,
            &mut || now_unix_seconds,
            None,
        )
        .map(|_| ())
    }

    /// One supervisor pass restricted to one Work for a diagnostic
    /// `--work` pass: nothing outside the scope is launched, polled,
    /// stopped, handed off or recovered.
    ///
    /// Every item loop inside checks `deadline` before starting a new
    /// item, and the pass checks it between the loops — a pass that runs
    /// out of tick budget stops instead of launching, polling or
    /// signalling past the deadline; the untouched items are durable and
    /// wait for the next pass. `Ok(true)` means the deadline stopped the
    /// pass before it exhausted its work.
    pub fn run_once_scoped(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<bool, ExecError> {
        self.consume_requests_scoped(store, now_unix_seconds, scope, clock, deadline)?;
        if deadline_reached(clock, deadline) {
            return Ok(true);
        }
        self.poll_invocations_scoped(store, now_unix_seconds, scope, clock, deadline)?;
        if deadline_reached(clock, deadline) {
            return Ok(true);
        }
        self.drive_stops_scoped(store, now_unix_seconds, scope, clock, deadline)?;
        if deadline_reached(clock, deadline) {
            return Ok(true);
        }
        self.drive_handoffs_scoped(store, scope, clock, deadline)?;
        if deadline_reached(clock, deadline) {
            return Ok(true);
        }
        self.bind_v2_assessments_scoped(store, scope, clock, deadline)?;
        if deadline_reached(clock, deadline) {
            return Ok(true);
        }
        self.run_recovery_scoped(store, now_unix_seconds, scope, clock, deadline)?;
        self.sweep_isolation_scratch(store)?;
        self.sweep_invocation_io(store)?;
        Ok(deadline_reached(clock, deadline))
    }

    /// Bound `.isolation`: scratch trees are released with their
    /// Invocation's lifecycle, but a crashed pass leaves orphans the
    /// catalog-driven retention sweep cannot see — they are not
    /// registered bodies. Once per pass, reclaim every scratch tree no
    /// unresolved Invocation still owns; the keep-set is global, so a
    /// live or unproven writer's `HOME`/`TMPDIR` is never touched.
    pub fn sweep_isolation_scratch(&self, store: &impl Store) -> Result<(), ExecError> {
        let keep: HashSet<PathBuf> = store
            .transact(|tx| tx.unresolved_invocations())?
            .iter()
            .flat_map(|invocation| {
                [
                    self.launch_scratch(&invocation.id.0),
                    self.check_workspace(&invocation.id.0),
                ]
            })
            .collect();
        isolation::sweep_orphans(&isolation::scratch_root(&self.workspace_root), &keep);
        Ok(())
    }

    /// Durable artifact handoff: resume every incomplete row (the
    /// restart path re-derives seeds from recorded ids) and open rows
    /// for Done invocations whose workspace carries git objects. Runs
    /// only when a handoff layout is configured; export is reached only
    /// after confirmed exit and stop proof — the point at which a
    /// finished Invocation becomes `Done`.
    pub fn drive_handoffs(&self, store: &impl Store) -> Result<(), ExecError> {
        // No deadline is set, so the clock serves only observation
        // timestamps — wall time is the honest source here; scoped
        // passes keep their injected clock.
        self.drive_handoffs_scoped(
            store,
            None,
            &mut || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            },
            None,
        )
    }

    /// `drive_handoffs` restricted to one Work for a diagnostic `--work`
    /// pass: rows and Executions outside the scope are left alone, and
    /// the deadline is checked before each item so a pass out of tick
    /// budget drives nothing further — the remaining rows stay durable.
    pub fn drive_handoffs_scoped(
        &self,
        store: &impl Store,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let Some(layout) = &self.handoff else {
            return Ok(());
        };
        let mut seeds: Vec<(String, crate::handoff::HandoffSeed, u32)> = Vec::new();
        // Rows already in flight resume first: a crash mid-export leaves
        // the row to advance from its last committed stage.
        for row in store.transact(|tx| tx.incomplete_handoffs())? {
            if deadline_reached(clock, deadline) {
                break;
            }
            let Some(invocation) =
                store.transact(|tx| tx.invocation_for_request(&row.request_id))?
            else {
                continue;
            };
            if let Some(work_id) = scope {
                let in_scope = store
                    .transact(|tx| tx.execution(&invocation.execution_id))?
                    .is_some_and(|e| e.work_id == *work_id);
                if !in_scope {
                    continue;
                }
            }
            if let Some((seed, bound)) = self.handoff_seed(store, &invocation)? {
                seeds.push((row.artifact_id.clone(), seed, bound));
            }
        }
        // Done invocations on nonterminal executions may need a row.
        // A non-git workspace keeps the file-artifact path untouched.
        for execution in store.transact(|tx| tx.nonterminal_executions())? {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope
                && execution.work_id != *work_id
            {
                continue;
            }
            if !self.workspace_of(&execution.work_id).join(".git").exists() {
                continue;
            }
            for invocation in store.transact(|tx| tx.invocations_for_execution(&execution.id))? {
                if invocation.state != InvocationState::Done
                    || invocation.required_artifacts.is_empty()
                {
                    continue;
                }
                // `Done` is a record, not stop proof — an unverified
                // Exited observation (a worker's own done claim)
                // resolves the Invocation the same way. Export opens
                // only after every recorded identity inspects stopped;
                // a live or uninspectable writer blocks export, and a
                // live straggler is re-signaled, never bundled.
                let intent = store.transact(|tx| tx.stop_intent(&invocation.id))?;
                match self.prove_tree_stopped(store, &invocation, intent.as_ref())? {
                    TreeProof::Proven => {}
                    TreeProof::Withheld | TreeProof::Uncertain => continue,
                }
                let Some((seed, bound)) = self.handoff_seed(store, &invocation)? else {
                    continue;
                };
                for required in &invocation.required_artifacts {
                    let artifact_id = format!("{}/{}", invocation.id.0, required.kind);
                    if seeds.iter().any(|(id, ..)| id == &artifact_id) {
                        continue;
                    }
                    seeds.push((artifact_id, seed.clone(), bound));
                }
            }
        }
        for (artifact_id, seed, bound) in seeds {
            if deadline_reached(clock, deadline) {
                break;
            }
            // A charged transport retry is not an error to the sweep —
            // but a store or filesystem failure must surface, never be
            // counted as a successful pass.
            match crate::handoff::drive(store, layout, &artifact_id, &seed, bound, clock) {
                Ok(handoff_row) => {
                    self.record_claim_check(
                        store,
                        &seed.invocation,
                        &artifact_id,
                        &handoff_row,
                        clock(),
                    )?;
                }
                Err(crate::handoff::HandoffError::Transport(_)) => {}
                Err(crate::handoff::HandoffError::Store(e)) => {
                    return Err(ExecError::Store(e));
                }
                Err(crate::handoff::HandoffError::Io(e)) => return Err(ExecError::Io(e)),
            }
        }
        Ok(())
    }

    /// Re-derive gate evidence (`Assessment`/`Review` facts) from
    /// finished v2 assessment-role Invocations. Binding is a pure
    /// function of durable state — the verified request body, the
    /// retained report envelope, the acknowledged handoff rows, and
    /// the current accepted target — so every pass retries whatever
    /// does not yet bind: a pre-publication InvariantCheck emits once
    /// its artifact becomes the accepted head, and a review whose
    /// target moved simply never binds. Identical facts dedupe on
    /// delivery identity; nothing the Worker claims can mark its own
    /// assessment complete.
    pub fn bind_v2_assessments(&self, store: &impl Store) -> Result<(), ExecError> {
        self.bind_v2_assessments_scoped(
            store,
            None,
            &mut || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            },
            None,
        )
    }

    /// `bind_v2_assessments` restricted to one Work for a diagnostic
    /// `--work` pass.
    fn bind_v2_assessments_scoped(
        &self,
        store: &impl Store,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let bodies = self
            .body_store
            .as_ref()
            .or(self.v2.as_ref().map(|d| &d.bodies));
        for execution in store.transact(|tx| tx.nonterminal_executions())? {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope
                && execution.work_id != *work_id
            {
                continue;
            }
            for invocation in store.transact(|tx| tx.invocations_for_execution(&execution.id))? {
                if deadline_reached(clock, deadline) {
                    break;
                }
                if invocation.state != InvocationState::Done
                    || !matches!(
                        invocation.role,
                        InvocationRole::InvariantCheck | InvocationRole::Review
                    )
                {
                    continue;
                }
                let Some((request, fact)) = crate::assessment_v2::bind(store, bodies, &invocation)?
                else {
                    continue;
                };
                self.report(store, &request, fact, clock())?;
            }
        }
        Ok(())
    }

    /// The ambient launch context a handoff row does not carry: base
    /// revision, branch, and declared verification argv, plus the
    /// execution's artifact-recovery bound.
    fn handoff_seed(
        &self,
        store: &impl Store,
        invocation: &Invocation,
    ) -> Result<Option<(crate::handoff::HandoffSeed, u32)>, ExecError> {
        let found = store.transact(|tx| {
            let Some(execution) = tx.execution(&invocation.execution_id)? else {
                return Ok(None);
            };
            let Some(work) = tx.work(&execution.work_id)? else {
                return Ok(None);
            };
            let Some(spec) = tx.spec(&work.id, Some(work.spec))? else {
                return Ok(None);
            };
            // The Work-declared verification egress hosts and the
            // workspace fork point live in the immutable v2 request
            // body; a v1 dispatch or an absent body declares neither.
            // An unverifiable body is corrupt durable state — the
            // parse failure is a fault, never a silently empty grant
            // or an unpinned base.
            let (verification_egress_hosts, pinned_base) =
                match tx.worker_request_body(&invocation.request_id)? {
                    Some(body) => {
                        if !body.verifies() {
                            return Err(StoreError::Internal {
                                detail: "worker request body fails integrity verification".into(),
                            });
                        }
                        let req = serde_json::from_slice::<WorkerRequestV2>(&body.canonical_body)
                            .map_err(|e| StoreError::Internal {
                            detail: format!("worker request body unreadable: {e}"),
                        })?;
                        (
                            req.required_capabilities.verification_egress_hosts,
                            req.base.object,
                        )
                    }
                    None => (Vec::new(), String::new()),
                };
            Ok(Some((
                invocation.clone(),
                work.id.clone(),
                spec.payload.verification_argv.clone(),
                verification_egress_hosts,
                pinned_base,
                execution.budget.artifact_recovery_attempts,
            )))
        })?;
        let Some((
            invocation,
            work_id,
            verification_argv,
            verification_egress_hosts,
            pinned_base,
            bound,
        )) = found
        else {
            return Ok(None);
        };
        // A v2 dispatch pins the fork point its immutable request
        // recorded — the stale-base, nothing-produced, and manifest
        // bounds then read durable state, never a ref the Worker
        // could rewrite inside its own workspace. Only a v1 dispatch
        // leaves `base` empty, and the managed branch's fork point is
        // then derived inside the bounded export stage (never from
        // `SourceSnapshot.source_revision`, a host issue-version
        // token, and never here, where a derivation failure would
        // abort the sweep instead of converging on
        // `recovery_exhausted`).
        let branch = format!("work/{}", work_id.0);
        let base = pinned_base;
        Ok(Some((
            crate::handoff::HandoffSeed {
                invocation,
                base,
                branch,
                work_id,
                verification_argv,
                verification_egress_hosts,
            },
            bound,
        )))
    }

    /// Compare the retained `claimed_head` of an acknowledged
    /// artifact's Worker report against the commit the handoff
    /// actually promoted, and record the verdict as evidence
    /// (`docs/v0.3.md` §6.3). Runs after `handoff::drive` returns, so
    /// `produced` is durable. A report classified `invalid` or
    /// `absent` — or carrying no claim — records nothing: claims are
    /// evidence for later gates, never authority over the artifact,
    /// and a `false` verdict never moves or replaces it.
    fn record_claim_check(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        artifact_id: &str,
        handoff: &ArtifactHandoff,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        if handoff.state != HandoffState::Acknowledged {
            return Ok(());
        }
        let Some(produced) = handoff.produced.clone() else {
            return Ok(());
        };
        let observations =
            store.transact(|tx| tx.observations_for_execution(&invocation.execution_id))?;
        // A verdict for this exact produced commit is already durable:
        // the acknowledged row is re-seeded every pass while its
        // Execution stays nonterminal, and re-resolving the evidence
        // to repeat a recorded verdict buys nothing — and makes a
        // later CAS reclamation fault the whole handoff sweep.
        if observations.iter().any(|o| {
            matches!(
                &o.fact,
                ObservationFact::ClaimedHeadCheck {
                    invocation_id,
                    artifact_id: a,
                    produced: p,
                    ..
                } if invocation_id == &invocation.id && a == artifact_id && p == &produced
            )
        }) {
            return Ok(());
        }
        let report_fact = observations
            .into_iter()
            .find(|o| {
                matches!(
                    &o.fact,
                    ObservationFact::WorkerReport { invocation_id, .. }
                        if invocation_id == &invocation.id
                )
            })
            .map(|o| o.fact);
        let Some(ObservationFact::WorkerReport {
            classification,
            report: Some(report_evidence),
            ..
        }) = report_fact
        else {
            return Ok(());
        };
        // Only a validated envelope's claim is read — `invalid`
        // retains raw bytes but never a trusted claim, and `absent`
        // has none.
        if matches!(
            classification,
            ReportClassification::Invalid { .. } | ReportClassification::Absent
        ) {
            return Ok(());
        }
        let Some(bodies) = self
            .body_store
            .as_ref()
            .or(self.v2.as_ref().map(|d| &d.bodies))
        else {
            // Report evidence exists but its CAS root is unconfigured —
            // the same fault `report_v2::collect` refuses to paper over.
            return Err(ExecError::Fault(
                "worker report claim check requires the CAS body store".into(),
            ));
        };
        let resolved = match bodies.resolve(store.body_catalog(), &report_evidence.body_id) {
            Ok(Some(resolved)) => resolved,
            // The verdict is advisory evidence for later gates, never
            // authority over the artifact: a body the retention sweep
            // already reclaimed — or that never catalogued — records
            // no verdict, never a pass-aborting fault.
            Ok(None) | Err(BodyError::Unavailable { .. }) => return Ok(()),
            Err(e) => {
                return Err(ExecError::Fault(format!("resolve worker report body: {e}")));
            }
        };
        let report: WorkerReportV2 = serde_json::from_slice(&resolved.bytes)
            .map_err(|e| ExecError::Fault(format!("retained worker report does not parse: {e}")))?;
        let Some(claimed) = report.claimed_head else {
            return Ok(());
        };
        let Some(request) = store.transact(|tx| tx.request(&invocation.request_id))? else {
            return Ok(());
        };
        let agrees = claimed == produced;
        self.report(
            store,
            &request,
            ObservationFact::ClaimedHeadCheck {
                invocation_id: invocation.id.clone(),
                artifact_id: artifact_id.to_string(),
                claimed,
                produced,
                agrees,
            },
            now_unix_seconds,
        )
    }

    /// Replay the durable request log: for each unconsumed request either
    /// reconcile the recorded Invocation (never a second start) or record
    /// one and launch.
    pub fn consume_requests(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        self.consume_requests_scoped(
            store,
            now_unix_seconds,
            None,
            &mut || now_unix_seconds,
            None,
        )
    }

    /// Replay the durable request log for one scope only: a scoped pass
    /// never launches another Work's Invocation. The deadline is checked
    /// before each request starts, so a pass that runs out of tick budget
    /// launches nothing further — the remaining requests stay durable.
    pub fn consume_requests_scoped(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        for request in store.transact(|tx| tx.pending_requests())? {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope
                && request.work_id != *work_id
            {
                continue;
            }
            self.reconcile_or_start(store, &request, now_unix_seconds, clock)?;
        }
        Ok(())
    }

    fn reconcile_or_start(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        now_unix_seconds: i64,
        clock: &mut dyn FnMut() -> i64,
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
                            exit_code: None,
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

        // The v2 consume path materializes the immutable request body
        // from durable state before any Invocation exists; a request
        // already consumed above replays its recorded Invocation
        // regardless of which profile is configured now.
        if self.v2.is_some() {
            return self.start_v2(store, request, now_unix_seconds, clock);
        }

        // The Execution state gates the describe probe too: a request
        // that can never launch never starts the worker binary, not even
        // the effect-free declaration probe.
        let exec_state = store
            .transact(|tx| tx.execution(&request.execution_id))?
            .map(|e| e.state);

        // Resolve the request's adapter before any Invocation is
        // recorded. A request that already carries a binding keeps it
        // verbatim — a later `worker_argv` swap can only affect requests
        // created after the change — but the bound argv is probed again
        // and must still declare the stored binding; a mismatch is a
        // configuration repair that authorizes no start. An unbound
        // request selects now and records the binding in the consume
        // commit. A failed `describe` is fail-closed: the adapter is not
        // selectable and the request does not dispatch.
        let (binding, repair_reason, gate) = if exec_state != Some(ExecutionState::Ready) {
            (None, None, None)
        } else {
            let workspace = self.workspace_of(&request.work_id);
            std::fs::create_dir_all(&workspace)?;
            // The containment gate covers the describe probe and the
            // launch alike; a refused proof is a configuration repair,
            // never a degraded launch.
            let probe_argv = request
                .adapter_binding
                .as_ref()
                .map(|binding| binding.argv.clone())
                .unwrap_or_else(|| self.worker_argv.clone());
            let gate = self.containment_gate(&probe_argv, now_unix_seconds)?;
            // The probe runs under the profile's environment — an
            // untrusted adapter binary sees no host credential even
            // during the effect-free declaration. A preparation failure
            // is fail-closed: the request does not dispatch under a
            // degraded environment. The probe's scratch tree is
            // reclaimed as soon as the probe process is gone.
            let probe_scratch = isolation::scratch_root(&self.workspace_root).join(format!(
                "probe-{}",
                isolation::sanitize_key(&request.request_id)
            ));
            let probe_env = isolation::prepare(&self.isolation_profile, &probe_scratch);
            let selected = match (&gate, probe_env) {
                (Err(reason), _) => (
                    None,
                    Some(format!("isolation.containment_refused: {reason}")),
                ),
                (Ok(_), Err(why)) => (
                    None,
                    Some(format!("isolation.environment_preparation_failed: {why}")),
                ),
                (Ok(gate), Ok(env)) => {
                    // The probe gets the same containment the launch
                    // will: the workspace bound read-only, the
                    // generated gitconfig, no writable surfaces — and
                    // the same runtime boundary. A boundary refusal is
                    // a configuration repair like a refused gate.
                    match self.contained_probe(
                        gate,
                        &probe_argv,
                        Some(&workspace),
                        &probe_scratch.join("gitconfig"),
                        &env,
                    ) {
                        Err(reason) => (
                            None,
                            Some(format!("isolation.containment_refused: {reason}")),
                        ),
                        Ok((probe_exec_argv, probe_exec_env)) => match &request.adapter_binding {
                            Some(stored) => {
                                match adapter::observe_worker_declaration_with_argv(
                                    &self.port,
                                    &probe_exec_argv,
                                    &workspace,
                                    &probe_exec_env,
                                ) {
                                    Ok(observed) => {
                                        match adapter::select_adapter(&observed, stored) {
                                            adapter::AdapterSelection::Ready(binding) => {
                                                (Some(binding), None)
                                            }
                                            adapter::AdapterSelection::ConfigurationRepair {
                                                reason,
                                            } => (None, Some(reason)),
                                        }
                                    }
                                    Err(why) => (
                                        None,
                                        Some(format!("adapter.declaration_unavailable: {why}")),
                                    ),
                                }
                            }
                            None => {
                                match adapter::observe_worker_declaration_with_argv(
                                    &self.port,
                                    &probe_exec_argv,
                                    &workspace,
                                    &probe_exec_env,
                                ) {
                                    Ok(observed) => (Some(observed.binding(&probe_argv)), None),
                                    Err(why) => (
                                        None,
                                        Some(format!("adapter.declaration_unavailable: {why}")),
                                    ),
                                }
                            }
                        },
                    }
                }
            };
            // The probe process is gone once `run_probe` returned, so
            // the scratch tree is reclaimed on every arm — a refused
            // gate or a failed preparation leaves nothing behind.
            isolation::release(&probe_scratch);
            let (binding, repair_reason) = selected;
            (binding, repair_reason, gate.ok())
        };

        // First consumption: persist the WorkerRequest export and the
        // Invocation inside the same commit that consumes the request —
        // crash order is export, then identity, then launch. The export
        // is durable before any process exists; a launch that fails or
        // never returns leaves it behind for reconciliation. Recording
        // the Invocation flips `consumed` itself; the explicit mark
        // records the supervisor's handoff and is idempotent. The
        // request's first adapter binding lands in the same commit.
        let invocation = Invocation {
            id: InvocationId(store.transact(|tx| tx.alloc_id("inv"))?),
            request_id: request.request_id.clone(),
            execution_id: request.execution_id.clone(),
            role: request.role.clone(),
            state: InvocationState::Prepared,
            process_identity: None,
            descendants: vec![],
            required_artifacts: required_artifacts(request),
            evidence_refs: vec![],
        };
        // The export names the workspace the launch actually runs
        // against: the producing workspace for a writer, the
        // per-Invocation snapshot for the read-only check — never the
        // live workspace a check must not mutate.
        let export_workspace = match request.role {
            InvocationRole::InvariantCheck => self.check_workspace(&invocation.id.0),
            _ => self.workspace_of(&request.work_id),
        };
        let export = WorkerRequest::start(
            invocation.id.clone(),
            self.isolation_profile.id.to_string(),
            export_workspace.to_string_lossy().into_owned(),
        );
        let export_record = WorkerRequestExport {
            invocation_id: invocation.id.clone(),
            request_id: export.request_id.clone(),
            profile_id: export.profile_id.clone(),
            workspace_root: export.workspace_root.clone(),
            digest: export.digest(),
            state: WorkerRequestExportState::Exported,
            process_identity: None,
        };
        store.transact(|tx| {
            if request.adapter_binding.is_none()
                && let Some(binding) = &binding
            {
                tx.apply(&Effect::BindRequestAdapter {
                    request_id: request.request_id.clone(),
                    binding: binding.clone(),
                })?;
            }
            tx.apply(&Effect::InsertInvocation {
                invocation: invocation.clone(),
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            tx.apply(&Effect::InsertWorkerRequestExport {
                export: export_record.clone(),
            })?;
            Ok(())
        })?;

        if let Some(reason) = repair_reason {
            // A declaration mismatch or an unanswered `describe` is a
            // configuration repair: the attempt is recorded and abandoned,
            // but no worker starts. The consumed request is terminal for
            // its binding — a re-issued request binds a fresh declaration.
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
            return Ok(());
        }

        // The pre-probe Execution-state read is stale by up to the full
        // probe bound: re-read it after the consume commit so a stop or
        // cancel committed while `describe` ran still blocks the spawn —
        // the request is consumed, so blocking the launch abandons the
        // recorded Invocation. The spawn also requires the recorded
        // binding: when the pre-probe read skipped `describe` the bound
        // argv was never re-verified, so a state that flipped back to
        // Ready must not fall through to the configured `worker_argv`
        // and launch with no recorded selection.
        let launchable = binding.is_some()
            && store
                .transact(|tx| tx.execution(&request.execution_id))?
                .is_some_and(|e| e.state == ExecutionState::Ready);
        if !launchable {
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
        // The invariant check never launches in the producing
        // workspace: it runs against a per-Invocation snapshot copy, so
        // the check leg cannot mutate the state the producing
        // Invocation's artifact evidence describes. A snapshot that
        // cannot be taken refuses the launch through the same path as
        // a refused spawn — the check never degrades to the live
        // workspace.
        let workspace = match request.role {
            InvocationRole::InvariantCheck => {
                match self.snapshot_check_workspace(&invocation, &work_id) {
                    Ok(path) => path,
                    Err(why) => {
                        self.refuse_launch(
                            store,
                            request,
                            &invocation,
                            format!("invariant check workspace snapshot failed: {why}"),
                            now_unix_seconds,
                        )?;
                        return Ok(());
                    }
                }
            }
            _ => self.workspace_of(&work_id),
        };
        std::fs::create_dir_all(&workspace)?;
        // The bound argv is the recorded selection — a `worker_argv`
        // change after binding never retargets this request, and the
        // launchable gate above already proved a binding was recorded.
        // Under containment the worker sees the sandbox layout instead:
        // the workspace at `/workspace` (read-only for assessment
        // roles), the generated gitconfig, and no host credential —
        // never the unwrapped argv.
        let Some(gate) = gate else {
            self.refuse_launch(
                store,
                request,
                &invocation,
                "isolation.containment_refused: gate not evaluated".into(),
                now_unix_seconds,
            )?;
            return Ok(());
        };
        let inner_argv = binding
            .map(|binding| binding.argv)
            .expect("a launch requires a recorded binding");
        // The launch runs under the profile's environment — a
        // preparation failure refuses the launch through the same path
        // as a refused spawn; there is no degrade-to-inherited-env
        // fallback.
        let launch_env_base = match isolation::prepare(
            &self.isolation_profile,
            &self.launch_scratch(&invocation.id.0),
        ) {
            Ok(env) => env,
            Err(why) => {
                self.refuse_launch(
                    store,
                    request,
                    &invocation,
                    format!("isolation.environment_preparation_failed: {why}"),
                    now_unix_seconds,
                )?;
                return Ok(());
            }
        };
        let (argv, launch_env) = match &gate {
            Gate::Uncontained => (inner_argv, launch_env_base),
            Gate::Ready { primitive, worker } => {
                let plan = MountPlan {
                    workspace: Some((
                        workspace.clone(),
                        // §14.6: assessment roles bind the workspace
                        // read-only; writers keep read-write.
                        matches!(
                            request.role,
                            InvocationRole::InvariantCheck | InvocationRole::Review
                        ),
                    )),
                    request: None,
                    input: None,
                    out: None,
                    gitconfig: Some(self.launch_scratch(&invocation.id.0).join("gitconfig")),
                    runtime_auth: None,
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                };
                match self.contained_exec(
                    primitive,
                    plan,
                    &inner_argv,
                    "/workspace",
                    &launch_env_base,
                ) {
                    Ok(contained) => contained,
                    Err(reason) => {
                        self.refuse_launch(
                            store,
                            request,
                            &invocation,
                            format!("isolation.containment_refused: {reason}"),
                            now_unix_seconds,
                        )?;
                        return Ok(());
                    }
                }
            }
        };
        match self.port.spawn(&argv, &workspace, &launch_env) {
            Ok(identity) => {
                // Descendant coverage starts at launch: a writer that
                // escapes its tree before the first poll is still a
                // recorded identity the exit proof must account for. A
                // failed table read is not a launch failure — the
                // intent-time snapshot re-covers on the stop path.
                let descendants = self
                    .port
                    .snapshot_descendants(identity.pid)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| d.pid != identity.pid)
                    .collect();
                store.transact(|tx| {
                    let mut next = invocation.clone();
                    next.state = InvocationState::Running;
                    next.process_identity = Some(identity.clone());
                    next.descendants = descendants;
                    tx.apply(&Effect::UpdateInvocation {
                        invocation: next,
                        expected_state: InvocationState::Prepared,
                    })?;
                    // The launch binds the recorded request_id to the
                    // spawned process handle — same transaction as the
                    // running Invocation, so reconciliation can resolve
                    // the writer through the binding or not at all.
                    tx.apply(&Effect::BindWorkerRequestExport {
                        request_id: export_record.request_id.clone(),
                        process_identity: format!("{}@{}", identity.pid, identity.started_at),
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
                        exit_code: None,
                    },
                    now_unix_seconds,
                )?;
            }
            Err(SpawnError::Refused { reason }) => {
                self.refuse_launch(store, request, &invocation, reason, now_unix_seconds)?;
            }
            Err(SpawnError::Unknown { .. }) => {
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: String::new(),
                        state: ProcessStateFact::Unknown,
                        exit_code: None,
                    },
                    now_unix_seconds,
                )?;
            }
        }
        Ok(())
    }

    /// The v2 consume path (`docs/v0.3.md` §4): materialize the
    /// immutable request body and the per-Invocation input/output tree
    /// from durable state, record the body in the same commit that
    /// consumes the request, negotiate the declaration, then launch
    /// `<argv> run --request <request.json> --out <out/>`. Every
    /// refusal is typed and lands before any process exists; an
    /// infrastructure fault fails the pass so the request stays
    /// pending for the next one.
    fn start_v2(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        now_unix_seconds: i64,
        clock: &mut dyn FnMut() -> i64,
    ) -> Result<(), ExecError> {
        let dispatch = self
            .v2
            .as_ref()
            .expect("start_v2 requires the v2 dispatch profile");

        // The Execution state gates everything, exactly as the v1
        // probe gate: a request that can never launch is consumed
        // without spending a build, a probe, or a process on it.
        let exec_state = store
            .transact(|tx| tx.execution(&request.execution_id))?
            .map(|e| e.state);
        if exec_state != Some(ExecutionState::Ready) {
            self.mark_consumed(store, &request.request_id)?;
            return Ok(());
        }

        let invocation_id = InvocationId(store.transact(|tx| tx.alloc_id("inv"))?);

        let body = match crate::request_v2::build_request_body(
            store,
            dispatch,
            request,
            &invocation_id.0,
            now_unix_seconds,
        ) {
            Ok(body) => body,
            Err(crate::request_v2::BuildError::Refusal(refusal)) => {
                return self.refuse_dispatch(
                    store,
                    request,
                    &invocation_id.0,
                    refusal,
                    now_unix_seconds,
                );
            }
            Err(crate::request_v2::BuildError::Fault(detail)) => {
                return Err(ExecError::Fault(detail));
            }
        };

        // The builder validates its own output, but the launch path
        // parses the durable bytes again — negotiation reads the wire
        // request, never the builder's in-memory value.
        let wire = match WorkerRequestV2::parse(&body.canonical_body) {
            Ok(wire) => wire,
            Err(e) => {
                return self.refuse_dispatch(
                    store,
                    request,
                    &invocation_id.0,
                    DispatchRefusal::BodyCorrupt {
                        detail: format!("built request does not parse: {e}"),
                    },
                    now_unix_seconds,
                );
            }
        };

        // Every required input body re-resolves through the catalogue
        // and re-verifies before the consume commit — a body reclaimed
        // or corrupted since the build refuses here, still before any
        // process exists.
        let io = match crate::request_v2::materialize_invocation_io(
            store,
            dispatch,
            &crate::request_v2::invocation_io_root(&self.workspace_root),
            &invocation_id.0,
            &body,
        ) {
            Ok(io) => io,
            Err(crate::request_v2::MaterializeError::Refusal(refusal)) => {
                return self.refuse_dispatch(
                    store,
                    request,
                    &invocation_id.0,
                    refusal,
                    now_unix_seconds,
                );
            }
            Err(crate::request_v2::MaterializeError::Fault(detail)) => {
                return Err(ExecError::Fault(detail));
            }
        };

        // The containment gate covers the describe probe and the
        // launch alike: one negative-probe proof per `(profile,
        // worker argv)` identity, replayed from recorded evidence.
        // A refused proof is a configuration repair carried through
        // the consume commit — never a degraded launch.
        let launch_argv = request
            .adapter_binding
            .as_ref()
            .map(|binding| binding.argv.clone())
            .unwrap_or_else(|| self.worker_argv.clone());
        let gate = self.containment_gate(&launch_argv, now_unix_seconds)?;

        // Declaration probe plus the exact §5.2 negotiation, under the
        // same fail-closed shape as the v1 probe: a stored binding is
        // re-verified verbatim, an unbound request selects now, and
        // every failure is a configuration repair carried through the
        // consume commit — never a degraded launch.
        let (binding, repair_reason) = match &gate {
            Err(reason) => (
                None,
                Some(format!("isolation.containment_refused: {reason}")),
            ),
            Ok(gate) => {
                let probe_scratch = isolation::scratch_root(&self.workspace_root).join(format!(
                    "probe-{}",
                    isolation::sanitize_key(&request.request_id)
                ));
                // `describe` is effect-free: it runs in a scratch cwd, never
                // the managed workspace — creating `ws-<id>` here would
                // leave an empty directory `workspace::prepare` later reads
                // as an unreceipted conflict.
                let probe_cwd = probe_scratch.join("cwd");
                let probe_env = isolation::prepare(&self.isolation_profile, &probe_scratch);
                match probe_env {
                    Err(why) => (
                        None,
                        Some(format!("isolation.environment_preparation_failed: {why}")),
                    ),
                    Ok(env) => {
                        if let Err(e) = std::fs::create_dir_all(&probe_cwd) {
                            isolation::release(&probe_scratch);
                            return Err(ExecError::Fault(format!(
                                "create probe cwd {}: {e}",
                                probe_cwd.display()
                            )));
                        }
                        let probe_argv = request
                            .adapter_binding
                            .as_ref()
                            .map(|binding| binding.argv.clone())
                            .unwrap_or_else(|| self.worker_argv.clone());
                        // The probe gets the same containment the
                        // launch will: the workspace bound read-only
                        // when it exists, the generated gitconfig, and
                        // no writable surfaces — `describe` answers
                        // inside the boundary it declares for, runtime
                        // credential and egress profile included.
                        let probe_workspace = self.workspace_of(&request.work_id);
                        let selected = match self.contained_probe(
                            gate,
                            &probe_argv,
                            Some(&probe_workspace),
                            &probe_scratch.join("gitconfig"),
                            &env,
                        ) {
                            Err(reason) => (
                                None,
                                Some(format!("isolation.containment_refused: {reason}")),
                            ),
                            Ok((probe_exec_argv, probe_exec_env)) => {
                                match adapter::observe_worker_declaration_v2_with_argv(
                                    &self.port,
                                    &probe_exec_argv,
                                    &probe_cwd,
                                    &probe_exec_env,
                                ) {
                                    Ok((declared, surface)) => match declared.negotiate_v2(&wire) {
                                        Ok(()) => match &request.adapter_binding {
                                            Some(stored) => {
                                                match adapter::select_adapter(&surface, stored)
                                                    {
                                                        adapter::AdapterSelection::Ready(binding) => {
                                                            (Some(binding), None)
                                                        }
                                                        adapter::AdapterSelection::ConfigurationRepair {
                                                            reason,
                                                        } => (None, Some(reason)),
                                                    }
                                            }
                                            None => (Some(surface.binding(&probe_argv)), None),
                                        },
                                        Err(why) => (None, Some(why.to_string())),
                                    },
                                    Err(why) => (
                                        None,
                                        Some(format!("adapter.declaration_unavailable: {why}")),
                                    ),
                                }
                            }
                        };
                        isolation::release(&probe_scratch);
                        selected
                    }
                }
            }
        };

        // First consumption: the Invocation, the consumed mark, the
        // first adapter binding, and the canonical request body land
        // in one commit — a restart replays the stored bytes verbatim
        // and never rebuilds from mutable configuration. There is no
        // v1 WorkerRequest export for a v2 request: the durable body
        // row is the launch record.
        let invocation = Invocation {
            id: invocation_id.clone(),
            request_id: request.request_id.clone(),
            execution_id: request.execution_id.clone(),
            role: request.role.clone(),
            state: InvocationState::Prepared,
            process_identity: None,
            descendants: vec![],
            required_artifacts: required_artifacts(request),
            evidence_refs: vec![],
        };
        store.transact(|tx| {
            if request.adapter_binding.is_none()
                && let Some(binding) = &binding
            {
                tx.apply(&Effect::BindRequestAdapter {
                    request_id: request.request_id.clone(),
                    binding: binding.clone(),
                })?;
            }
            tx.apply(&Effect::InsertWorkerRequestBody { body: body.clone() })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: invocation.clone(),
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request.request_id.clone(),
            })?;
            Ok(())
        })?;

        if let Some(reason) = repair_reason {
            // A declaration that cannot negotiate the request — or no
            // answer at all — is a configuration repair: the attempt is
            // recorded and abandoned, but no worker starts. The
            // consumed request is terminal for its binding.
            store.transact(|tx| {
                let mut next = invocation.clone();
                next.state = InvocationState::Abandoned;
                tx.apply(&Effect::UpdateInvocation {
                    invocation: next,
                    expected_state: InvocationState::Prepared,
                })?;
                Ok(())
            })?;
            self.release_invocation_io(&invocation.id.0);
            self.report(
                store,
                request,
                ObservationFact::LaunchRefused {
                    invocation_id: invocation.id.clone(),
                    reason,
                },
                now_unix_seconds,
            )?;
            return Ok(());
        }

        // The pre-build Execution-state read is stale by up to the
        // build and probe bounds: re-read it after the consume commit
        // so a stop or cancel committed meanwhile still blocks the
        // spawn — the request is consumed, so blocking the launch
        // abandons the recorded Invocation.
        let launchable = binding.is_some()
            && store
                .transact(|tx| tx.execution(&request.execution_id))?
                .is_some_and(|e| e.state == ExecutionState::Ready);
        if !launchable {
            store.transact(|tx| {
                let mut next = invocation.clone();
                next.state = InvocationState::Abandoned;
                tx.apply(&Effect::UpdateInvocation {
                    invocation: next,
                    expected_state: InvocationState::Prepared,
                })?;
                Ok(())
            })?;
            self.release_invocation_io(&invocation.id.0);
            return Ok(());
        }

        let work_id = request.work_id.clone();
        // Writer roles launch inside the exact prepared workspace —
        // `workspace::prepare` re-validates the durable body, the
        // pins, the reservations, and the predecessor against live git
        // state and refuses rather than approximating. Read roles keep
        // their existing launch trees: the check's per-Invocation
        // snapshot and the review's managed workspace (mount-level
        // read-only enforcement is the containment slice).
        let workspace = match request.role {
            InvocationRole::Coding | InvocationRole::Fix => {
                let work = store.transact(|tx| tx.work(&work_id))?.ok_or_else(|| {
                    ExecError::Fault(format!("request {} names no work", request.request_id))
                })?;
                let Some(source) = dispatch.source_for(&work.binding.repository) else {
                    // The build already refused a missing source;
                    // reaching here means the configuration changed
                    // mid-pass — refuse the launch, never guess a
                    // checkout.
                    self.refuse_launch(
                        store,
                        request,
                        &invocation,
                        "repository source is no longer configured".into(),
                        now_unix_seconds,
                    )?;
                    return Ok(());
                };
                match self.prepare_workspace(store, &request.request_id, source, now_unix_seconds) {
                    Ok(prepared) => prepared.path,
                    Err(why) => {
                        self.refuse_launch(
                            store,
                            request,
                            &invocation,
                            format!("workspace preparation refused: {why}"),
                            now_unix_seconds,
                        )?;
                        return Ok(());
                    }
                }
            }
            InvocationRole::InvariantCheck => {
                match self.snapshot_check_workspace(&invocation, &work_id) {
                    Ok(path) => path,
                    Err(why) => {
                        self.refuse_launch(
                            store,
                            request,
                            &invocation,
                            format!("invariant check workspace snapshot failed: {why}"),
                            now_unix_seconds,
                        )?;
                        return Ok(());
                    }
                }
            }
            InvocationRole::Review => self.workspace_of(&work_id),
        };
        std::fs::create_dir_all(&workspace)?;

        // The bound argv is the recorded selection; the approved v2
        // transport appends the request/out pair — no shell, no stdin
        // protocol, no fallback argv. Under containment the worker sees
        // the sandbox paths (`/taisan/request.json`, `/taisan/out`);
        // uncontained launches keep the host paths they always had.
        let gate = match gate {
            Ok(gate) => gate,
            Err(reason) => {
                // Unreachable in practice — a refused gate already
                // carried its reason through `repair_reason` — but the
                // launch never proceeds on an unproven boundary.
                self.release_launch_workspace(request, &workspace);
                self.refuse_launch(
                    store,
                    request,
                    &invocation,
                    format!("isolation.containment_refused: {reason}"),
                    now_unix_seconds,
                )?;
                return Ok(());
            }
        };
        let contained = matches!(gate, Gate::Ready { .. });
        let mut inner_argv = binding
            .map(|binding| binding.argv)
            .expect("a launch requires a recorded binding");
        inner_argv.push("run".into());
        inner_argv.push("--request".into());
        inner_argv.push(if contained {
            containment::SANDBOX_REQUEST.to_string()
        } else {
            io.request_path.to_string_lossy().into_owned()
        });
        inner_argv.push("--out".into());
        inner_argv.push(if contained {
            containment::SANDBOX_OUT.to_string()
        } else {
            io.out_dir.to_string_lossy().into_owned()
        });

        let launch_env_base = match isolation::prepare(
            &self.isolation_profile,
            &self.launch_scratch(&invocation.id.0),
        ) {
            Ok(env) => env,
            Err(why) => {
                self.release_launch_workspace(request, &workspace);
                self.refuse_launch(
                    store,
                    request,
                    &invocation,
                    format!("isolation.environment_preparation_failed: {why}"),
                    now_unix_seconds,
                )?;
                return Ok(());
            }
        };
        let (argv, launch_env) = match &gate {
            Gate::Uncontained => (inner_argv, launch_env_base),
            Gate::Ready { primitive, worker } => {
                let plan = MountPlan {
                    workspace: Some((workspace.clone(), wire.workspace.read_only)),
                    request: Some(io.request_path.clone()),
                    input: Some(io.input_dir.clone()),
                    out: Some(io.out_dir.clone()),
                    gitconfig: Some(self.launch_scratch(&invocation.id.0).join("gitconfig")),
                    runtime_auth: None,
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                };
                match self.contained_exec(
                    primitive,
                    plan,
                    &inner_argv,
                    "/workspace",
                    &launch_env_base,
                ) {
                    Ok(contained) => contained,
                    Err(reason) => {
                        self.release_launch_workspace(request, &workspace);
                        self.refuse_launch(
                            store,
                            request,
                            &invocation,
                            format!("isolation.containment_refused: {reason}"),
                            now_unix_seconds,
                        )?;
                        return Ok(());
                    }
                }
            }
        };
        // The post-consume Ready read is stale by workspace
        // preparation and boundary resolution: re-verify authorization
        // at the spawn boundary itself — the Execution still Ready and
        // the request's own deadline unexpired — so a stop, deadline,
        // or cancel committed during declaration, materialization, or
        // preparation still blocks the launch. The deadline comparison
        // reads the clock again: `now_unix_seconds` is the pass
        // timestamp `build_request_body` already validated against,
        // so only a fresh read catches a deadline that elapsed while
        // the launch was being prepared. The recorded Invocation
        // abandons exactly as the earlier gate does; no late writer
        // starts after authorization expires.
        let authorized = wire.budget.deadline_unix_seconds > clock()
            && store
                .transact(|tx| tx.execution(&request.execution_id))?
                .is_some_and(|e| e.state == ExecutionState::Ready);
        if !authorized {
            store.transact(|tx| {
                let mut next = invocation.clone();
                next.state = InvocationState::Abandoned;
                tx.apply(&Effect::UpdateInvocation {
                    invocation: next,
                    expected_state: InvocationState::Prepared,
                })?;
                Ok(())
            })?;
            // Unlike the earlier gate this runs after workspace
            // preparation: the launch scratch, any check snapshot, and
            // the prepared workspace itself may already exist. A
            // writer workspace must go together with its `.prepare`
            // receipt — bound to this request's digest, it would
            // refuse a Coding retry on 'receipt belongs to another
            // request' — while a read role's workspace is never
            // released. No process exists, so nothing owns the tree;
            // a retry re-prepares from source or handoff.
            self.release_launch_workspace(request, &workspace);
            isolation::release(&self.launch_scratch(&invocation.id.0));
            isolation::release(&self.check_workspace(&invocation.id.0));
            self.release_invocation_io(&invocation.id.0);
            return Ok(());
        }
        // Any refusal below this point also owns the prepared
        // workspace: the `SpawnError::Refused` arm releases it for
        // the same receipt-conflict reason.
        match self.port.spawn(&argv, &workspace, &launch_env) {
            Ok(identity) => {
                let descendants = self
                    .port
                    .snapshot_descendants(identity.pid)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| d.pid != identity.pid)
                    .collect();
                store.transact(|tx| {
                    let mut next = invocation.clone();
                    next.state = InvocationState::Running;
                    next.process_identity = Some(identity.clone());
                    next.descendants = descendants;
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
                        exit_code: None,
                    },
                    now_unix_seconds,
                )?;
            }
            Err(SpawnError::Refused { reason }) => {
                // No process exists; the workspace a writer role
                // prepared must not wedge a retry on 'receipt belongs
                // to another request', and a read role's workspace is
                // never released.
                self.release_launch_workspace(request, &workspace);
                self.refuse_launch(store, request, &invocation, reason, now_unix_seconds)?;
            }
            Err(SpawnError::Unknown { .. }) => {
                self.report(
                    store,
                    request,
                    ObservationFact::ProcessState {
                        invocation_id: invocation.id.clone(),
                        process_identity: String::new(),
                        state: ProcessStateFact::Unknown,
                        exit_code: None,
                    },
                    now_unix_seconds,
                )?;
            }
        }
        Ok(())
    }

    /// Record a typed dispatch refusal: the `dispatch_refused` fact
    /// pauses the waiting Execution and the request is marked consumed
    /// — terminal for its own `request_id`, never silently retried.
    /// The per-Invocation tree a refused materialization may have
    /// partially written is reclaimed with the refusal; no worker ever
    /// ran, so there is nothing to collect from `out/`.
    fn refuse_dispatch(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        invocation_id: &str,
        refusal: DispatchRefusal,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        self.release_invocation_io(invocation_id);
        self.report(
            store,
            request,
            ObservationFact::DispatchRefused {
                request_id: request.request_id.clone(),
                code: refusal.code().to_string(),
                detail: refusal.detail().to_string(),
            },
            now_unix_seconds,
        )?;
        self.mark_consumed(store, &request.request_id)
    }

    /// Reclaim one Invocation's input/output tree. Used on every path
    /// where no worker could have written into `out/` — a refused
    /// dispatch, a refused launch, an abandoned attempt. Trees of
    /// resolved Invocations that did run are the retention sweep's
    /// business, not this helper's.
    fn release_invocation_io(&self, invocation_id: &str) {
        let _ = std::fs::remove_dir_all(
            crate::request_v2::invocation_io_root(&self.workspace_root)
                .join(isolation::sanitize_key(invocation_id)),
        );
    }

    /// Reclaim the workspace a refused or abandoned launch owned. A
    /// writer role's `workspace::prepare` output goes together with
    /// its `.prepare` receipt — the receipt lives in a sibling
    /// control area, not inside `ws-<work>`, so a deleted workspace
    /// alone still refuses the Work's next preparation on 'receipt
    /// belongs to another request'. The check's per-Invocation
    /// snapshot is this launch's own scratch. The review leg's
    /// workspace is the shared writer checkout `ws-<work>` — never
    /// released here: `run_recovery_scoped`/`drive_handoffs` still
    /// read the artifact and recovery source from it, so a refused
    /// review launch must not destroy the producing tree.
    fn release_launch_workspace(&self, request: &InvocationRequest, workspace: &Path) {
        match request.role {
            InvocationRole::Review => {}
            InvocationRole::InvariantCheck => isolation::release(workspace),
            _ => {
                isolation::release(workspace);
                crate::workspace::release_receipt(&self.workspace_root, &request.work_id.0);
            }
        }
    }

    /// Bound `.invocations`: per-Invocation request/input/output trees
    /// are released with their Invocation's lifecycle, but a refused
    /// dispatch or a crash between materialization and the consume
    /// commit leaves orphans no unresolved Invocation names. Once per
    /// pass, reclaim every tree no unresolved Invocation still owns.
    pub fn sweep_invocation_io(&self, store: &impl Store) -> Result<(), ExecError> {
        let root = crate::request_v2::invocation_io_root(&self.workspace_root);
        let keep: HashSet<PathBuf> = store
            .transact(|tx| tx.unresolved_invocations())?
            .iter()
            .map(|invocation| root.join(isolation::sanitize_key(&invocation.id.0)))
            .collect();
        isolation::sweep_orphans(&root, &keep);
        Ok(())
    }

    /// Record a launch refusal: the consumed request's Invocation is
    /// abandoned and `LaunchRefused` is reported — the request is
    /// terminal for its binding, never retried under a degraded setup.
    /// No process exists for this Invocation, so its scratch tree
    /// (fully built for a refused spawn, partial for a refused
    /// preparation), any check-workspace snapshot, and any v2
    /// input/output tree are reclaimed with the refusal.
    fn refuse_launch(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        invocation: &Invocation,
        reason: String,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        isolation::release(&self.launch_scratch(&invocation.id.0));
        isolation::release(&self.check_workspace(&invocation.id.0));
        self.release_invocation_io(&invocation.id.0);
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
        self.poll_invocations_scoped(
            store,
            now_unix_seconds,
            None,
            &mut || now_unix_seconds,
            None,
        )
    }

    /// Inspect live invocations for one scope only. The deadline is
    /// checked before each invocation is inspected: a poll that runs out
    /// of tick budget stops with the rest untouched for the next pass.
    pub fn poll_invocations_scoped(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let unresolved = store.transact(|tx| tx.unresolved_invocations())?;
        for invocation in unresolved {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope {
                let belongs = store.transact(|tx| {
                    Ok::<_, StoreError>(
                        tx.execution(&invocation.execution_id)?
                            .is_some_and(|e| &e.work_id == work_id),
                    )
                })?;
                if !belongs {
                    continue;
                }
            }
            let Some(identity) = invocation.process_identity.clone() else {
                // Prepared but never identified: the consume commit
                // landed but the launch commit did not, so the spawn
                // outcome is unrecorded. Resolve it — re-bind a found
                // orphaned writer, prove absence after the bounded
                // window, or keep the Execution uncertain rather than
                // starting a second writer.
                if invocation.state == InvocationState::Prepared
                    && let Some(request) =
                        store.transact(|tx| tx.request(&invocation.request_id))?
                {
                    self.resolve_unidentified_launch(
                        store,
                        &invocation,
                        &request,
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
                ProcessEvidence::Stopped { exit_code, .. } => {
                    // A persisted intent may name an escapee absent
                    // from every live snapshot — it is part of the
                    // proof set here too, not only in drive_stops.
                    let intent = store.transact(|tx| tx.stop_intent(&invocation.id))?;
                    match self.prove_tree_stopped(store, &invocation, intent.as_ref())? {
                        TreeProof::Proven => {
                            // The status `inspect` proved non-destructively
                            // is committed before the zombie is reaped:
                            // a failed finish leaves the child for the
                            // retry, which peeks the same status again.
                            self.finish_invocation(
                                store,
                                &invocation,
                                now_unix_seconds,
                                exit_code,
                            )?;
                            let _ = self.port.reap(identity.pid);
                        }
                        // Live stragglers were re-signaled; the exit fact
                        // is withheld until the whole tree proves stopped.
                        TreeProof::Withheld => {}
                        TreeProof::Uncertain => {
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
                                    process_identity: format!(
                                        "{}@{}",
                                        identity.pid, identity.started_at
                                    ),
                                    state: ProcessStateFact::Unknown,
                                    exit_code: None,
                                },
                                now_unix_seconds,
                            )?;
                        }
                    }
                }
                ProcessEvidence::Alive { .. } => {
                    self.refresh_descendants(store, &invocation)?;
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
                            exit_code: None,
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
                            exit_code: None,
                        },
                        now_unix_seconds,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Refresh the durable descendant snapshot while the leader is alive.
    /// Escapees are cheapest to record while they are still children; a
    /// failed or unchanged table read leaves the stored set alone.
    fn refresh_descendants(
        &self,
        store: &impl Store,
        invocation: &Invocation,
    ) -> Result<(), ExecError> {
        let Some(leader) = &invocation.process_identity else {
            return Ok(());
        };
        let Ok(mut snap) = self.port.snapshot_descendants(leader.pid) else {
            return Ok(());
        };
        snap.retain(|i| i.pid != leader.pid);
        // Union, never replace: an escapee that reparented out of the
        // ppid tree stays a recorded identity — leaving the live tree
        // is not proof it stopped.
        for recorded in &invocation.descendants {
            if recorded.pid != leader.pid && !snap.iter().any(|i| i.pid == recorded.pid) {
                snap.push(recorded.clone());
            }
        }
        snap.sort_by_key(|i| i.pid);
        let mut recorded = invocation.descendants.clone();
        recorded.sort_by_key(|i| i.pid);
        if recorded
            .iter()
            .zip(&snap)
            .all(|(a, b)| a.pid == b.pid && a.started_at == b.started_at)
            && recorded.len() == snap.len()
        {
            return Ok(());
        }
        let mut next = invocation.clone();
        next.descendants = snap;
        match store.transact(|tx| {
            tx.apply(&Effect::UpdateInvocation {
                invocation: next,
                expected_state: invocation.state.clone(),
            })
        }) {
            Ok(()) | Err(StoreError::ConstraintConflict { .. }) => Ok(()),
            Err(e) => Err(ExecError::Store(e)),
        }
    }

    /// Resolve an Invocation recorded `Prepared` with no process
    /// identity — the crash window between the consume commit and the
    /// launch commit where the spawn outcome was never written down.
    /// The orphaned writer, if it exists, still carries the recorded
    /// launch signature: scan the process table for the bound argv,
    /// exclude every identity the store already records, and either
    /// re-bind the only unrecorded match — command equal, cwd inside
    /// the launch directory, and no second same-signature candidate
    /// that could be the real writer instead — through the same
    /// durable commit a normal launch uses, or report the proven
    /// absence that lets `work` abandon the launch. Absence requires
    /// BOTH no unrecorded signature match (a chdir'd writer still
    /// matches the argv) AND the bounded uncertain window (a process
    /// inside its fork-exec visibility gap is never declared absent).
    /// Anything else keeps reporting `Unknown`: an ambiguous or
    /// unreadable answer is never a guess.
    fn resolve_unidentified_launch(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        request: &InvocationRequest,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let execution = store
            .transact(|tx| tx.execution(&invocation.execution_id))?
            .ok_or_else(|| {
                ExecError::Store(StoreError::Internal {
                    detail: "invocation without execution".into(),
                })
            })?;
        // The directory this launch ran in — the producing workspace
        // for a writer, the per-Invocation snapshot for the read-only
        // check — is the attribution evidence, never the match alone.
        let launch_dir = match request.role {
            InvocationRole::InvariantCheck => self.check_workspace(&invocation.id.0),
            _ => self.workspace_of(&request.work_id),
        };
        let canonical_dir =
            std::fs::canonicalize(&launch_dir).unwrap_or_else(|_| launch_dir.clone());
        // Every process identity the store already accounts for —
        // leaders, descendant snapshots, and stop-intent proofs across
        // all nonterminal executions — so a recorded writer is never
        // re-bound as this launch's orphan.
        let recorded = store.transact(|tx| recorded_process_identities(tx))?;
        // The bound argv is the recorded selection — the launch
        // signature an unrecorded writer can still be found by. A
        // request with no recorded binding has no signature to search:
        // nothing can be proven, so it stays uncertain.
        let Some(binding) = request.adapter_binding.as_ref() else {
            return self.report_unidentified_unknown(
                store,
                request,
                &invocation.id,
                now_unix_seconds,
            );
        };
        // The signature is the full command line the spawn actually
        // exec'd, which is the bare argv only for v1. A v2 launch
        // appended the `run --request/--out` transport pair — the
        // sandbox paths under containment, the host io paths
        // uncontained — and the durable record does not name which
        // gate the spawn took, so every shape the line could have
        // taken is a signature. Missing the v2 shapes would leave a
        // v2 writer unmatchable: never re-bound, and eventually
        // declared absent on directory occupancy alone. The bare argv
        // alone is never a v2 signature either — the spawn exec'd none
        // of it, so a foreign process running just those words must
        // neither adopt the launch nor hold absence open forever.
        let body = store.transact(|tx| tx.worker_request_body(&request.request_id))?;
        let mut signatures = Vec::new();
        if body.is_none() {
            signatures.push(binding.argv.join(" "));
        }
        let io_dir = crate::request_v2::invocation_io_root(&self.workspace_root)
            .join(crate::isolation::sanitize_key(&invocation.id.0));
        if body.is_some() {
            let mut contained = binding.argv.clone();
            contained.extend([
                "run".to_string(),
                "--request".to_string(),
                containment::SANDBOX_REQUEST.to_string(),
                "--out".to_string(),
                containment::SANDBOX_OUT.to_string(),
            ]);
            signatures.push(contained.join(" "));
            let mut host = binding.argv.clone();
            host.extend([
                "run".to_string(),
                "--request".into(),
                io_dir.join("request.json").to_string_lossy().into_owned(),
                "--out".into(),
                io_dir.join("out").to_string_lossy().into_owned(),
            ]);
            signatures.push(host.join(" "));
        }
        // A contained launch exec'd none of those lines — the spawn
        // ran the bwrap outer argv `bwrap_args` produced. Re-evaluate
        // the recorded binding's gate and rebuild the exact plan the
        // launch used: `Uncontained` means no wrapped argv could
        // exist; a refused gate or an unresolvable boundary means the
        // wrapped argv cannot be reconstructed — the orphan's
        // signature is then unprovable, so stay uncertain rather than
        // declare absence on an incomplete search.
        match self.containment_gate(&binding.argv, now_unix_seconds)? {
            Ok(Gate::Uncontained) => {}
            Err(_) => {
                return self.report_unidentified_unknown(
                    store,
                    request,
                    &invocation.id,
                    now_unix_seconds,
                );
            }
            Ok(Gate::Ready { primitive, worker }) => {
                let (inner_argv, workspace_read_only, mounts_io) = if let Some(body) = &body {
                    // A durable body that no longer parses is corrupt
                    // evidence, not a v1 request — the contained shape
                    // cannot be reconstructed either, so stay
                    // uncertain.
                    let Ok(wire) = WorkerRequestV2::parse(&body.canonical_body) else {
                        return self.report_unidentified_unknown(
                            store,
                            request,
                            &invocation.id,
                            now_unix_seconds,
                        );
                    };
                    let mut inner = binding.argv.clone();
                    inner.extend([
                        "run".to_string(),
                        "--request".to_string(),
                        containment::SANDBOX_REQUEST.to_string(),
                        "--out".to_string(),
                        containment::SANDBOX_OUT.to_string(),
                    ]);
                    (inner, wire.workspace.read_only, true)
                } else {
                    (binding.argv.clone(), false, false)
                };
                let plan = MountPlan {
                    workspace: Some((launch_dir.clone(), workspace_read_only)),
                    request: mounts_io.then(|| io_dir.join("request.json")),
                    input: mounts_io.then(|| io_dir.join("input")),
                    out: mounts_io.then(|| io_dir.join("out")),
                    gitconfig: Some(self.launch_scratch(&invocation.id.0).join("gitconfig")),
                    runtime_auth: None,
                    provider_config: None,
                    proxy_sock: None,
                    toolchain: None,
                    caches: vec![],
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                };
                match self.contained_exec(
                    &primitive,
                    plan,
                    &inner_argv,
                    "/workspace",
                    &WorkerEnv::default(),
                ) {
                    Ok((argv, _)) => signatures.push(argv.join(" ")),
                    Err(_) => {
                        return self.report_unidentified_unknown(
                            store,
                            request,
                            &invocation.id,
                            now_unix_seconds,
                        );
                    }
                }
            }
        }
        let mut matches = Vec::new();
        for signature in &signatures {
            match self.port.matching_processes(signature) {
                Ok(found) => matches.extend(found),
                // An unreadable process table proves nothing in either
                // direction — stay uncertain.
                Err(_) => {
                    return self.report_unidentified_unknown(
                        store,
                        request,
                        &invocation.id,
                        now_unix_seconds,
                    );
                }
            }
        }
        // Every signature match the store does not already account for.
        // A recorded writer — another work's live process, a snapshotted
        // descendant, a stop-proof identity — is never this launch's
        // orphan. An unrecorded match blocks absence *and* cannot be
        // bound unless its cwd also lands inside the launch directory:
        // a process that merely shares the argv (a foreign run, or the
        // writer after it chdir'd away) keeps the launch uncertain
        // rather than being guessed at.
        let unrecorded: Vec<&ProcessMatch> = matches
            .iter()
            .filter(|m| !recorded.contains(&(m.identity.pid, m.identity.started_at.clone())))
            .collect();
        // A candidate whose cwd cannot be read can be neither bound
        // nor ruled out: while one exists, no bind is provable — the
        // in-directory match might be a foreign process and the
        // unreadable one the real orphan. Stay uncertain rather than
        // adopt the wrong writer. (Absence is already blocked: the
        // candidate counts in `unrecorded` either way.)
        if unrecorded.iter().any(|m| m.cwd.is_none()) {
            return self.report_unidentified_unknown(
                store,
                request,
                &invocation.id,
                now_unix_seconds,
            );
        }
        // A bind is provable only when the candidate is the ONLY
        // unrecorded signature match AND its cwd lands inside the
        // launch directory. Any second same-signature process keeps
        // the launch uncertain: inside the directory the matches
        // cannot be told apart, and outside it the match might be the
        // real writer after a chdir — which makes the in-directory
        // lookalike a foreign process that must not be adopted,
        // inspected, or signaled as this launch's writer.
        match unrecorded.as_slice() {
            [child] if cwd_within(child.cwd.as_deref(), &canonical_dir) => {
                let child = *child;
                // Descendant coverage starts at re-bind, same as at a
                // normal launch; a failed table read is not a bind
                // failure — the stop path re-covers.
                let descendants = self
                    .port
                    .snapshot_descendants(child.identity.pid)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| d.pid != child.identity.pid)
                    .collect();
                let bound_request_id = export_request_id(&invocation.id);
                let bound_identity =
                    format!("{}@{}", child.identity.pid, child.identity.started_at);
                match store.transact(|tx| {
                    // Re-verify inside the commit: a concurrent
                    // resolution of another unidentified launch may
                    // have bound this same orphan since the scan — the
                    // snapshot the bind writes against is the one that
                    // must still show it unrecorded.
                    if recorded_process_identities(tx)?
                        .contains(&(child.identity.pid, child.identity.started_at.clone()))
                    {
                        return Ok(None);
                    }
                    let mut next = invocation.clone();
                    next.state = InvocationState::Running;
                    next.process_identity = Some(child.identity.clone());
                    next.descendants = descendants;
                    tx.apply(&Effect::UpdateInvocation {
                        invocation: next,
                        expected_state: invocation.state.clone(),
                    })?;
                    // A v1 launch binds its export row to the spawned
                    // handle. A v2 request has no export — its durable
                    // body is the launch record and the running
                    // Invocation carries the identity — so the bind
                    // applies only when the export row exists;
                    // requiring it would reject the whole commit.
                    if tx.worker_request_export(&bound_request_id)?.is_some() {
                        tx.apply(&Effect::BindWorkerRequestExport {
                            request_id: bound_request_id,
                            process_identity: bound_identity.clone(),
                        })?;
                    }
                    Ok(Some(()))
                }) {
                    Ok(Some(())) => self.report(
                        store,
                        request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: bound_identity,
                            state: ProcessStateFact::Running,
                            exit_code: None,
                        },
                        now_unix_seconds,
                    ),
                    // The orphan was bound by a concurrent resolution
                    // since the scan, or the world moved under the
                    // read (e.g. a stop settled first): the next pass
                    // re-evaluates from the committed state — never
                    // force the bind.
                    Ok(None) | Err(StoreError::ConstraintConflict { .. }) => Ok(()),
                    Err(e) => Err(ExecError::Store(e)),
                }
            }
            [] => {
                // Absence is proven only when NO unrecorded signature
                // match remains — a writer that left its launch
                // directory still matches the argv and keeps the launch
                // uncertain — AND the launch directory itself holds no
                // unexplained process: an escaped descendant keeps the
                // leader's cwd after leaving the argv signature, so the
                // signature scan alone cannot prove the writer's whole
                // tree is gone. The directory scan runs only once the
                // bounded window has elapsed — before that the answer
                // is Unknown regardless. The anchor is the Execution's
                // `uncertain_since`, set on the transition into
                // `uncertain` and never refreshed by repeat `Unknown`
                // reports; an unidentified Prepared is the only
                // unresolved Invocation that can hold its Execution
                // `ready`, so the anchor always postdates this launch's
                // commit — the measured window covers the whole
                // spawn-to-visible gap plus poll lag.
                let absence_proven = unrecorded.is_empty()
                    && execution.uncertain_since_unix_seconds.is_some_and(|since| {
                        now_unix_seconds - since >= UNIDENTIFIED_LAUNCH_ABSENT_SECONDS
                    })
                    && self.launch_dir_clear(&canonical_dir, &recorded);
                if absence_proven {
                    self.report(
                        store,
                        request,
                        ObservationFact::ProcessState {
                            invocation_id: invocation.id.clone(),
                            process_identity: String::new(),
                            state: ProcessStateFact::Exited,
                            exit_code: None,
                        },
                        now_unix_seconds,
                    )
                } else {
                    self.report_unidentified_unknown(
                        store,
                        request,
                        &invocation.id,
                        now_unix_seconds,
                    )
                }
            }
            // Anything else is ambiguous — more than one candidate,
            // or a single candidate outside the launch directory.
            // Never guess which process is this launch's writer.
            _ => self.report_unidentified_unknown(store, request, &invocation.id, now_unix_seconds),
        }
    }

    /// Whether the launch directory holds no process the store cannot
    /// account for. An escaped descendant keeps the leader's working
    /// directory after leaving the argv signature, so directory
    /// occupancy is the only remaining evidence that the vanished
    /// writer's tree is really gone. A failed OR INCOMPLETE
    /// enumeration — a live process whose cwd the port cannot map is
    /// a possible occupant — proves nothing: fail closed, stay
    /// uncertain.
    fn launch_dir_clear(&self, dir: &Path, recorded: &HashSet<(i32, String)>) -> bool {
        match self.port.processes_in_directory(dir) {
            Ok(occupants) => occupants
                .iter()
                .all(|m| recorded.contains(&(m.identity.pid, m.identity.started_at.clone()))),
            Err(_) => false,
        }
    }

    /// Report an unidentified launch as still unknown — the same
    /// process-state fact the pre-resolution path emitted.
    fn report_unidentified_unknown(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        invocation_id: &InvocationId,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        self.report(
            store,
            request,
            ObservationFact::ProcessState {
                invocation_id: invocation_id.clone(),
                process_identity: String::new(),
                state: ProcessStateFact::Unknown,
                exit_code: None,
            },
            now_unix_seconds,
        )
    }

    /// The identities whose stop must be proven before the exit fact:
    /// the stop intent's snapshot when one exists, else the recorded
    /// leader — always plus the persisted descendant snapshot and the
    /// leader's current process-group members (orphans reparent out of
    /// the ppid tree but keep the group). A failed group enumeration is
    /// an error, not an empty set, so a gap cannot pass as proof. A
    /// group whose owner pid was recycled by an unrelated process is
    /// attributed foreign and excluded — its members are never ours to
    /// inspect or signal.
    fn proof_identities(
        &self,
        invocation: &Invocation,
        intent: Option<&StopIntent>,
    ) -> Result<Vec<ProcessIdentity>, String> {
        let mut out: Vec<ProcessIdentity> =
            intent.map(|i| i.identities.clone()).unwrap_or_default();
        if out.is_empty() {
            out.extend(invocation.process_identity.iter().cloned());
        } else if let Some(leader) = &invocation.process_identity
            && !out.iter().any(|i| i.pid == leader.pid)
        {
            out.push(leader.clone());
        }
        for id in &invocation.descendants {
            if !out.iter().any(|i| i.pid == id.pid) {
                out.push(id.clone());
            }
        }
        if let Some(leader) = &invocation.process_identity
            && let Some(pgid) = leader.pgid
        {
            // pgid == the leader's pid (spawn sets process_group(0)), so
            // the owner slot attributes the group: a live process at
            // pgid whose start differs from the recorded leader's means
            // the number was recycled by an unrelated process — that
            // group is not ours to inspect or signal. An absent owner
            // is a straggler-held group of ours (or, at the documented
            // harness boundary, a recycled group whose own leader
            // already exited).
            let members = self.port.group_members(pgid)?;
            let foreign = members
                .iter()
                .any(|m| m.pid == pgid && m.started_at != leader.started_at);
            if !foreign {
                for id in members {
                    if !out.iter().any(|i| i.pid == id.pid) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Inspect every identity the exit fact depends on; live stragglers
    /// are re-signaled under re-verification. `Withheld` means at least
    /// one writer is still alive; `Uncertain` means a lookup failed.
    /// The first signal is always delivered behind a durable intent —
    /// the proof set itself is the record, so a crash after the signal
    /// still finds whom it targeted (this path runs on handoff and poll
    /// sweeps where no intent may exist yet).
    fn prove_tree_stopped(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        intent: Option<&StopIntent>,
    ) -> Result<TreeProof, ExecError> {
        let identities = match self.proof_identities(invocation, intent) {
            Ok(ids) => ids,
            Err(_) => return Ok(TreeProof::Uncertain),
        };
        let mut withheld = false;
        let mut uncertain = false;
        let mut intent_persisted = intent.is_some();
        for id in &identities {
            match self.port.inspect(id) {
                ProcessEvidence::Stopped { .. } => {}
                ProcessEvidence::Alive { identity } => {
                    withheld = true;
                    if !intent_persisted {
                        self.ensure_stop_intent(store, invocation, &identities)?;
                        intent_persisted = true;
                    }
                    self.port.signal_verified(&identity, libc::SIGKILL);
                }
                ProcessEvidence::Unknown { .. } => {
                    withheld = true;
                    uncertain = true;
                }
            }
        }
        Ok(if uncertain {
            TreeProof::Uncertain
        } else if withheld {
            TreeProof::Withheld
        } else {
            TreeProof::Proven
        })
    }

    /// Persist the proof set as the stop intent when none exists — the
    /// durable-intent-before-signal boundary for prove paths that find
    /// a live writer without an earlier `request_stop`.
    fn ensure_stop_intent(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        identities: &[ProcessIdentity],
    ) -> Result<(), ExecError> {
        store.transact(|tx| {
            if tx.stop_intent(&invocation.id)?.is_none() {
                let seq = tx.alloc_seq()?;
                tx.apply(&Effect::InsertStopIntent {
                    intent: StopIntent {
                        invocation_id: invocation.id.clone(),
                        identities: identities.to_vec(),
                        recorded_seq: seq,
                    },
                })?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Save every artifact fact before the exit fact resolves the writer.
    /// Interrupted reporting leaves the Invocation in the next scan.
    /// `exit_code` is the status `inspect` proved for the writer's own
    /// exit without consuming it — `None` when no normal exit status
    /// could be proven, which the exit fact records honestly rather
    /// than as a pass. Callers reap only after this commits, so a
    /// failed report is retried against the same zombie's status.
    fn finish_invocation(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        now_unix_seconds: i64,
        exit_code: Option<i32>,
    ) -> Result<(), ExecError> {
        // The writer is proven stopped before this is called: its
        // scratch tree (HOME/TMPDIR) and any check-workspace snapshot
        // can never be read again and are reclaimed with the
        // Invocation instead of accumulating under `.isolation`.
        isolation::release(&self.launch_scratch(&invocation.id.0));
        isolation::release(&self.check_workspace(&invocation.id.0));
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
        // The process tree is proven stopped before this runs, so the
        // Invocation-owned out directory is final: collect the v2
        // report evidence now, before the exit fact resolves the
        // writer — a crash after the exit commit could never retry the
        // collection. v1 requests collect nothing.
        if let Some(fact) = crate::report_v2::collect(
            store,
            self.body_store
                .as_ref()
                .or(self.v2.as_ref().map(|d| &d.bodies)),
            &self.workspace_root,
            invocation,
            &request,
            now_unix_seconds,
        )? {
            self.report(store, &request, fact, now_unix_seconds)?;
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
                exit_code,
            },
            now_unix_seconds,
        )?;
        Ok(())
    }

    /// Executions in `stopping` (and `uncertain` wanting stop) need the
    /// durable-intent → signal → inspect loop. A `requested` stop is never
    /// proof; only inspecting every recorded identity settles the state.
    pub fn drive_stops(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {
        self.drive_stops_scoped(
            store,
            now_unix_seconds,
            None,
            &mut || now_unix_seconds,
            None,
        )
    }

    /// Drive stop intents for one scope only. The deadline is checked
    /// before each Execution's stop is driven: signalling is new work,
    /// so a pass out of tick budget leaves the remaining Executions for
    /// the next pass.
    pub fn drive_stops_scoped(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let executions = store.transact(|tx| tx.nonterminal_executions())?;
        for execution in executions {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope
                && execution.work_id != *work_id
            {
                continue;
            }
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
                let intent = store.transact(|tx| tx.stop_intent(&invocation.id))?;
                let unproven = match self.proof_identities(invocation, intent.as_ref()) {
                    Ok(identities) => identities.iter().any(|id| {
                        !matches!(self.port.inspect(id), ProcessEvidence::Stopped { .. })
                    }),
                    // A gap in enumeration is inconclusive, not proof.
                    Err(_) => true,
                };
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
                            exit_code: None,
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
                                exit_code: None,
                            },
                            now_unix_seconds,
                        )?;
                    }
                    continue;
                };
                let intent = match store.transact(|tx| tx.stop_intent(&invocation.id))? {
                    Some(recorded) => Some(recorded),
                    None => match self.request_stop(store, &invocation, &identity)? {
                        // Re-read so the just-persisted snapshot is part
                        // of the proof below — it may hold an escapee
                        // neither the recorded descendants nor the live
                        // group still cover.
                        StopReceipt::Requested | StopReceipt::AlreadyStopped => {
                            store.transact(|tx| tx.stop_intent(&invocation.id))?
                        }
                        StopReceipt::Unknown { .. } => continue,
                    },
                };
                let identities = intent
                    .as_ref()
                    .map(|i| i.identities.clone())
                    .unwrap_or_default();
                // Stop proof = inspecting every recorded identity: the
                // intent's snapshot, the persisted descendant snapshot,
                // and the leader's current group members. A failed group
                // enumeration is inconclusive — never proof.
                let mut all_stopped = true;
                let mut any_unknown = false;
                // The leader's exit status as `inspect` proved it
                // non-destructively — committed by `finish_invocation`
                // before the zombie is reaped, so a failed write is
                // retried against the same status.
                let mut leader_exit = None;
                let proof = match self.proof_identities(&invocation, intent.as_ref()) {
                    Ok(ids) => ids,
                    Err(_) => {
                        all_stopped = false;
                        any_unknown = true;
                        identities
                    }
                };
                for id in &proof {
                    match self.port.inspect(id) {
                        ProcessEvidence::Stopped { exit_code, .. } => {
                            if id.pid == identity.pid {
                                leader_exit = exit_code;
                            }
                        }
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
                    // `finish_invocation` saves the artifact facts,
                    // reports the proven exit, and reclaims the launch
                    // scratch — the same completion the poll path runs.
                    // The poll path skips `Stopping` Executions, so a
                    // launch stopped by cancel/fail/pause reaches its
                    // exit fact only here; reporting it directly would
                    // leave `inv-*` scratch under `.isolation` forever.
                    self.finish_invocation(store, &invocation, now_unix_seconds, leader_exit)?;
                    // The leader is our child: reap it so a stopped
                    // writer does not linger as a zombie of this
                    // supervisor — only after the exit fact committed.
                    let _ = self.port.reap(identity.pid);
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
                            exit_code: None,
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
    pub fn run_recovery(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {
        self.run_recovery_scoped(
            store,
            now_unix_seconds,
            None,
            &mut || now_unix_seconds,
            None,
        )
    }

    /// Run one artifact-recovery attempt for one scope only. The
    /// deadline is checked before each recovery attempt starts — an
    /// attempt is new work, so a pass out of tick budget starts none.
    pub fn run_recovery_scoped(
        &self,
        store: &impl Store,
        _now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let executions = store.transact(|tx| tx.nonterminal_executions())?;
        for execution in executions {
            if deadline_reached(clock, deadline) {
                break;
            }
            if let Some(work_id) = scope
                && execution.work_id != *work_id
            {
                continue;
            }
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
            ProcessEvidence::Stopped { exit_code, .. } => {
                let intent = store.transact(|tx| tx.stop_intent(&invocation.id))?;
                match self.prove_tree_stopped(store, invocation, intent.as_ref())? {
                    TreeProof::Proven => {
                        // Commit the non-destructively proven status
                        // before reaping: a failed finish is retried
                        // against the same zombie's status.
                        self.finish_invocation(store, invocation, now_unix_seconds, exit_code)?;
                        if let Some(id) = &invocation.process_identity {
                            let _ = self.port.reap(id.pid);
                        }
                        Ok(())
                    }
                    TreeProof::Withheld => Ok(()),
                    TreeProof::Uncertain => {
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
                                exit_code: None,
                            },
                            now_unix_seconds,
                        )
                    }
                }
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
                        exit_code: None,
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
                        exit_code: None,
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
    match request.payload.get("required_artifacts") {
        // An explicitly declared set is authoritative — including the
        // empty set a read-only role records. Only an absent or
        // non-array declaration falls back to the writer default.
        Some(serde_json::Value::Array(items)) => items
            .iter()
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
            .collect(),
        _ => vec![RequiredArtifact {
            kind: "artifact.json".to_string(),
            path: "artifact.json".to_string(),
            expected_digest: None,
        }],
    }
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

/// Recursively copy `source` into `dest` for the invariant check's
/// snapshot workspace. Every entry is copied — the check must see the
/// producing state exactly as the writer left it, including hidden
/// stores like `.git` and `.artifact-store`. Symlinks are recreated as
/// links rather than followed, so a link inside the workspace cannot
/// pull outside content into the snapshot. Any unreadable entry fails
/// the whole copy: a partial snapshot is never a check input.
fn copy_workspace_tree(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_workspace_tree(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
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
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn expected")
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Err("process table unavailable".into())
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
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
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
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
                    contract_version: 1,
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
            descendants: vec![],
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

    /// A scripted group for the recycled-pgid boundary: members are
    /// returned verbatim and every signal is recorded so a test can
    /// assert foreign members are never signalled. `inspect` honours
    /// the identity check — a pid that lives under a different start
    /// time is a reuse, not the recorded process.
    struct ScriptedGroup {
        members: Vec<ProcessIdentity>,
        alive: Vec<ProcessIdentity>,
        signaled: std::sync::Mutex<Vec<i32>>,
    }

    impl ProcessControlPort for ScriptedGroup {
        fn spawn(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
        ) -> Result<ProcessIdentity, SpawnError> {
            panic!("no spawn expected")
        }
        fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            panic!("no snapshot expected")
        }
        fn group_members(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {
            Ok(self.members.clone())
        }
        fn inspect(&self, id: &ProcessIdentity) -> ProcessEvidence {
            match self
                .alive
                .iter()
                .find(|a| a.pid == id.pid && a.started_at == id.started_at)
            {
                Some(found) => ProcessEvidence::Alive {
                    identity: found.clone(),
                },
                None => ProcessEvidence::Stopped {
                    identity: id.clone(),
                    exit_code: Some(0),
                },
            }
        }
        fn signal_verified(&self, id: &ProcessIdentity, _: i32) -> SignalOutcome {
            self.signaled.lock().unwrap().push(id.pid);
            SignalOutcome::Sent
        }
        fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {
            SignalOutcome::Sent
        }
        fn reap(&self, _: i32) -> Option<i32> {
            None
        }
        fn run_probe(
            &self,
            _: &[String],
            _: &Path,
            _: &WorkerEnv,
            _: Duration,
        ) -> Result<ProbeOutcome, SpawnError> {
            panic!("no probe expected")
        }
    }

    fn scripted_config() -> workloop_core::work::WorkloopConfig {
        workloop_core::work::WorkloopConfig {
            host: workloop_core::work::HostTemplate {
                adapter: workloop_core::identity::AdapterId::trusted("fixture"),
                repository: workloop_core::identity::RepositoryRef::new(
                    workloop_core::identity::HostInstanceId::trusted("host-local"),
                    workloop_core::identity::RepositoryId::trusted("local-repo"),
                ),
                contract_version: 1,
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
        }
    }

    fn group_invocation(leader: &ProcessIdentity) -> Invocation {
        Invocation {
            id: InvocationId("inv-g".into()),
            request_id: "req-g".into(),
            execution_id: ExecutionId("e-g".into()),
            role: InvocationRole::Coding,
            state: InvocationState::Running,
            process_identity: Some(leader.clone()),
            required_artifacts: vec![],
            evidence_refs: vec![],
            descendants: vec![],
        }
    }

    /// After the leader is reaped, its pid can be recycled as the owner
    /// of a *different* group carrying the same pgid number. Those
    /// members are not ours: excluded from the proof and never
    /// signalled.
    #[test]
    fn a_recycled_group_owner_excludes_foreign_members() {
        let leader = ProcessIdentity {
            pid: 9,
            started_at: "leader start".into(),
            pgid: Some(9),
        };
        let foreign = vec![
            ProcessIdentity {
                pid: 9,
                started_at: "foreign start".into(),
                pgid: Some(9),
            },
            ProcessIdentity {
                pid: 700,
                started_at: "foreign start".into(),
                pgid: Some(9),
            },
        ];
        let sup = Supervisor::new(
            ScriptedGroup {
                members: foreign.clone(),
                alive: foreign,
                signaled: std::sync::Mutex::new(vec![]),
            },
            PathBuf::from("/unused"),
            vec![],
            scripted_config(),
        );
        let invocation = group_invocation(&leader);
        let store = workloop_store::sqlite::SqliteStore::in_memory(1).unwrap();
        assert_eq!(
            sup.prove_tree_stopped(&store, &invocation, None).unwrap(),
            TreeProof::Proven
        );
        assert!(
            sup.port.signaled.lock().unwrap().is_empty(),
            "a foreign group member must never be signalled"
        );
    }

    /// A group whose owner pid is absent is still ours — stragglers
    /// hold the pgid after the leader is reaped — so its members are
    /// inspected and re-signalled.
    #[test]
    fn an_ownerless_group_is_still_ours_to_signal() {
        let leader = ProcessIdentity {
            pid: 9,
            started_at: "leader start".into(),
            pgid: Some(9),
        };
        let straggler = ProcessIdentity {
            pid: 700,
            started_at: "straggler start".into(),
            pgid: Some(9),
        };
        let sup = Supervisor::new(
            ScriptedGroup {
                members: vec![straggler.clone()],
                alive: vec![straggler.clone()],
                signaled: std::sync::Mutex::new(vec![]),
            },
            PathBuf::from("/unused"),
            vec![],
            scripted_config(),
        );
        let invocation = group_invocation(&leader);
        let store = workloop_store::sqlite::SqliteStore::in_memory(1).unwrap();
        assert_eq!(
            sup.prove_tree_stopped(&store, &invocation, None).unwrap(),
            TreeProof::Withheld
        );
        assert_eq!(*sup.port.signaled.lock().unwrap(), vec![700]);
        // The signal was delivered behind a durable intent naming the
        // proof set — a crash after it still finds whom it targeted.
        let intent = store
            .transact(|tx| tx.stop_intent(&invocation.id))
            .unwrap()
            .expect("signalling without an intent is the bug this test guards");
        assert!(intent.identities.contains(&straggler));
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

    /// `ps` renders `lstart` in the process locale — under ja_JP a CJK
    /// weekday and an `M/D` date replace the ctime fields. The parser
    /// takes the numeric prefix and treats the whole remaining tail as
    /// the start string, so a localized row must never read back as an
    /// incomplete table row (#158).
    #[test]
    fn a_localized_lstart_row_still_parses() {
        // C-locale ctime row, including the space-padded day field.
        let (fields, lstart) = parse_table_row::<3>("  247     1   247 Sat Oct  4 06:14:00 2026")
            .unwrap()
            .expect("a full row parses");
        assert_eq!(fields, [247, 1, 247]);
        assert_eq!(lstart, "Sat Oct 4 06:14:00 2026");

        // The ja_JP row from the issue report: `水` (Wednesday) plus a
        // numeric `M/D` date — the row that produced
        // "incomplete process-table row" before the parse tolerated it.
        let (fields, lstart) = parse_table_row::<3>("  247     1   247 水 11/11 14:15:57 2026")
            .unwrap()
            .expect("a localized row parses");
        assert_eq!(fields, [247, 1, 247]);
        assert_eq!(lstart, "水 11/11 14:15:57 2026");

        // The two-prefix shape used by `group_members`, with the
        // space-padded day ja_JP also emits (`木 11/ 5`).
        let (fields, lstart) = parse_table_row::<2>("  247   247 木 11/ 5 06:14:00 2026")
            .unwrap()
            .expect("a localized group row parses");
        assert_eq!(fields, [247, 247]);
        assert_eq!(lstart, "木 11/ 5 06:14:00 2026");

        // Blank lines are skipped; every other malformed row still
        // aborts the table read.
        assert_eq!(parse_table_row::<3>("   ").unwrap(), None);
        assert_eq!(
            parse_table_row::<3>("  247     1").unwrap_err(),
            "incomplete process-table row:   247     1"
        );
        assert_eq!(
            parse_table_row::<3>("  247     1   247").unwrap_err(),
            "incomplete process-table row:   247     1   247"
        );
        assert_eq!(
            parse_table_row::<3>("  abc     1   247 Sat Oct 4 06:14:00 2026").unwrap_err(),
            "invalid process-table row:   abc     1   247 Sat Oct 4 06:14:00 2026"
        );
    }

    /// The bounded-probe signal plan applies the recorded-group owner
    /// check: a leader that re-verifies is ours to signal, an absent
    /// owner pid leaves only our own stragglers holding the group, and
    /// a pid that lives under a different start is recycled — signalling
    /// it would hit an unrelated process (#152).
    #[test]
    fn a_probe_timeout_signals_only_verified_identities() {
        let recorded = "Sat Oct 4 06:14:00 2026";
        assert_eq!(
            probe_signal_plan(1, recorded, &LstartLookup::Present(recorded.into())),
            ProbeSignalPlan::LeaderAndGroup
        );
        assert_eq!(
            probe_signal_plan(1, recorded, &LstartLookup::Absent),
            ProbeSignalPlan::GroupOnly
        );
        assert_eq!(
            probe_signal_plan(
                1,
                recorded,
                &LstartLookup::Present("Sun Nov 1 00:00:00 2026".into())
            ),
            ProbeSignalPlan::Nothing
        );
        // A failed lookup over an unallocated pid still reaches the
        // group — `kill(pid, 0)` is enough to prove no foreign owner is
        // heading a group under it.
        assert_eq!(
            probe_signal_plan(
                i32::MAX,
                recorded,
                &LstartLookup::Failed("ps unavailable".into())
            ),
            ProbeSignalPlan::GroupOnly
        );
        // A failed lookup over an occupied pid stays unverifiable — the
        // group is left alone rather than risk a recycled foreign owner.
        assert_eq!(
            probe_signal_plan(
                std::process::id() as i32,
                recorded,
                &LstartLookup::Failed("ps unavailable".into())
            ),
            ProbeSignalPlan::Nothing
        );
    }

    /// A probe that leaves a same-group straggler past the bound loses
    /// the whole group: the leader's pid is the group id by
    /// construction, so the straggler writes nothing after the bound.
    #[test]
    fn a_timed_out_probe_group_is_killed_with_its_leader() {
        let marker =
            std::env::temp_dir().join(format!("workloop-probe-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // The straggler writes the marker at ~1s if the group survives;
        // the leader keeps the bound wedged well past that point.
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("(sleep 1; touch \"{}\") & sleep 30", marker.display()),
        ];
        let outcome = PosixProcessControl
            .run_probe(
                &argv,
                Path::new("/tmp"),
                &WorkerEnv::default(),
                Duration::from_millis(50),
            )
            .expect("a spawned probe reports its outcome");
        assert_eq!(outcome, ProbeOutcome::Bounded);
        std::thread::sleep(Duration::from_millis(1600));
        assert!(
            !marker.exists(),
            "a same-group straggler must be killed with the probe leader"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// An empty mount plan for `contained_exec` boundary tests — the
    /// boundary fields start unset; the grant fills them.
    fn empty_mount_plan() -> MountPlan {
        MountPlan {
            workspace: None,
            request: None,
            input: None,
            out: None,
            gitconfig: None,
            runtime_auth: None,
            provider_config: None,
            proxy_sock: None,
            toolchain: None,
            caches: vec![],
            extra_binds: vec![],
            extra_rw_binds: vec![],
            worker: None,
        }
    }

    fn boundary_config(dir: &Path) -> crate::boundary::BoundaryConfig {
        crate::boundary::BoundaryConfig {
            credential_ref: None,
            provider_config: None,
            model_api_hosts: vec![],
            runtime_dir: dir.join("runtime"),
            egress_dir: dir.join("egress"),
            relay_exe: PathBuf::from("/bin/sh"),
        }
    }

    /// A boundary that cannot resolve its credential is a typed
    /// refusal carried as launch evidence — never an ambient
    /// credential and never an unwrapped launch.
    #[test]
    fn contained_exec_boundary_refusal_is_typed() {
        let dir =
            std::env::temp_dir().join(format!("taisan-boundary-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = boundary_config(&dir);
        cfg.credential_ref = Some("env:TAISAN_CONTAINED_EXEC_DEFINITELY_UNSET".into());
        let sup = Supervisor::new(
            crate::scripted::ScriptedProcessControl::new(),
            dir.clone(),
            vec![],
            scripted_config(),
        )
        .with_runtime_boundary(crate::boundary::RuntimeBoundary::new(cfg));
        let err = sup
            .contained_exec(
                Path::new("/opt/bwrap"),
                empty_mount_plan(),
                &["/bin/sh".into(), "-c".into(), "true".into()],
                "/tmp",
                &WorkerEnv::default(),
            )
            .unwrap_err();
        assert!(err.contains("credential.unresolvable"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resolved boundary becomes sandbox binds: the staged
    /// credential and provider config mount read-only, the proxy
    /// socket mounts and relay-wraps the inner argv, and the
    /// environment exports only the relay proxy URL — the secret
    /// value itself appears in neither argv nor env.
    #[test]
    fn contained_exec_grants_credential_config_and_relay() {
        // SAFETY: test-only env mutation; the name is unique to this
        // test so parallel tests cannot observe it.
        unsafe { std::env::set_var("TAISAN_CONTAINED_EXEC_TOKEN", "sekrit-test-value") };
        let dir =
            std::env::temp_dir().join(format!("taisan-boundary-grant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let provider_config = dir.join("provider.toml");
        std::fs::write(&provider_config, "model = \"demo\"\n").unwrap();
        let mut cfg = boundary_config(&dir);
        cfg.credential_ref = Some("env:TAISAN_CONTAINED_EXEC_TOKEN".into());
        cfg.provider_config = Some(provider_config.clone());
        cfg.model_api_hosts = vec!["model.example".into()];
        let sup = Supervisor::new(
            crate::scripted::ScriptedProcessControl::new(),
            dir.clone(),
            vec![],
            scripted_config(),
        )
        .with_runtime_boundary(crate::boundary::RuntimeBoundary::new(cfg));
        let (argv, env) = sup
            .contained_exec(
                Path::new("/opt/bwrap"),
                empty_mount_plan(),
                &["/bin/sh".into(), "-c".into(), "true".into()],
                "/tmp",
                &WorkerEnv::default(),
            )
            .unwrap();
        let joined = argv.join("\n");
        assert!(joined.contains("/taisan/runtime-auth"), "{joined}");
        assert!(joined.contains("/taisan/provider-config"), "{joined}");
        assert!(joined.contains("/taisan/proxy.sock"), "{joined}");
        assert!(joined.contains("__sandbox-relay"), "{joined}");
        assert!(
            env.vars
                .iter()
                .any(|(k, v)| k == "HTTPS_PROXY" && v == "http://127.0.0.1:18080"),
            "{:?}",
            env.vars
        );
        assert!(
            !joined.contains("sekrit-test-value")
                && !env
                    .vars
                    .iter()
                    .any(|(_, v)| v.contains("sekrit-test-value")),
            "the credential value never enters argv or env"
        );
        unsafe { std::env::remove_var("TAISAN_CONTAINED_EXEC_TOKEN") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No boundary installed is the v1-compatible empty boundary:
    /// contained argv carries no credential, provider config, proxy,
    /// or relay — and no proxy variables.
    #[test]
    fn contained_exec_without_boundary_binds_nothing_extra() {
        let dir =
            std::env::temp_dir().join(format!("taisan-boundary-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sup = Supervisor::new(
            crate::scripted::ScriptedProcessControl::new(),
            dir.clone(),
            vec![],
            scripted_config(),
        );
        let (argv, env) = sup
            .contained_exec(
                Path::new("/opt/bwrap"),
                empty_mount_plan(),
                &["/bin/sh".into(), "-c".into(), "true".into()],
                "/tmp",
                &WorkerEnv::default(),
            )
            .unwrap();
        let joined = argv.join("\n");
        for absent in [
            "runtime-auth",
            "provider-config",
            "proxy.sock",
            "__sandbox-relay",
        ] {
            assert!(!joined.contains(absent), "{joined}");
        }
        assert!(
            !env.vars
                .iter()
                .any(|(k, _)| k.contains("PROXY") || k.contains("proxy")),
            "{:?}",
            env.vars
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
