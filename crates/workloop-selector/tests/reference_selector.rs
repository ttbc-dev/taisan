//! Reference-executable scenarios that have no counterpart in the
//! shared contract suite (#217): the `gh api` live-source backend,
//! which the independent fixture deliberately does not implement.
//! File-backend protocol conformance — the part both adapters must
//! satisfy identically — lives in `adapter_contract.rs` and runs
//! against every adapter in `common::ADAPTERS`.

mod common;

use common::*;
use workloop_core::identity::{HostInstanceId, RepositoryId, RepositoryRef};
use workloop_selector::protocol::{self, FailureCode, SelectorReply};

#[test]
fn gh_backend_derives_issue_identity_from_the_issue_number() {
    let dir = tmp_dir(&REFERENCE, "gh-id");
    // A stub `gh` serving one open issue on the first page and an
    // empty page afterwards. The issue carries a node_id that must
    // NOT become the scoped identity: the manual submit path keys
    // issues by number, so the selector must too.
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let count_file = dir.join("gh-calls");
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\nn=$(cat {} 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > {}\nif [ $n -eq 1 ]; then\necho '[{{\"number\":42,\"title\":\"fix the thing\",\"state\":\"open\",\"created_at\":\"2024-01-01T00:00:00Z\",\"updated_at\":\"2024-01-02T00:00:00Z\",\"labels\":[{{\"name\":\"intake\"}}],\"node_id\":\"I_kwDO_notused\",\"id\":777}}]'\nelse\necho '[]'\nfi\n",
            count_file.display(),
            count_file.display()
        ),
    )
    .unwrap();
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
    let spec = request_spec(
        &REFERENCE,
        "req-gh-id",
        &repositories,
        &[],
        4,
        deadline_ms(),
    );
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let declared = [
        "--github-endpoint".to_string(),
        "https://api.github.com".to_string(),
    ];
    let (stdout, _stderr, code) = run_exe_env(&REFERENCE, &declared, &request.bytes, &env);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success, got {reply:?}");
    };
    assert_eq!(candidates.len(), 1);
    let source = &candidates[0].sources[0].issue;
    // IssueId is the issue number — identical to what
    // `submit --issue o/n:42` stores — never node_id or the
    // database id.
    assert_eq!(source.issue.as_str(), "42");
    assert_eq!(source.display_number, Some(42));
}

#[test]
fn gh_backend_serves_only_declared_github_endpoints() {
    let dir = tmp_dir(&REFERENCE, "gh-host");
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
    let spec = request_spec(&REFERENCE, "req-gh", &repositories, &[], 4, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();

    // No --github-endpoint: the backend cannot know which scoped host
    // instance is github.com, so it refuses rather than scanning
    // github.com under a foreign scope.
    let (stdout, _stderr, code) = run_exe_env(&REFERENCE, &[], &request.bytes, &env);
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
    let (stdout, _stderr, code) = run_exe_env(&REFERENCE, &declared, &request.bytes, &env);
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
    let spec = request_spec(
        &REFERENCE,
        "req-gh-foreign",
        &foreign,
        &[],
        4,
        deadline_ms(),
    );
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _stderr, code) = run_exe_env(&REFERENCE, &declared, &request.bytes, &env);
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
