//! Protocol conformance tests for the packaged reference executable
//! (#214): the real `public-issue-selector` binary driven over stdin
//! with scripted source files, its responses validated by the same
//! [`protocol::validate_response`] the intake coordinator runs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use workloop_core::identity::{HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::collect::CollectedIssue;
use workloop_selector::protocol::{self, FailureCode, RequestSpec, SelectorReply};
use workloop_selector::{IssueState, SELECTOR_VERSION};

const EXE: &str = env!("CARGO_BIN_EXE_public-issue-selector");

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("repo-a"))
}

fn issue(number: u64, id: &str) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(id),
        display_number: Some(number),
    }
}

fn fact(
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

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-refsel-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a sources file: `repos` maps `host/repo` keys to either a
/// page list of issues or an error spec.
fn write_sources(dir: &Path, repos: serde_json::Value) -> PathBuf {
    let path = dir.join("sources.json");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({ "repositories": repos })).unwrap(),
    )
    .unwrap();
    path
}

fn repo_key(repository: &RepositoryRef) -> String {
    format!("{}/{}", repository.host, repository.repository)
}

fn pages(issues: Vec<CollectedIssue>) -> serde_json::Value {
    serde_json::json!([{ "issues": issues }])
}

fn request_spec<'a>(
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
        selector: "public-issue-selector",
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
fn run_exe(args: &[String], request: &[u8]) -> (Vec<u8>, String, Option<i32>) {
    run_exe_env(args, request, &[])
}

fn run_exe_env(
    args: &[String],
    request: &[u8],
    env: &[(&str, &str)],
) -> (Vec<u8>, String, Option<i32>) {
    let mut command = Command::new(EXE);
    command.arg("select").args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn public-issue-selector");
    child.stdin.take().unwrap().write_all(request).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn deadline_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000
}

fn sources_arg(path: &Path) -> Vec<String> {
    vec!["--sources".into(), path.to_string_lossy().into_owned()]
}

#[test]
fn executable_proposes_eligible_issues_in_rank_order() {
    let dir = tmp_dir("rank");
    let r = repo();
    // Three open labelled issues whose creation order contradicts
    // their Issue numbers — created ascending must win, so a
    // number-ordered implementation fails this test. Plus a closed
    // one, an unlabelled one, and a labelled-but-wrong-label one.
    let issues = vec![
        fact(
            issue(30, "i-c"),
            IssueState::Open,
            &["intake"],
            100,
            "created-oldest",
        ),
        fact(
            issue(10, "i-a"),
            IssueState::Open,
            &["intake"],
            200,
            "created-middle",
        ),
        fact(
            issue(20, "i-b"),
            IssueState::Open,
            &["intake"],
            300,
            "created-newest",
        ),
        fact(
            issue(40, "i-closed"),
            IssueState::Closed,
            &["intake"],
            50,
            "closed",
        ),
        fact(
            issue(50, "i-unlabelled"),
            IssueState::Open,
            &[],
            10,
            "no label",
        ),
        fact(
            issue(60, "i-other"),
            IssueState::Open,
            &["other"],
            10,
            "wrong label",
        ),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r.clone()];
    let spec = request_spec("req-1", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, stderr, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).expect("conforming response");
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success, got {reply:?}");
    };
    assert_eq!(candidates.len(), 3, "only labelled open issues propose");
    let intents: Vec<&str> = candidates.iter().map(|c| c.task.intent.as_str()).collect();
    // created ascending decides before Issue number: number order
    // would be 10/20/30, creation order is 100/200/300.
    assert_eq!(
        intents,
        ["created-oldest", "created-middle", "created-newest"]
    );
    // Every candidate binds the request's selector identity and task
    // mapping verbatim.
    for candidate in &candidates {
        assert_eq!(candidate.selector_version, SELECTOR_VERSION);
        assert_eq!(candidate.policy_revision, 3);
        assert_eq!(candidate.task.constraints, ["lane:selector"]);
        assert_eq!(candidate.task.verification_argv, ["true"]);
        assert_eq!(candidate.required_permissions, ["source:read"]);
        assert_eq!(candidate.sources.len(), 1);
        assert!(candidate.provenance.snapshot_cursor.is_none());
    }
    // Diagnostics name the skip reasons without payload text.
    assert!(stderr.contains("missing_labels=2"), "stderr: {stderr}");
    assert!(stderr.contains("not_open=1"), "stderr: {stderr}");
}

#[test]
fn executable_honours_exclusions_and_max_candidates() {
    let dir = tmp_dir("bounds");
    let r = repo();
    let issues = vec![
        fact(issue(1, "i-a"), IssueState::Open, &["intake"], 100, "a"),
        fact(issue(2, "i-b"), IssueState::Open, &["intake"], 200, "b"),
        fact(issue(3, "i-c"), IssueState::Open, &["intake"], 300, "c"),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    // The *oldest* issue is excluded: an implementation that ignored
    // exclusions would still rank "a" first under max_candidates=1,
    // so this fixture discriminates.
    let excluded = vec![issue(1, "i-a")];
    let repositories = vec![r.clone()];
    let spec = request_spec("req-2", &repositories, &excluded, 1, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, stderr, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    // Exclusion removed i-a; max_candidates=1 keeps the next-oldest.
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].task.intent, "b");
    assert!(
        stderr.contains("excluded=1"),
        "stderr must count the excluded fact: {stderr}"
    );
    assert!(
        stderr.contains("1 eligible facts truncated by max_candidates"),
        "stderr: {stderr}"
    );
}

#[test]
fn executable_ranks_across_repositories_by_scoped_identity() {
    let dir = tmp_dir("scope");
    let r1 = RepositoryRef::new(host(), RepositoryId::trusted("repo-a"));
    let r2 = RepositoryRef::new(host(), RepositoryId::trusted("repo-b"));
    // Identical created time AND identical Issue number across every
    // fact, so only the scoped-identity tie-break can order them:
    // repository id first ("repo-a" before "repo-b"), then issue id
    // within a repository ("i-y" before "i-z").
    let mk = |repository: &RepositoryRef, id: &str, title: &str| {
        fact(
            IssueRef {
                repository: repository.clone(),
                issue: IssueId::trusted(id),
                display_number: Some(7),
            },
            IssueState::Open,
            &["intake"],
            100,
            title,
        )
    };
    let a_z = mk(&r1, "i-z", "repo-a zed");
    let a_y = mk(&r1, "i-y", "repo-a why");
    let b_a = mk(&r2, "i-a", "repo-b ay");
    let sources = write_sources(
        &dir,
        serde_json::json!({
            repo_key(&r1): { "pages": pages(vec![a_z, a_y]) },
            repo_key(&r2): { "pages": pages(vec![b_a]) },
        }),
    );
    let repositories = vec![r1, r2];
    let spec = request_spec("req-3", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, _, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    assert_eq!(candidates.len(), 3);
    let intents: Vec<&str> = candidates.iter().map(|c| c.task.intent.as_str()).collect();
    assert_eq!(intents, ["repo-a why", "repo-a zed", "repo-b ay"]);
}

#[test]
fn executable_fails_closed_on_source_errors() {
    let dir = tmp_dir("errors");
    let r = repo();
    for (kind, expected) in [
        ("denied", "authentication_failed"),
        ("unavailable", "source_unavailable"),
        ("malformed", "source_unavailable"),
    ] {
        let sources = write_sources(
            &dir,
            serde_json::json!({
                repo_key(&r): { "error": { "kind": kind, "reason": "scripted failure" } }
            }),
        );
        let repositories = vec![r.clone()];
        let spec = request_spec("req-err", &repositories, &[], 8, deadline_ms());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        let (stdout, _, code) = run_exe(&sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{kind} must still exit 0");
        let reply = protocol::validate_response(&stdout, &request).unwrap();
        let SelectorReply::Failure { code: failure, .. } = reply else {
            panic!("{kind} must fail, got {reply:?}");
        };
        let code_name = match failure {
            protocol::FailureCode::AuthenticationFailed => "authentication_failed",
            protocol::FailureCode::SourceUnavailable => "source_unavailable",
            protocol::FailureCode::DeadlineExceeded => "deadline_exceeded",
            other => panic!("unexpected failure code {other:?}"),
        };
        assert_eq!(code_name, expected, "{kind}");
    }
}

#[test]
fn executable_rejects_malformed_and_wrong_version_requests() {
    let dir = tmp_dir("badreq");
    let sources = write_sources(&dir, serde_json::json!({}));
    // Not JSON at all.
    let (stdout, _, code) = run_exe(&sources_arg(&sources), b"this is not json");
    assert_eq!(code, Some(0));
    let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "invalid_request");

    // Wrong protocol version: rewrite the envelope field on the wire.
    let r = repo();
    let repositories = vec![r];
    let spec = request_spec("req-v", &repositories, &[], 8, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
    value["protocol_version"] = serde_json::json!("2");
    let bad_request = workloop_core::domain::canonical_json(&value);
    let (stdout, _, _) = run_exe(&sources_arg(&sources), &bad_request);
    let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(value["error"]["code"], "unsupported_version");
}

#[test]
fn executable_omits_control_bearing_request_ids_from_failures() {
    let dir = tmp_dir("ctlreq");
    let sources = write_sources(&dir, serde_json::json!({}));
    let repositories = vec![repo()];
    let spec = request_spec("req-ctl", &repositories, &[], 8, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
    // A control character makes the id wire-invalid; echoing it into
    // the failure envelope would itself fail the caller's response
    // validation, so the adapter must emit `null` instead.
    value["request_id"] = serde_json::json!("bad\u{1}id");
    let bad_request = workloop_core::domain::canonical_json(&value);
    let (stdout, _, code) = run_exe(&sources_arg(&sources), &bad_request);
    assert_eq!(code, Some(0));
    let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "invalid_request");
    assert!(value["request_id"].is_null(), "{value}");
}

#[test]
fn executable_reports_bound_violations_as_input_limit() {
    let dir = tmp_dir("boundreq");
    let sources = write_sources(&dir, serde_json::json!({}));
    let repositories = vec![repo()];
    let spec = request_spec("req-bound", &repositories, &[], 8, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
    // 65 repository entries break the 1-64 wire bound; the bound is
    // checked before uniqueness, so duplicates are fine here.
    let entry = value["repositories"][0].clone();
    let repos = value["repositories"].as_array_mut().unwrap();
    for _ in 0..64 {
        repos.push(entry.clone());
    }
    let bad_request = workloop_core::domain::canonical_json(&value);
    let (stdout, _, code) = run_exe(&sources_arg(&sources), &bad_request);
    assert_eq!(code, Some(0));
    let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "input_limit");
}

#[test]
fn executable_serves_paged_sources() {
    let dir = tmp_dir("paged");
    let r = repo();
    // Two pages: the reader must follow next_cursor until exhaustion.
    let page1 = vec![fact(
        issue(1, "i-1"),
        IssueState::Open,
        &["intake"],
        100,
        "one",
    )];
    let page2 = vec![fact(
        issue(2, "i-2"),
        IssueState::Open,
        &["intake"],
        200,
        "two",
    )];
    let sources = write_sources(
        &dir,
        serde_json::json!({
            repo_key(&r): { "pages": [
                { "issues": page1 },
                { "issues": page2 }
            ] }
        }),
    );
    let repositories = vec![r];
    let spec = request_spec("req-page", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, _, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    assert_eq!(candidates.len(), 2);
}

#[test]
fn executable_proposal_identity_is_stable_across_runs() {
    let dir = tmp_dir("stable");
    let r = repo();
    let issues = vec![fact(
        issue(1, "i-1"),
        IssueState::Open,
        &["intake"],
        100,
        "stable",
    )];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r];
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let keys = ["req-a", "req-b"]
        .iter()
        .map(|request_id| {
            let spec = request_spec(request_id, &repositories, &[], 8, deadline_ms());
            let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
            let (stdout, _, _) = run_exe(&args, &request.bytes);
            let reply = protocol::validate_response(&stdout, &request).unwrap();
            let SelectorReply::Success { candidates } = reply else {
                panic!("expected success");
            };
            candidates[0].key.clone()
        })
        .collect::<Vec<_>>();
    // Different request ids and fetch times, identical facts and
    // policy: the proposal identity must not rotate.
    assert_eq!(keys[0], keys[1]);
}

#[test]
fn executable_without_label_config_proposes_all_open_issues() {
    let dir = tmp_dir("nolabel");
    let r = repo();
    let issues = vec![
        fact(issue(1, "i-1"), IssueState::Open, &[], 100, "plain"),
        fact(issue(2, "i-2"), IssueState::Open, &["any"], 200, "labelled"),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r];
    let spec = request_spec("req-nl", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(&sources_arg(&sources), &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    assert_eq!(candidates.len(), 2);
}

#[test]
fn executable_skips_facts_whose_title_cannot_form_an_intent() {
    let dir = tmp_dir("intent");
    let r = repo();
    // Whitespace-only title: schema-valid string, invalid intent.
    let blank = fact(
        issue(1, "i-blank"),
        IssueState::Open,
        &["intake"],
        100,
        "   ",
    );
    // Control character in the title: the title reaches the selector
    // (collect bounds it only by size), so the intent check must
    // refuse it. Revision/digest stay clean tokens — a real source
    // reports them independently of the title.
    let mut ctrl = fact(
        issue(2, "i-ctrl"),
        IssueState::Open,
        &["intake"],
        200,
        "bad\u{7}title",
    );
    ctrl.revision = "rev-ctrl".to_string();
    ctrl.content_digest = "digest-ctrl".to_string();
    let issues = vec![
        blank,
        ctrl,
        fact(issue(3, "i-ok"), IssueState::Open, &["intake"], 300, "good"),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r];
    let spec = request_spec("req-intent", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, stderr, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    // The response must still conform — the bad facts are skipped,
    // never emitted into a candidate the validator would reject.
    let reply = protocol::validate_response(&stdout, &request).expect("conforming response");
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success, got {reply:?}");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].task.intent, "good");
    assert!(
        stderr.contains("invalid_intent=2"),
        "stderr must count both refused facts: {stderr}"
    );
}

#[test]
fn executable_skips_facts_with_a_zero_display_number() {
    let dir = tmp_dir("display0");
    let r = repo();
    // display_number must be positive or absent on the wire; a source
    // fact carrying 0 would make the whole response invalid, so the
    // fact is refused with a typed reason instead.
    let issues = vec![
        fact(
            issue(0, "i-zero"),
            IssueState::Open,
            &["intake"],
            100,
            "renumbered",
        ),
        fact(issue(7, "i-ok"), IssueState::Open, &["intake"], 200, "good"),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r];
    let spec = request_spec("req-display0", &repositories, &[], 32, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let mut args = sources_arg(&sources);
    args.extend(["--label".into(), "intake".into()]);
    let (stdout, stderr, code) = run_exe(&args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).expect("conforming response");
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success, got {reply:?}");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].task.intent, "good");
    assert!(
        stderr.contains("invalid_issue_ref=1"),
        "stderr must count the refused fact: {stderr}"
    );
}

#[test]
fn executable_rejects_out_of_scope_source_facts() {
    // A fact scoped outside the requested repository makes the whole
    // repository scan Incomplete — the executable must fail closed.
    let dir = tmp_dir("oos");
    let r = repo();
    let foreign = fact(
        IssueRef {
            repository: RepositoryRef::new(host(), RepositoryId::trusted("repo-zzz")),
            issue: IssueId::trusted("i-f"),
            display_number: Some(9),
        },
        IssueState::Open,
        &["intake"],
        100,
        "foreign",
    );
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(vec![foreign]) } }),
    );
    let repositories = vec![r];
    let spec = request_spec("req-oos", &repositories, &[], 8, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(&sources_arg(&sources), &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    assert!(matches!(reply, SelectorReply::Failure { .. }));
}

#[test]
fn executable_gh_backend_serves_only_declared_github_endpoints() {
    let dir = tmp_dir("gh-host");
    // A stub `gh` answering every api call with an empty page —
    // isolates the endpoint gate from real authentication.
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, "#!/bin/sh\necho '[]'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = [("PATH", path.as_str())];

    let repositories = vec![RepositoryRef::new(
        HostInstanceId::trusted("https://api.github.com"),
        RepositoryId::trusted("o/n"),
    )];
    let spec = request_spec("req-gh", &repositories, &[], 4, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();

    // No --github-endpoint: the backend cannot know which scoped host
    // instance is github.com, so it refuses rather than scanning
    // github.com under a foreign scope.
    let (stdout, _stderr, code) = run_exe_env(&[], &request.bytes, &env);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    assert!(matches!(
        reply,
        SelectorReply::Failure {
            code: FailureCode::SourceUnavailable,
            ..
        }
    ));

    // The declared endpoint serves; the empty page yields a conforming
    // success with zero candidates.
    let declared = [
        "--github-endpoint".to_string(),
        "https://api.github.com".to_string(),
    ];
    let (stdout, _stderr, code) = run_exe_env(&declared, &request.bytes, &env);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    assert!(matches!(
        reply,
        SelectorReply::Success { candidates } if candidates.is_empty()
    ));

    // A repository scoped to a foreign host instance is refused even
    // with an endpoint declared.
    let foreign = vec![RepositoryRef::new(
        HostInstanceId::trusted("ghe.example.com"),
        RepositoryId::trusted("o/n"),
    )];
    let spec = request_spec("req-gh-foreign", &foreign, &[], 4, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _stderr, code) = run_exe_env(&declared, &request.bytes, &env);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    assert!(matches!(
        reply,
        SelectorReply::Failure {
            code: FailureCode::SourceUnavailable,
            ..
        }
    ));
}
