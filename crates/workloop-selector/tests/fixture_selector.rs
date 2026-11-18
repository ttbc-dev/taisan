//! Fixture-only scenarios for `workloop-fixture-selector` (#217):
//! the grouped-sources extension the independent adapter implements
//! to exercise the protocol's multi-source candidate shape end to
//! end. Everything here runs against [`common::FIXTURE`] alone — the
//! shared contract suite in `adapter_contract.rs` is what proves the
//! *contract*; these tests prove the fixture's extra capability.

mod common;

use common::*;
use workloop_core::identity::{IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::protocol::{self, FailureCode, SelectorReply};

fn member(
    number: u64,
    id: &str,
    created: i64,
    title: &str,
    group: Option<&str>,
) -> serde_json::Value {
    fact_json(
        &issue(number, id),
        "open",
        &["intake"],
        created,
        title,
        &format!("rev-{id}"),
        &format!("digest-{id}"),
        group,
    )
}

/// Two grouped facts plus one singleton: the group emits as ONE
/// candidate carrying both members in (host, repository, issue)
/// order, ranked at its earliest member's position.
#[test]
fn fixture_grouped_facts_emit_one_multi_source_candidate() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group");
    let r = repo();
    // i-z ranks before i-a by creation time? No — created decides
    // first: g-member-old (100) before singleton (200) before
    // g-member-new (300). The group's rank is its earliest member's.
    let issues = vec![
        member(30, "i-z", 300, "group new", Some("squad")),
        member(10, "i-s", 200, "solo", None),
        member(20, "i-a", 100, "group old", Some("squad")),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": [{ "issues": issues }] } }),
    );
    let repositories = vec![r];
    let spec = request_spec(adapter, "req-group", &repositories, &[], 8, deadline_ms());
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _stderr, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).expect("conforming response");
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success, got {reply:?}");
    };
    // The group is one candidate; the singleton is the other — two
    // candidates total, group first (earliest member created=100).
    assert_eq!(candidates.len(), 2);
    let group = &candidates[0];
    assert_eq!(group.sources.len(), 2);
    assert_eq!(group.task.intent, "group old + group new");
    // Member order is (host, repository, issue): i-a before i-z.
    assert_eq!(group.sources[0].issue.issue.as_str(), "i-a");
    assert_eq!(group.sources[1].issue.issue.as_str(), "i-z");
    assert_eq!(group.sources[0].revision, "rev-i-a");
    assert_eq!(group.sources[1].revision, "rev-i-z");
    assert_eq!(candidates[1].task.intent, "solo");
    assert_eq!(candidates[1].sources.len(), 1);
}

/// `max_candidates` bounds candidates, not facts: a group counts as
/// one slot.
#[test]
fn fixture_group_counts_as_one_candidate_slot() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group-slot");
    let r = repo();
    let issues = vec![
        member(1, "i-a", 100, "g1", Some("g")),
        member(2, "i-b", 200, "g2", Some("g")),
        member(3, "i-c", 300, "solo", None),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": [{ "issues": issues }] } }),
    );
    let repositories = vec![r];
    // max_candidates=1: the group (rank 0, two members) wins the only
    // slot; the singleton is truncated.
    let spec = request_spec(adapter, "req-slot", &repositories, &[], 1, deadline_ms());
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].sources.len(), 2);
    // The dropped singleton is one truncated *fact*.
    assert!(
        stderr.contains("1 eligible facts truncated by max_candidates"),
        "stderr: {stderr}"
    );
}

/// Ineligible facts never join a group: a closed or excluded member
/// is absent from the candidate's sources.
#[test]
fn fixture_group_contains_only_eligible_members() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group-elig");
    let r = repo();
    let mut closed = member(1, "i-closed", 50, "closed member", Some("g"));
    closed["state"] = serde_json::json!("closed");
    let issues = vec![
        member(2, "i-a", 100, "open member", Some("g")),
        closed,
        member(3, "i-b", 200, "excluded member", Some("g")),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": [{ "issues": issues }] } }),
    );
    let excluded = vec![issue(3, "i-b")];
    let repositories = vec![r];
    let spec = request_spec(
        adapter,
        "req-ge",
        &repositories,
        &excluded,
        8,
        deadline_ms(),
    );
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    // Only the open, unexcluded member remains — a one-source
    // candidate that still carries the group intent shape.
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].sources.len(), 1);
    assert_eq!(candidates[0].sources[0].issue.issue.as_str(), "i-a");
    assert_eq!(candidates[0].task.intent, "open member");
}

/// A group may span repositories — members sort by
/// (host, repository, issue) across the whole scope.
#[test]
fn fixture_group_spans_repositories() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group-xrepo");
    let r1 = RepositoryRef::new(host(), RepositoryId::trusted("repo-a"));
    let r2 = RepositoryRef::new(host(), RepositoryId::trusted("repo-b"));
    let in_repo = |repository: &RepositoryRef, number: u64, id: &str, title: &str| {
        fact_json(
            &IssueRef {
                repository: repository.clone(),
                issue: IssueId::trusted(id),
                display_number: Some(number),
            },
            "open",
            &["intake"],
            100,
            title,
            &format!("rev-{id}"),
            &format!("digest-{id}"),
            Some("cross"),
        )
    };
    let sources = write_sources(
        &dir,
        serde_json::json!({
            repo_key(&r1): { "pages": [{ "issues": [in_repo(&r1, 1, "i-1", "alpha")] }] },
            repo_key(&r2): { "pages": [{ "issues": [in_repo(&r2, 2, "i-2", "beta")] }] },
        }),
    );
    let repositories = vec![r1, r2];
    let spec = request_spec(adapter, "req-gx", &repositories, &[], 8, deadline_ms());
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).expect("conforming response");
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success");
    };
    assert_eq!(candidates.len(), 1);
    let sources = &candidates[0].sources;
    assert_eq!(sources.len(), 2);
    // repo-a sorts before repo-b regardless of discovery order.
    assert_eq!(sources[0].issue.repository.repository.as_str(), "repo-a");
    assert_eq!(sources[1].issue.repository.repository.as_str(), "repo-b");
    assert_eq!(candidates[0].task.intent, "alpha + beta");
}

/// More than 32 members cannot fit the wire bound — the invocation
/// fails `member_limit` rather than emit a response the caller must
/// reject wholesale.
#[test]
fn fixture_oversized_group_fails_member_limit() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group-big");
    let r = repo();
    let issues: Vec<serde_json::Value> = (0..33)
        .map(|i| {
            member(
                i + 1,
                &format!("i-{i:02}"),
                100 + i as i64,
                &format!("m{i}"),
                Some("huge"),
            )
        })
        .collect();
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": [{ "issues": issues }] } }),
    );
    let repositories = vec![r];
    let spec = request_spec(adapter, "req-big", &repositories, &[], 8, deadline_ms());
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Failure { code: failure, .. } = reply else {
        panic!("expected member_limit failure, got {reply:?}");
    };
    assert_eq!(failure, FailureCode::MemberLimit);
}

/// A joined group intent that overflows the intent bound fails
/// `member_limit` — the group cannot fit response bounds.
#[test]
fn fixture_group_intent_overflow_fails_member_limit() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "group-intent");
    let r = repo();
    // Two 9 KiB titles join past the 16 KiB intent bound.
    let big = "x".repeat(9 * 1024);
    let issues = vec![
        member(1, "i-a", 100, &big, Some("fat")),
        member(2, "i-b", 200, &big, Some("fat")),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": [{ "issues": issues }] } }),
    );
    let repositories = vec![r];
    let spec = request_spec(adapter, "req-fat", &repositories, &[], 8, deadline_ms());
    let mut args = sources_arg(&sources);
    args.extend(intake_label_arg());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
    assert_eq!(code, Some(0));
    let reply = protocol::validate_response(&stdout, &request).unwrap();
    let SelectorReply::Failure { code: failure, .. } = reply else {
        panic!("expected member_limit failure, got {reply:?}");
    };
    assert_eq!(failure, FailureCode::MemberLimit);
}

/// The fixture rejects unknown argv and a missing --sources with a
/// usage error (nonzero exit), matching the reference's argv
/// discipline for its own surface.
#[test]
fn fixture_rejects_bad_argv() {
    let adapter = &FIXTURE;
    let dir = tmp_dir(adapter, "argv");
    // Unknown subcommand.
    let mut command = std::process::Command::new(adapter.exe);
    command.arg("bogus");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    // Missing --sources: argv parses far enough to learn there is
    // no scripted backend, then exits as a usage error.
    let repositories = vec![repo()];
    let spec = request_spec(adapter, "req-argv", &repositories, &[], 8, deadline_ms());
    let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
    let (stdout, _, code) = run_exe(adapter, &[], &request.bytes);
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    let _ = dir;
}
