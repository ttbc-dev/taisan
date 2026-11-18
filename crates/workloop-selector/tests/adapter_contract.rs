//! The shared external-adapter contract suite (#217): every scenario
//! below runs **identically** against each adapter in
//! [`common::ADAPTERS` — the packaged reference executable and the
//! independent `workloop-fixture-selector` fixture. Assertions never
//! branch on the adapter: a conforming response is a conforming
//! response, whoever emitted it. This is the interchange proof —
//! two independent implementations satisfying one wire contract.
//!
//! gh-backend scenarios are reference-only and stay in
//! `reference_selector.rs`; the fixture has no live-source backend.

mod common;

use common::*;
use workloop_core::domain::canonical_json;
use workloop_core::identity::{IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::protocol::{self, FailureCode, RequestSpec, SelectorReply};
use workloop_selector::{IssueState, SELECTOR_VERSION};

/// Structural pin for the suite itself: it must exercise exactly the
/// reference and the independent fixture — a silently dropped adapter
/// would make every scenario below reference-only again.
#[test]
fn contract_suite_covers_both_adapters() {
    assert_eq!(ADAPTERS.len(), 2);
    assert_eq!(ADAPTERS[0].name, "reference");
    assert_eq!(ADAPTERS[1].name, "fixture");
    assert_ne!(ADAPTERS[0].exe, ADAPTERS[1].exe);
    assert_ne!(ADAPTERS[0].selector, ADAPTERS[1].selector);
}

#[test]
fn contract_rank_order_and_skip_accounting() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "rank");
        let r = repo();
        // Three open labelled issues whose creation order contradicts
        // their Issue numbers — created ascending must win, so a
        // number-ordered implementation fails this test. Plus a
        // closed one, an unlabelled one, and a labelled-but-wrong-label
        // one.
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
        let spec = request_spec(adapter, "req-1", &repositories, &[], 32, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success, got {reply:?}", adapter.name);
        };
        assert_eq!(
            candidates.len(),
            3,
            "{}: only labelled open issues propose",
            adapter.name
        );
        let intents: Vec<&str> = candidates.iter().map(|c| c.task.intent.as_str()).collect();
        // created ascending decides before Issue number: number order
        // would be 10/20/30, creation order is 100/200/300.
        assert_eq!(
            intents,
            ["created-oldest", "created-middle", "created-newest"],
            "{}",
            adapter.name
        );
        // Every candidate binds the request's selector identity and
        // task mapping verbatim.
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
        assert!(
            stderr.contains("missing_labels=2"),
            "{} stderr: {stderr}",
            adapter.name
        );
        assert!(
            stderr.contains("not_open=1"),
            "{} stderr: {stderr}",
            adapter.name
        );
    }
}

#[test]
fn contract_exclusions_bounds_and_truncation() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "bounds");
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
        // The *oldest* issue is excluded: an implementation that
        // ignored exclusions would still rank "a" first under
        // max_candidates=1, so this fixture discriminates.
        let excluded = vec![issue(1, "i-a")];
        let repositories = vec![r.clone()];
        let spec = request_spec(adapter, "req-2", &repositories, &excluded, 1, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        // Exclusion removed i-a; max_candidates=1 keeps the next-oldest.
        assert_eq!(candidates.len(), 1, "{}", adapter.name);
        assert_eq!(candidates[0].task.intent, "b");
        assert!(
            stderr.contains("excluded=1"),
            "{} stderr must count the excluded fact: {stderr}",
            adapter.name
        );
        assert!(
            stderr.contains("1 eligible facts truncated by max_candidates"),
            "{} stderr: {stderr}",
            adapter.name
        );
    }
}

#[test]
fn contract_excludes_by_display_number() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "display-excl");
        let r = repo();
        // The exclusion names a *different* opaque issue id but the
        // same repository-scoped display number — the policy's
        // display-number rule must still refuse the fact.
        let issues = vec![
            fact(
                issue(7, "i-real"),
                IssueState::Open,
                &["intake"],
                100,
                "numbered seven",
            ),
            fact(
                issue(8, "i-other"),
                IssueState::Open,
                &["intake"],
                200,
                "kept",
            ),
        ];
        let sources = write_sources(
            &dir,
            serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
        );
        let excluded = vec![IssueRef {
            repository: r.clone(),
            issue: IssueId::trusted("i-alias"),
            display_number: Some(7),
        }];
        let repositories = vec![r];
        let spec = request_spec(
            adapter,
            "req-dn",
            &repositories,
            &excluded,
            8,
            deadline_ms(),
        );
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        assert_eq!(candidates.len(), 1, "{}", adapter.name);
        assert_eq!(candidates[0].task.intent, "kept");
        assert!(
            stderr.contains("excluded=1"),
            "{} stderr: {stderr}",
            adapter.name
        );
    }
}

#[test]
fn contract_ranks_across_repositories_by_scoped_identity() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "scope");
        let r1 = RepositoryRef::new(host(), RepositoryId::trusted("repo-a"));
        let r2 = RepositoryRef::new(host(), RepositoryId::trusted("repo-b"));
        // Identical created time AND identical Issue number across
        // every fact, so only the scoped-identity tie-break can order
        // them: repository id first ("repo-a" before "repo-b"), then
        // issue id within a repository ("i-y" before "i-z").
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
        let spec = request_spec(adapter, "req-3", &repositories, &[], 32, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        assert_eq!(candidates.len(), 3, "{}", adapter.name);
        let intents: Vec<&str> = candidates.iter().map(|c| c.task.intent.as_str()).collect();
        assert_eq!(
            intents,
            ["repo-a why", "repo-a zed", "repo-b ay"],
            "{}",
            adapter.name
        );
    }
}

#[test]
fn contract_typed_failures_for_source_errors() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "errors");
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
            let spec = request_spec(adapter, "req-err", &repositories, &[], 8, deadline_ms());
            let now = deadline_ms() - 30_000;
            let request = protocol::prepare_request(&spec, now).unwrap();
            let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
            assert_eq!(code, Some(0), "{}: {kind} must still exit 0", adapter.name);
            let reply = protocol::validate_response(&stdout, &request)
                .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
            let SelectorReply::Failure { code: failure, .. } = reply else {
                panic!("{}: {kind} must fail, got {reply:?}", adapter.name);
            };
            let code_name = match failure {
                FailureCode::AuthenticationFailed => "authentication_failed",
                FailureCode::SourceUnavailable => "source_unavailable",
                FailureCode::DeadlineExceeded => "deadline_exceeded",
                other => panic!("{}: unexpected failure code {other:?}", adapter.name),
            };
            assert_eq!(code_name, expected, "{}: {kind}", adapter.name);
        }
    }
}

#[test]
fn contract_deadline_exceeded_is_a_typed_failure() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "deadline");
        let r = repo();
        let issues = vec![fact(
            issue(1, "i-1"),
            IssueState::Open,
            &["intake"],
            100,
            "never reached",
        )];
        let sources = write_sources(
            &dir,
            serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
        );
        let repositories = vec![r];
        let spec = request_spec(adapter, "req-dl", &repositories, &[], 8, deadline_ms());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        // Rewrite the deadline into the past on the wire — the
        // request-side freshness bound is the caller's concern; the
        // adapter must map an already-past deadline to
        // `deadline_exceeded`, never hang or emit a partial scan.
        let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
        value["deadline_unix_ms"] = serde_json::json!(1);
        let expired = canonical_json(&value);
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &expired);
        assert_eq!(code, Some(0), "{}", adapter.name);
        // The response is validated against the *original* request —
        // the deadline field is not part of response identity.
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Failure { code: failure, .. } = reply else {
            panic!("{}: expected failure, got {reply:?}", adapter.name);
        };
        assert_eq!(failure, FailureCode::DeadlineExceeded, "{}", adapter.name);
    }
}

#[test]
fn contract_rejects_malformed_and_wrong_version_requests() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "badreq");
        let sources = write_sources(&dir, serde_json::json!({}));
        // Not JSON at all.
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), b"this is not json");
        assert_eq!(code, Some(0), "{}", adapter.name);
        let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(value["status"], "error", "{}", adapter.name);
        assert_eq!(
            value["error"]["code"], "invalid_request",
            "{}",
            adapter.name
        );

        // Wrong protocol version: rewrite the envelope field on the
        // wire.
        let r = repo();
        let repositories = vec![r];
        let spec = request_spec(adapter, "req-v", &repositories, &[], 8, deadline_ms());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
        value["protocol_version"] = serde_json::json!("2");
        let bad_request = canonical_json(&value);
        let (stdout, _, _) = run_exe(adapter, &sources_arg(&sources), &bad_request);
        let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            value["error"]["code"], "unsupported_version",
            "{}",
            adapter.name
        );
    }
}

#[test]
fn contract_omits_control_bearing_request_ids_from_failures() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "ctlreq");
        let sources = write_sources(&dir, serde_json::json!({}));
        let repositories = vec![repo()];
        let spec = request_spec(adapter, "req-ctl", &repositories, &[], 8, deadline_ms());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
        // A control character makes the id wire-invalid; echoing it
        // into the failure envelope would itself fail the caller's
        // response validation, so the adapter must emit `null`.
        value["request_id"] = serde_json::json!("bad\u{1}id");
        let bad_request = canonical_json(&value);
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &bad_request);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(value["status"], "error", "{}", adapter.name);
        assert_eq!(
            value["error"]["code"], "invalid_request",
            "{}",
            adapter.name
        );
        assert!(value["request_id"].is_null(), "{}: {value}", adapter.name);
    }
}

#[test]
fn contract_reports_bound_violations_as_input_limit() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "boundreq");
        let sources = write_sources(&dir, serde_json::json!({}));
        let repositories = vec![repo()];
        let spec = request_spec(adapter, "req-bound", &repositories, &[], 8, deadline_ms());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
        // 65 repository entries break the 1-64 wire bound; the bound
        // is checked before uniqueness, so duplicates are fine here.
        let entry = value["repositories"][0].clone();
        let repos = value["repositories"].as_array_mut().unwrap();
        for _ in 0..64 {
            repos.push(entry.clone());
        }
        let bad_request = canonical_json(&value);
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &bad_request);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(value["status"], "error", "{}", adapter.name);
        assert_eq!(value["error"]["code"], "input_limit", "{}", adapter.name);
    }
}

#[test]
fn contract_serves_paged_sources() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "paged");
        let r = repo();
        // Two pages: the reader must follow next_cursor until
        // exhaustion.
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
        let spec = request_spec(adapter, "req-page", &repositories, &[], 32, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        assert_eq!(candidates.len(), 2, "{}", adapter.name);
    }
}

#[test]
fn contract_proposal_identity_is_stable_across_runs() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "stable");
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
        args.extend(intake_label_arg());
        let keys = ["req-a", "req-b"]
            .iter()
            .map(|request_id| {
                let spec = request_spec(adapter, request_id, &repositories, &[], 8, deadline_ms());
                let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
                let (stdout, _, _) = run_exe(adapter, &args, &request.bytes);
                let reply = protocol::validate_response(&stdout, &request)
                    .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
                let SelectorReply::Success { candidates } = reply else {
                    panic!("{}: expected success", adapter.name);
                };
                candidates[0].key.clone()
            })
            .collect::<Vec<_>>();
        // Different request ids and fetch times, identical facts and
        // policy: the proposal identity must not rotate.
        assert_eq!(keys[0], keys[1], "{}", adapter.name);
    }
}

/// The interchange core: the *same* request bytes — one shared
/// selector id — sent to both executables must yield candidates with
/// identical proposal keys and identical semantic content. The key
/// digests `proposal-v1` over selector id, version, policy revision,
/// sources, task, and permissions; two independent digest
/// implementations agreeing byte-for-byte is the strongest form of
/// the interchange claim.
#[test]
fn contract_proposal_keys_match_across_adapters() {
    let dir = tmp_dir(&FIXTURE, "xkeys");
    let r = repo();
    let issues = vec![
        fact(issue(2, "i-b"), IssueState::Open, &["intake"], 200, "bravo"),
        fact(issue(1, "i-a"), IssueState::Open, &["intake"], 100, "alpha"),
    ];
    let sources = write_sources(
        &dir,
        serde_json::json!({ repo_key(&r): { "pages": pages(issues) } }),
    );
    let repositories = vec![r];
    // One selector id for both runs: the id is caller configuration,
    // not adapter identity — either executable may serve it.
    let shared = common::Adapter {
        selector: "contract-shared-selector",
        ..FIXTURE
    };
    let mut per_adapter: Vec<Vec<workloop_selector::Proposal>> = Vec::new();
    for adapter in ADAPTERS {
        let spec = request_spec(&shared, "req-x", &repositories, &[], 8, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let request = protocol::prepare_request(&spec, deadline_ms() - 30_000).unwrap();
        let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        per_adapter.push(candidates);
    }
    let (reference_keys, fixture_keys) = (&per_adapter[0], &per_adapter[1]);
    assert_eq!(reference_keys.len(), fixture_keys.len());
    for (a, b) in reference_keys.iter().zip(fixture_keys.iter()) {
        assert_eq!(a.key, b.key, "proposal keys diverged across adapters");
        assert_eq!(a.sources, b.sources, "source bindings diverged");
        assert_eq!(a.task, b.task, "task inputs diverged");
        assert_eq!(a.required_permissions, b.required_permissions);
        assert_eq!(a.selector_version, b.selector_version);
        assert_eq!(a.policy_revision, b.policy_revision);
    }
}

#[test]
fn contract_without_label_config_proposes_all_open_issues() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "nolabel");
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
        let spec = request_spec(adapter, "req-nl", &repositories, &[], 32, deadline_ms());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        assert_eq!(candidates.len(), 2, "{}", adapter.name);
    }
}

#[test]
fn contract_skips_facts_whose_title_cannot_form_an_intent() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "intent");
        let r = repo();
        // Whitespace-only title: schema-valid string, invalid intent.
        let blank = fact(
            issue(1, "i-blank"),
            IssueState::Open,
            &["intake"],
            100,
            "   ",
        );
        // Control character in the title: the title reaches the
        // selector (collect bounds it only by size), so the intent
        // check must refuse it. Revision/digest stay clean tokens —
        // a real source reports them independently of the title.
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
        let spec = request_spec(adapter, "req-intent", &repositories, &[], 32, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        // The response must still conform — the bad facts are
        // skipped, never emitted into a candidate the validator would
        // reject.
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success, got {reply:?}", adapter.name);
        };
        assert_eq!(candidates.len(), 1, "{}", adapter.name);
        assert_eq!(candidates[0].task.intent, "good");
        assert!(
            stderr.contains("invalid_intent=2"),
            "{} stderr must count both refused facts: {stderr}",
            adapter.name
        );
    }
}

#[test]
fn contract_skips_facts_with_a_zero_display_number() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "display0");
        let r = repo();
        // display_number must be positive or absent on the wire; a
        // source fact carrying 0 would make the whole response
        // invalid, so the fact is refused with a typed reason.
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
        let spec = request_spec(
            adapter,
            "req-display0",
            &repositories,
            &[],
            32,
            deadline_ms(),
        );
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, stderr, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success, got {reply:?}", adapter.name);
        };
        assert_eq!(candidates.len(), 1, "{}", adapter.name);
        assert_eq!(candidates[0].task.intent, "good");
        assert!(
            stderr.contains("invalid_issue_ref=1"),
            "{} stderr must count the refused fact: {stderr}",
            adapter.name
        );
    }
}

#[test]
fn contract_rejects_out_of_scope_source_facts() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "oos");
        // A fact scoped outside the requested repository makes the
        // whole repository scan fail — the executable must fail
        // closed.
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
        let spec = request_spec(adapter, "req-oos", &repositories, &[], 8, deadline_ms());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        assert!(
            matches!(reply, SelectorReply::Failure { .. }),
            "{}: expected failure",
            adapter.name
        );
    }
}

#[test]
fn contract_contradictory_facts_fail_closed() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "contra");
        let r = repo();
        // Two pages listing the same issue identity with different
        // titles — an inconsistent read must fail the repository,
        // never silently pick one.
        let page1 = vec![fact(
            issue(1, "i-1"),
            IssueState::Open,
            &["intake"],
            100,
            "version one",
        )];
        let mut page2_fact = fact(
            issue(1, "i-1"),
            IssueState::Open,
            &["intake"],
            100,
            "version two",
        );
        // Same identity, different content → contradiction. (The
        // display number may differ; identity ignores it.)
        page2_fact.issue.display_number = Some(2);
        let sources = write_sources(
            &dir,
            serde_json::json!({
                repo_key(&r): { "pages": [
                    { "issues": page1 },
                    { "issues": vec![page2_fact] }
                ] }
            }),
        );
        let repositories = vec![r];
        let spec = request_spec(adapter, "req-contra", &repositories, &[], 8, deadline_ms());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Failure { code: failure, .. } = reply else {
            panic!("{}: expected failure, got {reply:?}", adapter.name);
        };
        assert_eq!(failure, FailureCode::SourceUnavailable, "{}", adapter.name);
    }
}

#[test]
fn contract_duplicate_facts_across_pages_dedup() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "dedup");
        let r = repo();
        // The same fact listed on two pages is a legitimate repeat —
        // it must dedup to one candidate, not contradict.
        let one = fact(issue(1, "i-1"), IssueState::Open, &["intake"], 100, "same");
        let sources = write_sources(
            &dir,
            serde_json::json!({
                repo_key(&r): { "pages": [
                    { "issues": vec![one.clone()] },
                    { "issues": vec![one] }
                ] }
            }),
        );
        let repositories = vec![r];
        let spec = request_spec(adapter, "req-dedup", &repositories, &[], 8, deadline_ms());
        let mut args = sources_arg(&sources);
        args.extend(intake_label_arg());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &args, &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Success { candidates } = reply else {
            panic!("{}: expected success", adapter.name);
        };
        assert_eq!(candidates.len(), 1, "{}", adapter.name);
    }
}

#[test]
fn contract_empty_page_list_is_a_source_failure() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "emptypages");
        let r = repo();
        // A repository whose scripted source declares zero pages:
        // there is no complete scan to admit, so both adapters must
        // fail the invocation `source_unavailable` — never an empty
        // "successful" scan.
        let sources = write_sources(&dir, serde_json::json!({ repo_key(&r): { "pages": [] } }));
        let repositories = vec![r];
        let spec = request_spec(adapter, "req-empty", &repositories, &[], 8, deadline_ms());
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        let reply = protocol::validate_response(&stdout, &request)
            .unwrap_or_else(|v| panic!("{}: non-conforming response: {v:?}", adapter.name));
        let SelectorReply::Failure { code: failure, .. } = reply else {
            panic!("{}: expected failure, got {reply:?}", adapter.name);
        };
        assert_eq!(failure, FailureCode::SourceUnavailable, "{}", adapter.name);
    }
}

#[test]
fn contract_oversized_response_is_rejected_identically() {
    for adapter in ADAPTERS {
        let dir = tmp_dir(adapter, "toobig");
        let r = repo();
        // An adversarial-but-valid request: 32 eligible facts and the
        // maximum constraint payload every candidate must echo. The
        // response bound is launcher-enforced — neither adapter
        // self-truncates — so both must emit the same over-limit
        // `status:"ok"` envelope that validate_response rejects
        // wholesale. Identical observable behavior is the contract.
        let issues: Vec<serde_json::Value> = (0..32)
            .map(|i| {
                fact_json(
                    &issue(100 + i, &format!("i-{i:02}")),
                    "open",
                    &["intake"],
                    100 + i as i64,
                    &format!("work {i:02}"),
                    &format!("rev-{i}"),
                    &format!("digest-{i}"),
                    None,
                )
            })
            .collect();
        let sources = write_sources(
            &dir,
            serde_json::json!({
                repo_key(&r): { "pages": [{ "issues": issues }] }
            }),
        );
        let repositories = vec![r];
        let constraints: Vec<String> = (0..64)
            .map(|i| format!("constraint-{i:02}-{}", "x".repeat(4000)))
            .collect();
        let spec = RequestSpec {
            request_id: "req-toobig",
            selector: adapter.selector,
            selector_version: SELECTOR_VERSION,
            policy_revision: 3,
            repositories: &repositories,
            excluded_sources: &[],
            max_candidates: 32,
            deadline_unix_ms: deadline_ms(),
            task_constraints: Box::leak(constraints.into_boxed_slice()),
            verification_argv: Box::leak(vec!["true".to_string()].into_boxed_slice()),
            required_permissions: Box::leak(vec!["source:read".to_string()].into_boxed_slice()),
        };
        let now = deadline_ms() - 30_000;
        let request = protocol::prepare_request(&spec, now).unwrap();
        let (stdout, _, code) = run_exe(adapter, &sources_arg(&sources), &request.bytes);
        assert_eq!(code, Some(0), "{}", adapter.name);
        assert!(
            stdout.len() > protocol::MAX_RESPONSE_BYTES,
            "{}: expected an over-limit response, got {} bytes",
            adapter.name,
            stdout.len()
        );
        let value: serde_json::Value = serde_json::from_slice(&stdout)
            .unwrap_or_else(|e| panic!("{}: oversized stdout is not JSON: {e}", adapter.name));
        assert_eq!(value["status"], "ok", "{}", adapter.name);
        assert_eq!(
            value["candidates"].as_array().unwrap().len(),
            32,
            "{}",
            adapter.name
        );
        // The launcher's whole-response bound rejects it — for both
        // adapters identically.
        let violation = protocol::validate_response(&stdout, &request)
            .expect_err("oversized response must not validate");
        assert!(
            matches!(
                violation,
                protocol::Violation::Contract("response_too_large")
            ),
            "{}: unexpected violation {violation:?}",
            adapter.name
        );
    }
}
