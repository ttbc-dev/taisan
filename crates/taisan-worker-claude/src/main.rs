//! `taisan-worker-claude` — the public reference Worker adapter for
//! `taisan.worker.declaration/1` + `taisan.worker.request/2` +
//! `taisan.worker.report/2` over the argv-v2 transport.
//!
//! The adapter drives the Claude Code CLI in headless mode
//! (`claude -p --output-format stream-json`) as the coding-agent
//! runtime. One Invocation is one runtime process tree in its own
//! process group; the shim owns the deadline kill for that group and
//! the supervisor's descendant snapshot still covers it on an external
//! stop. The shim holds no authority of its own: it echoes the request
//! binding, claims only what it mechanically observed (exit codes, the
//! produced `HEAD`, captured output digests), and can never publish.
//!
//! Transport (exactly two commands, no shell, no stdin protocol):
//!
//! ```text
//! taisan-worker-claude [flags] describe
//! taisan-worker-claude [flags] run --request <path> --out <dir>
//! ```
//!
//! Flags bound by installation config (`worker_argv`):
//!   --runtime <argv0>           runtime executable (default `claude`)
//!   --runtime-arg <arg>         extra leading argv for the runtime
//!                               (repeatable; e.g. a wrapper's flags)
//!   --runtime-auth <path>       file containing the runtime credential
//!                               (default /taisan/runtime-auth when
//!                               present — the sandbox mount point)
//!   --provider-config <path>    JSON provider config (default
//!                               /taisan/provider-config when present):
//!                               {"model": "...", "endpoint": "...",
//!                                "proxy_endpoint": "..."}
//!
//! `describe` is effect-free and bounded: it probes the runtime's
//! `--version` under a short deadline and prints one declaration JSON
//! object. `run` reads one request file, spawns the runtime once,
//! captures a bounded `runtime.log`, runs the declared verification
//! argv itself, and writes one bounded `report.json`. Any exit before
//! a report is written is the contract's `absent` class.

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use workloop_core::{
    domain::{canonical_json, sha256_hex},
    worker_v2::{
        FailedEvidenceBody, InvariantSetBody, IssueSnapshotBody, MAX_REPORT_MEMBERS,
        ReviewSourceBody, TaskBody, WORKER_DECLARATION_V1_FORMAT, WORKER_REPORT_V2_FORMAT,
        WORKER_REQUEST_V2_FORMAT, WorkerRequestV2, report_canonical_body, report_v2_digest,
    },
};

const ADAPTER: &str = "taisan-worker-claude";
const DEFAULT_RUNTIME: &str = "claude";
const DEFAULT_PRODUCT: &str = "claude-code";
const SANDBOX_RUNTIME_AUTH: &str = "/taisan/runtime-auth";
const SANDBOX_PROVIDER_CONFIG: &str = "/taisan/provider-config";
/// Bytes the request body itself may occupy — requests are small;
/// a corrupt giant file fails fast instead of being parsed.
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// Runtime `--version` probe deadline for `describe`/preflight.
const VERSION_PROBE_SECONDS: u64 = 10;
/// Cap on the captured `--version` output before it is read.
const VERSION_OUTPUT_CAP: usize = 8 * 1024;
/// A single stream-json line larger than this is logged verbatim but
/// never parsed — a runaway line cannot exhaust parse effort.
const MAX_STREAM_LINE: usize = 256 * 1024;
/// Per-verification-command output capture bound (each claimed check
/// reports only the digest of this capture).
const CHECK_OUTPUT_CAP: usize = 1024 * 1024;
/// Grace between SIGTERM and SIGKILL on the runtime process group.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// Poll interval while the runtime runs.
const POLL: Duration = Duration::from_millis(50);

const USAGE: &str = "taisan-worker-claude [--runtime <argv0>] [--runtime-arg <arg>]... \
[--runtime-auth <path>] [--provider-config <path>] <describe | run --request <path> --out <dir> | --version>";

/// The closed, fail-closed invocation flags. Everything not listed is
/// rejected — a mistyped flag must never silently reshape the launch.
struct Config {
    runtime_argv: Vec<String>,
    runtime_auth: PathBuf,
    provider_config: PathBuf,
}

fn parse_args(args: &[String]) -> Result<(Config, Vec<String>), String> {
    let mut cfg = Config {
        runtime_argv: vec![DEFAULT_RUNTIME.to_string()],
        runtime_auth: PathBuf::from(SANDBOX_RUNTIME_AUTH),
        provider_config: PathBuf::from(SANDBOX_PROVIDER_CONFIG),
    };
    let mut runtime_argv_set = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--runtime" => {
                let v = args.get(i + 1).ok_or("--runtime needs a value")?;
                if !runtime_argv_set {
                    cfg.runtime_argv.clear();
                    runtime_argv_set = true;
                }
                cfg.runtime_argv.push(v.clone());
                i += 2;
            }
            "--runtime-arg" => {
                let v = args.get(i + 1).ok_or("--runtime-arg needs a value")?;
                cfg.runtime_argv.push(v.clone());
                i += 2;
            }
            "--runtime-auth" => {
                cfg.runtime_auth =
                    PathBuf::from(args.get(i + 1).ok_or("--runtime-auth needs a value")?);
                i += 2;
            }
            "--provider-config" => {
                cfg.provider_config =
                    PathBuf::from(args.get(i + 1).ok_or("--provider-config needs a value")?);
                i += 2;
            }
            _token => {
                // First non-flag token is the subcommand; the rest is
                // its argv. The supervisor appends exactly this tail.
                return Ok((cfg, args[i..].to_vec()));
            }
        }
    }
    Ok((cfg, Vec::new()))
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            2
        }
        Ok((cfg, tail)) => match tail.first().map(String::as_str) {
            Some("describe") if tail.len() == 1 => cmd_describe(&cfg),
            Some("--version") if tail.len() == 1 => cmd_version(&cfg),
            Some("run") => cmd_run(&cfg, &tail[1..]),
            _ => {
                eprintln!("{USAGE}");
                2
            }
        },
    };
    std::process::exit(code);
}

fn cmd_version(cfg: &Config) -> i32 {
    // The supervisor's installation preflight probes
    // `<worker_argv...> --version` to prove the bound runtime exists —
    // forward the probe so a missing runtime fails here, not mid-run.
    match probe_runtime_version(cfg) {
        Ok(v) => {
            println!("{ADAPTER} {} (runtime {v})", env!("CARGO_PKG_VERSION"));
            0
        }
        Err(e) => {
            eprintln!("runtime probe failed: {e}");
            1
        }
    }
}

fn cmd_describe(cfg: &Config) -> i32 {
    // Effect-free and bounded: the only probe is the runtime's own
    // --version under a short deadline.
    let version = match probe_runtime_version(cfg) {
        Ok(v) => v,
        Err(e) => {
            // A failed probe is `adapter.declaration_unavailable` at the
            // probe boundary — a non-zero exit with a stderr note, never
            // a fabricated declaration.
            eprintln!("runtime probe failed: {e}");
            return 1;
        }
    };
    let declaration = json!({
        "format_version": WORKER_DECLARATION_V1_FORMAT,
        "adapter": ADAPTER,
        "adapter_version": env!("CARGO_PKG_VERSION"),
        "runtime": {"product": DEFAULT_PRODUCT, "version": version},
        "protocol_versions": [WORKER_REQUEST_V2_FORMAT, WORKER_REPORT_V2_FORMAT],
        "roles": ["coding", "fix", "invariant_check", "review"],
        "capabilities": [
            "invocation_transport.argv-v2",
            "process_control",
            "report_json",
            "workspace_read_only",
            "workspace_write",
        ],
        "resume": "unsupported",
        "checkpoints": "workspace_commits",
        "runtime_artifact_store": null,
        "local_retries": "runtime_internal",
    });
    println!(
        "{}",
        serde_json::to_string(&declaration).expect("declaration serializes")
    );
    0
}

/// The bounded `--version` probe. Spawn may fail (binary missing);
/// the child may hang; its output may be noisy. All three collapse to
/// one refusal — the caller never receives a partial version.
fn probe_runtime_version(cfg: &Config) -> Result<String, String> {
    let mut cmd = Command::new(&cfg.runtime_argv[0]);
    cmd.args(&cfg.runtime_argv[1..])
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(VERSION_PROBE_SECONDS);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("wait: {e}"))? {
            if !status.success() {
                return Err(format!("--version exited {status}"));
            }
            let mut out = String::new();
            child
                .stdout
                .take()
                .map(|s| s.take(VERSION_OUTPUT_CAP as u64).read_to_string(&mut out))
                .transpose()
                .map_err(|e| format!("read: {e}"))?;
            let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let version: String = line.trim().chars().take(256).collect();
            if version.is_empty() {
                return Err("empty --version output".into());
            }
            return Ok(version);
        }
        if Instant::now() > deadline {
            kill_tree(&mut child);
            return Err("version probe timed out".into());
        }
        thread::sleep(POLL);
    }
}

// ----------------------------------------------------------------------
// run
// ----------------------------------------------------------------------

/// Provider configuration the supervisor validated and staged
/// read-only. It may name a model and endpoints; it never carries the
/// credential itself.
#[derive(serde::Deserialize, Default)]
struct ProviderConfig {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    proxy_endpoint: Option<String>,
}

fn cmd_run(cfg: &Config, argv: &[String]) -> i32 {
    let (request_path, out_dir) = match parse_run_args(argv) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    match run_invocation(cfg, &request_path, &out_dir) {
        Ok(code) => code,
        // Any failure before `report.json` is durable is the contract's
        // `absent` class — never a silently minted fallback report.
        Err(e) => {
            eprintln!("run failed: {e}");
            1
        }
    }
}

fn parse_run_args(argv: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let mut request = None;
    let mut out = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--request" => {
                request = Some(PathBuf::from(
                    argv.get(i + 1).ok_or("--request needs a value")?,
                ));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(argv.get(i + 1).ok_or("--out needs a value")?));
                i += 2;
            }
            other => return Err(format!("unexpected run argument {other}")),
        }
    }
    Ok((
        request.ok_or("run needs --request <path>")?,
        out.ok_or("run needs --out <dir>")?,
    ))
}

fn run_invocation(cfg: &Config, request_path: &Path, out_dir: &Path) -> Result<i32, String> {
    let request_bytes = read_bounded(request_path, MAX_REQUEST_BYTES)?;
    // Closed-schema parse + digest verification: a request that fails
    // never produces a process or a report — `absent`, by contract.
    let request =
        WorkerRequestV2::parse(&request_bytes).map_err(|e| format!("request rejected: {e:?}"))?;
    fs::create_dir_all(out_dir).map_err(|e| format!("create out dir: {e}"))?;

    let workspace = workspace_dir(&request);
    let input_dir = request_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("input");
    let prompt = match build_prompt(&request, &input_dir) {
        Ok(p) => p,
        Err(reason) => {
            write_simple_report(&request, out_dir, "unavailable", &reason)?;
            return Ok(0);
        }
    };

    let provider = load_provider_config(cfg);
    let runtime_env = runtime_environment(cfg, &provider);
    let mcp_path = out_dir.join("taisan-empty-mcp.json");
    fs::write(&mcp_path, b"{\"mcpServers\":{}}\n").map_err(|e| format!("mcp config: {e}"))?;

    let read_only = request.workspace.read_only;
    let mut argv = cfg.runtime_argv.clone();
    argv.extend([
        "-p".into(),
        prompt,
        "--output-format".into(),
        "stream-json".into(),
        "--max-turns".into(),
        request.budget.max_turns.to_string(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        mcp_path.to_string_lossy().into_owned(),
        // Read-only roles run in plan mode so the runtime's edit tools
        // are denied even where containment is unavailable; writers
        // may edit and make local git commits only.
        "--permission-mode".into(),
        if read_only { "plan" } else { "acceptEdits" }.into(),
    ]);
    if !read_only {
        argv.extend(["--allowedTools".into(), "Bash(git *)".into()]);
    }
    if let Some(model) = provider.model.as_deref() {
        argv.extend(["--model".into(), model.to_string()]);
    }

    let budget = request.budget.clone();
    let outcome = run_runtime(&argv, &workspace, &runtime_env, &budget, out_dir)?;
    write_report(&request, out_dir, &workspace, &outcome, &provider)?;
    Ok(0)
}

/// The runtime works inside the granted workspace. Inside containment
/// that is the sandbox mount `/workspace`; uncontained it is whatever
/// the supervisor bound. When the mount cannot be resolved the shim
/// still stays inside its own cwd — never somewhere unspecified.
fn workspace_dir(request: &WorkerRequestV2) -> PathBuf {
    let mount = PathBuf::from(&request.workspace.mount);
    if mount.is_dir() {
        mount
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn load_provider_config(cfg: &Config) -> ProviderConfig {
    let Ok(bytes) = fs::read(&cfg.provider_config) else {
        return ProviderConfig::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// The environment the runtime child sees. The runtime credential is
/// resolved by reference and staged only here — it is set on this one
/// child's env, never on the shim's own process, so verification
/// commands and the report never observe it. Publication credentials
/// are stripped outright.
fn runtime_environment(cfg: &Config, provider: &ProviderConfig) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = env::vars()
        .filter(|(k, _)| k != "GH_TOKEN" && k != "GITHUB_TOKEN")
        .collect();
    if let Ok(token) = fs::read_to_string(&cfg.runtime_auth) {
        let token = token.trim();
        if !token.is_empty() {
            env.retain(|(k, _)| k != "ANTHROPIC_API_KEY");
            env.push(("ANTHROPIC_API_KEY".into(), token.to_string()));
        }
    }
    if let Some(endpoint) = provider.endpoint.as_deref() {
        env.push(("ANTHROPIC_BASE_URL".into(), endpoint.to_string()));
    }
    if let Some(proxy) = provider.proxy_endpoint.as_deref() {
        env.push(("HTTPS_PROXY".into(), proxy.to_string()));
        env.push(("https_proxy".into(), proxy.to_string()));
    }
    env
}

// ----------------------------------------------------------------------
// prompts
// ----------------------------------------------------------------------

/// The runtime's machine-readable tail. Writers are claimed by the
/// shim's own observations (git HEAD, check exit codes) so the block is
/// optional for them; readers have no other output channel and the
/// block is required.
const RESULT_BLOCK_TAG: &str = "taisan-result";

fn build_prompt(request: &WorkerRequestV2, input_dir: &Path) -> Result<String, String> {
    match &request.task {
        TaskBody::Coding {
            issues,
            constraints,
            verification_argv,
            ..
        } => {
            let goal = read_input(input_dir, "goal.md")?;
            let mut p = String::from(
                "You are a coding agent inside a managed workspace. Implement the goal below by editing files and committing your work locally with git.\n\n## Goal\n",
            );
            p.push_str(&goal);
            push_issues(&mut p, input_dir, issues.len())?;
            push_constraints(&mut p, constraints);
            if !verification_argv.is_empty() {
                p.push_str(
                    "\n## Verification\nThe supervisor will run these checks on your commits:\n",
                );
                for argv in verification_argv {
                    p.push_str(&format!("- `{}`\n", argv.join(" ")));
                }
            }
            p.push_str(WRITER_FOOTER);
            Ok(p)
        }
        TaskBody::Fix {
            issues,
            predecessor,
            origin,
            constraints,
            verification_argv,
            ..
        } => {
            let goal = read_input(input_dir, "goal.md")?;
            let mut p = String::from(
                "You are a coding agent inside a managed workspace. Repair the existing work described below: edit files and commit locally with git. Do not rewrite unrelated history.\n\n## Repair goal\n",
            );
            p.push_str(&goal);
            p.push_str(&format!(
                "\n## Predecessor\nartifact id: {}\nproduced commit: {}\nmanifest digest: {}\n",
                predecessor.artifact_id, predecessor.produced, predecessor.manifest_digest
            ));
            match origin.as_ref() {
                workloop_core::worker_v2::FixOrigin::Review {
                    round,
                    judgment,
                    findings,
                    ..
                } => {
                    p.push_str(&format!(
                        "\n## Review findings to fix (round {round}, judgment {judgment})\n"
                    ));
                    for (i, f) in findings.iter().enumerate() {
                        p.push_str(&format!(
                            "- {}. {}\n",
                            i + 1,
                            f.summary.as_deref().unwrap_or("(sealed finding)")
                        ));
                    }
                }
                workloop_core::worker_v2::FixOrigin::Prepublication { failed_evidence } => {
                    p.push_str("\n## Failed evidence to repair\n");
                    for (i, e) in failed_evidence.iter().enumerate() {
                        match read_input(input_dir, &format!("evidence-{i}.json"))
                            .ok()
                            .and_then(|b| FailedEvidenceBody::parse(b.as_bytes()).ok())
                        {
                            Some(body) => p.push_str(&format!(
                                "- {}. requirement `{}` ran `{:?}` → {}:\n  {}\n",
                                i + 1,
                                body.requirement,
                                body.argv,
                                body.result,
                                body.output.chars().take(2000).collect::<String>()
                            )),
                            None => p.push_str(&format!(
                                "- {}. sealed evidence {} ({})\n",
                                i + 1,
                                e.evidence_id,
                                e.kind
                            )),
                        }
                    }
                }
            }
            push_issues(&mut p, input_dir, issues.len())?;
            push_constraints(&mut p, constraints);
            if !verification_argv.is_empty() {
                p.push_str(
                    "\n## Verification\nThe supervisor will run these checks on your commits:\n",
                );
                for argv in verification_argv {
                    p.push_str(&format!("- `{}`\n", argv.join(" ")));
                }
            }
            p.push_str(WRITER_FOOTER);
            Ok(p)
        }
        TaskBody::InvariantCheck {
            artifact,
            invariant_set,
        } => {
            let bytes = read_input(input_dir, "invariant-set.json")?;
            let set = InvariantSetBody::parse(bytes.as_bytes())
                .map_err(|e| format!("missing_required_input: invariant set: {e:?}"))?;
            let mut p = String::from(
                "You are an assessor inside a READ-ONLY managed workspace. Do not modify any file. Assess the artifact against every invariant below and report one verdict per invariant.\n\n",
            );
            p.push_str(&format!(
                "## Artifact under check\nid: {}\nproduced commit: {}\nmanifest digest: {}\n\n## Invariant set {} (revision {})\n",
                artifact.artifact_id,
                artifact.produced,
                artifact.manifest_digest,
                invariant_set.id,
                invariant_set.revision
            ));
            for item in &set.items {
                p.push_str(&format!("- `{item}`\n"));
            }
            p.push_str(&format!(
                "\nInspect the workspace and, for each invariant id, decide `conforms`, `violates`, or `inconclusive`. End your reply with exactly one fenced block:\n\n```{RESULT_BLOCK_TAG}\n{{\"invariant_items\":[{{\"id\":\"<invariant id>\",\"verdict\":\"conforms|violates|inconclusive\"}}]}}\n```\n"
            ));
            Ok(p)
        }
        TaskBody::CodeReview {
            target,
            review_source,
        } => {
            let bytes = read_input(input_dir, "review-source.json")?;
            let source = ReviewSourceBody::parse(bytes.as_bytes())
                .map_err(|e| format!("missing_required_input: review source: {e:?}"))?;
            let mut p = String::from(
                "You are a code reviewer inside a READ-ONLY managed workspace. Do not modify any file. Review the diff below and report concrete findings.\n\n",
            );
            p.push_str(&format!(
                "## Diff target\nchange: {}\nbase: {}\nhead: {}\ndiff_digest: {}\nreview source digest: {}\n\nRun `git diff {} {}` to see the change under review.\n",
                target.change,
                target.base,
                target.head,
                target.diff_digest,
                review_source.digest,
                target.base,
                target.head
            ));
            if let Some(round) = &source.prior_round {
                p.push_str(&format!(
                    "\n## Prior sealed round {} (for context; do not re-report superseded findings)\n",
                    round.id
                ));
                for o in &round.occurrences {
                    if let Some(s) = &o.summary {
                        p.push_str(&format!("- {}\n", s.chars().take(500).collect::<String>()));
                    }
                }
            }
            p.push_str(&format!(
                "\nReport only actionable findings on this exact diff. End your reply with exactly one fenced block (empty findings list when the diff is clean):\n\n```{RESULT_BLOCK_TAG}\n{{\"findings\":[{{\"key\":\"<stable key>\",\"summary\":\"<what is wrong and where>\",\"severity\":\"low|medium|high\",\"position\":\"<file:line or hunk>\"}}]}}\n```\n"
            ));
            Ok(p)
        }
    }
}

const WRITER_FOOTER: &str = "\n## Output contract\nWork directly in this workspace. Commit your changes with git before you finish (local commits only — never push). Optionally end your reply with one fenced block:\n\n```taisan-result\n{\"notes\":\"<short summary of what you did>\"}\n```\n";

fn read_input(input_dir: &Path, name: &str) -> Result<String, String> {
    let path = input_dir.join(name);
    fs::read_to_string(&path)
        .map_err(|e| format!("missing_required_input: {}: {e}", path.display()))
}

fn push_issues(p: &mut String, input_dir: &Path, count: usize) -> Result<(), String> {
    if count == 0 {
        return Ok(());
    }
    p.push_str("\n## Source issues\n");
    for i in 0..count {
        let bytes = read_input(input_dir, &format!("issue-{i}.json"))?;
        let snapshot = IssueSnapshotBody::parse(bytes.as_bytes())
            .map_err(|e| format!("missing_required_input: issue-{i}.json: {e:?}"))?;
        p.push_str(&format!(
            "\n### Issue {} ({})\n{}\n{}\n",
            snapshot.issue.issue, snapshot.issue.repository, snapshot.title, snapshot.body
        ));
    }
    Ok(())
}

fn push_constraints(p: &mut String, constraints: &[String]) {
    if constraints.is_empty() {
        return;
    }
    p.push_str("\n## Constraints\n");
    for c in constraints {
        p.push_str(&format!("- {c}\n"));
    }
}

// ----------------------------------------------------------------------
// runtime
// ----------------------------------------------------------------------

/// What the bounded runtime execution produced — the shim's own
/// mechanical observations, never the runtime's self-report.
struct RuntimeOutcome {
    /// The result event's subtype, when one arrived (`success`,
    /// `error_max_turns`, `error_during_execution`, ...).
    subtype: Option<String>,
    is_error: bool,
    exit_success: bool,
    /// The runtime's final text — the only place a result block may live.
    result_text: String,
    turns: Option<u64>,
    elapsed_ms: Option<u64>,
    /// `deadline_reached` when the shim killed the tree on the request
    /// budget; `output_limit_reached` when the log bound forced it.
    limit: Option<&'static str>,
    log_bytes: u64,
    log_truncated: bool,
    log_digest: String,
    spawn_error: Option<String>,
}

fn run_runtime(
    argv: &[String],
    cwd: &Path,
    runtime_env: &[(String, String)],
    budget: &workloop_core::worker_v2::RequestBudget,
    out_dir: &Path,
) -> Result<RuntimeOutcome, String> {
    let log_path = out_dir.join("runtime.log");
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(runtime_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group: the shim's deadline kill covers the whole
    // runtime tree, and the supervisor's descendant snapshot still
    // records every member for its own stop path.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(RuntimeOutcome {
                subtype: None,
                is_error: true,
                exit_success: false,
                result_text: String::new(),
                turns: None,
                elapsed_ms: None,
                limit: None,
                log_bytes: 0,
                log_truncated: false,
                log_digest: sha256_hex("worker-runtime-log-v1", &[]),
                spawn_error: Some(format!("spawn {}: {e}", argv[0])),
            });
        }
    };
    #[cfg(unix)]
    let pgid = child.id() as i32;

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let mut streams: Vec<(bool, Box<dyn Read + Send>)> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        streams.push((true, Box::new(stdout)));
    }
    if let Some(stderr) = child.stderr.take() {
        streams.push((false, Box::new(stderr)));
    }
    for (is_stdout, stream) in streams {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send((is_stdout, buf.clone())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    drop(tx);

    let mut log = fs::File::create(&log_path).map_err(|e| format!("runtime.log: {e}"))?;
    let log_cap = budget.max_log_bytes;
    let mut log_bytes = 0u64;
    let mut log_truncated = false;
    let mut line_buf: Vec<u8> = Vec::new();
    let mut last_result: Option<Value> = None;
    let mut limit: Option<&'static str> = None;
    let mut killed_at: Option<Instant> = None;
    let started = Instant::now();
    let now_unix = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    let deadline = {
        let by_budget = started + Duration::from_secs(budget.max_runtime_seconds);
        match budget.deadline_unix_seconds.checked_sub(now_unix()) {
            Some(rem) if rem <= 0 => Instant::now(),
            Some(rem) if (rem as u64) < budget.max_runtime_seconds => {
                Instant::now() + Duration::from_secs(rem as u64)
            }
            _ => by_budget,
        }
    };

    loop {
        match rx.recv_timeout(POLL) {
            Ok((is_stdout, bytes)) => {
                if !log_truncated {
                    if log_bytes + bytes.len() as u64 > log_cap {
                        let keep = log_cap.saturating_sub(log_bytes) as usize;
                        let _ = log.write_all(&bytes[..keep]);
                        log_bytes += keep as u64;
                        log_truncated = true;
                        let _ = log.write_all(b"\n[taisan] log truncated at bound\n");
                    } else {
                        let _ = log.write_all(&bytes);
                        log_bytes += bytes.len() as u64;
                    }
                }
                if is_stdout {
                    line_buf.extend_from_slice(&bytes);
                    while let Some(pos) = line_buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = line_buf.drain(..=pos).collect();
                        if line.len() <= MAX_STREAM_LINE
                            && let Ok(v) = serde_json::from_slice::<Value>(&line)
                            && v.get("type").and_then(|t| t.as_str()) == Some("result")
                        {
                            last_result = Some(v);
                        }
                    }
                    if line_buf.len() > MAX_STREAM_LINE {
                        line_buf.clear();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if killed_at.is_none() && Instant::now() > deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            let _ = child.kill();
            killed_at = Some(Instant::now());
            limit = Some("deadline_reached");
        }
        if killed_at.is_none() && log_truncated {
            // The log bound is the Worker's own output bound — a
            // runtime that floods output is stopped, not silenced.
            #[cfg(unix)]
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            let _ = child.kill();
            killed_at = Some(Instant::now());
            limit = Some("output_limit_reached");
        }
        if let Some(at) = killed_at
            && at.elapsed() > KILL_GRACE
        {
            #[cfg(unix)]
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            let _ = child.kill();
        }
    }
    let exit_status = child.wait().map_err(|e| format!("wait: {e}"))?;
    drop(log);
    let log_digest = sha256_hex(
        "worker-runtime-log-v1",
        &fs::read(&log_path).unwrap_or_default(),
    );

    let mut outcome = RuntimeOutcome {
        subtype: None,
        is_error: false,
        exit_success: exit_status.success(),
        result_text: String::new(),
        turns: None,
        elapsed_ms: None,
        limit,
        log_bytes,
        log_truncated,
        log_digest,
        spawn_error: None,
    };
    if let Some(result) = &last_result {
        outcome.subtype = result
            .get("subtype")
            .and_then(|v| v.as_str())
            .map(String::from);
        outcome.is_error = result
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        outcome.result_text = result
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        outcome.turns = result.get("num_turns").and_then(|v| v.as_u64());
        outcome.elapsed_ms = result.get("duration_ms").and_then(|v| v.as_u64());
    }
    Ok(outcome)
}

#[cfg(unix)]
fn kill_tree(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ----------------------------------------------------------------------
// report
// ----------------------------------------------------------------------

/// Extract the last ` ```taisan-result ` fenced block from the
/// runtime's final text.
fn extract_result_block(text: &str) -> Option<Value> {
    let open = format!("```{RESULT_BLOCK_TAG}");
    let mut found = None;
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let body = match after.find("```") {
            Some(end) => &after[..end],
            None => break,
        };
        if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
            found = Some(v);
        }
        rest = &after[body.len() + 3..];
    }
    found
}

fn write_report(
    request: &WorkerRequestV2,
    out_dir: &Path,
    workspace: &Path,
    outcome: &RuntimeOutcome,
    provider: &ProviderConfig,
) -> Result<(), String> {
    let read_only = request.workspace.read_only;
    let block = extract_result_block(&outcome.result_text);

    // claims the shim mechanically owns
    let claimed_head = if read_only {
        None
    } else {
        git_rev_parse_head(workspace)
    };
    let claimed_checks = if read_only {
        Vec::new()
    } else {
        run_verification(request, workspace, outcome.limit.is_some())
    };

    let mut findings = Vec::new();
    let mut invariant_items = Vec::new();
    let mut member_error: Option<String> = None;
    match &request.task {
        TaskBody::CodeReview { .. } => {
            if let Some(b) = &block {
                match collect_members(b, "findings", true) {
                    Ok(m) => findings = m,
                    Err(e) => member_error = Some(e),
                }
            } else if outcome.limit.is_none() && !outcome.is_error && outcome.exit_success {
                member_error = Some("runtime returned no result block".into());
            }
        }
        TaskBody::InvariantCheck { .. } => {
            if let Some(b) = &block {
                match collect_members(b, "invariant_items", false) {
                    Ok(m) => invariant_items = m,
                    Err(e) => member_error = Some(e),
                }
            } else if outcome.limit.is_none() && !outcome.is_error && outcome.exit_success {
                member_error = Some("runtime returned no result block".into());
            }
        }
        _ => {}
    }

    let checks_failed = claimed_checks
        .iter()
        .any(|c| c["status"].as_str() == Some("failed"));
    let auth_hint = outcome.result_text.to_ascii_lowercase().contains("auth");

    let (status, reason) = if outcome.spawn_error.is_some() {
        ("unavailable", "provider_unavailable")
    } else if let Some(limit) = outcome.limit {
        ("limit_reached", limit)
    } else if outcome.subtype.as_deref() == Some("error_max_turns") {
        ("limit_reached", "max_turns_reached")
    } else if outcome.is_error || !outcome.exit_success {
        if auth_hint {
            ("unavailable", "auth_unavailable")
        } else {
            ("unavailable", "provider_unavailable")
        }
    } else if member_error.is_some() {
        ("failed", "internal_error")
    } else {
        match &request.task {
            TaskBody::Coding { .. } | TaskBody::Fix { .. } => {
                if checks_failed {
                    ("negative", "checks_failed")
                } else {
                    ("success", "completed")
                }
            }
            TaskBody::InvariantCheck { .. } => {
                if invariant_items
                    .iter()
                    .any(|m| m.get("verdict").and_then(|v| v.as_str()) == Some("violates"))
                {
                    ("negative", "invariant_violated")
                } else {
                    ("success", "completed")
                }
            }
            TaskBody::CodeReview { .. } => {
                if findings.is_empty() {
                    ("success", "completed")
                } else {
                    ("negative", "findings_reported")
                }
            }
        }
    };

    let continuation_commit = if read_only {
        None
    } else {
        claimed_head.clone()
    };
    let mut report = json!({
        "format_version": WORKER_REPORT_V2_FORMAT,
        "request_id": request.request_id,
        "invocation_id": request.invocation_id,
        "request_digest": request.request_digest,
        "role": request.role,
        "status": status,
        "reason": {"code": reason},
        "claimed_head": claimed_head,
        "claimed_checks": claimed_checks,
        "findings": findings,
        "invariant_items": invariant_items,
        "continuation": {
            "workspace_commit": continuation_commit,
            "report_refs": [],
        },
        "runtime_usage": {
            "turns": outcome.turns,
            "elapsed_ms": outcome.elapsed_ms,
            "runtime_product": match provider.model.as_deref() {
                Some(m) => format!("{DEFAULT_PRODUCT}({m})"),
                None => DEFAULT_PRODUCT.to_string(),
            },
        },
        "log": {
            "path": "runtime.log",
            "bytes": outcome.log_bytes,
            "truncated": outcome.log_truncated,
            "digest": outcome.log_digest,
        },
    });
    let digest = report_v2_digest(&report_canonical_body(&report));
    report["report_digest"] = json!(digest);
    let body = canonical_json(&report);
    fs::write(out_dir.join("report.json"), &body).map_err(|e| format!("report.json: {e}"))?;
    Ok(())
}

/// A pre-runtime failure report (`unavailable` family) for requests
/// that parsed but cannot start — a retained input is missing. Still
/// echoes the exact request binding.
fn write_simple_report(
    request: &WorkerRequestV2,
    out_dir: &Path,
    status: &str,
    reason: &str,
) -> Result<(), String> {
    let code = reason.split(':').next().unwrap_or("internal_error");
    let mut report = json!({
        "format_version": WORKER_REPORT_V2_FORMAT,
        "request_id": request.request_id,
        "invocation_id": request.invocation_id,
        "request_digest": request.request_digest,
        "role": request.role,
        "status": status,
        "reason": {"code": code},
        "claimed_head": null,
        "claimed_checks": [],
        "findings": [],
        "invariant_items": [],
        "continuation": {"workspace_commit": null, "report_refs": []},
        "runtime_usage": null,
        "log": null,
    });
    let digest = report_v2_digest(&report_canonical_body(&report));
    report["report_digest"] = json!(digest);
    let body = canonical_json(&report);
    fs::write(out_dir.join("report.json"), &body).map_err(|e| format!("report.json: {e}"))?;
    Ok(())
}

/// Validate and bound the reader-role members the runtime claimed.
/// A malformed member is unusable evidence — the whole result is a
/// typed failure, never a silently dropped member.
fn collect_members(block: &Value, field: &str, findings: bool) -> Result<Vec<Value>, String> {
    let members = block
        .get(field)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("result block lacks `{field}` array"))?;
    if members.len() > MAX_REPORT_MEMBERS {
        return Err(format!(
            "{field} has {} members, bound is {MAX_REPORT_MEMBERS}",
            members.len()
        ));
    }
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        if findings {
            workloop_core::worker_v2::ReportFinding::parse_member(m)
                .map_err(|e| format!("finding member rejected: {e}"))?;
        } else {
            workloop_core::worker_v2::ReportInvariantItem::parse_member(m)
                .map_err(|e| format!("invariant item rejected: {e}"))?;
        }
        out.push(m.clone());
    }
    Ok(out)
}

fn git_rev_parse_head(workspace: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (head.len() == 40 && head.chars().all(|c| c.is_ascii_hexdigit())).then_some(head)
}

/// Hard bound per declared verification command — these claims run
/// after the runtime, so each command gets its own process group and
/// a fixed ceiling rather than the request budget.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);

/// The shim itself runs the declared verification argv — claims about
/// checks are mechanical facts (argv, exit code, captured-output
/// digest), never the runtime's word. Commands run without the
/// runtime credential and each is bounded by [`CHECK_TIMEOUT`].
fn run_verification(
    request: &WorkerRequestV2,
    workspace: &Path,
    runtime_over_budget: bool,
) -> Vec<Value> {
    let argv_list: &[Vec<String>] = match &request.task {
        TaskBody::Coding {
            verification_argv, ..
        }
        | TaskBody::Fix {
            verification_argv, ..
        } => verification_argv,
        _ => &[],
    };
    let env: Vec<(String, String)> = env::vars()
        .filter(|(k, _)| k != "GH_TOKEN" && k != "GITHUB_TOKEN" && k != "ANTHROPIC_API_KEY")
        .collect();
    let mut checks = Vec::new();
    for argv in argv_list
        .iter()
        .take(request.budget.max_verification_commands as usize)
    {
        if argv.is_empty() || runtime_over_budget {
            continue;
        }
        checks.push(run_one_check(argv, workspace, &env));
    }
    checks
}

fn run_one_check(argv: &[String], workspace: &Path, env: &[(String, String)]) -> Value {
    let fail = |note: &str| {
        json!({
            "argv": argv,
            "exit_code": -1,
            "output_digest": sha256_hex("worker-claimed-check-output-v1", note.as_bytes()),
            "status": "failed",
        })
    };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(workspace)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(&format!("spawn failed: {e}")),
    };
    #[cfg(unix)]
    let pgid = child.id() as i32;

    // Drain both pipes on reader threads so a verbose check cannot
    // deadlock on a full pipe while the clock runs.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        streams.push(Box::new(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        streams.push(Box::new(stderr));
    }
    for stream in streams {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(buf.clone()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    drop(tx);

    let mut captured: Vec<u8> = Vec::new();
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let mut timed_out = false;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(bytes) => {
                if captured.len() < CHECK_OUTPUT_CAP {
                    let keep = (CHECK_OUTPUT_CAP - captured.len()).min(bytes.len());
                    captured.extend_from_slice(&bytes[..keep]);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() > deadline {
                    timed_out = true;
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                    let _ = child.kill();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if timed_out {
            break;
        }
    }
    let status = child.wait();
    match status {
        Ok(s) if !timed_out => json!({
            "argv": argv,
            "exit_code": s.code().unwrap_or(-1),
            "output_digest": sha256_hex("worker-claimed-check-output-v1", &captured),
            "status": if s.success() { "passed" } else { "failed" },
        }),
        _ => fail("check timed out"),
    }
}

fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if meta.len() > cap {
        return Err(format!("{} exceeds {cap} bytes", path.display()));
    }
    fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}
