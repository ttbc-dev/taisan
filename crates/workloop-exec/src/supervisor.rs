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

/// Consecutive incomplete absence scans an unidentified launch
/// tolerates before the unprovable absence is surfaced to the
/// operator. The bound decides only WHEN the operator is told, never
/// what is concluded — the launch stays an unresolved writer either
/// way — and a completed scan (clear or occupied) restarts the
/// count: one transient unmappable process should not converge the
/// outcome, but a scan that can never complete must not hold
/// `uncertain` forever either.
const UNIDENTIFIED_LAUNCH_SCAN_LIMIT: u32 = 3;

/// The verdict one absence check returns for an unidentified launch.
/// `Clear` — the enumeration completed and nothing unexplained
/// remains. `Occupied` — the enumeration completed and a live
/// unrecorded process was found; absence is disproven for this pass.
/// `Incomplete` — the enumeration itself could not be completed or a
/// member could not be verified; absence is unprovable for this pass
/// and the carried reason feeds the bounded escalation's record.
#[derive(Debug)]
enum Clearance {
    Clear,
    Occupied,
    Incomplete(String),
}

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

/// The working directory `pid` runs in: `lsof` first, then the
/// `/proc/<pid>/cwd` symlink where that filesystem exists. `None` on
/// any failure — a denied read, a dead process, or no mechanism at
/// all — never a guessed path. Only ever used to *attribute* a
/// candidate; `None` makes a child less attributable, never
/// absent-proof.
fn cwd_of(pid: i32) -> Option<PathBuf> {
    if let Some(cwd) = lsof_cwd_of(pid) {
        return Some(strip_deleted_marker(&cwd).0);
    }
    proc_cwd_of(pid).map(|cwd| strip_deleted_marker(&cwd).0)
}

fn lsof_cwd_of(pid: i32) -> Option<PathBuf> {
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

/// `/proc/<pid>/cwd` is the same answer `lsof -d cwd` gives, without
/// the tool — the fallback for minimal images that do not ship lsof.
/// The raw link target is returned; a deleted working directory's
/// ` (deleted)` marker is stripped by `cwd_of`.
#[cfg(target_os = "linux")]
fn proc_cwd_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(target_os = "linux"))]
fn proc_cwd_of(_: i32) -> Option<PathBuf> {
    None
}

/// `/proc/<pid>/cwd` — and lsof's `n` field for the same table —
/// annotates a deleted working directory's path with a trailing
/// ` (deleted)`; the marker is not part of the path. Judged raw it
/// never matches component-wise: `dir/gone (deleted)` is not inside
/// `dir`, so a live writer in a deleted directory would slip every
/// membership check — and because its cwd read succeeded it is
/// already `mapped`, sliding past the completeness gate into a
/// clear table while an occupant still runs. The marker is also
/// positive evidence: the mechanism reported the exact directory
/// the process WAS in. Returns the pre-deletion spelling and
/// whether the marker was present, so callers can count a marked
/// path inside `dir` as an occupant outright — never a silent
/// skip. The marker compares as bytes so a non-UTF-8 spelling is
/// handled the same way.
pub(crate) fn strip_deleted_marker(cwd: &Path) -> (PathBuf, bool) {
    use std::os::unix::ffi::OsStrExt;
    const MARKER: &[u8] = b" (deleted)";
    match cwd.as_os_str().as_bytes().strip_suffix(MARKER) {
        Some(spelling) => (PathBuf::from(std::ffi::OsStr::from_bytes(spelling)), true),
        None => (cwd.to_path_buf(), false),
    }
}

/// Whether a child's reported cwd is the launch directory or inside it.
/// Both sides are canonicalized so symlinked temp roots and `.`/`..`
/// spellings compare equal; an unresolvable cwd is never a match. A
/// ` (deleted)` marker is stripped first — membership judges the
/// pre-deletion spelling.
pub(crate) fn cwd_within(cwd: Option<&Path>, dir: &Path) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let (cwd, _) = strip_deleted_marker(cwd);
    std::fs::canonicalize(&cwd).is_ok_and(|canon| canon.starts_with(dir))
}

/// Whether a reported cwd places its process inside the launch scope:
/// within `dir`, or under `SANDBOX_WORKSPACE` — a contained worker's
/// launch workspace is bind-mounted at that root, so its
/// `/proc/<pid>/cwd` resolves to the sandbox spelling rather than the
/// host launch directory. Launch-scope call sites only — the bind
/// judgment and the absence proof's scope union: the
/// `processes_in_directory` contract answers "cwd inside `dir`" alone,
/// and widening that answer would report the sandbox's occupants to
/// every scanned directory. A ` (deleted)` marker is stripped so a
/// deleted mount spelling still reads as the sandbox workspace.
pub(crate) fn cwd_in_launch_scope(cwd: Option<&Path>, dir: &Path) -> bool {
    cwd_within(cwd, dir)
        || cwd
            .map(|cwd| strip_deleted_marker(cwd).0)
            .is_some_and(|cwd| cwd.starts_with(containment::SANDBOX_WORKSPACE))
}

/// Every process one scan mechanism could map to a working directory:
/// `mapped` is the pid set the completeness gate checks, `cwds` the
/// pid → cwd pairs the occupant filter reads.
struct CwdScan {
    mapped: HashSet<i32>,
    cwds: Vec<(i32, PathBuf)>,
}

/// One `lsof -d cwd` pass maps every inspectable pid to its working
/// directory — per-pid lookups would not scale to a system-wide
/// absence proof. Field output (`p`/`n` pairs) is parsed positionally:
/// a process lsof cannot inspect simply has no `n` line and cannot be
/// placed inside the scanned directory. The `n` text is kept verbatim:
/// lsof appends ` (deleted)` to a deleted directory's name just as
/// `/proc` does, and the occupant filter reads that marker.
fn lsof_cwds() -> Result<CwdScan, String> {
    let out = Command::new("lsof")
        .args(["-d", "cwd", "-F", "pn"])
        .output()
        .map_err(|e| format!("lsof spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "lsof failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut mapped = HashSet::new();
    let mut cwds = Vec::new();
    let mut pid: Option<i32> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.trim().parse().ok();
        } else if let Some(path) = line.strip_prefix('n')
            && let Some(pid) = pid
        {
            mapped.insert(pid);
            cwds.push((pid, PathBuf::from(path)));
        }
    }
    Ok(CwdScan { mapped, cwds })
}

/// The same table read straight out of `/proc`: each numeric entry's
/// `cwd` symlink. A process whose link cannot be read — it exited, is
/// a zombie, or belongs to a user this one cannot inspect — is simply
/// unmapped, exactly as under lsof; the completeness gate decides
/// whether that is a gap. Rows keep the raw link target: a deleted
/// working directory's ` (deleted)` marker is evidence the occupant
/// filter reads, not noise to discard here.
#[cfg(target_os = "linux")]
fn proc_cwds() -> Result<CwdScan, String> {
    let entries = std::fs::read_dir("/proc").map_err(|e| format!("/proc scan failed: {e}"))?;
    let mut mapped = HashSet::new();
    let mut cwds = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) {
            mapped.insert(pid);
            cwds.push((pid, cwd));
        }
    }
    Ok(CwdScan { mapped, cwds })
}

/// Non-Linux platforms have no `/proc`; without lsof the scan cannot
/// enumerate at all and must fail closed.
#[cfg(not(target_os = "linux"))]
fn proc_cwds() -> Result<CwdScan, String> {
    Err("no /proc filesystem on this platform".into())
}

/// The working-directory table the absence proof enumerates over.
/// `lsof` is the primary mechanism; when it cannot produce a table —
/// absent from the image, or a failed run — the fallback enumerates
/// `/proc/<pid>/cwd` directly, the same table lsof reports on Linux.
/// `Err` means neither mechanism could enumerate — never a silent
/// empty table the absence proof could read as "no occupants".
fn directory_cwds() -> Result<CwdScan, String> {
    with_proc_fallback(lsof_cwds())
}

/// Mechanism selection, split from `directory_cwds` so a test can
/// drive the lsof-failure arm without shadowing PATH.
fn with_proc_fallback(lsof: Result<CwdScan, String>) -> Result<CwdScan, String> {
    match lsof {
        Err(lsof_err) => proc_cwds()
            .map_err(|proc_err| format!("directory process scan failed: {lsof_err}; {proc_err}")),
        ok => ok,
    }
}

/// `PF_KTHREAD` — field 9 of `/proc/<pid>/stat` — marks a kernel
/// thread in every PID namespace. The field is counted after `comm`,
/// whose parenthesized text may itself contain spaces and `)`, so
/// parsing resumes after the LAST `)`.
#[cfg(target_os = "linux")]
const PF_KTHREAD: u64 = 0x0020_0000;

#[cfg(target_os = "linux")]
fn proc_stat_flags(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(6)?.parse().ok()
}

/// Whether `pid` is a kernel thread — the only kind of live process
/// that can never hold a working directory, so its unmappable cwd is
/// no enumeration gap. Without the exclusion a scan running as root
/// could never complete: kernel threads share the euid and never map
/// a cwd under any mechanism. On Linux the marker is `PF_KTHREAD`,
/// which holds in every PID namespace — the pid-2 descent test does
/// not: inside a container namespace pid 2 is ordinary userspace, and
/// exempting it would let a live writer slip the completeness gate.
/// An unreadable stat is not proof of kernel-ness — the process stays
/// a gap and the scan fails closed. On macOS the kernel enters the
/// process table as `kernel_task`, pid 0.
#[cfg(target_os = "linux")]
fn is_kernel_thread(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| proc_stat_flags(&stat))
        .is_some_and(|flags| flags & PF_KTHREAD != 0)
}

#[cfg(not(target_os = "linux"))]
fn is_kernel_thread(pid: i32) -> bool {
    pid == 0
}

/// The completeness gate over one scan's `mapped` set. Every live
/// same-user pid the mechanism left unmapped is a gap in the proof —
/// its cwd might be inside the scanned directory — UNLESS the pid
/// belongs to a class that provably cannot possess a cwd at all. The
/// exclusion predicate `cannot_hold_cwd` is an independent structural
/// property of the process — the kernel-thread flag — never "the
/// scan could not map it", which would be circular: any other
/// ambiguous process remains the gap it must be. A pid absent from
/// the table, one whose stat cannot be re-read, or a zombie is
/// skipped — none can hold a live cwd to write with.
fn unmappable_cwd_gap(
    same_user_pids: &[i32],
    mapped: &HashSet<i32>,
    present: impl Fn(i32) -> LstartLookup,
    stat: impl Fn(i32) -> Option<String>,
    cannot_hold_cwd: impl Fn(i32) -> bool,
) -> Result<(), String> {
    for pid in same_user_pids {
        if mapped.contains(pid) {
            continue;
        }
        match present(*pid) {
            // Exited between the pid-list and cwd reads — not a
            // gap, just a process that was already gone.
            LstartLookup::Absent => continue,
            LstartLookup::Failed(reason) => return Err(reason),
            LstartLookup::Present(_) => {}
        }
        // Gone between the liveness check and the stat read, or a
        // zombie — either way it cannot write inside the dir.
        let Some(stat) = stat(*pid) else {
            continue;
        };
        if stat.contains('Z') {
            continue;
        }
        if cannot_hold_cwd(*pid) {
            continue;
        }
        return Err(format!(
            "directory scan incomplete: live process {pid} has no readable cwd"
        ));
    }
    Ok(())
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

/// How the directory-occupant scan treats a reported cwd that reads
/// inside `dir` but the host cannot resolve — a mount-namespace
/// spelling (the sandbox workspace root) with no host inode behind
/// it. A ` (deleted)`-marked cwd is judged before this split: the
/// marker reports the exact directory the process was in, so a
/// marked spelling inside `dir` is a known occupant under either
/// contract, never an ambiguous spelling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnresolvedOccupant {
    /// `processes_in_directory`: the path cannot be proven outside
    /// `dir`, so the enumeration is incomplete and the whole read
    /// fails — never a truncated proof set.
    FailClosed,
    /// `processes_in_namespace_dir`: under a namespace spelling of
    /// `dir` the reported path IS the occupancy evidence, so the
    /// process is returned with its literal cwd — data the caller's
    /// recorded-occupant filter judges per pid.
    Report,
}

/// The working-directory occupant scan behind both directory
/// queries. `processes_in_directory` and
/// `processes_in_namespace_dir` share the process table, the
/// completeness gate, and the canonical containment judgment; they
/// differ only at the unresolvable-spelling edge
/// (`UnresolvedOccupant`).
fn directory_occupants(
    dir: &Path,
    unresolved: UnresolvedOccupant,
) -> Result<Vec<ProcessMatch>, String> {
    // Enumerate this user's processes before the cwd pass: the
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
        // `ps` prints some kernel/nobody uids as negatives on macOS
        // (nobody is -2); a signed parse keeps those legitimate
        // rows and they can never equal the non-negative euid, so
        // the comparison stays fail-closed.
        let uid: i64 = uid
            .parse()
            .map_err(|_| format!("invalid process-table row: {line}"))?;
        if uid == euid as i64 {
            same_user_pids.push(pid);
        }
    }
    // One pass maps every inspectable pid to its working directory
    // — per-pid lookups would not scale to a system-wide absence
    // proof. `lsof` is the primary mechanism and `/proc` the Linux
    // fallback (`directory_cwds`); a process neither could map
    // simply has no cwd row and the completeness gate decides
    // whether that is a gap.
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let scan = directory_cwds()?;
    // Completeness gate before any occupant answer: a live
    // same-user process the scan could not map to a cwd makes the
    // whole enumeration partial — its cwd might be inside `dir`,
    // so the table can never authorize absence. The only exclusion
    // is `is_kernel_thread` — a class that provably cannot hold a
    // cwd (`unmappable_cwd_gap`).
    unmappable_cwd_gap(
        &same_user_pids,
        &scan.mapped,
        lstart_of,
        stat_of,
        is_kernel_thread,
    )?;
    let mut out = Vec::new();
    for (pid, cwd) in scan.cwds {
        // `cwd_within` canonicalizes the reported path and treats
        // an unresolvable cwd as "not inside" — but a reported
        // path spelled inside `dir` cannot be proven outside it
        // either (a mount-namespace spelling no host inode backs).
        // The pid is already mapped, so skipping it here would slip
        // it past the completeness gate: the genuine scan fails
        // closed, the namespace query reports the literal
        // spelling as an occupant the caller judges per pid. A
        // ` (deleted)` marker is not ambiguous that way — it is
        // the mechanism reporting the exact directory the process
        // WAS in, so a marked spelling inside `dir` is a known
        // occupant under either contract; judged raw it would
        // never match and the pid would silently skip into a
        // clear table, the forbidden outcome.
        let (cwd, deleted) = strip_deleted_marker(&cwd);
        let marked_inside = deleted && cwd.starts_with(&dir);
        let unresolvable_inside =
            !deleted && std::fs::canonicalize(&cwd).is_err() && cwd.starts_with(&dir);
        if unresolvable_inside && unresolved == UnresolvedOccupant::FailClosed {
            return Err(format!(
                "directory scan incomplete: process {pid} cwd {} is not resolvable",
                cwd.display()
            ));
        }
        if !marked_inside && !unresolvable_inside && !cwd_within(Some(&cwd), &dir) {
            continue;
        }
        // Same re-verification as the signature scan: a pid that
        // exited between the cwd pass and now drops out, and a
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
                    // The fork produced a process and `process_group(0)`
                    // put it in its own group: the group id survives the
                    // lost identity and may still hold live descendants.
                    pgid: Some(pid),
                });
            }
            LstartLookup::Failed(reason) => {
                return Err(SpawnError::Unknown {
                    reason,
                    pgid: Some(pid),
                });
            }
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
        directory_occupants(dir, UnresolvedOccupant::FailClosed)
    }

    fn processes_in_namespace_dir(&self, dir: &Path) -> Result<Vec<ProcessMatch>, String> {
        directory_occupants(dir, UnresolvedOccupant::Report)
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
                return Err(SpawnError::Unknown {
                    reason,
                    pgid: Some(pid),
                });
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
                    pgid: Some(pid),
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
                        pgid: Some(pid),
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
    /// Installation-owned roots a contained launch masks with a fresh
    /// tmpfs when a fixed system bind would otherwise expose them —
    /// the state root holding the store and the workspace root
    /// holding workspaces, the handoff repository, and credential
    /// staging (`docs/v0.3.md` §14.3). Defaults to the workspace
    /// root; the composition root adds the state root.
    pub sandbox_mask_roots: Vec<PathBuf>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Consecutive incomplete absence scans per unidentified
    /// Invocation — the bounded-escalation streak. In-memory on
    /// purpose: "consecutive" is scoped to one supervisor's lifetime,
    /// and an entry is dropped the moment a scan completes or the
    /// Invocation resolves.
    incomplete_scans: Mutex<HashMap<String, u32>>,
}

impl<P: ProcessControlPort> Supervisor<P> {
    pub fn new(
        port: P,
        workspace_root: PathBuf,
        worker_argv: Vec<String>,
        config: workloop_core::work::WorkloopConfig,
    ) -> Self {
        let sandbox_mask_roots = vec![workspace_root.clone()];
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
            sandbox_mask_roots,
            locks: Mutex::new(HashMap::new()),
            incomplete_scans: Mutex::new(HashMap::new()),
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

    /// Installation-owned roots a contained launch masks when a fixed
    /// system bind would expose them — the composition root passes
    /// the state root beside the workspace root the default names.
    pub fn with_sandbox_mask_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.sandbox_mask_roots = roots;
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
            if let Some(source) = &grant.credential_source {
                plan.masked_files.push(source.clone());
            }
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
                    ca_bundles: containment::ca_trust_binds(&self.isolation_profile),
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: self.sandbox_mask_roots.clone(),
                    masked_files: vec![],
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
        produced: Option<&str>,
    ) -> Result<PathBuf, ExecError> {
        let source = self.workspace_of(work_id);
        let dest = self.check_workspace(&invocation.id.0);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        // The v2 check's evidence binds the artifact's produced
        // commit — in a git-backed world the snapshot must be that
        // tree, not whatever uncommitted state the live producing
        // workspace happens to hold when this launch finally runs.
        // The snapshot is cloned, never copied: a local clone writes
        // a fresh `.git` with none of the worker-writable source's
        // hooks, filter commands, or `core.worktree` redirect, so a
        // planted `post-checkout` hook or smudge filter can never
        // execute supervisor-side. The `.git` entry must be a real
        // directory: the worker has write access to its workspace and
        // could plant a symlink whose target the supervisor's clone
        // would follow — `is_dir()` follows links, so lstat the
        // entry and refuse anything else (a staged workspace is a
        // real `git init` repository, never a gitfile). Git runs
        // through `workspace::git`'s scrubbed environment — the same
        // hardened settings workspace preparation uses. A commit no
        // repository carries refuses through the same path as a
        // snapshot that could not be taken. A v2 check pins its
        // evidence to a produced commit and must never run against
        // the uncommitted live tree: when the workspace repository
        // is gone — or a successor request re-prepared it at a new
        // base so its object store no longer carries the produced
        // commit — the durable handoff repository (the artifact's own
        // object store, not a worker-writable surface) supplies the
        // commit instead; only when no repository carries it does the
        // launch refuse. The raw copy is the fallback only for a v1
        // check, which pins no commit.
        let real_git_dir = match std::fs::symlink_metadata(source.join(".git")) {
            Ok(meta) if meta.file_type().is_dir() => true,
            Ok(_) => {
                return Err(ExecError::Fault(
                    "workspace .git is not a real directory".into(),
                ));
            }
            Err(_) => false,
        };
        if let Some(produced) = produced {
            let clone_source = if real_git_dir
                && crate::workspace::git(&source, &["cat-file", "-e", produced]).is_ok()
            {
                Some(source.clone())
            } else {
                self.handoff.as_ref().and_then(|layout| {
                    crate::workspace::git(&layout.repository, &["cat-file", "-e", produced])
                        .ok()
                        .map(|_| layout.repository.clone())
                })
            };
            let Some(clone_source) = clone_source else {
                return Err(ExecError::Fault(
                    "check snapshot: no repository carries the produced commit".into(),
                ));
            };
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let snapshot_git = |dir: &Path, args: &[&str]| {
                crate::workspace::git(dir, args).map_err(|e| {
                    ExecError::Fault(format!("check snapshot git {}: {e}", args.join(" ")))
                })
            };
            snapshot_git(
                &clone_source,
                &[
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    clone_source.to_string_lossy().as_ref(),
                    &dest.to_string_lossy(),
                ],
            )?;
            snapshot_git(&dest, &["checkout", "--quiet", "-f", "--detach", produced])?;
        } else {
            copy_workspace_tree(&source, &dest)?;
        }
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
        self.collect_deferred_reports_scoped(store, now_unix_seconds, scope, clock, deadline)?;
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
            let Some(request) = tx.request(&invocation.request_id)? else {
                return Ok(None);
            };
            // Verification reads the spec revision the request was
            // bound at — a later approved revision moves `work.spec`
            // and must never silently re-bind an in-flight launch's
            // verification commands.
            let Some(spec) = tx.spec(&work.id, Some(request.spec_revision))? else {
                return Ok(None);
            };
            // The verification argv, egress hosts, and workspace fork
            // point a v2 dispatch runs are the immutable request
            // body's — the bound spec the Worker was launched against,
            // never the Work's current spec revision, which a re-edit
            // could move mid-flight and silently re-bind verification
            // to commands the dispatch never approved. A v1 dispatch
            // or an absent body declares neither, and keeps the
            // recorded spec argv it always had. An unverifiable body
            // is corrupt durable state — a fault, never a silently
            // empty grant or an unpinned base.
            let (verification_argv, verification_egress_hosts, pinned_base, branch_start) =
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
                        // The branch start a no-op check measures
                        // against: a Fix workspace branches at its
                        // predecessor's produced commit, every other
                        // role at the request's pinned base.
                        let start = match &req.task {
                            workloop_core::worker_v2::TaskBody::Fix { predecessor, .. } => {
                                predecessor.produced.clone()
                            }
                            _ => req.base.object.clone(),
                        };
                        (
                            req.verification_argv().to_vec(),
                            req.required_capabilities.verification_egress_hosts,
                            req.base.object,
                            start,
                        )
                    }
                    None => (
                        spec.payload.verification_argv.clone(),
                        Vec::new(),
                        String::new(),
                        String::new(),
                    ),
                };
            Ok(Some((
                invocation.clone(),
                work.id.clone(),
                verification_argv,
                verification_egress_hosts,
                pinned_base,
                branch_start,
                execution.budget.artifact_recovery_attempts,
            )))
        })?;
        let Some((
            invocation,
            work_id,
            verification_argv,
            verification_egress_hosts,
            pinned_base,
            branch_start,
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
                branch_start,
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
    /// `absent` — or carrying no claim — records no verdict but the
    /// bare `HandoffAcknowledged` fact instead, so publication is
    /// still re-driven: claims are evidence for later gates, never
    /// authority over the artifact, and a `false` verdict never moves
    /// or replaces it.
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
            return self.report_handoff_acknowledged(
                store,
                invocation,
                artifact_id,
                &produced,
                now_unix_seconds,
            );
        };
        // Only a validated envelope's claim is read — `invalid`
        // retains raw bytes but never a trusted claim, and `absent`
        // has none.
        if matches!(
            classification,
            ReportClassification::Invalid { .. } | ReportClassification::Absent
        ) {
            return self.report_handoff_acknowledged(
                store,
                invocation,
                artifact_id,
                &produced,
                now_unix_seconds,
            );
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
            // no verdict and never a pass-aborting fault, but the
            // acknowledgement still re-drives publication.
            Ok(None) | Err(BodyError::Unavailable { .. }) => {
                return self.report_handoff_acknowledged(
                    store,
                    invocation,
                    artifact_id,
                    &produced,
                    now_unix_seconds,
                );
            }
            Err(e) => {
                return Err(ExecError::Fault(format!("resolve worker report body: {e}")));
            }
        };
        let report: WorkerReportV2 = serde_json::from_slice(&resolved.bytes)
            .map_err(|e| ExecError::Fault(format!("retained worker report does not parse: {e}")))?;
        let Some(claimed) = report.claimed_head else {
            return self.report_handoff_acknowledged(
                store,
                invocation,
                artifact_id,
                &produced,
                now_unix_seconds,
            );
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

    /// The acknowledgement itself, without a claim verdict: emitted
    /// when `record_claim_check` reaches a durable `produced` commit
    /// but cannot form a `ClaimedHeadCheck` — no retained report, an
    /// `absent`/`invalid` classification, unavailable report bytes, or
    /// no `claimed_head`. The fact re-drives `maybe_publish` for the
    /// waiting Execution the same way the verdict does; without it
    /// the publish chain would sit until the stage budget expires.
    fn report_handoff_acknowledged(
        &self,
        store: &impl Store,
        invocation: &Invocation,
        artifact_id: &str,
        produced: &str,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let Some(request) = store.transact(|tx| tx.request(&invocation.request_id))? else {
            return Ok(());
        };
        self.report(
            store,
            &request,
            ObservationFact::HandoffAcknowledged {
                artifact_id: artifact_id.to_string(),
                produced: produced.to_string(),
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
                match self.snapshot_check_workspace(&invocation, &work_id, None) {
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
                    // Assessment roles read the produced state; only a
                    // writer mounts the workspace read-write (§14.6).
                    workspace: Some((
                        workspace.clone(),
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
                    ca_bundles: containment::ca_trust_binds(&self.isolation_profile),
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: self.sandbox_mask_roots.clone(),
                    masked_files: vec![],
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
                // Commit the launch's process group the moment the
                // spawn call returns, before the running commit: when
                // the leader's identity record is the one that is
                // lost, the group stays the durable handle the absence
                // proof enumerates for group-keeping escapes. The
                // leader's start is the owner identity the group's
                // recycled-pgid check verifies against.
                if let Some(pgid) = identity.pgid {
                    self.report(
                        store,
                        request,
                        ObservationFact::LaunchGroup {
                            invocation_id: invocation.id.clone(),
                            pgid,
                            leader_started_at: Some(identity.started_at.clone()),
                        },
                        now_unix_seconds,
                    )?;
                }
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
            Err(SpawnError::Unknown { pgid, .. }) => {
                // Same durability order as the `Ok` arm: the group the
                // spawn produced is committed before the unknown-state
                // fact, so the unidentified launch's absence proof can
                // still enumerate it. The leader's start is the owner
                // identity the recycled-pgid check verifies against —
                // captured here from the group itself, the one read of
                // the owner slot the failed identity capture still
                // allows.
                if let Some(pgid) = pgid {
                    self.report(
                        store,
                        request,
                        ObservationFact::LaunchGroup {
                            invocation_id: invocation.id.clone(),
                            pgid,
                            leader_started_at: self.launch_owner_start(pgid),
                        },
                        now_unix_seconds,
                    )?;
                }
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
                // The recorded Assessment binds `artifact.produced`;
                // the snapshot must be that commit's tree — a
                // role/task mismatch is impossible, the builder
                // proved the wire before consume.
                let produced = match &wire.task {
                    workloop_core::worker_v2::TaskBody::InvariantCheck { artifact, .. } => {
                        artifact.produced.as_str()
                    }
                    _ => unreachable!("invariant check request carries a different task"),
                };
                match self.snapshot_check_workspace(&invocation, &work_id, Some(produced)) {
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
                    ca_bundles: containment::ca_trust_binds(&self.isolation_profile),
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: self.sandbox_mask_roots.clone(),
                    masked_files: vec![],
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
        // abandons with a refusal report exactly as the earlier gate
        // does; no late writer starts after authorization expires.
        let deadline_ok = wire.budget.deadline_unix_seconds > clock();
        let authorized = deadline_ok
            && store
                .transact(|tx| tx.execution(&request.execution_id))?
                .is_some_and(|e| e.state == ExecutionState::Ready);
        if !authorized {
            // Unlike the earlier gate this runs after workspace
            // preparation — and it must still report the refusal: a
            // consumed request is terminal for its binding, so a
            // silent abandon would park the Execution `Ready` with no
            // pending request and no observation to drive it on. A
            // writer workspace must go together with its `.prepare`
            // receipt — the tree is pinned to this consumed request's
            // head, and a re-bindable receipt that outlives the launch
            // would let a retry adopt a stale tree sight-unseen — while
            // a read role's workspace is never released.
            let reason = if deadline_ok {
                "execution left Ready during launch preparation"
            } else {
                "request deadline elapsed during launch preparation"
            };
            self.release_launch_workspace(request, &workspace);
            self.refuse_launch(store, request, &invocation, reason.into(), now_unix_seconds)?;
            return Ok(());
        }
        // Any refusal below this point also owns the prepared
        // workspace: the `SpawnError::Refused` arm releases it for
        // the same receipt-conflict reason.
        match self.port.spawn(&argv, &workspace, &launch_env) {
            Ok(identity) => {
                // The group is committed before the running commit —
                // the durable handle the absence proof keeps when the
                // leader's identity record is the one that is lost.
                if let Some(pgid) = identity.pgid {
                    self.report(
                        store,
                        request,
                        ObservationFact::LaunchGroup {
                            invocation_id: invocation.id.clone(),
                            pgid,
                            leader_started_at: Some(identity.started_at.clone()),
                        },
                        now_unix_seconds,
                    )?;
                }
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
            Err(SpawnError::Unknown { pgid, .. }) => {
                // The leader's start is the owner identity the
                // recycled-pgid check verifies against — captured
                // here from the group itself, the one read of the
                // owner slot the failed identity capture still allows.
                if let Some(pgid) = pgid {
                    self.report(
                        store,
                        request,
                        ObservationFact::LaunchGroup {
                            invocation_id: invocation.id.clone(),
                            pgid,
                            leader_started_at: self.launch_owner_start(pgid),
                        },
                        now_unix_seconds,
                    )?;
                }
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
    /// A resolved Invocation whose Worker report is still pending keeps
    /// its tree too: the deferred-report sweep may collect it on a
    /// later (or out-of-scope) pass, and reclaiming `out/` here would
    /// lose the report bytes before that collection.
    pub fn sweep_invocation_io(&self, store: &impl Store) -> Result<(), ExecError> {
        let root = crate::request_v2::invocation_io_root(&self.workspace_root);
        let mut keep: HashSet<PathBuf> = store
            .transact(|tx| tx.unresolved_invocations())?
            .iter()
            .map(|invocation| root.join(isolation::sanitize_key(&invocation.id.0)))
            .collect();
        for invocation in store.transact(|tx| tx.pending_report_invocations())? {
            keep.insert(root.join(isolation::sanitize_key(&invocation.id.0)));
        }
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
                // starting a second writer. An `unknown` launch — the
                // escalation's recorded outcome — keeps resolving too:
                // the escalation only bounds how soon the operator is
                // told, and a later provable answer still settles the
                // writer under the recorded desired action.
                if matches!(
                    invocation.state,
                    InvocationState::Prepared | InvocationState::Unknown
                ) && let Some(request) =
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

    /// Retry report collection for resolved v2 Invocations whose
    /// required process observations are durable but whose accepted
    /// report fact is not. Normally `finish_invocation` collects inline
    /// right after the terminal commit; this sweep only has work when a
    /// pass crashed in that window. Collection re-checks the
    /// launch-then-terminal gate and is idempotent, so an invocation
    /// that never produced a readable report records `absent` once and
    /// leaves the set.
    fn collect_deferred_reports_scoped(
        &self,
        store: &impl Store,
        now_unix_seconds: i64,
        scope: Option<&WorkId>,
        clock: &mut dyn FnMut() -> i64,
        deadline: Option<i64>,
    ) -> Result<(), ExecError> {
        let pending = store.transact(|tx| tx.pending_report_invocations())?;
        for invocation in pending {
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
            let Some(request) = store.transact(|tx| tx.request(&invocation.request_id))? else {
                continue;
            };
            if let Some(fact) = crate::report_v2::collect(
                store,
                self.body_store
                    .as_ref()
                    .or(self.v2.as_ref().map(|d| &d.bodies)),
                &self.workspace_root,
                &invocation,
                &request,
                now_unix_seconds,
            )? {
                self.report(store, &request, fact, now_unix_seconds)?;
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
    /// ALL of: no unrecorded signature match (a chdir'd writer still
    /// matches the argv), no unexplained process in the launch
    /// directory (an argv-escaped descendant keeps the cwd), no live
    /// unrecorded member of the recorded launch group (a
    /// chdir+argv-escaped descendant keeps the group), and the
    /// bounded uncertain window (a process inside its fork-exec
    /// visibility gap is never declared absent). Anything else keeps
    /// reporting `Unknown`: an ambiguous or unreadable answer is
    /// never a guess — and a bounded count of consecutive incomplete
    /// scans escalates to the `absence_unprovable` outcome rather
    /// than hold `uncertain` forever. The streak counts every
    /// unprovable pass, not just a failed absence verdict: a scan
    /// that could not run at all (unreadable table, unmappable
    /// candidate, unformable signature) is the same gap to the
    /// operator. The escalation marks the launch `unknown` but does
    /// not end the resolution: the loop keeps scanning so a later
    /// provable answer — a re-bound writer or a completed clear —
    /// still settles it under the Execution's recorded desired
    /// action.
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
        // signature is then unprovable, an incomplete scan: uncertain
        // below the bound, escalated at it, never a declared absence
        // on an incomplete search.
        let contained = match self.containment_gate(&binding.argv, now_unix_seconds)? {
            Ok(Gate::Uncontained) => false,
            Err(reason) => {
                return self.report_unidentified_incomplete(
                    store,
                    request,
                    invocation,
                    format!("containment gate refused: {reason}"),
                    now_unix_seconds,
                );
            }
            Ok(Gate::Ready { primitive, worker }) => {
                let (inner_argv, workspace_read_only, mounts_io) = if let Some(body) = &body {
                    // A durable body that no longer parses is corrupt
                    // evidence, not a v1 request — the contained shape
                    // cannot be reconstructed either, an incomplete
                    // scan like any other unprovable pass.
                    let Ok(wire) = WorkerRequestV2::parse(&body.canonical_body) else {
                        return self.report_unidentified_incomplete(
                            store,
                            request,
                            invocation,
                            "the recorded request body no longer parses".to_string(),
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
                    ca_bundles: containment::ca_trust_binds(&self.isolation_profile),
                    extra_binds: containment::extra_binds(&inner_argv, worker.as_deref()),
                    extra_rw_binds: vec![],
                    worker: worker.clone(),
                    mask_roots: self.sandbox_mask_roots.clone(),
                    masked_files: vec![],
                };
                match self.contained_exec(
                    &primitive,
                    plan,
                    &inner_argv,
                    "/workspace",
                    &WorkerEnv::default(),
                ) {
                    Ok((argv, _)) => signatures.push(argv.join(" ")),
                    Err(reason) => {
                        return self.report_unidentified_incomplete(
                            store,
                            request,
                            invocation,
                            format!(
                                "the contained launch argv could not be reconstructed: {reason}"
                            ),
                            now_unix_seconds,
                        );
                    }
                }
                true
            }
        };
        let mut matches = Vec::new();
        for signature in &signatures {
            match self.port.matching_processes(signature) {
                Ok(found) => matches.extend(found),
                // An unreadable process table proves nothing in either
                // direction — an incomplete scan: uncertain below the
                // bound, escalated at it.
                Err(reason) => {
                    return self.report_unidentified_incomplete(
                        store,
                        request,
                        invocation,
                        format!("the process table could not be enumerated: {reason}"),
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
        // unreadable one the real orphan. An incomplete scan rather
        // than a guess at the wrong writer — uncertain below the
        // bound, escalated at it. (Absence is already blocked: the
        // candidate counts in `unrecorded` either way.)
        if unrecorded.iter().any(|m| m.cwd.is_none()) {
            return self.report_unidentified_incomplete(
                store,
                request,
                invocation,
                "an unrecorded signature match's working directory could not be read".to_string(),
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
        // inspected, or signaled as this launch's writer. A contained
        // worker's cwd reads as the sandbox mount `SANDBOX_WORKSPACE`
        // instead of the host launch dir — its namespace's `/workspace`
        // is exactly the launch's workspace bind, so it satisfies the
        // same "inside the launch directory" rule.
        match unrecorded.as_slice() {
            [child] if cwd_in_launch_scope(child.cwd.as_deref(), &canonical_dir) => {
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
                    Ok(Some(())) => {
                        self.incomplete_scans
                            .lock()
                            .unwrap()
                            .remove(&invocation.id.0);
                        self.report(
                            store,
                            request,
                            ObservationFact::ProcessState {
                                invocation_id: invocation.id.clone(),
                                process_identity: bound_identity,
                                state: ProcessStateFact::Running,
                                exit_code: None,
                            },
                            now_unix_seconds,
                        )
                    }
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
                // tree is gone — AND no live unrecorded process remains
                // in the recorded launch group: a descendant that
                // changed BOTH argv and working directory is invisible
                // to the first two checks but stays in the leader's
                // group unless it left the group entirely — `setsid`,
                // `setpgid`, or joining another group in the same
                // session. The named residual is exactly that escape —
                // leaving the recorded group plus chdir + argv change
                // leaves every handle the launch committed — and so
                // does a spawn whose group record was lost before it
                // could be committed; neither can be told from a truly
                // absent writer by enumeration alone.
                //
                // The scans run only once the bounded window has
                // elapsed — before that the answer is Unknown
                // regardless. The anchor is the Execution's
                // `uncertain_since`, set on the transition into
                // `uncertain` and never refreshed by repeat `Unknown`
                // reports; an unidentified Prepared is the only
                // unresolved Invocation that can hold its Execution
                // `ready`, so the anchor always postdates this launch's
                // commit — the measured window covers the whole
                // spawn-to-visible gap plus poll lag.
                let window_elapsed = execution.uncertain_since_unix_seconds.is_some_and(|since| {
                    now_unix_seconds - since >= UNIDENTIFIED_LAUNCH_ABSENT_SECONDS
                });
                if !window_elapsed {
                    return self.report_unidentified_unknown(
                        store,
                        request,
                        &invocation.id,
                        now_unix_seconds,
                    );
                }
                // The launch group was committed the moment the spawn
                // call returned — read it back so a live group member
                // that escaped both other scans still defeats the
                // clear verdict. The recorded leader start is the
                // owner identity the group check verifies before any
                // member counts.
                let launch_group = store.transact(|tx| {
                    tx.observations_for_execution(&execution.id).map(|obs| {
                        obs.iter().rev().find_map(|o| match &o.fact {
                            ObservationFact::LaunchGroup {
                                invocation_id,
                                pgid,
                                leader_started_at,
                            } if *invocation_id == invocation.id => {
                                Some((*pgid, leader_started_at.clone()))
                            }
                            _ => None,
                        })
                    })
                })?;
                let clearance =
                    match self.launch_dir_clearance(&canonical_dir, contained, &recorded) {
                        Clearance::Clear => self.launch_group_clearance(launch_group, &recorded),
                        other => other,
                    };
                match clearance {
                    Clearance::Clear => {
                        self.incomplete_scans
                            .lock()
                            .unwrap()
                            .remove(&invocation.id.0);
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
                    }
                    Clearance::Occupied => {
                        // A completed scan that found unexplained life
                        // restarts the streak: this pass is uncertain
                        // on positive evidence, not on a gap.
                        self.incomplete_scans
                            .lock()
                            .unwrap()
                            .remove(&invocation.id.0);
                        self.report_unidentified_unknown(
                            store,
                            request,
                            &invocation.id,
                            now_unix_seconds,
                        )
                    }
                    Clearance::Incomplete(reason) => self.report_unidentified_incomplete(
                        store,
                        request,
                        invocation,
                        reason,
                        now_unix_seconds,
                    ),
                }
            }
            // Anything else is ambiguous — more than one candidate,
            // or a single candidate outside the launch directory.
            // Never guess which process is this launch's writer. The
            // match itself is positive evidence of unexplained life —
            // a completed scan, not a gap — so it restarts the
            // incomplete streak the same way `Occupied` does.
            _ => {
                self.incomplete_scans
                    .lock()
                    .unwrap()
                    .remove(&invocation.id.0);
                self.report_unidentified_unknown(store, request, &invocation.id, now_unix_seconds)
            }
        }
    }

    /// Whether the launch directory holds no process the store cannot
    /// account for. An escaped descendant keeps the leader's working
    /// directory after leaving the argv signature, so directory
    /// occupancy is the only remaining evidence that the vanished
    /// writer's tree is really gone. A failed OR INCOMPLETE
    /// enumeration — a live process whose cwd the port cannot map is
    /// a possible occupant — proves nothing: `Incomplete` keeps the
    /// launch uncertain and feeds the bounded escalation.
    ///
    /// The launch scope is `dir` plus, for a contained launch, the
    /// sandbox workspace root: the worker's mount namespace binds the
    /// launch workspace at `SANDBOX_WORKSPACE` and chdirs into it, so a
    /// contained escapee's `/proc` cwd reads `/workspace` and never
    /// lands inside the host dir. `processes_in_directory` stays
    /// unscoped — each scan answers "cwd inside this dir" alone — so
    /// the launch scope is one scan per root and an unexplained
    /// occupant in either keeps absence unproven.
    fn launch_dir_clearance(
        &self,
        dir: &Path,
        contained: bool,
        recorded: &HashSet<(i32, String)>,
    ) -> Clearance {
        let clear = |occupants: Result<Vec<ProcessMatch>, String>| match occupants {
            Err(reason) => Clearance::Incomplete(reason),
            Ok(occupants) => {
                if occupants
                    .iter()
                    .all(|m| recorded.contains(&(m.identity.pid, m.identity.started_at.clone())))
                {
                    Clearance::Clear
                } else {
                    Clearance::Occupied
                }
            }
        };
        // The sandbox root is a mount-namespace spelling, not a host
        // dir: on a host with no real `/workspace` every live
        // contained worker's `/proc` cwd reads `/workspace` and
        // cannot canonicalize, which fails the genuine scan before
        // the recorded filter ever runs — another execution's
        // recorded contained worker would hold absence unprovable
        // forever. The namespace query reports that spelling as an
        // occupant instead, so the same recorded judgment applies
        // per pid: a recorded contained worker no longer blocks,
        // an unexplained one still does.
        match clear(self.port.processes_in_directory(dir)) {
            Clearance::Clear if contained => clear(
                self.port
                    .processes_in_namespace_dir(Path::new(containment::SANDBOX_WORKSPACE)),
            ),
            other => other,
        }
    }

    /// The group owner's start as the enumeration reports it — the
    /// owner-slot identity `launch_group_clearance` verifies against.
    /// `pgid` is the spawned leader's pid (`process_group(0)`), so the
    /// member at that slot is the leader itself while it lives; a read
    /// taken at launch-report time runs under the same post-spawn
    /// trust the `Started` arm's identity capture does. `None` when
    /// the enumeration fails or the slot is already empty.
    fn launch_owner_start(&self, pgid: i32) -> Option<String> {
        self.port.group_members(pgid).ok().and_then(|members| {
            members
                .into_iter()
                .find(|m| m.pid == pgid)
                .map(|m| m.started_at)
        })
    }

    /// The third absence check: whether the launch's recorded process
    /// group still holds a live process the store cannot account for.
    /// A descendant that changed both argv and working directory is
    /// invisible to the signature and directory scans but stays in
    /// the leader's group unless it left the group entirely —
    /// `setsid`, `setpgid`, or joining another group in the same
    /// session — so the recorded group is the last handle on that
    /// escape. `None` means the launch never committed a group
    /// record — a spawn fault before the record could land — so
    /// nothing can be enumerated and the check is vacuous: a strictly
    /// smaller residual than the group-leaving escape, because the
    /// leader's spawn had already been lost before the group existed
    /// on record.
    ///
    /// The owner slot is verified before any member counts — the same
    /// attribution `proof_identities` applies on the stop path. `pgid`
    /// is the leader's pid, so the member at `pid == pgid` is the
    /// owner; when the launch recorded the leader's start, a live
    /// process there with a different start means the number was
    /// recycled after the leader died — a group can never be recycled
    /// while a member lives, so a foreign owner also proves no member
    /// of the launch's group remains: the whole enumeration is a
    /// foreign family the verdict must not count in either direction.
    /// With no recorded owner start the slot cannot be verified and
    /// the member check below keeps its conservative reading.
    ///
    /// A recorded member is accounted for elsewhere; a dead member is
    /// gone; anything else live defeats the verdict — including a
    /// foreign member in a group whose owner is still this launch's
    /// (or cannot be verified), which can only ever block, never
    /// clear.
    fn launch_group_clearance(
        &self,
        launch_group: Option<(i32, Option<String>)>,
        recorded: &HashSet<(i32, String)>,
    ) -> Clearance {
        let Some((pgid, owner_started_at)) = launch_group else {
            return Clearance::Clear;
        };
        let members = match self.port.group_members(pgid) {
            Ok(members) => members,
            Err(reason) => return Clearance::Incomplete(reason),
        };
        if owner_started_at.as_ref().is_some_and(|start| {
            members
                .iter()
                .any(|m| m.pid == pgid && &m.started_at != start)
        }) {
            return Clearance::Clear;
        }
        for member in members {
            if recorded.contains(&(member.pid, member.started_at.clone())) {
                continue;
            }
            match self.port.inspect(&member) {
                ProcessEvidence::Alive { .. } => return Clearance::Occupied,
                ProcessEvidence::Stopped { .. } => {}
                // A member whose liveness cannot be verified keeps the
                // group unprovable — it might be the escaped writer.
                ProcessEvidence::Unknown { reason } => {
                    return Clearance::Incomplete(format!(
                        "launch group {pgid} member {} could not be verified: {reason}",
                        member.pid
                    ));
                }
            }
        }
        Clearance::Clear
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

    /// One unprovable resolution pass — a scan gap that proves
    /// nothing in either direction, wherever in the search it struck:
    /// an unreadable table, an unmappable candidate, a signature that
    /// could not be formed, an incomplete directory or group
    /// enumeration. Counts the consecutive-incomplete streak and
    /// takes the bounded escalation at the limit while the launch is
    /// still `prepared`: a distinct operator-visible
    /// `absence_unprovable`, reported once — the launch then scans on
    /// as `unknown`, so a later provable answer still resolves it and
    /// further gaps stay `uncertain` without re-escalating. Below the
    /// bound (or already escalated) the pass reports the ordinary
    /// Unknown.
    fn report_unidentified_incomplete(
        &self,
        store: &impl Store,
        request: &InvocationRequest,
        invocation: &Invocation,
        reason: String,
        now_unix_seconds: i64,
    ) -> Result<(), ExecError> {
        let streak = {
            let mut streaks = self.incomplete_scans.lock().unwrap();
            let n = streaks.entry(invocation.id.0.clone()).or_insert(0);
            *n += 1;
            *n
        };
        if streak >= UNIDENTIFIED_LAUNCH_SCAN_LIMIT && invocation.state == InvocationState::Prepared
        {
            self.report(
                store,
                request,
                ObservationFact::LaunchAbsenceUnprovable {
                    invocation_id: invocation.id.clone(),
                    reason,
                },
                now_unix_seconds,
            )
        } else {
            self.report_unidentified_unknown(store, request, &invocation.id, now_unix_seconds)
        }
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

    /// Save every artifact fact, commit the terminal process
    /// observation, then collect the v2 report. The report gate needs
    /// the launch acceptance (the observed `Running` fact or the
    /// Invocation row's durable identity) and this terminal fact in the
    /// Invocation's record, so the order is part of the contract; a
    /// crash between the two commits leaves a durable candidate for
    /// the deferred-report sweep. `exit_code` is the status `inspect`
    /// proved for the writer's own exit without consuming it — `None`
    /// when no normal exit status could be proven, which the exit fact
    /// records honestly rather than as a pass. Once the terminal fact
    /// is durable the child's status is consumed, so this reaps even
    /// when collection then fails: a `Done` Invocation is never
    /// re-entered and its report retry belongs to the deferred sweep,
    /// not to holding the zombie open. A failure before the terminal
    /// commit leaves the child for the retry to inspect again.
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
        // A recorded v2 request body marks the dispatch whose
        // workspace-root `artifact.json` is the shim's bound manifest:
        // the slot requires the manifest contract. A v1 artifact at
        // the same path stays an opaque payload.
        let v2_manifest = store
            .transact(|tx| tx.worker_request_body(&invocation.request_id))?
            .is_some();
        for required in &invocation.required_artifacts {
            let state = artifact_fact(&workspace, required, invocation, v2_manifest);
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
        // The process tree is proven stopped before this runs. Commit
        // the terminal process observation first: bounded report
        // collection may begin only after the launch acceptance and
        // this terminal fact for the same process are both recorded.
        // The collection itself is idempotent and is retried by the
        // pending-report sweep if this pass fails between the two
        // commits. v1 requests collect nothing.
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
        let collected = crate::report_v2::collect(
            store,
            self.body_store
                .as_ref()
                .or(self.v2.as_ref().map(|d| &d.bodies)),
            &self.workspace_root,
            invocation,
            &request,
            now_unix_seconds,
        );
        // The terminal fact is durable, so the child's exit status has
        // been consumed: reap now even if collection fails. The
        // Invocation is already `Done`, so no later pass re-enters
        // finish; a stranded collector error is retried by the
        // deferred-report sweep, not by keeping the zombie.
        if let Some(identity) = &invocation.process_identity {
            let _ = self.port.reap(identity.pid);
        }
        if let Some(fact) = collected? {
            self.report(store, &request, fact, now_unix_seconds)?;
        }
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
                let v2_manifest = store
                    .transact(|tx| tx.worker_request_body(&invocation.request_id))?
                    .is_some();
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
                        let digest =
                            match artifact_fact(&workspace, required, &invocation, v2_manifest) {
                                ArtifactFact::Present { digest } => digest,
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

/// Whether the bytes at a required-artifact path are the v2 handoff
/// manifest, and whose. The manifest names its producing invocation:
/// a file in that format is only ever evidence for the producer it
/// binds — a previous round's manifest a reused workspace kept is
/// `Foreign` to every other invocation that finds it.
enum ManifestBinding {
    /// `taisan.worker.artifact/1` naming this invocation and request.
    Bound,
    /// The manifest format naming another producer.
    Foreign,
    /// Not the manifest format — an opaque artifact payload.
    Opaque,
}

fn manifest_binding(bytes: &[u8], invocation: &Invocation) -> ManifestBinding {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return ManifestBinding::Opaque;
    };
    if value.get("format_version").and_then(|v| v.as_str()) != Some("taisan.worker.artifact/1") {
        return ManifestBinding::Opaque;
    }
    if value.get("invocation_id").and_then(|v| v.as_str()) == Some(invocation.id.0.as_str())
        && value.get("request_id").and_then(|v| v.as_str()) == Some(invocation.request_id.as_str())
    {
        ManifestBinding::Bound
    } else {
        ManifestBinding::Foreign
    }
}

/// The evidence state one required artifact earns in the workspace
/// root. Presence is never reported without a digest — `Present`
/// carries the exact payload digest and an unexpected payload is
/// corrupt evidence, never present. On top of the digest rule the
/// manifest binds its producer: a manifest naming another invocation
/// is stale, never `Present`, and a v2 dispatch's `artifact.json`
/// slot holding anything but the bound manifest is malformed — both
/// degrade to the same `missing` a failed write already records.
/// `v2_manifest` marks an invocation whose dispatch recorded a
/// durable v2 request body; only then does the `artifact.json` path
/// require the manifest contract — a v1 artifact stays opaque.
///
/// A tracked `artifact.json` is one exception to that degradation.
/// The shim never rewrites repository content, so under a v2 dispatch
/// a tracked file that is not the bound manifest can never become
/// `Present`; it is reported `Unfulfillable` so Core fails the
/// request at once instead of spending the artifact-recovery budget.
/// Whether the repository tracked the file or the runtime's own
/// commit captured it makes no difference — the collision is the
/// same.
fn artifact_fact(
    workspace: &Path,
    required: &RequiredArtifact,
    invocation: &Invocation,
    v2_manifest: bool,
) -> ArtifactFact {
    let candidate = workspace.join(&required.path);
    let artifact_slot = v2_manifest && Path::new(&required.path) == Path::new("artifact.json");
    // A tracked slot is repository content the shim refuses to write,
    // whether the worktree still holds it or not. Unless the bytes are
    // the bound manifest, the declared artifact is structurally
    // unfulfillable — no recovery attempt can change a tracked path
    // into one the shim may overwrite.
    let tracked = artifact_slot && artifact_path_tracked(workspace, &required.path);
    let unfulfillable = || ArtifactFact::Unfulfillable {
        reason: format!(
            "workspace-root {} is tracked repository content; the worker shim \
             never rewrites it, so it cannot be this invocation's bound \
             handoff manifest",
            required.path
        ),
    };
    if !candidate.is_file() {
        return if tracked {
            unfulfillable()
        } else {
            ArtifactFact::Missing
        };
    }
    let Ok(bytes) = std::fs::read(&candidate) else {
        return if tracked {
            unfulfillable()
        } else {
            ArtifactFact::Missing
        };
    };
    if bytes.is_empty() {
        return if tracked {
            unfulfillable()
        } else {
            ArtifactFact::Missing
        };
    }
    let binding = manifest_binding(&bytes, invocation);
    if artifact_slot {
        if !matches!(binding, ManifestBinding::Bound) {
            // An untracked stale or malformed file is output recovery
            // may still replace; only the tracked collision is final.
            return if tracked {
                unfulfillable()
            } else {
                ArtifactFact::Missing
            };
        }
    } else if matches!(binding, ManifestBinding::Foreign) {
        return ArtifactFact::Missing;
    }
    let digest = sha256_hex("artifact-v1", &bytes);
    // A present artifact must carry its exact payload digest; an
    // unexpected payload is corrupt evidence, never present.
    if required
        .expected_digest
        .as_ref()
        .is_none_or(|expected| expected == &digest)
    {
        ArtifactFact::Present { digest }
    } else {
        ArtifactFact::Corrupt {
            reason: Some("payload digest does not match the declared expectation".into()),
        }
    }
}

/// Whether the workspace repository tracks `path` — staged or
/// committed. Outside a git repository nothing is tracked. A tracked
/// path is repository content the shim's manifest write path refuses
/// to rewrite; it is not stale manifest debris.
fn artifact_path_tracked(workspace: &Path, path: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--error-unmatch", "--", path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

    /// A Linux scan without lsof still enumerates — `/proc/<pid>/cwd`
    /// carries the same table. The calling test process is the
    /// fixture: its own cwd must appear in both the whole-table scan
    /// and the single-pid read.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_fallback_maps_cwds_without_lsof() {
        let pid = std::process::id() as i32;
        let scan = with_proc_fallback(Err("injected lsof failure".into()))
            .expect("/proc fallback enumerates");
        assert!(scan.mapped.contains(&pid));
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(
            scan.cwds
                .iter()
                .any(|(p, c)| *p == pid && std::fs::canonicalize(c).ok().as_ref() == Some(&cwd)),
            "the /proc scan maps this process's cwd"
        );
        let single = proc_cwd_of(pid).expect("own cwd through /proc");
        assert_eq!(std::fs::canonicalize(single).unwrap(), cwd);
    }

    /// `PF_KTHREAD` (field 9) marks a kernel thread in every PID
    /// namespace; the parser must count past a `comm` that itself
    /// contains spaces and `)`.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_flags_reads_flags_past_comm() {
        // `kthreadd` carries PF_KTHREAD in field 9.
        assert_eq!(
            proc_stat_flags("2 (kthreadd) S 0 0 0 0 0 2097152 0 0"),
            Some(0x0020_0000)
        );
        // A userspace row never does.
        assert_eq!(
            proc_stat_flags("42 (worker) S 1 42 42 0 -1 4194304 0"),
            Some(4194304)
        );
        // Splitting on the FIRST `)` would misfield a `comm` like this.
        assert_eq!(
            proc_stat_flags("7 (odd ) name) S 1 7 7 0 -1 2097152 0"),
            Some(0x0020_0000)
        );
        // Malformed input yields no answer rather than a wrong one.
        assert_eq!(proc_stat_flags("garbage"), None);
        assert_eq!(proc_stat_flags("5 (x) S 1"), None);
    }

    /// The live check agrees: pid 1 is userspace in every PID
    /// namespace — including a container's, where pid 2 is userspace
    /// too — and an unreadable stat is never kernel-thread proof, so
    /// the completeness gate stays fail-closed.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_thread_check_uses_stat_flags_not_pid2() {
        assert!(!is_kernel_thread(1));
        assert!(!is_kernel_thread(std::process::id() as i32));
        assert!(!is_kernel_thread(i32::MAX));
    }

    /// The completeness gate excludes only the class that provably
    /// cannot possess a cwd — an independent structural predicate,
    /// never "the scan did not map it". The mappable population
    /// completes with cwd-less processes of the excluded class
    /// present (the uid-0/kernel-thread case); every other live
    /// unmappable process stays a gap.
    #[test]
    fn completeness_gate_excludes_only_the_structural_no_cwd_class() {
        let live = |_: i32| LstartLookup::Present("lstart".into());
        let running = |_: i32| Some("S".to_string());
        // The excluded class is skipped — the scan completes for the
        // mappable population with cwd-less processes present.
        assert!(
            unmappable_cwd_gap(&[7, 8], &HashSet::from([8]), live, running, |p| p == 7).is_ok()
        );
        // A same-user live process with an unmappable cwd outside the
        // excluded class is still a gap — the contract is unchanged.
        assert!(unmappable_cwd_gap(&[7], &HashSet::new(), live, running, |_| false).is_err());
        // A zombie and a process gone between the table reads can hold
        // no live cwd — neither is a gap.
        assert!(
            unmappable_cwd_gap(
                &[7],
                &HashSet::new(),
                live,
                |_| Some("Z".to_string()),
                |_| false
            )
            .is_ok()
        );
        assert!(
            unmappable_cwd_gap(
                &[7],
                &HashSet::new(),
                |_| LstartLookup::Absent,
                running,
                |_| false
            )
            .is_ok()
        );
        // A failed liveness lookup fails closed — it can never read as
        // "the process is gone", so the whole enumeration stays
        // incomplete rather than silently skip an ambiguous row.
        assert!(
            unmappable_cwd_gap(
                &[7],
                &HashSet::new(),
                |_| LstartLookup::Failed("ps failed".into()),
                running,
                |_| false
            )
            .is_err()
        );
    }

    /// A contained worker's cwd reads as the sandbox mount, not the
    /// host launch directory — launch scope counts `/workspace` (and
    /// paths under it) as inside, so the occupant filter cannot miss
    /// a live contained descendant.
    #[test]
    fn launch_scope_covers_the_sandbox_workspace_mount() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let sandbox = Path::new(containment::SANDBOX_WORKSPACE);
        assert!(cwd_in_launch_scope(Some(sandbox), &dir));
        assert!(cwd_in_launch_scope(Some(&sandbox.join("nested")), &dir));
        assert!(cwd_in_launch_scope(Some(dir.as_path()), &dir));
        assert!(!cwd_in_launch_scope(Some(Path::new("/")), &dir));
        assert!(!cwd_in_launch_scope(None, &dir));
        // Component-wise matching: a sibling spelling does not count.
        assert!(!cwd_in_launch_scope(
            Some(Path::new("/workspace-sibling")),
            &dir
        ));
        // The port contract predicate never widens to the sandbox —
        // `processes_in_directory` answers "cwd inside `dir`" alone,
        // so the launch scope is layered on by its call sites, not by
        // the shared filter.
        assert!(!cwd_within(Some(sandbox), &dir));
        assert!(!cwd_within(Some(&sandbox.join("nested")), &dir));
        assert!(cwd_within(Some(dir.as_path()), &dir));
    }

    /// `/proc/<pid>/cwd` (and lsof) annotate a deleted working
    /// directory's reported path with ` (deleted)` — a marker, not
    /// part of the path. Membership judges the pre-deletion
    /// spelling, and the flag carries the "the process WAS there"
    /// evidence the scan counts as occupancy.
    #[test]
    fn deleted_marker_judges_the_pre_deletion_spelling() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let marked = PathBuf::from(format!("{} (deleted)", dir.display()));
        assert_eq!(strip_deleted_marker(&marked), (dir.clone(), true));
        assert_eq!(strip_deleted_marker(&dir), (dir.clone(), false));
        // Only a trailing marker goes — mid-path spellings and other
        // suffixes are untouched.
        let other = dir.join("a (deleted) dir");
        assert_eq!(strip_deleted_marker(&other), (other, false));
        // `cwd_within` strips too: a marked spelling of a live dir
        // compares by the pre-deletion path.
        assert!(cwd_within(Some(&marked), &dir));
        // A marked sandbox mount still reads in launch scope — a
        // contained worker's `/workspace` stays attributable even
        // after the mount dir is gone — while a marked sibling
        // spelling stays outside: membership is component-wise on
        // the pre-deletion path.
        assert!(cwd_in_launch_scope(
            Some(Path::new("/workspace (deleted)")),
            &dir
        ));
        assert!(!cwd_in_launch_scope(
            Some(Path::new("/workspace-sibling (deleted)")),
            &dir
        ));
    }

    /// With no working mechanism the scan is `Err` — an empty table
    /// would read back as a false absence proof.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn directory_scan_fails_closed_when_no_mechanism_enumerates() {
        assert!(with_proc_fallback(Err("injected lsof failure".into())).is_err());
    }

    /// `Present` evidence requires the manifest to bind the invocation
    /// it is recorded against: a previous round's manifest naming
    /// another producer is stale — the same `missing` a failed write
    /// records — and under a v2 dispatch the `artifact.json` slot only
    /// ever holds the bound manifest. A v1 artifact at the same path
    /// stays an opaque payload under the plain digest rule.
    #[test]
    fn artifact_evidence_binds_the_manifest_to_its_invocation() {
        let dir = std::env::temp_dir().join(format!(
            "artifact-fact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let invocation = Invocation {
            id: InvocationId("inv-1".into()),
            request_id: "req-1".into(),
            execution_id: ExecutionId("e-1".into()),
            role: InvocationRole::Coding,
            state: InvocationState::Done,
            process_identity: None,
            required_artifacts: vec![],
            evidence_refs: vec![],
            descendants: vec![],
        };
        let required = RequiredArtifact {
            kind: "artifact.json".into(),
            path: "artifact.json".into(),
            expected_digest: None,
        };
        let write = |body: &str| std::fs::write(dir.join("artifact.json"), body).unwrap();

        // The manifest this invocation produced binds it — Present.
        write(
            &serde_json::json!({
                "format_version": "taisan.worker.artifact/1",
                "request_id": "req-1",
                "invocation_id": "inv-1",
            })
            .to_string(),
        );
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Present { .. }
        ));

        // A previous round's manifest names another producer: stale,
        // missing — under v2 and under the v1 digest rule alike,
        // because a manifest-format file only ever evidences its
        // namesake.
        write(
            &serde_json::json!({
                "format_version": "taisan.worker.artifact/1",
                "request_id": "req-9",
                "invocation_id": "inv-9",
            })
            .to_string(),
        );
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Missing
        ));
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, false),
            ArtifactFact::Missing
        ));

        // An opaque payload is not the manifest contract: under v2 the
        // slot holds nothing bound — missing, like a failed write;
        // under v1 the plain digest rule still stands.
        write("opaque payload");
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Missing
        ));
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, false),
            ArtifactFact::Present { .. }
        ));
        // ...and an opaque payload at a non-manifest path is an
        // artifact under either rule.
        let other = RequiredArtifact {
            kind: "result.bin".into(),
            path: "result.bin".into(),
            expected_digest: None,
        };
        std::fs::write(dir.join("result.bin"), "opaque payload").unwrap();
        assert!(matches!(
            artifact_fact(&dir, &other, &invocation, true),
            ArtifactFact::Present { .. }
        ));

        // Absent and digest-mismatched files keep their states.
        std::fs::remove_file(dir.join("artifact.json")).unwrap();
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Missing
        ));
        write("opaque payload");
        let wrong = RequiredArtifact {
            expected_digest: Some("0".repeat(64)),
            ..required.clone()
        };
        assert!(matches!(
            artifact_fact(&dir, &wrong, &invocation, false),
            ArtifactFact::Corrupt { .. }
        ));
    }

    /// A tracked `artifact.json` is repository content the shim never
    /// rewrites: under a v2 dispatch an untracked stale or opaque file
    /// stays `missing` for recovery, but the tracked collision is a
    /// structural `Unfulfillable`. A bound manifest is still `Present`
    /// whatever the index says, and the untracked rule returns once
    /// the path leaves the index.
    #[test]
    fn a_tracked_artifact_json_is_unfulfillable_under_a_v2_dispatch() {
        let dir = std::env::temp_dir().join(format!(
            "artifact-tracked-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        let invocation = Invocation {
            id: InvocationId("inv-tracked".into()),
            request_id: "req-tracked".into(),
            execution_id: ExecutionId("e-tracked".into()),
            role: InvocationRole::Coding,
            state: InvocationState::Done,
            process_identity: None,
            required_artifacts: vec![],
            evidence_refs: vec![],
            descendants: vec![],
        };
        let required = RequiredArtifact {
            kind: "artifact.json".into(),
            path: "artifact.json".into(),
            expected_digest: None,
        };
        let write = |body: &str| {
            std::fs::write(dir.join("artifact.json"), body).unwrap();
        };

        // Tracked opaque content is repository content that can never
        // be the bound manifest: a decisive, non-recoverable refusal.
        write("tracked repository content");
        git(&["add", "artifact.json"]);
        git(&["commit", "-qm", "track artifact.json"]);
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Unfulfillable { .. }
        ));
        // The shim refuses to write a tracked path even when the
        // worktree entry is gone, so the missing file is the same
        // collision, not recoverable absence.
        std::fs::remove_file(dir.join("artifact.json")).unwrap();
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Unfulfillable { .. }
        ));

        // A tracked manifest naming another producer is the same
        // collision, not the ordinary stale-manifest `missing`.
        write(
            &serde_json::json!({
                "format_version": "taisan.worker.artifact/1",
                "request_id": "req-other",
                "invocation_id": "inv-other",
            })
            .to_string(),
        );
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Unfulfillable { .. }
        ));

        // A manifest bound to this invocation is evidence regardless of
        // the index.
        write(
            &serde_json::json!({
                "format_version": "taisan.worker.artifact/1",
                "request_id": "req-tracked",
                "invocation_id": "inv-tracked",
            })
            .to_string(),
        );
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Present { .. }
        ));

        // Untracked stale manifests keep the #413 recovery rule: the
        // shim may sweep them, so they stay `missing`, never
        // `Unfulfillable`.
        git(&["rm", "--cached", "-q", "artifact.json"]);
        write(
            &serde_json::json!({
                "format_version": "taisan.worker.artifact/1",
                "request_id": "req-other",
                "invocation_id": "inv-other",
            })
            .to_string(),
        );
        assert!(matches!(
            artifact_fact(&dir, &required, &invocation, true),
            ArtifactFact::Missing
        ));
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
            ca_bundles: vec![],
            extra_binds: vec![],
            extra_rw_binds: vec![],
            worker: None,
            mask_roots: vec![],
            masked_files: vec![],
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
