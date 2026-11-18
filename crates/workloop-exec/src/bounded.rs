//! Bounded child-process exchange for external invocations such as the
//! v0.2 Selector protocol call (#201): one argv vector, one request on
//! stdin through EOF, one response on stdout through EOF, diagnostics
//! on stderr. The exchange is deliberately narrower than worker
//! supervision — there is no store record, no artifact handoff, and no
//! lifecycle decision — but it reuses the same containment contract:
//! direct exec (never a shell), an allowlisted environment, an owned
//! process group, identity-verified group termination on the bound,
//! and bounded drains that can never deadlock or grow memory past the
//! contract limits.
//!
//! Failure classification is transport-level only. Callers map these
//! variants onto their own bounded diagnostic vocabulary; nothing here
//! interprets the payload.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use workloop_core::ports::WorkerEnv;

use crate::supervisor::{self, ProbeSignalPlan};

/// Byte bounds for one exchange, mirroring the selector protocol's
/// framing table. `max_stdin` bounds the serialized request; the
/// caller validates the request against it before spawn — this
/// function enforces the cap again defensively. `max_stdout` and
/// `max_stderr` are the drain caps: reading one byte past either is
/// the detection byte that ends the read and the invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExchangeBounds {
    pub max_stdin: usize,
    pub max_stdout: usize,
    pub max_stderr: usize,
}

/// The selector protocol's framing bounds (#200/#201).
pub const SELECTOR_EXCHANGE_BOUNDS: ExchangeBounds = ExchangeBounds {
    max_stdin: 1_048_576,
    max_stdout: 4_194_304,
    max_stderr: 16_384,
};

/// What one completed exchange produced. `stderr_len` is the observed
/// diagnostic byte count — raw stderr is never retained, forwarded, or
/// persisted, matching the protocol's "fixed local error code and byte
/// counts" rule.
#[derive(Debug)]
pub struct ExchangeOutput {
    pub stdout: Vec<u8>,
    pub stderr_len: usize,
    /// The proven normal-exit status; `None` when the leader ended by
    /// signal or its status could not be proven.
    pub exit_code: Option<i32>,
}

/// Why an exchange produced no usable output. Every variant is a
/// bounded, payload-free diagnostic — child stderr text and partial
/// stdout never appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    /// The argv could not be spawned, or the request exceeds the
    /// stdin bound; no usable process ran.
    Launch { reason: String },
    /// The bound elapsed. The owned process group was signalled and
    /// the leader reaped when the plan allowed it; `reason` carries
    /// the bounded detail — an unverifiable identity is reported
    /// rather than claimed stopped.
    Deadline { reason: String },
    /// stdout exceeded `max_stdout` (the detection byte was read).
    /// The process group was terminated; no partial output is
    /// returned.
    OutputOverflow,
    /// stderr exceeded `max_stderr`. The process group was
    /// terminated; the diagnostic stream is untrusted past the bound.
    StderrOverflow,
    /// The leader exited with a nonzero status or by signal.
    Exit { exit_code: Option<i32> },
    /// The wait/reap machinery failed or descendant state stayed
    /// uncertain after the leader exited — the invocation's fate is
    /// not provably clean, so no output is trusted.
    Unknown { reason: String },
}

/// Environment allowlist for selector invocations: exactly the
/// variables a stateless external command plausibly needs to find its
/// own configuration and executables — nothing else survives
/// `env_clear`, so publication credentials (`TAISAN_*`, `GH_TOKEN`,
/// role `credential_ref` env providers), store paths, and Worker
/// authority can never reach the child by inheritance. The adapter
/// resolves its own source-read authentication through its own
/// configuration; this list grants it nothing.
const SELECTOR_ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];

/// Build the selector child's complete environment: the fixed
/// allowlist above, read from this process's environment only when
/// present. Deterministic — no other variable can appear.
pub fn selector_env() -> WorkerEnv {
    let vars = SELECTOR_ENV_PASSTHROUGH
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v)))
        .collect();
    WorkerEnv { vars }
}

/// What a drain thread reports back: kept bytes (stdout only), the
/// observed byte count (stderr only — content is never retained),
/// whether the bound tripped, and any read failure — a read error is
/// never EOF, so it must surface as an uncertain stream rather than a
/// clean one.
struct DrainReport {
    kept: Vec<u8>,
    observed: usize,
    overflowed: bool,
    error: Option<std::io::ErrorKind>,
}

/// Drain `reader` until EOF or `cap` bytes plus the detection byte.
/// Past the cap the thread stops accumulating — the exchange loop
/// terminates the process — and reports the trip immediately.
/// `Interrupted` retries: a signal landing mid-read is not stream
/// end.
fn drain<R: Read>(mut reader: R, cap: usize, overflow: Arc<AtomicBool>, keep: bool) -> DrainReport {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut observed = 0usize;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                observed += n;
                if observed > cap {
                    overflow.store(true, Ordering::SeqCst);
                    return DrainReport {
                        kept,
                        observed,
                        overflowed: true,
                        error: None,
                    };
                }
                if keep {
                    kept.extend_from_slice(&buf[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return DrainReport {
                    kept,
                    observed,
                    overflowed: false,
                    error: Some(e.kind()),
                };
            }
        }
    }
    DrainReport {
        kept,
        observed,
        overflowed: false,
        error: None,
    }
}

/// How long the exchange waits for the drain threads to observe EOF
/// after the leader's exit was proven — covers a straggler still
/// holding a pipe. Past it, surviving descendants make the outcome
/// uncertain rather than clean.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Wait-slice granularity for the bound loop: overflow flags are
/// polled between slices so an exceeding child is terminated without
/// waiting out the whole timeout.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// Run one bounded exchange: write `stdin_bytes` (the caller validates
/// the request against `bounds.max_stdin` before spawn; the cap is
/// enforced again defensively here), spawn `argv` under `env` in
/// `cwd`, drain both output streams under their caps, enforce
/// `timeout` from call time, and classify the outcome.
///
/// The child leads its own process group (`process_group(0)`), so the
/// bound and any overflow terminate the whole tree the leader did not
/// let escape. Termination re-verifies the recorded `pid + lstart`
/// identity first — the same stop-proof rule the supervisor applies —
/// so a recycled pid is never signalled.
pub fn exchange(
    argv: &[String],
    cwd: &Path,
    env: &WorkerEnv,
    stdin_bytes: &[u8],
    bounds: &ExchangeBounds,
    timeout: Duration,
) -> Result<ExchangeOutput, ExchangeError> {
    // The deadline is anchored at call time — spawn, identity capture,
    // and thread setup all spend the same budget.
    let deadline = std::time::Instant::now() + timeout;
    if argv.is_empty() {
        return Err(ExchangeError::Launch {
            reason: "empty argv".into(),
        });
    }
    if stdin_bytes.len() > bounds.max_stdin {
        return Err(ExchangeError::Launch {
            reason: "request exceeds the stdin bound".into(),
        });
    }
    let program = argv[0].clone();
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (k, v) in &env.vars {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| ExchangeError::Launch {
        reason: format!("spawn {program}: {e}"),
    })?;
    let pid = child.id() as i32;

    // Record the leader identity while the handle proves the pid is
    // still ours — every later bound signal re-verifies it.
    let started_at = match supervisor::PosixProcessControl::identity_of(pid) {
        Ok(identity) => identity.started_at,
        Err(reason) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExchangeError::Unknown { reason });
        }
    };

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // The request writer runs beside the drains: a short write or a
    // dead reader must never stall the exchange — EOF on stdin is the
    // protocol's request end and is sent by dropping the handle.
    let request = stdin_bytes.to_vec();
    std::thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(&request);
    });

    let stdout_overflow = Arc::new(AtomicBool::new(false));
    let stderr_overflow = Arc::new(AtomicBool::new(false));
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let max_stdout = bounds.max_stdout;
    let max_stderr = bounds.max_stderr;
    {
        let flag = Arc::clone(&stdout_overflow);
        std::thread::spawn(move || {
            let report = drain(stdout, max_stdout, flag, true);
            let _ = stdout_tx.send(report);
        });
    }
    {
        let flag = Arc::clone(&stderr_overflow);
        std::thread::spawn(move || {
            let report = drain(stderr, max_stderr, flag, false);
            let _ = stderr_tx.send(report);
        });
    }

    // The waiter reaps the leader and reports its status; the main
    // loop bounds the wait and polls the overflow flags between
    // slices.
    let (exit_tx, exit_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = exit_tx.send(status);
    });

    let mut terminated: Option<ExchangeError> = None;
    let mut exit_status = None;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            terminated = Some(bound_terminate(
                pid,
                &started_at,
                ExchangeError::Deadline {
                    reason: "invocation deadline elapsed".into(),
                },
            ));
            break;
        }
        if stdout_overflow.load(Ordering::SeqCst) {
            terminated = Some(bound_terminate(
                pid,
                &started_at,
                ExchangeError::OutputOverflow,
            ));
            break;
        }
        if stderr_overflow.load(Ordering::SeqCst) {
            terminated = Some(bound_terminate(
                pid,
                &started_at,
                ExchangeError::StderrOverflow,
            ));
            break;
        }
        let slice = WAIT_SLICE.min(deadline.saturating_duration_since(now));
        match exit_rx.recv_timeout(slice) {
            Ok(status) => {
                exit_status = Some(status);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminated = Some(ExchangeError::Unknown {
                    reason: "wait channel closed before the leader's status".into(),
                });
                break;
            }
        }
    }

    if let Some(error) = terminated {
        // The bound already signalled the group; give the drains a
        // short grace to observe EOF, then report regardless — a
        // terminated invocation's partial output is never trusted.
        let _ = stdout_rx.recv_timeout(DRAIN_GRACE);
        let _ = stderr_rx.recv_timeout(DRAIN_GRACE);
        return Err(error);
    }

    // The leader's status arrived. A drain may have tripped its bound
    // in the same instant — the overflow still invalidates the whole
    // invocation, so check before classifying the exit.
    if stdout_overflow.load(Ordering::SeqCst) {
        let _ = stdout_rx.recv_timeout(DRAIN_GRACE);
        let _ = stderr_rx.recv_timeout(DRAIN_GRACE);
        return Err(ExchangeError::OutputOverflow);
    }
    if stderr_overflow.load(Ordering::SeqCst) {
        let _ = stdout_rx.recv_timeout(DRAIN_GRACE);
        let _ = stderr_rx.recv_timeout(DRAIN_GRACE);
        return Err(ExchangeError::StderrOverflow);
    }

    let status = exit_status.expect("exit status was received");
    // Classify the exit before draining: a nonzero status or signal
    // death is a transport failure regardless of how much output
    // arrived.
    let exit_code = match &status {
        Ok(s) if s.success() => Some(s.code().unwrap_or(0)),
        Ok(s) => {
            let code = s.code();
            // Still collect the drains for completeness of the
            // record, but the outcome is already decided.
            let _ = stdout_rx.recv_timeout(DRAIN_GRACE);
            let _ = stderr_rx.recv_timeout(DRAIN_GRACE);
            return Err(ExchangeError::Exit { exit_code: code });
        }
        Err(e) => {
            let _ = stdout_rx.recv_timeout(DRAIN_GRACE);
            let _ = stderr_rx.recv_timeout(DRAIN_GRACE);
            return Err(ExchangeError::Unknown {
                reason: format!("wait failed: {e}"),
            });
        }
    };

    // Normal exit proven. EOF on both streams is still required: a
    // surviving same-group descendant holds the pipes open, so first
    // sweep the group once, then give the drains their grace.
    sweep_group_stragglers(pid, &started_at);
    let out = match stdout_rx.recv_timeout(DRAIN_GRACE) {
        Ok(r) => r,
        Err(_) => {
            return Err(ExchangeError::Unknown {
                reason: "stdout did not reach EOF after the leader exited".into(),
            });
        }
    };
    let err = match stderr_rx.recv_timeout(DRAIN_GRACE) {
        Ok(r) => r,
        Err(_) => {
            return Err(ExchangeError::Unknown {
                reason: "stderr did not reach EOF after the leader exited".into(),
            });
        }
    };
    // A bound tripping during the grace window still invalidates.
    if out.overflowed {
        return Err(ExchangeError::OutputOverflow);
    }
    if err.overflowed {
        return Err(ExchangeError::StderrOverflow);
    }
    // A read failure is not EOF: the stream's completeness is
    // unproven, so the outcome is uncertain rather than clean.
    if let Some(kind) = out.error {
        return Err(ExchangeError::Unknown {
            reason: format!("stdout read failed: {kind:?}"),
        });
    }
    if let Some(kind) = err.error {
        return Err(ExchangeError::Unknown {
            reason: format!("stderr read failed: {kind:?}"),
        });
    }
    Ok(ExchangeOutput {
        stdout: out.kept,
        stderr_len: err.observed,
        exit_code,
    })
}

/// Terminate the owned process group for a bound violation and map
/// the signal plan onto the error to report: `Nothing` means the
/// leader identity could not be re-verified, so the invocation's
/// fate is unknown rather than cleanly bounded.
fn bound_terminate(pid: i32, recorded_lstart: &str, error: ExchangeError) -> ExchangeError {
    match supervisor::signal_probe_bound(pid, recorded_lstart) {
        ProbeSignalPlan::Nothing => ExchangeError::Unknown {
            reason: "bound elapsed but the leader identity could not be re-verified; \
                     nothing was signalled"
                .into(),
        },
        _ => {
            // Give the waiter a moment to collect the killed leader;
            // an orphaned status is harmless — the process is dead.
            error
        }
    }
}

/// After the leader's exit is proven, kill any same-group stragglers
/// still holding the output pipes so the drains can reach EOF. The
/// sweep applies the supervisor's proven plan: an absent or
/// unverifiable-but-unallocated pid leaves only this invocation's own
/// stragglers holding the group, while a live process at the pid under
/// a different start is a recycled foreign owner and is never
/// signalled. Members that escaped the group (`setsid`) are beyond
/// this sweep — the drain grace then reports the outcome uncertain
/// rather than claiming clean EOF.
fn sweep_group_stragglers(pid: i32, recorded_lstart: &str) {
    let plan = supervisor::probe_signal_plan(pid, recorded_lstart, &supervisor::lstart_of(pid));
    if plan != ProbeSignalPlan::Nothing {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}
