//! Shared helpers for the external-adapter contract suite (#217).
//!
//! Everything here is adapter-agnostic: an [`Adapter`] is addressed
//! only by its executable path, its declared selector id, and a
//! diagnostic name. Scenario assertions live in `adapter_contract.rs`
//! and must stay identical across adapters — no per-adapter
//! branching.
//!
//! Each test binary links this module but uses only a subset of it —
//! dead-code warnings are expected and suppressed module-wide.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use workloop_core::identity::{HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::collect::CollectedIssue;
use workloop_selector::protocol::{self, RequestSpec};
use workloop_selector::{IssueState, SELECTOR_VERSION};

/// One adapter under contract test: the executable to spawn, the
/// selector id its requests declare, and a name for temp dirs.
#[derive(Debug, Clone, Copy)]
pub struct Adapter {
    pub exe: &'static str,
    pub selector: &'static str,
    pub name: &'static str,
}

/// The packaged reference executable — same-package binary.
pub const REFERENCE: Adapter = Adapter {
    exe: env!("CARGO_BIN_EXE_public-issue-selector"),
    selector: "public-issue-selector",
    name: "reference",
};

/// The independent fixture executable — a separate crate's binary,
/// resolved through the workspace `target/debug` convention the
/// exec contract tests already use.
pub const FIXTURE: Adapter = Adapter {
    exe: concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug/workloop-fixture-selector"
    ),
    selector: "workloop-fixture-selector",
    name: "fixture",
};

/// Every adapter the shared contract suite exercises.
pub const ADAPTERS: &[Adapter] = &[REFERENCE, FIXTURE];

pub fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

pub fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("repo-a"))
}

pub fn issue(number: u64, id: &str) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(id),
        display_number: Some(number),
    }
}

pub fn fact(
    issue: IssueRef,
    state: IssueState,
    labels: &[&str],
    created: i64,
    title: &str,
) -> CollectedIssue {
    CollectedIssue {
        issue,
        state,
        labels: labels.iter().map(|l| l.to_string()).collect(),
        created_unix_seconds: created,
        title: title.to_string(),
        revision: format!("rev-{title}"),
        content_digest: format!("digest-{title}"),
    }
}

pub fn tmp_dir(adapter: &Adapter, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-contract-{}-{}-{}",
        adapter.name,
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a sources file: `repos` maps `host/repo` keys to either a
/// page list of issues or an error spec.
pub fn write_sources(dir: &Path, repos: serde_json::Value) -> PathBuf {
    let path = dir.join("sources.json");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({ "repositories": repos })).unwrap(),
    )
    .unwrap();
    path
}

pub fn repo_key(repository: &RepositoryRef) -> String {
    format!("{}/{}", repository.host, repository.repository)
}

pub fn pages(issues: Vec<CollectedIssue>) -> serde_json::Value {
    serde_json::json!([{ "issues": issues }])
}

pub fn request_spec<'a>(
    adapter: &Adapter,
    request_id: &'a str,
    repositories: &'a [RepositoryRef],
    excluded: &'a [IssueRef],
    max_candidates: u32,
    deadline: i64,
) -> RequestSpec<'a> {
    // Leaked 'static slices: RequestSpec borrows them only for the
    // prepare_request call, which clones everything it keeps.
    RequestSpec {
        request_id,
        selector: adapter.selector,
        selector_version: SELECTOR_VERSION,
        policy_revision: 3,
        repositories,
        excluded_sources: excluded,
        max_candidates,
        deadline_unix_ms: deadline,
        task_constraints: Box::leak(vec!["lane:selector".to_string()].into_boxed_slice()),
        verification_argv: Box::leak(vec!["true".to_string()].into_boxed_slice()),
        required_permissions: Box::leak(vec!["source:read".to_string()].into_boxed_slice()),
    }
}

/// Run the real executable: request bytes on stdin, argv tail given.
/// Returns (stdout bytes, stderr text, exit code).
pub fn run_exe(
    adapter: &Adapter,
    args: &[String],
    request: &[u8],
) -> (Vec<u8>, String, Option<i32>) {
    run_exe_env(adapter, args, request, &[])
}

pub fn run_exe_env(
    adapter: &Adapter,
    args: &[String],
    request: &[u8],
    env: &[(&str, &str)],
) -> (Vec<u8>, String, Option<i32>) {
    let mut command = Command::new(adapter.exe);
    command.arg("select").args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn selector executable");
    child.stdin.take().unwrap().write_all(request).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

pub fn deadline_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000
}

pub fn sources_arg(path: &Path) -> Vec<String> {
    vec!["--sources".into(), path.to_string_lossy().into_owned()]
}

/// The `--label intake` argv tail every labelled scenario shares.
pub fn intake_label_arg() -> Vec<String> {
    vec!["--label".into(), "intake".into()]
}

/// Prepare a request and run one adapter end to end, returning the
/// validated reply plus stderr — the shape every scenario asserts on.
pub fn exchange(
    adapter: &Adapter,
    args: &[String],
    spec: &RequestSpec<'_>,
    now_ms: i64,
) -> (
    protocol::ValidatedRequest,
    protocol::SelectorReply,
    String,
    Option<i32>,
) {
    let request = protocol::prepare_request(spec, now_ms).unwrap();
    let (stdout, stderr, code) = run_exe(adapter, args, &request.bytes);
    let reply = protocol::validate_response(&stdout, &request)
        .unwrap_or_else(|v| panic!("{} emitted a non-conforming response: {v:?}", adapter.name));
    (request, reply, stderr, code)
}

/// A raw-JSON fact builder for fixture-only fields (`group`) the
/// shared `CollectedIssue` shape does not carry. The reference
/// reader ignores the extra key; only fixture-specific scenarios
/// use this.
#[allow(clippy::too_many_arguments)]
pub fn fact_json(
    issue: &IssueRef,
    state: &str,
    labels: &[&str],
    created: i64,
    title: &str,
    revision: &str,
    digest: &str,
    group: Option<&str>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "issue": {
            "repository": {
                "host": issue.repository.host.as_str(),
                "repository": issue.repository.repository.as_str(),
            },
            "issue": issue.issue.as_str(),
            "display_number": issue.display_number,
        },
        "state": state,
        "labels": labels,
        "created_unix_seconds": created,
        "title": title,
        "revision": revision,
        "content_digest": digest,
    });
    if let Some(group) = group {
        value["group"] = serde_json::json!(group);
    }
    value
}
