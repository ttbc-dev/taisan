//! Selector wire protocol (`docs/selector-protocol.md`): request
//! construction and whole-response validation for the stateless
//! external Selector exchange.
//!
//! This module owns both ends of the contract's *shape*:
//!
//! - [`prepare_request`] turns an intake run's parameters into the
//!   canonical request bytes sent on the child's stdin, after
//!   enforcing every request-side bound the protocol declares.
//! - [`validate_response`] parses one bounded stdout payload under
//!   the strict JSON reader, then applies every normative response
//!   rule — envelope, correlation, per-candidate structure, sorted
//!   distinct sets, exclusion and allowlist scope, recomputed
//!   proposal keys — and yields domain [`Proposal`]s in returned
//!   order, or one [`Violation`] that admits **zero** candidates.
//!
//! The caller — never this module — decides what a conforming
//! response means operationally; transport failures and malformed
//! payloads stay here as bounded, payload-free diagnostics.

use std::collections::BTreeSet;

use serde::Deserialize;
use workloop_core::domain::canonical_json;
use workloop_core::identity::{
    AdapterId, HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef,
};

use crate::strict_json::{self, JsonError};
use crate::{Proposal, Provenance, SourceBinding, TaskInput, excluded, proposal_key};

/// Protocol version this build speaks and requires.
pub const PROTOCOL_VERSION: &str = "1";
/// The single capability the request advertises and a success must
/// echo verbatim.
pub const CAPABILITY_GROUPED_SOURCES: &str = "grouped_sources_v1";

// Wire bounds declared by docs/selector-protocol.md.
pub const MAX_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_RESPONSE_BYTES: usize = 4_194_304;
pub const MAX_STDERR_BYTES: usize = 16_384;
pub const MAX_ID_BYTES: usize = 512;
pub const MAX_ADAPTER_BYTES: usize = 64;
pub const MAX_INTENT_BYTES: usize = 16_384;
pub const MAX_CONSTRAINT_BYTES: usize = 4_096;
pub const MAX_ARGV_ITEM_BYTES: usize = 4_096;
pub const MAX_ARGV_TOTAL_BYTES: usize = 16_384;
pub const MAX_PERMISSION_BYTES: usize = 128;
pub const MAX_CURSOR_BYTES: usize = 1_024;
pub const MAX_MESSAGE_BYTES: usize = 256;
pub const MAX_REPOSITORIES: usize = 64;
pub const MAX_EXCLUSIONS: usize = 4_096;
pub const MAX_CANDIDATES: u32 = 32;
pub const MAX_GROUP_MEMBERS: usize = 32;
pub const MAX_TOTAL_SOURCES: usize = 1_024;
pub const MAX_CONSTRAINTS: usize = 64;
pub const MAX_ARGV_ITEMS: usize = 64;
pub const MAX_PERMISSIONS: usize = 32;
/// The request deadline must be at most this far ahead of the clock
/// reading taken when the request was prepared.
pub const MAX_DEADLINE_AHEAD_MS: i64 = 300_000;

/// Everything one intake run needs to state to the adapter. Borrowed
/// inputs are normalized — sorted and deduplicated-checked — by
/// [`prepare_request`]; the returned [`ValidatedRequest`] carries the
/// canonical bytes plus the normalized fields [`validate_response`]
/// checks candidates against.
pub struct RequestSpec<'a> {
    /// Caller-chosen bounded correlation id, echoed in the response.
    pub request_id: &'a str,
    /// Configured selector adapter id (`[a-z][a-z0-9-]{0,63}`).
    pub selector: &'a str,
    pub selector_version: u32,
    pub policy_revision: u64,
    /// Complete allowlisted repository array — sorted on the wire.
    pub repositories: &'a [RepositoryRef],
    /// Excluded source issues — sorted on the wire.
    pub excluded_sources: &'a [IssueRef],
    /// 1–32; the free pending-queue positions at preflight.
    pub max_candidates: u32,
    /// Absolute unix-ms deadline, at most 300s ahead of `now`.
    pub deadline_unix_ms: i64,
    pub task_constraints: &'a [String],
    pub verification_argv: &'a [String],
    pub required_permissions: &'a [String],
}

/// A request that passed every request-side rule. `bytes` is the
/// canonical JSON written to the child's stdin.
pub struct ValidatedRequest {
    pub bytes: Vec<u8>,
    pub request_id: String,
    pub selector: String,
    pub selector_version: u32,
    pub policy_revision: u64,
    pub repositories: Vec<RepositoryRef>,
    pub excluded_sources: Vec<IssueRef>,
    pub max_candidates: u32,
    pub task_constraints: Vec<String>,
    pub verification_argv: Vec<String>,
    pub required_permissions: Vec<String>,
}

/// Why a request or response failed the contract. Every variant is a
/// bounded, payload-free diagnostic — wire contents never enter it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The bytes were not a single strict JSON value.
    Parse(JsonError),
    /// A normative contract rule failed; the tag names the rule.
    Contract(&'static str),
}

/// The adapter's own declared outcome inside a conforming envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorReply {
    /// `status: "ok"` — every candidate structurally validated; the
    /// vector preserves returned order.
    Success { candidates: Vec<Proposal> },
    /// `status: "error"` — the adapter reported a bounded failure.
    Failure { code: FailureCode, message: String },
}

/// The protocol's closed failure-code vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    InvalidRequest,
    UnsupportedVersion,
    UnsupportedCapability,
    InputLimit,
    MemberLimit,
    SourceUnavailable,
    AuthenticationFailed,
    DeadlineExceeded,
    InternalError,
}

impl FailureCode {
    fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "invalid_request" => Self::InvalidRequest,
            "unsupported_version" => Self::UnsupportedVersion,
            "unsupported_capability" => Self::UnsupportedCapability,
            "input_limit" => Self::InputLimit,
            "member_limit" => Self::MemberLimit,
            "source_unavailable" => Self::SourceUnavailable,
            "authentication_failed" => Self::AuthenticationFailed,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "internal_error" => Self::InternalError,
            _ => return None,
        })
    }
}

fn is_adapter_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ADAPTER_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Whether `value` satisfies the protocol `id` definition: 1–512
/// bytes, no control characters. The strict parser has already
/// rejected control characters in every string it produced, so this
/// is a length check in practice — kept explicit because the bound is
/// normative, not incidental.
fn is_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

/// Nonempty, byte-bounded, control-free — the schema's string shape.
/// Whitespace-only content is schema-valid for every field except
/// `intent`, which adds its own non-whitespace rule at the call site.
fn is_wire_string(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn sorted_distinct(items: &[String]) -> bool {
    items.windows(2).all(|w| w[0] < w[1])
}

/// Validate `spec` against every request-side rule and return the
/// canonical request bytes plus the normalized fields. `now_unix_ms`
/// is the caller's clock reading used only for the deadline bound.
pub fn prepare_request(
    spec: &RequestSpec<'_>,
    now_unix_ms: i64,
) -> Result<ValidatedRequest, Violation> {
    if !is_id(spec.request_id) {
        return Err(Violation::Contract("request_id"));
    }
    if !is_adapter_id(spec.selector) {
        return Err(Violation::Contract("selector_id"));
    }
    if spec.selector_version == 0 {
        return Err(Violation::Contract("selector_version"));
    }
    if spec.repositories.is_empty() || spec.repositories.len() > MAX_REPOSITORIES {
        return Err(Violation::Contract("repositories_bound"));
    }
    let mut repositories: Vec<RepositoryRef> = spec.repositories.to_vec();
    repositories.sort();
    repositories.dedup();
    if repositories.len() != spec.repositories.len() {
        return Err(Violation::Contract("repositories_unique"));
    }
    for repo in &repositories {
        if !is_id(repo.host.as_str()) || !is_id(repo.repository.as_str()) {
            return Err(Violation::Contract("repository_id"));
        }
    }

    if spec.excluded_sources.len() > MAX_EXCLUSIONS {
        return Err(Violation::Contract("exclusions_bound"));
    }
    let mut excluded_sources: Vec<IssueRef> = spec.excluded_sources.to_vec();
    excluded_sources.sort_by(|a, b| (&a.repository, &a.issue).cmp(&(&b.repository, &b.issue)));
    // Uniqueness is over scoped issue identity — the same issue listed
    // twice, even under different display numbers, is contradictory.
    for pair in excluded_sources.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.repository == b.repository && a.issue == b.issue {
            return Err(Violation::Contract("exclusions_unique"));
        }
    }
    for issue in &excluded_sources {
        if !is_id(issue.issue.as_str())
            || !is_id(issue.repository.host.as_str())
            || !is_id(issue.repository.repository.as_str())
        {
            return Err(Violation::Contract("exclusion_id"));
        }
        // Exclusions may name issues outside the allowlist — they are
        // simply inert there.
    }

    if !(1..=MAX_CANDIDATES).contains(&spec.max_candidates) {
        return Err(Violation::Contract("max_candidates"));
    }
    if spec.deadline_unix_ms < 1
        || spec.deadline_unix_ms <= now_unix_ms
        || spec.deadline_unix_ms - now_unix_ms > MAX_DEADLINE_AHEAD_MS
    {
        return Err(Violation::Contract("deadline"));
    }

    if spec.task_constraints.len() > MAX_CONSTRAINTS
        || !spec
            .task_constraints
            .iter()
            .all(|c| is_wire_string(c, MAX_CONSTRAINT_BYTES))
        || !sorted_distinct(spec.task_constraints)
    {
        return Err(Violation::Contract("task_constraints"));
    }
    if spec.verification_argv.len() > MAX_ARGV_ITEMS
        || !spec
            .verification_argv
            .iter()
            .all(|a| is_wire_string(a, MAX_ARGV_ITEM_BYTES))
        || spec
            .verification_argv
            .iter()
            .map(|a| a.len() + 1)
            .sum::<usize>()
            > MAX_ARGV_TOTAL_BYTES
    {
        return Err(Violation::Contract("verification_argv"));
    }
    if spec.required_permissions.len() > MAX_PERMISSIONS
        || !spec
            .required_permissions
            .iter()
            .all(|p| is_wire_string(p, MAX_PERMISSION_BYTES))
        || !sorted_distinct(spec.required_permissions)
    {
        return Err(Violation::Contract("required_permissions"));
    }

    let value = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "capabilities": [CAPABILITY_GROUPED_SOURCES],
        "request_id": spec.request_id,
        "selector": spec.selector,
        "selector_version": spec.selector_version,
        "policy_revision": spec.policy_revision,
        "repositories": repositories.iter().map(|r| serde_json::json!({
            "host": r.host.as_str(),
            "repository": r.repository.as_str(),
        })).collect::<Vec<_>>(),
        "excluded_sources": excluded_sources.iter().map(|i| serde_json::json!({
            "repository": {
                "host": i.repository.host.as_str(),
                "repository": i.repository.repository.as_str(),
            },
            "issue": i.issue.as_str(),
            "display_number": i.display_number,
        })).collect::<Vec<_>>(),
        "max_candidates": spec.max_candidates,
        "deadline_unix_ms": spec.deadline_unix_ms,
        "task_constraints": spec.task_constraints,
        "verification_argv": spec.verification_argv,
        "required_permissions": spec.required_permissions,
    });
    let bytes = canonical_json(&value);
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Violation::Contract("request_too_large"));
    }
    Ok(ValidatedRequest {
        bytes,
        request_id: spec.request_id.to_owned(),
        selector: spec.selector.to_owned(),
        selector_version: spec.selector_version,
        policy_revision: spec.policy_revision,
        repositories,
        excluded_sources,
        max_candidates: spec.max_candidates,
        task_constraints: spec.task_constraints.to_vec(),
        verification_argv: spec.verification_argv.to_vec(),
        required_permissions: spec.required_permissions.to_vec(),
    })
}

// --- Response wire shapes -------------------------------------------------
//
// `deny_unknown_fields` enforces `additionalProperties: false`;
// `Option` fields without `default` are required-but-nullable, which
// is exactly the schema's `anyOf [null, T]` shape. Field-level byte
// and count bounds are checked in `validate_response`, not here.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSuccess {
    protocol_version: String,
    capabilities: Vec<String>,
    request_id: String,
    status: String,
    candidates: Vec<WireProposal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFailure {
    protocol_version: String,
    request_id: Option<String>,
    status: String,
    error: WireError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProposal {
    key: String,
    selector: String,
    selector_version: u32,
    policy_revision: u64,
    sources: Vec<WireSource>,
    task: WireTask,
    required_permissions: Vec<String>,
    provenance: WireProvenance,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSource {
    issue: WireIssue,
    revision: String,
    content_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIssue {
    repository: WireRepository,
    issue: String,
    display_number: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRepository {
    host: String,
    repository: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTask {
    intent: String,
    constraints: Vec<String>,
    verification_argv: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProvenance {
    snapshot_cursor: Option<String>,
    fetched_unix_seconds: Option<i64>,
}

/// Validate one response payload against `req`. On success the
/// returned reply carries every candidate — a single [`Violation`]
/// means the whole response admitted zero candidates, including a
/// malformed later one.
pub fn validate_response(bytes: &[u8], req: &ValidatedRequest) -> Result<SelectorReply, Violation> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(Violation::Contract("response_too_large"));
    }
    let value = strict_json::parse_strict(bytes).map_err(Violation::Parse)?;
    let serde_json::Value::Object(map) = &value else {
        return Err(Violation::Contract("envelope_object"));
    };
    let status = map
        .get("status")
        .and_then(|s| s.as_str())
        .ok_or(Violation::Contract("envelope_status"))?;
    match status {
        "ok" => validate_success(value, req),
        "error" => validate_failure(value),
        _ => Err(Violation::Contract("envelope_status")),
    }
}

fn validate_success(
    value: serde_json::Value,
    req: &ValidatedRequest,
) -> Result<SelectorReply, Violation> {
    let wire: WireSuccess =
        serde_json::from_value(value).map_err(|_| Violation::Contract("success_shape"))?;
    if wire.protocol_version != PROTOCOL_VERSION {
        return Err(Violation::Contract("protocol_version"));
    }
    if wire.capabilities != [CAPABILITY_GROUPED_SOURCES] {
        return Err(Violation::Contract("capabilities"));
    }
    if wire.request_id != req.request_id {
        return Err(Violation::Contract("request_id_mismatch"));
    }
    if wire.status != "ok" {
        return Err(Violation::Contract("status"));
    }
    if wire.candidates.len() > req.max_candidates as usize
        || wire.candidates.len() > MAX_CANDIDATES as usize
    {
        return Err(Violation::Contract("candidate_bound"));
    }

    let repo_set: BTreeSet<&RepositoryRef> = req.repositories.iter().collect();
    let excluded_set: BTreeSet<IssueRef> = req.excluded_sources.iter().cloned().collect();
    let mut total_sources = 0usize;
    let mut seen_issues: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    let mut candidates = Vec::with_capacity(wire.candidates.len());

    for wire_candidate in &wire.candidates {
        if wire_candidate.selector != req.selector
            || wire_candidate.selector_version != req.selector_version
            || wire_candidate.policy_revision != req.policy_revision
        {
            return Err(Violation::Contract("selector_binding"));
        }
        if wire_candidate.sources.is_empty() || wire_candidate.sources.len() > MAX_GROUP_MEMBERS {
            return Err(Violation::Contract("member_bound"));
        }
        total_sources += wire_candidate.sources.len();
        if total_sources > MAX_TOTAL_SOURCES {
            return Err(Violation::Contract("source_total_bound"));
        }

        let mut sources: Vec<SourceBinding> = Vec::with_capacity(wire_candidate.sources.len());
        for wire_source in &wire_candidate.sources {
            if wire_source.issue.display_number == Some(0) {
                return Err(Violation::Contract("display_number"));
            }
            if !is_id(&wire_source.revision) || !is_id(&wire_source.content_digest) {
                return Err(Violation::Contract("source_id"));
            }
            let repository = RepositoryRef {
                host: HostInstanceId::new(wire_source.issue.repository.host.clone())
                    .map_err(|_| Violation::Contract("repository_id"))?,
                repository: RepositoryId::new(wire_source.issue.repository.repository.clone())
                    .map_err(|_| Violation::Contract("repository_id"))?,
            };
            if !repo_set.contains(&repository) {
                return Err(Violation::Contract("repository_scope"));
            }
            let issue_ref = IssueRef {
                repository,
                issue: IssueId::new(wire_source.issue.issue.clone())
                    .map_err(|_| Violation::Contract("issue_id"))?,
                display_number: wire_source.issue.display_number,
            };
            if !is_id(issue_ref.issue.as_str())
                || !is_id(issue_ref.repository.host.as_str())
                || !is_id(issue_ref.repository.repository.as_str())
            {
                return Err(Violation::Contract("issue_id"));
            }
            if excluded(&excluded_set, &issue_ref) {
                return Err(Violation::Contract("excluded_source"));
            }
            sources.push(SourceBinding {
                issue: issue_ref,
                revision: wire_source.revision.clone(),
                content_digest: wire_source.content_digest.clone(),
            });
        }
        // Members arrive sorted by (host, repository, issue) UTF-8
        // bytes with distinct scoped identities — strict increase in
        // wire order covers both rules, and display numbers take no
        // part in the comparison.
        let member_keys: Vec<(&str, &str, &str)> = sources
            .iter()
            .map(|s| {
                (
                    s.issue.repository.host.as_str(),
                    s.issue.repository.repository.as_str(),
                    s.issue.issue.as_str(),
                )
            })
            .collect();
        if !member_keys.windows(2).all(|w| w[0] < w[1]) {
            return Err(Violation::Contract("member_order"));
        }
        for key in &member_keys {
            if !seen_issues.insert((key.0.to_owned(), key.1.to_owned(), key.2.to_owned())) {
                return Err(Violation::Contract("duplicate_source"));
            }
        }

        if !is_wire_string(&wire_candidate.task.intent, MAX_INTENT_BYTES)
            || !wire_candidate
                .task
                .intent
                .chars()
                .any(|c| !c.is_whitespace())
        {
            return Err(Violation::Contract("intent"));
        }
        let task = TaskInput {
            intent: wire_candidate.task.intent.clone(),
            constraints: wire_candidate.task.constraints.clone(),
            verification_argv: wire_candidate.task.verification_argv.clone(),
        };
        if task.constraints.len() > MAX_CONSTRAINTS
            || !task
                .constraints
                .iter()
                .all(|c| is_wire_string(c, MAX_CONSTRAINT_BYTES))
            || !sorted_distinct(&task.constraints)
            || !req
                .task_constraints
                .iter()
                .all(|required| task.constraints.contains(required))
        {
            return Err(Violation::Contract("task_constraints"));
        }
        if task.verification_argv.len() > MAX_ARGV_ITEMS
            || !task
                .verification_argv
                .iter()
                .all(|a| is_wire_string(a, MAX_ARGV_ITEM_BYTES))
            || task
                .verification_argv
                .iter()
                .map(|a| a.len() + 1)
                .sum::<usize>()
                > MAX_ARGV_TOTAL_BYTES
            || task.verification_argv != req.verification_argv
        {
            return Err(Violation::Contract("verification_argv"));
        }
        let permissions = &wire_candidate.required_permissions;
        if permissions.len() > MAX_PERMISSIONS
            || !permissions
                .iter()
                .all(|p| is_wire_string(p, MAX_PERMISSION_BYTES))
            || !sorted_distinct(permissions)
            || permissions.iter().cloned().collect::<BTreeSet<String>>()
                != req
                    .required_permissions
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<String>>()
        {
            return Err(Violation::Contract("required_permissions"));
        }

        let provenance = Provenance {
            snapshot_cursor: wire_candidate.provenance.snapshot_cursor.clone(),
            fetched_unix_seconds: wire_candidate.provenance.fetched_unix_seconds,
        };
        if let Some(cursor) = &provenance.snapshot_cursor
            && (cursor.is_empty()
                || cursor.len() > MAX_CURSOR_BYTES
                || cursor.chars().any(char::is_control))
        {
            return Err(Violation::Contract("provenance_cursor"));
        }
        if let Some(secs) = provenance.fetched_unix_seconds
            && secs < 0
        {
            return Err(Violation::Contract("provenance_time"));
        }

        // The declared key must be a digest-shaped value equal to the
        // recomputed proposal-v1 digest — the wire never gets to
        // assert identity.
        if wire_candidate.key.len() != 64
            || !wire_candidate
                .key
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Violation::Contract("proposal_key_shape"));
        }
        let expected_key = proposal_key(
            &req.selector,
            req.selector_version,
            req.policy_revision,
            &sources,
            &task,
            permissions,
        );
        if wire_candidate.key != expected_key {
            return Err(Violation::Contract("proposal_key"));
        }
        if !seen_keys.insert(wire_candidate.key.clone()) {
            return Err(Violation::Contract("duplicate_candidate"));
        }

        candidates.push(Proposal {
            key: wire_candidate.key.clone(),
            selector: AdapterId::new(req.selector.clone())
                .map_err(|_| Violation::Contract("selector_id"))?,
            selector_version: req.selector_version,
            policy_revision: req.policy_revision,
            sources,
            task,
            required_permissions: permissions.clone(),
            provenance,
        });
    }
    Ok(SelectorReply::Success { candidates })
}

fn validate_failure(value: serde_json::Value) -> Result<SelectorReply, Violation> {
    let wire: WireFailure =
        serde_json::from_value(value).map_err(|_| Violation::Contract("failure_shape"))?;
    if wire.protocol_version != PROTOCOL_VERSION {
        return Err(Violation::Contract("protocol_version"));
    }
    if wire.status != "error" {
        return Err(Violation::Contract("status"));
    }
    if let Some(id) = &wire.request_id
        && !is_id(id)
    {
        return Err(Violation::Contract("request_id"));
    }
    let code = FailureCode::parse(&wire.error.code).ok_or(Violation::Contract("failure_code"))?;
    if wire.error.message.is_empty()
        || wire.error.message.len() > MAX_MESSAGE_BYTES
        || wire.error.message.chars().any(char::is_control)
    {
        return Err(Violation::Contract("failure_message"));
    }
    Ok(SelectorReply::Failure {
        code,
        message: wire.error.message,
    })
}

/// A request that passed every request-side rule after parsing —
/// the adapter's view of what it was asked. Field order on the wire
/// is normalized exactly as [`prepare_request`] emits it: sets are
/// sorted, argv keeps its order, and duplicates are refused rather
/// than silently collapsed.
pub struct ParsedRequest {
    pub request_id: String,
    pub selector: String,
    pub selector_version: u32,
    pub policy_revision: u64,
    pub repositories: Vec<RepositoryRef>,
    pub excluded_sources: Vec<IssueRef>,
    pub max_candidates: u32,
    pub deadline_unix_ms: i64,
    pub task_constraints: Vec<String>,
    pub verification_argv: Vec<String>,
    pub required_permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    protocol_version: String,
    capabilities: Vec<String>,
    request_id: String,
    selector: String,
    selector_version: u32,
    policy_revision: u64,
    repositories: Vec<WireRepository>,
    excluded_sources: Vec<WireExcludedSource>,
    max_candidates: u32,
    deadline_unix_ms: i64,
    task_constraints: Vec<String>,
    verification_argv: Vec<String>,
    required_permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExcludedSource {
    repository: WireRepository,
    issue: String,
    display_number: Option<u64>,
}

/// Parse one request payload — the adapter side of the contract.
/// Applies the same bounds [`prepare_request`] enforces on emission
/// (envelope, identity shapes, set ordering and uniqueness, count and
/// byte limits) so a conforming caller's request always parses and a
/// malformed one fails before any source read. The deadline's
/// freshness is *not* checked here: it is a function of the adapter's
/// own clock, which the caller compares against
/// [`ParsedRequest::deadline_unix_ms`].
pub fn parse_request(bytes: &[u8]) -> Result<ParsedRequest, Violation> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Violation::Contract("request_too_large"));
    }
    let value = strict_json::parse_strict(bytes).map_err(Violation::Parse)?;
    let wire: WireRequest =
        serde_json::from_value(value).map_err(|_| Violation::Contract("request_shape"))?;
    if wire.protocol_version != PROTOCOL_VERSION {
        return Err(Violation::Contract("protocol_version"));
    }
    if wire.capabilities != [CAPABILITY_GROUPED_SOURCES] {
        return Err(Violation::Contract("capabilities"));
    }
    if !is_id(&wire.request_id) {
        return Err(Violation::Contract("request_id"));
    }
    if !is_adapter_id(&wire.selector) {
        return Err(Violation::Contract("selector_id"));
    }
    if wire.selector_version == 0 {
        return Err(Violation::Contract("selector_version"));
    }
    if wire.repositories.is_empty() || wire.repositories.len() > MAX_REPOSITORIES {
        return Err(Violation::Contract("repositories_bound"));
    }
    let mut repositories: Vec<RepositoryRef> = Vec::with_capacity(wire.repositories.len());
    for repo in &wire.repositories {
        let repository = RepositoryRef {
            host: HostInstanceId::new(repo.host.clone())
                .map_err(|_| Violation::Contract("repository_id"))?,
            repository: RepositoryId::new(repo.repository.clone())
                .map_err(|_| Violation::Contract("repository_id"))?,
        };
        if !is_id(repository.host.as_str()) || !is_id(repository.repository.as_str()) {
            return Err(Violation::Contract("repository_id"));
        }
        repositories.push(repository);
    }
    repositories.sort();
    repositories.dedup();
    if repositories.len() != wire.repositories.len() {
        return Err(Violation::Contract("repositories_unique"));
    }
    if wire.excluded_sources.len() > MAX_EXCLUSIONS {
        return Err(Violation::Contract("exclusions_bound"));
    }
    let mut excluded_sources: Vec<IssueRef> = Vec::with_capacity(wire.excluded_sources.len());
    for entry in &wire.excluded_sources {
        if entry.display_number == Some(0) {
            return Err(Violation::Contract("display_number"));
        }
        let issue = IssueRef {
            repository: RepositoryRef {
                host: HostInstanceId::new(entry.repository.host.clone())
                    .map_err(|_| Violation::Contract("exclusion_id"))?,
                repository: RepositoryId::new(entry.repository.repository.clone())
                    .map_err(|_| Violation::Contract("exclusion_id"))?,
            },
            issue: IssueId::new(entry.issue.clone())
                .map_err(|_| Violation::Contract("exclusion_id"))?,
            display_number: entry.display_number,
        };
        if !is_id(issue.issue.as_str())
            || !is_id(issue.repository.host.as_str())
            || !is_id(issue.repository.repository.as_str())
        {
            return Err(Violation::Contract("exclusion_id"));
        }
        excluded_sources.push(issue);
    }
    excluded_sources.sort_by(|a, b| (&a.repository, &a.issue).cmp(&(&b.repository, &b.issue)));
    for pair in excluded_sources.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.repository == b.repository && a.issue == b.issue {
            return Err(Violation::Contract("exclusions_unique"));
        }
    }
    if !(1..=MAX_CANDIDATES).contains(&wire.max_candidates) {
        return Err(Violation::Contract("max_candidates"));
    }
    if wire.deadline_unix_ms < 1 {
        return Err(Violation::Contract("deadline"));
    }
    if wire.task_constraints.len() > MAX_CONSTRAINTS
        || !wire
            .task_constraints
            .iter()
            .all(|c| is_wire_string(c, MAX_CONSTRAINT_BYTES))
        || !sorted_distinct(&wire.task_constraints)
    {
        return Err(Violation::Contract("task_constraints"));
    }
    if wire.verification_argv.len() > MAX_ARGV_ITEMS
        || !wire
            .verification_argv
            .iter()
            .all(|a| is_wire_string(a, MAX_ARGV_ITEM_BYTES))
        || wire
            .verification_argv
            .iter()
            .map(|a| a.len() + 1)
            .sum::<usize>()
            > MAX_ARGV_TOTAL_BYTES
    {
        return Err(Violation::Contract("verification_argv"));
    }
    if wire.required_permissions.len() > MAX_PERMISSIONS
        || !wire
            .required_permissions
            .iter()
            .all(|p| is_wire_string(p, MAX_PERMISSION_BYTES))
        || !sorted_distinct(&wire.required_permissions)
    {
        return Err(Violation::Contract("required_permissions"));
    }
    Ok(ParsedRequest {
        request_id: wire.request_id,
        selector: wire.selector,
        selector_version: wire.selector_version,
        policy_revision: wire.policy_revision,
        repositories,
        excluded_sources,
        max_candidates: wire.max_candidates,
        deadline_unix_ms: wire.deadline_unix_ms,
        task_constraints: wire.task_constraints,
        verification_argv: wire.verification_argv,
        required_permissions: wire.required_permissions,
    })
}

/// Serialize a conforming success response — the emitting half of the
/// adapter contract, symmetric to [`validate_response`]. `candidates`
/// are [`Proposal`]s already built by the adapter's policy; they are
/// serialized verbatim in the given order.
pub fn success_response(request_id: &str, candidates: &[Proposal]) -> Vec<u8> {
    canonical_json(&serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "capabilities": [CAPABILITY_GROUPED_SOURCES],
        "request_id": request_id,
        "status": "ok",
        "candidates": candidates,
    }))
}

/// Serialize a conforming failure response — used by in-process
/// fixtures and the packaged reference executable's error paths.
pub fn failure_response(request_id: Option<&str>, code: FailureCode, message: &str) -> Vec<u8> {
    let code = match code {
        FailureCode::InvalidRequest => "invalid_request",
        FailureCode::UnsupportedVersion => "unsupported_version",
        FailureCode::UnsupportedCapability => "unsupported_capability",
        FailureCode::InputLimit => "input_limit",
        FailureCode::MemberLimit => "member_limit",
        FailureCode::SourceUnavailable => "source_unavailable",
        FailureCode::AuthenticationFailed => "authentication_failed",
        FailureCode::DeadlineExceeded => "deadline_exceeded",
        FailureCode::InternalError => "internal_error",
    };
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect();
    let message = if message.is_empty() {
        "unspecified".to_string()
    } else {
        message
    };
    canonical_json(&serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id,
        "status": "error",
        "error": { "code": code, "message": message },
    }))
}
