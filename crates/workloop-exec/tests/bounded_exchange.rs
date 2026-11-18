//! Contract tests for the bounded selector exchange (#201): real
//! child processes covering the issue's fixture matrix — delayed
//! output, excessive output, malformed payload, child-process
//! lifetime, and interrupted launch — plus the environment
//! allowlist and argv-without-shell guarantees.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use workloop_core::ports::WorkerEnv;
use workloop_exec::bounded::{self, ExchangeBounds, ExchangeError, SELECTOR_EXCHANGE_BOUNDS};

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-bounded-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write an executable `#!/bin/sh` fixture and return its path.
fn fixture_script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn exchange(
    argv: &[String],
    dir: &Path,
    env: &WorkerEnv,
    stdin: &[u8],
    bounds: &ExchangeBounds,
    timeout: Duration,
) -> Result<bounded::ExchangeOutput, ExchangeError> {
    bounded::exchange(argv, dir, env, stdin, bounds, timeout)
}

fn selector_env() -> WorkerEnv {
    bounded::selector_env()
}

#[test]
fn exchange_round_trips_stdin_and_stdout() {
    let dir = tmp_root("roundtrip");
    let script = fixture_script(&dir, "echo.sh", "cat");
    let request = b"{\"protocol_version\":\"1\"}".to_vec();
    let out = exchange(
        &[script],
        &dir,
        &selector_env(),
        &request,
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect("exchange succeeds");
    assert_eq!(out.stdout, request);
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stderr_len, 0);
}

#[test]
fn exchange_writes_large_stdin_without_deadlock() {
    let dir = tmp_root("largstdin");
    let script = fixture_script(&dir, "cat.sh", "cat");
    // Well past any pipe buffer: proves the writer thread drains
    // concurrently with the reader threads.
    let request = vec![b'x'; 512 * 1024];
    let out = exchange(
        &[script],
        &dir,
        &selector_env(),
        &request,
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(15),
    )
    .expect("exchange succeeds");
    assert_eq!(out.stdout.len(), request.len());
}

#[test]
fn exchange_passes_argv_without_shell() {
    let dir = tmp_root("noshell");
    // If argv went through a shell, the metacharacters would execute.
    let marker = dir.join("pwned");
    let script = fixture_script(&dir, "args.sh", "printf '%s\\n' \"$@\"");
    let arg = format!("a;touch {}", marker.display());
    let out = exchange(
        &[script.clone(), arg],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect("exchange succeeds");
    assert!(!marker.exists(), "shell metacharacters must not execute");
    assert!(String::from_utf8_lossy(&out.stdout).contains("a;touch"));
}

#[test]
fn exchange_environment_is_the_allowlist_only() {
    let dir = tmp_root("env");
    // selector_env() may only ever carry the fixed allowlist.
    let allowed = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];
    for (k, _) in selector_env().vars {
        assert!(allowed.contains(&k.as_str()), "unexpected env var {k}");
    }
    // env_clear is proven by a marker that cannot leak: the child's
    // environment contains exactly what WorkerEnv carried.
    let script = fixture_script(&dir, "env.sh", "env");
    let env = WorkerEnv {
        vars: vec![("ZZZ_BOUNDED_MARKER".into(), "yes".into())],
    };
    let out = exchange(
        &[script],
        &dir,
        &env,
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect("exchange succeeds");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ZZZ_BOUNDED_MARKER=yes"));
    // CARGO_* is set for every test binary and is never allowlisted.
    assert!(
        !text.lines().any(|l| l.starts_with("CARGO")),
        "inherited environment leaked into the child:\n{text}"
    );
}

#[test]
fn exchange_deadline_terminates_the_process_group() {
    let dir = tmp_root("timeout");
    let pgid_file = dir.join("pgid");
    // The leader records its pgid (== its pid under process_group(0))
    // then sleeps; a same-group child sleeps beside it.
    let script = fixture_script(
        &dir,
        "sleep.sh",
        &format!("echo $$ > {}\nsleep 60 &\nsleep 60\n", pgid_file.display()),
    );
    let start = Instant::now();
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_millis(1200),
    )
    .expect_err("deadline must fail the exchange");
    assert!(
        matches!(err, ExchangeError::Deadline { .. }),
        "expected Deadline, got {err:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    // The whole group must be gone: no process may still carry the
    // recorded pgid.
    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if group_member_count(pgid) == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "process group {pgid} survived");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn exchange_stdout_overflow_terminates() {
    let dir = tmp_root("outoverflow");
    let script = fixture_script(&dir, "flood.sh", "yes x | head -c 100000");
    let bounds = ExchangeBounds {
        max_stdin: SELECTOR_EXCHANGE_BOUNDS.max_stdin,
        max_stdout: 1024,
        max_stderr: SELECTOR_EXCHANGE_BOUNDS.max_stderr,
    };
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &bounds,
        Duration::from_secs(10),
    )
    .expect_err("overflow must fail the exchange");
    assert_eq!(err, ExchangeError::OutputOverflow);
}

#[test]
fn exchange_stderr_overflow_terminates() {
    let dir = tmp_root("eroverflow");
    let script = fixture_script(&dir, "flooderr.sh", "echo ok; yes x | head -c 100000 >&2");
    let bounds = ExchangeBounds {
        max_stdin: SELECTOR_EXCHANGE_BOUNDS.max_stdin,
        max_stdout: SELECTOR_EXCHANGE_BOUNDS.max_stdout,
        max_stderr: 1024,
    };
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &bounds,
        Duration::from_secs(10),
    )
    .expect_err("overflow must fail the exchange");
    assert_eq!(err, ExchangeError::StderrOverflow);
}

#[test]
fn exchange_malformed_payload_is_returned_for_the_caller() {
    let dir = tmp_root("malformed");
    // Transport succeeds; payload classification belongs to the
    // protocol layer, not the exchange.
    let script = fixture_script(&dir, "bad.sh", "printf 'not-json{{{'");
    let out = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect("transport succeeds");
    assert_eq!(out.stdout, b"not-json{{{");
    assert_eq!(out.exit_code, Some(0));
}

#[test]
fn exchange_nonzero_exit_and_signal_are_transport_failures() {
    let dir = tmp_root("exit");
    let nonzero = fixture_script(&dir, "fail.sh", "exit 3");
    let err = exchange(
        &[nonzero],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect_err("nonzero exit fails");
    assert_eq!(err, ExchangeError::Exit { exit_code: Some(3) });

    let signaled = fixture_script(&dir, "sig.sh", "kill -TERM $$");
    let err = exchange(
        &[signaled],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect_err("signal death fails");
    assert_eq!(err, ExchangeError::Exit { exit_code: None });
}

#[test]
fn exchange_interrupted_launch_reports_launch_failure() {
    let dir = tmp_root("launch");
    let err = exchange(
        &["/nonexistent/definitely-missing-binary".into()],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect_err("missing binary fails");
    assert!(matches!(err, ExchangeError::Launch { .. }));

    let err = exchange(
        &[],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect_err("empty argv fails");
    assert!(matches!(err, ExchangeError::Launch { .. }));
}

#[test]
fn exchange_rejects_oversized_stdin_before_spawn() {
    let dir = tmp_root("stdincap");
    let script = fixture_script(&dir, "cat.sh", "cat");
    let bounds = ExchangeBounds {
        max_stdin: 16,
        ..SELECTOR_EXCHANGE_BOUNDS
    };
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"this request is far larger than sixteen bytes",
        &bounds,
        Duration::from_secs(10),
    )
    .expect_err("oversized request fails");
    assert!(matches!(err, ExchangeError::Launch { .. }));
}

#[test]
fn exchange_stderr_is_bounded_and_counted_not_retained() {
    let dir = tmp_root("stderrcount");
    let script = fixture_script(&dir, "diag.sh", "echo result; echo diagnostic-line >&2");
    let out = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(10),
    )
    .expect("exchange succeeds");
    assert_eq!(out.stdout, b"result\n");
    assert_eq!(out.stderr_len, "diagnostic-line\n".len());
}

#[test]
fn exchange_kills_same_group_descendants_holding_pipes() {
    let dir = tmp_root("straggler");
    // The leader exits cleanly but a same-group child keeps the
    // output pipes open: the post-exit sweep must kill it so EOF is
    // proven rather than waited out.
    let script = fixture_script(&dir, "orphan.sh", "echo leader-out; sleep 30 &");
    let start = Instant::now();
    let out = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(15),
    )
    .expect("exchange succeeds after the group sweep");
    assert_eq!(out.stdout, b"leader-out\n");
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn exchange_nonzero_exit_still_sweeps_same_group_stragglers() {
    let dir = tmp_root("exit-straggler");
    let pgid_file = dir.join("pgid");
    // The leader exits nonzero while a same-group descendant keeps
    // running: the failure path must sweep the group too, or the
    // straggler survives the bounded invocation holding the pipes.
    let script = fixture_script(
        &dir,
        "exit-child.sh",
        &format!("echo $$ > {}\nsleep 30 &\nexit 3\n", pgid_file.display()),
    );
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(15),
    )
    .expect_err("nonzero exit must fail the exchange");
    assert!(
        matches!(err, ExchangeError::Exit { .. }),
        "expected Exit, got {err:?}"
    );
    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if group_member_count(pgid) == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "process group {pgid} survived a failed exchange"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn exchange_reports_uncertain_descendants_as_unknown() {
    let dir = tmp_root("escapee");
    if Command::new("python3").arg("--version").output().is_err() {
        // No setsid-capable helper on this host; the sweep path is
        // covered by the same-group test above.
        return;
    }
    // A descendant that escaped the process group (setsid) still
    // holds stdout: EOF can never be proven, so the outcome is
    // unknown rather than a clean success. The leader waits for the
    // escapee's readiness marker so the group sweep cannot catch it
    // mid-setsid.
    let ready = dir.join("escapee-ready");
    let script = fixture_script(
        &dir,
        "escape.sh",
        &format!(
            "python3 -c 'import os\nos.setsid()\nopen(\"{}\",\"w\").write(\"1\")\nos.execlp(\"sleep\",\"sleep\",\"4\")' &\nwhile [ ! -f {} ]; do sleep 0.05; done\necho leader-out",
            ready.display(),
            ready.display()
        ),
    );
    let start = Instant::now();
    let err = exchange(
        &[script],
        &dir,
        &selector_env(),
        b"",
        &SELECTOR_EXCHANGE_BOUNDS,
        Duration::from_secs(15),
    )
    .expect_err("unproven EOF must not succeed");
    assert!(
        matches!(err, ExchangeError::Unknown { .. }),
        "expected Unknown, got {err:?}"
    );
    // Bounded: the drain grace, not the child's lifetime, decides.
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// Count processes whose process group id equals `pgid`.
fn group_member_count(pgid: i32) -> usize {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,pgid="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<i32>().ok())
                == Some(pgid)
        })
        .count()
}
