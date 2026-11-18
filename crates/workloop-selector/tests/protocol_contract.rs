//! Contract tests for the selector wire protocol (#201): the strict
//! JSON reader's framing rules, `prepare_request`/`parse_request`
//! bound enforcement on both ends, and `validate_response`'s
//! whole-response rules — one violation admits zero candidates.

use workloop_core::identity::{HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::protocol::{
    self, FailureCode, RequestSpec, SelectorReply, ValidatedRequest, Violation,
};
use workloop_selector::strict_json::{self, JsonError};
use workloop_selector::{Proposal, Provenance, SourceBinding, TaskInput, proposal_key};

const NOW_MS: i64 = 1_700_000_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo(name: &str) -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted(name))
}

fn issue(name: &str) -> IssueRef {
    IssueRef {
        repository: repo("repo-a"),
        issue: IssueId::trusted(name),
        display_number: Some(7),
    }
}

/// Owned inputs a [`RequestSpec`] borrows — keeps the spec's lifetime
/// tied to one struct instead of scattered temporaries.
struct Parts {
    repositories: Vec<RepositoryRef>,
    excluded: Vec<IssueRef>,
    task_constraints: Vec<String>,
    verification_argv: Vec<String>,
    required_permissions: Vec<String>,
}

impl Parts {
    fn new() -> Self {
        Self {
            repositories: repositories(),
            excluded: vec![],
            task_constraints: vec!["constraint-a".to_string()],
            verification_argv: vec!["true".to_string()],
            required_permissions: vec!["perm-a".to_string()],
        }
    }

    fn spec<'a>(&'a self, max_candidates: u32) -> RequestSpec<'a> {
        RequestSpec {
            request_id: "req-1",
            selector: "test-sel",
            selector_version: 1,
            policy_revision: 7,
            repositories: &self.repositories,
            excluded_sources: &self.excluded,
            max_candidates,
            deadline_unix_ms: NOW_MS + 60_000,
            task_constraints: &self.task_constraints,
            verification_argv: &self.verification_argv,
            required_permissions: &self.required_permissions,
        }
    }
}

fn spec_with_deadline<'a>(parts: &'a Parts, max_candidates: u32, deadline: i64) -> RequestSpec<'a> {
    let mut spec = parts.spec(max_candidates);
    spec.deadline_unix_ms = deadline;
    spec
}

fn repositories() -> Vec<RepositoryRef> {
    vec![repo("repo-a"), repo("repo-b")]
}

fn request() -> ValidatedRequest {
    let parts = Parts::new();
    protocol::prepare_request(&parts.spec(4), NOW_MS).expect("request builds")
}

fn task(intent: &str) -> TaskInput {
    TaskInput {
        intent: intent.to_string(),
        constraints: vec!["constraint-a".to_string()],
        verification_argv: vec!["true".to_string()],
    }
}

fn proposal_for(issue_ref: IssueRef, intent: &str) -> Proposal {
    let source = SourceBinding {
        issue: issue_ref,
        revision: "rev-1".to_string(),
        content_digest: "digest-1".to_string(),
    };
    let task = task(intent);
    let permissions = vec!["perm-a".to_string()];
    let sources = vec![source];
    Proposal {
        key: proposal_key("test-sel", 1, 7, &sources, &task, &permissions),
        selector: workloop_core::identity::AdapterId::new("test-sel").unwrap(),
        selector_version: 1,
        policy_revision: 7,
        sources,
        task,
        required_permissions: permissions,
        provenance: Provenance::default(),
    }
}

fn response_bytes(request_id: &str, proposals: &[Proposal]) -> Vec<u8> {
    protocol::success_response(request_id, proposals)
}

/// Parse a success response into a mutable JSON value for surgical
/// violation injection.
fn response_json(request_id: &str, proposals: &[Proposal]) -> serde_json::Value {
    serde_json::from_slice(&response_bytes(request_id, proposals)).unwrap()
}

fn validate(value: &serde_json::Value, req: &ValidatedRequest) -> Result<SelectorReply, Violation> {
    protocol::validate_response(value.to_string().as_bytes(), req)
}

fn contract_rule<T>(result: Result<T, Violation>) -> &'static str {
    match result {
        Err(Violation::Contract(rule)) => rule,
        Err(Violation::Parse(e)) => panic!("expected a contract violation, got parse {e:?}"),
        Ok(_) => panic!("expected a contract violation, got Ok"),
    }
}

// --- strict_json ------------------------------------------------------------

#[test]
fn strict_json_rejects_bom_invalid_utf8_and_trailing_values() {
    assert_eq!(
        strict_json::parse_strict(b"\xEF\xBB\xBF{}"),
        Err(JsonError::Encoding)
    );
    assert_eq!(
        strict_json::parse_strict(b"{\"a\":\"\xFF\"}"),
        Err(JsonError::Encoding)
    );
    assert_eq!(
        strict_json::parse_strict(b"{} {}"),
        Err(JsonError::Trailing)
    );
    assert_eq!(strict_json::parse_strict(b"{} x"), Err(JsonError::Trailing));
    // Leading/trailing JSON whitespace is fine.
    assert!(strict_json::parse_strict(b"  {\"a\":1}\n\t").is_ok());
}

#[test]
fn strict_json_rejects_duplicate_keys_at_every_depth() {
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":1,\"a\":2}"),
        Err(JsonError::DuplicateKey { .. })
    ));
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":{\"b\":1,\"b\":2}}"),
        Err(JsonError::DuplicateKey { .. })
    ));
    assert!(matches!(
        strict_json::parse_strict(b"[{\"k\":1},{\"k\":1,\"k\":2}]"),
        Err(JsonError::DuplicateKey { .. })
    ));
}

#[test]
fn strict_json_bounds_container_depth() {
    // The root container sits at depth 1; 16 open containers is the
    // bound — the scalar inside adds no depth of its own.
    let mut ok = String::from("1");
    for _ in 0..16 {
        ok = format!("[{ok}]");
    }
    assert!(strict_json::parse_strict(ok.as_bytes()).is_ok());
    let mut deep = String::from("1");
    for _ in 0..17 {
        deep = format!("[{deep}]");
    }
    assert_eq!(
        strict_json::parse_strict(deep.as_bytes()),
        Err(JsonError::Depth)
    );
}

#[test]
fn strict_json_rejects_control_characters_however_encoded() {
    // Raw control byte inside a string.
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":\"x\x01y\"}"),
        Err(JsonError::Malformed { .. })
    ));
    // Escaped controls decode to control characters — still refused.
    for esc in [
        "\\u0000", "\\u0008", "\\u0009", "\\u001f", "\\u007f", "\\u0085", "\\u009f",
    ] {
        let doc = format!("{{\"a\":\"x{esc}y\"}}");
        assert!(
            matches!(
                strict_json::parse_strict(doc.as_bytes()),
                Err(JsonError::Malformed { .. })
            ),
            "escape {esc} must be refused"
        );
    }
    // A raw DEL byte is refused too.
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":\"x\x7Fy\"}"),
        Err(JsonError::Malformed { .. })
    ));
}

#[test]
fn strict_json_handles_escapes_and_numbers() {
    // Surrogate pair decodes to one scalar.
    let v = strict_json::parse_strict("{\"a\":\"\\uD83D\\uDE00\"}".as_bytes()).unwrap();
    assert_eq!(v["a"], "\u{1F600}");
    // Lone high surrogate refused.
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":\"\\uD83D\\u0041\"}"),
        Err(JsonError::Malformed { .. })
    ));
    // Lone low surrogate refused.
    assert!(matches!(
        strict_json::parse_strict(b"{\"a\":\"\\uDE00\"}"),
        Err(JsonError::Malformed { .. })
    ));
    // Bad escapes and numbers refused.
    for bad in [
        &b"{\"a\":\"\\x\"}"[..],
        &b"{\"a\":01}"[..],
        &b"{\"a\":1.}"[..],
        &b"{\"a\":+1}"[..],
        &b"{\"a\":.5}"[..],
        &b"{\"a\":1e}"[..],
        &b"{'a':1}"[..],
        &b"[1,]"[..],
        &b"{\"a\":}"[..],
    ] {
        assert!(
            matches!(
                strict_json::parse_strict(bad),
                Err(JsonError::Malformed { .. })
            ),
            "{bad:?} must be malformed"
        );
    }
    // Valid numbers and structures pass.
    for good in [
        &b"{\"a\":-1.5e3}"[..],
        &b"[true,false,null,\"s\"]"[..],
        &b"18446744073709551615"[..],
    ] {
        assert!(strict_json::parse_strict(good).is_ok(), "{good:?}");
    }
}

// --- prepare_request / parse_request ----------------------------------------

#[test]
fn prepare_request_round_trips_through_parse_request() {
    let mut parts = Parts::new();
    parts.excluded = vec![issue("owned-1")];
    parts.task_constraints = vec!["a-constraint".to_string(), "b-constraint".to_string()];
    parts.verification_argv = vec!["make".to_string(), "test".to_string()];
    parts.required_permissions = vec!["perm-a".to_string(), "perm-b".to_string()];
    let req = protocol::prepare_request(&parts.spec(4), NOW_MS).unwrap();
    assert!(req.bytes.len() <= protocol::MAX_REQUEST_BYTES);
    let parsed = protocol::parse_request(&req.bytes).expect("own request parses");
    assert_eq!(parsed.request_id, "req-1");
    assert_eq!(parsed.selector, "test-sel");
    assert_eq!(parsed.selector_version, 1);
    assert_eq!(parsed.policy_revision, 7);
    assert_eq!(parsed.max_candidates, 4);
    assert_eq!(parsed.deadline_unix_ms, NOW_MS + 60_000);
    assert_eq!(parsed.repositories.len(), 2);
    assert_eq!(parsed.excluded_sources.len(), 1);
    assert_eq!(
        parsed.task_constraints,
        vec!["a-constraint".to_string(), "b-constraint".to_string()]
    );
    assert_eq!(
        parsed.verification_argv,
        vec!["make".to_string(), "test".to_string()]
    );
    assert_eq!(
        parsed.required_permissions,
        vec!["perm-a".to_string(), "perm-b".to_string()]
    );
}

#[test]
fn prepare_request_sorts_sets_and_preserves_argv_order() {
    let mut parts = Parts::new();
    parts.repositories = vec![repo("repo-b"), repo("repo-a")];
    let req = protocol::prepare_request(&parts.spec(1), NOW_MS).unwrap();
    let text = String::from_utf8(req.bytes.clone()).unwrap();
    let repo_a = text.find("repo-a").unwrap();
    let repo_b = text.find("repo-b").unwrap();
    assert!(repo_a < repo_b, "repositories sorted on the wire");
}

#[test]
fn prepare_request_enforces_repository_and_exclusion_bounds() {
    // Empty and oversized repository sets.
    let mut parts = Parts::new();
    parts.repositories = vec![];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "repositories_bound"
    );
    parts.repositories = (0..=protocol::MAX_REPOSITORIES)
        .map(|i| RepositoryRef::new(host(), RepositoryId::trusted(format!("r{i:03}"))))
        .collect();
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "repositories_bound"
    );
    // Duplicates — even under different display numbers — refused.
    let mut parts = Parts::new();
    let mut dup = issue("same");
    dup.display_number = Some(9);
    let mut base = issue("same");
    base.display_number = Some(3);
    parts.excluded = vec![base, dup];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "exclusions_unique"
    );
}

#[test]
fn prepare_request_enforces_candidate_and_deadline_bounds() {
    let parts = Parts::new();
    for bad in [0u32, protocol::MAX_CANDIDATES + 1] {
        assert_eq!(
            contract_rule(protocol::prepare_request(&parts.spec(bad), NOW_MS)),
            "max_candidates"
        );
    }
    for deadline in [0i64, NOW_MS, NOW_MS + protocol::MAX_DEADLINE_AHEAD_MS + 1] {
        assert_eq!(
            contract_rule(protocol::prepare_request(
                &spec_with_deadline(&parts, 1, deadline),
                NOW_MS
            )),
            "deadline"
        );
    }
    assert!(
        protocol::prepare_request(
            &spec_with_deadline(&parts, 1, NOW_MS + protocol::MAX_DEADLINE_AHEAD_MS),
            NOW_MS
        )
        .is_ok()
    );
}

#[test]
fn prepare_request_enforces_task_field_bounds() {
    let mut parts = Parts::new();
    // Constraints must be sorted and distinct.
    parts.task_constraints = vec!["b".to_string(), "a".to_string()];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "task_constraints"
    );
    parts.task_constraints = vec!["a".to_string(), "a".to_string()];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "task_constraints"
    );
    parts.task_constraints = vec!["x".repeat(protocol::MAX_CONSTRAINT_BYTES + 1)];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "task_constraints"
    );
    // argv items bounded.
    parts.task_constraints = vec![];
    parts.verification_argv = vec!["x".repeat(protocol::MAX_ARGV_ITEM_BYTES + 1)];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "verification_argv"
    );
    parts.verification_argv = vec![];
    parts.required_permissions = vec!["p".repeat(protocol::MAX_PERMISSION_BYTES + 1)];
    assert_eq!(
        contract_rule(protocol::prepare_request(&parts.spec(1), NOW_MS)),
        "required_permissions"
    );
}

#[test]
fn prepare_request_enforces_selector_identity() {
    let mut parts = Parts::new();
    parts.required_permissions = vec![];
    let mut spec = parts.spec(1);
    spec.selector = "Bad_Selector";
    assert_eq!(
        contract_rule(protocol::prepare_request(&spec, NOW_MS)),
        "selector_id"
    );
    spec.selector = "test-sel";
    spec.selector_version = 0;
    assert_eq!(
        contract_rule(protocol::prepare_request(&spec, NOW_MS)),
        "selector_version"
    );
}

#[test]
fn prepare_request_refuses_oversized_wire() {
    // Enough long exclusions to push canonical JSON past 1 MiB.
    let mut parts = Parts::new();
    parts.excluded = (0..protocol::MAX_EXCLUSIONS)
        .map(|i| IssueRef {
            repository: repo("repo-a"),
            issue: IssueId::trusted(format!("issue-{i:04}-{}", "x".repeat(300))),
            display_number: Some(i as u64 + 1),
        })
        .collect();
    let result = protocol::prepare_request(&parts.spec(1), NOW_MS);
    assert_eq!(contract_rule(result), "request_too_large");
}

#[test]
fn parse_request_rejects_malformed_and_unknown_fields() {
    let req = request();
    // Duplicate key.
    let mut dup = String::from_utf8(req.bytes.clone()).unwrap();
    let pos = dup.find("\"selector\"").unwrap();
    dup.insert_str(pos, "\"request_id\":\"dup\",");
    assert!(matches!(
        protocol::parse_request(dup.as_bytes()),
        Err(Violation::Parse(JsonError::DuplicateKey { .. }))
    ));
    // Unknown field (deny_unknown_fields).
    let mut value: serde_json::Value = serde_json::from_slice(&req.bytes).unwrap();
    value["surprise"] = serde_json::json!(1);
    assert_eq!(
        contract_rule(protocol::parse_request(value.to_string().as_bytes())),
        "request_shape"
    );
    // Out-of-bound fields mirror the emission rules.
    value = serde_json::from_slice(&req.bytes).unwrap();
    value["max_candidates"] = serde_json::json!(0);
    assert_eq!(
        contract_rule(protocol::parse_request(value.to_string().as_bytes())),
        "max_candidates"
    );
    value = serde_json::from_slice(&req.bytes).unwrap();
    value["task_constraints"] = serde_json::json!(["b", "a"]);
    assert_eq!(
        contract_rule(protocol::parse_request(value.to_string().as_bytes())),
        "task_constraints"
    );
}

// --- validate_response --------------------------------------------------------

#[test]
fn validate_response_accepts_a_conforming_batch_in_order() {
    let req = request();
    let proposals = vec![
        proposal_for(issue("issue-1"), "first work"),
        proposal_for(issue("issue-2"), "second work"),
    ];
    let reply = validate(&response_json("req-1", &proposals), &req).unwrap();
    let SelectorReply::Success { candidates } = reply else {
        panic!("expected success")
    };
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].task.intent, "first work");
    assert_eq!(candidates[1].task.intent, "second work");
    assert_eq!(candidates[0].key, proposals[0].key);
    assert_eq!(candidates[0].selector.as_str(), "test-sel");
}

#[test]
fn validate_response_accepts_an_empty_candidate_set() {
    let req = request();
    let reply = validate(&response_json("req-1", &[]), &req).unwrap();
    assert!(matches!(reply, SelectorReply::Success { candidates } if candidates.is_empty()));
}

#[test]
fn validate_response_enforces_envelope_and_correlation() {
    let req = request();
    let proposals = vec![proposal_for(issue("issue-1"), "work")];
    let mut v = response_json("req-1", &proposals);
    // Not an object.
    assert_eq!(
        contract_rule(validate(&serde_json::json!([1, 2]), &req)),
        "envelope_object"
    );
    // Missing/unknown status.
    v["status"] = serde_json::json!("maybe");
    assert_eq!(contract_rule(validate(&v, &req)), "envelope_status");
    v = response_json("req-1", &proposals);
    v.as_object_mut().unwrap().remove("status");
    assert_eq!(contract_rule(validate(&v, &req)), "envelope_status");
    // Wrong protocol version and capability set.
    v = response_json("req-1", &proposals);
    v["protocol_version"] = serde_json::json!("2");
    assert_eq!(contract_rule(validate(&v, &req)), "protocol_version");
    v = response_json("req-1", &proposals);
    v["capabilities"] = serde_json::json!([]);
    assert_eq!(contract_rule(validate(&v, &req)), "capabilities");
    // Correlation: the response must echo this request's id.
    v = response_json("other-req", &proposals);
    assert_eq!(contract_rule(validate(&v, &req)), "request_id_mismatch");
    // Unknown envelope field refused by the wire shape.
    v = response_json("req-1", &proposals);
    v["extra"] = serde_json::json!(true);
    assert_eq!(contract_rule(validate(&v, &req)), "success_shape");
}

#[test]
fn validate_response_enforces_candidate_count_bounds() {
    let req = request(); // max_candidates = 4
    let proposals: Vec<Proposal> = (0..5)
        .map(|i| proposal_for(issue(&format!("issue-{i}")), "work"))
        .collect();
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &proposals), &req)),
        "candidate_bound"
    );
    // The absolute 32 bound applies even when max_candidates allows it.
    let parts = Parts::new();
    let req32 = protocol::prepare_request(&parts.spec(32), NOW_MS).unwrap();
    let proposals: Vec<Proposal> = (0..33)
        .map(|i| proposal_for(issue(&format!("issue-{i:02}")), "work"))
        .collect();
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &proposals), &req32)),
        "candidate_bound"
    );
}

#[test]
fn validate_response_enforces_selector_binding_and_members() {
    let req = request();
    let proposals = vec![proposal_for(issue("issue-1"), "work")];
    // Wrong selector id on the wire.
    let mut v = response_json("req-1", &proposals);
    v["candidates"][0]["selector"] = serde_json::json!("other-sel");
    assert_eq!(contract_rule(validate(&v, &req)), "selector_binding");
    // Empty sources.
    v = response_json("req-1", &proposals);
    v["candidates"][0]["sources"] = serde_json::json!([]);
    assert_eq!(contract_rule(validate(&v, &req)), "member_bound");
    // 33 members exceeds the group bound — the wire-length check runs
    // before member parsing, so the extra entry need not be valid.
    let grouped = proposal_for(issue("issue-9"), "grouped");
    let mut v = response_json("req-1", &[grouped]);
    for i in 0..33 {
        v["candidates"][0]["sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "issue": {"repository": {"host": "test-host", "repository": "repo-a"},
                          "issue": format!("g{i:02}"), "display_number": i + 1},
                "revision": "r", "content_digest": "d",
            }));
    }
    assert_eq!(contract_rule(validate(&v, &req)), "member_bound");
}

#[test]
fn validate_response_enforces_scope_exclusions_and_member_order() {
    let mut parts = Parts::new();
    // Display 42 keeps the exclusion clear of the display-7 fixtures
    // every other candidate carries.
    let mut owned = issue("owned-1");
    owned.display_number = Some(42);
    parts.excluded = vec![owned];
    let req = protocol::prepare_request(&parts.spec(4), NOW_MS).unwrap();

    // Repository outside the allowlist.
    let outside = IssueRef {
        repository: repo("repo-c"),
        issue: IssueId::trusted("x-1"),
        display_number: Some(1),
    };
    let p = proposal_for(outside, "work");
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &[p]), &req)),
        "repository_scope"
    );

    // Excluded by issue identity — display number plays no part.
    let p = proposal_for(issue("owned-1"), "work");
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &[p]), &req)),
        "excluded_source"
    );
    // Excluded by display number even when the opaque id differs.
    let mut by_number = issue("different-id");
    by_number.display_number = Some(42);
    let p = proposal_for(by_number, "work");
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &[p]), &req)),
        "excluded_source"
    );

    // Unsorted members inside one candidate.
    let mut unsorted = proposal_for(issue("issue-1"), "work");
    unsorted.sources.push(SourceBinding {
        issue: IssueRef {
            repository: repo("repo-a"),
            issue: IssueId::trusted("aaa-first"),
            display_number: Some(1),
        },
        revision: "r".into(),
        content_digest: "d".into(),
    });
    unsorted.key = proposal_key(
        "test-sel",
        1,
        7,
        &unsorted.sources,
        &unsorted.task,
        &unsorted.required_permissions,
    );
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &[unsorted]), &req)),
        "member_order"
    );

    // The same scoped issue in two candidates.
    let a = proposal_for(issue("dup-1"), "one");
    let b = proposal_for(issue("dup-1"), "two");
    assert_eq!(
        contract_rule(validate(&response_json("req-1", &[a, b]), &req)),
        "duplicate_source"
    );
}

#[test]
fn validate_response_enforces_task_fields_and_permissions() {
    let req = request();
    let p = proposal_for(issue("issue-1"), "work");

    // Intent: empty, whitespace-only, oversized. The key is recomputed
    // so validation reaches the intent rule, not proposal_key.
    for bad in [
        "".to_string(),
        "   ".to_string(),
        "x".repeat(protocol::MAX_INTENT_BYTES + 1),
    ] {
        let label = format!("{bad:?}");
        let mut q = p.clone();
        q.task.intent = bad;
        q.key = proposal_key(
            "test-sel",
            1,
            7,
            &q.sources,
            &q.task,
            &q.required_permissions,
        );
        let v = response_json("req-1", &[q]);
        assert_eq!(
            contract_rule(validate(&v, &req)),
            "intent",
            "intent {label}"
        );
    }

    // Constraints: must include every request constraint, sorted, bounded.
    let mut q = p.clone();
    q.task.constraints = vec![];
    q.key = proposal_key(
        "test-sel",
        1,
        7,
        &q.sources,
        &q.task,
        &q.required_permissions,
    );
    let v = response_json("req-1", &[q.clone()]);
    assert_eq!(contract_rule(validate(&v, &req)), "task_constraints");
    q.task.constraints = vec!["constraint-a".to_string(), "aaa-extra".to_string()];
    // "aaa-extra" sorts before "constraint-a" — unsorted.
    q.key = proposal_key(
        "test-sel",
        1,
        7,
        &q.sources,
        &q.task,
        &q.required_permissions,
    );
    let v = response_json("req-1", &[q]);
    assert_eq!(contract_rule(validate(&v, &req)), "task_constraints");

    // verification_argv must equal the request's exactly.
    let mut q = p.clone();
    q.task.verification_argv = vec!["false".to_string()];
    q.key = proposal_key(
        "test-sel",
        1,
        7,
        &q.sources,
        &q.task,
        &q.required_permissions,
    );
    let v = response_json("req-1", &[q]);
    assert_eq!(contract_rule(validate(&v, &req)), "verification_argv");

    // required_permissions must equal the request's set.
    let mut q = p.clone();
    q.required_permissions = vec!["perm-a".to_string(), "perm-b".to_string()];
    q.key = proposal_key(
        "test-sel",
        1,
        7,
        &q.sources,
        &q.task,
        &q.required_permissions,
    );
    let v = response_json("req-1", &[q]);
    assert_eq!(contract_rule(validate(&v, &req)), "required_permissions");
}

#[test]
fn validate_response_enforces_provenance_bounds() {
    let req = request();
    let mut p = proposal_for(issue("issue-1"), "work");
    p.provenance = Provenance {
        snapshot_cursor: Some("c".repeat(protocol::MAX_CURSOR_BYTES + 1)),
        fetched_unix_seconds: None,
    };
    // Cursor is provenance, not identity — the key still matches.
    let v = response_json("req-1", &[p]);
    assert_eq!(contract_rule(validate(&v, &req)), "provenance_cursor");
    let mut p = proposal_for(issue("issue-1"), "work");
    p.provenance = Provenance {
        snapshot_cursor: None,
        fetched_unix_seconds: Some(-1),
    };
    let v = response_json("req-1", &[p]);
    assert_eq!(contract_rule(validate(&v, &req)), "provenance_time");
}

#[test]
fn validate_response_recomputes_proposal_keys() {
    let req = request();
    let p = proposal_for(issue("issue-1"), "work");
    // Malformed key shape.
    let mut v = response_json("req-1", std::slice::from_ref(&p));
    v["candidates"][0]["key"] = serde_json::json!("not-a-digest");
    assert_eq!(contract_rule(validate(&v, &req)), "proposal_key_shape");
    // Well-formed but wrong digest.
    let mut v = response_json("req-1", std::slice::from_ref(&p));
    v["candidates"][0]["key"] = serde_json::json!("0".repeat(64));
    assert_eq!(contract_rule(validate(&v, &req)), "proposal_key");
    // Uppercase hex is not the digest shape.
    let mut v = response_json("req-1", &[p]);
    v["candidates"][0]["key"] = serde_json::json!("A".repeat(64));
    assert_eq!(contract_rule(validate(&v, &req)), "proposal_key_shape");
    // `duplicate_candidate` is unreachable through valid keys: the key
    // covers the sources, so equal keys imply equal members — which
    // `duplicate_source` refuses first.
}

#[test]
fn validate_response_rejects_malformed_payloads() {
    let req = request();
    for bad in [
        &b""[..],
        b"not json",
        b"{\"status\":\"ok\"} trailing",
        b"\xEF\xBB\xBF{}",
    ] {
        assert!(
            matches!(
                protocol::validate_response(bad, &req),
                Err(Violation::Parse(_))
            ),
            "{bad:?} must be a parse violation"
        );
    }
    // Oversized response refused before parsing.
    let oversized = vec![b' '; protocol::MAX_RESPONSE_BYTES + 1];
    assert_eq!(
        contract_rule(protocol::validate_response(&oversized, &req)),
        "response_too_large"
    );
}

#[test]
fn validate_response_maps_adapter_failures() {
    let req = request();
    for expected in [
        FailureCode::SourceUnavailable,
        FailureCode::AuthenticationFailed,
        FailureCode::DeadlineExceeded,
        FailureCode::InternalError,
    ] {
        let bytes = protocol::failure_response(Some("req-1"), expected, "bounded message");
        let reply = protocol::validate_response(&bytes, &req).unwrap();
        assert!(matches!(reply, SelectorReply::Failure { code: c, .. } if c == expected));
    }
    // A failure for a different request id is still a failure reply —
    // correlation is only enforced on success.
    let bytes = protocol::failure_response(Some("other"), FailureCode::InternalError, "x");
    assert!(matches!(
        protocol::validate_response(&bytes, &req),
        Ok(SelectorReply::Failure { .. })
    ));
    // Unknown failure code and oversized message refused.
    let mut v: serde_json::Value = serde_json::from_slice(&protocol::failure_response(
        None,
        FailureCode::InternalError,
        "x",
    ))
    .unwrap();
    v["error"]["code"] = serde_json::json!("made_up_code");
    assert_eq!(contract_rule(validate(&v, &req)), "failure_code");
    v = serde_json::from_slice(&protocol::failure_response(
        None,
        FailureCode::InternalError,
        "x",
    ))
    .unwrap();
    v["error"]["message"] = serde_json::json!("m".repeat(protocol::MAX_MESSAGE_BYTES + 1));
    assert_eq!(contract_rule(validate(&v, &req)), "failure_message");
}

#[test]
fn validate_response_failure_via_mutated_success_envelope() {
    let req = request();
    // status:"error" with a success-shaped body fails the failure shape;
    // the failure envelope admits no `capabilities` field.
    let mut v = response_json("req-1", &[]);
    v["status"] = serde_json::json!("error");
    v["error"] = serde_json::json!({"code": "internal_error", "message": "boom"});
    v.as_object_mut().unwrap().remove("candidates");
    v.as_object_mut().unwrap().remove("capabilities");
    let reply = protocol::validate_response(v.to_string().as_bytes(), &req).unwrap();
    assert!(matches!(reply, SelectorReply::Failure { .. }));
}
