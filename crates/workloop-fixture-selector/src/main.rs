//! `workloop-fixture-selector` — an independent test implementation of
//! the selector wire protocol, version 1 (`docs/selector-protocol.md`).
//! It exists to prove interchange: this crate shares **no code** with
//! `workloop-selector` or `workloop-intake` — its own request parser,
//! selection policy, response writer, canonical-JSON encoder, and
//! SHA-256 proposal-key digest — so a contract suite passing against
//! both executables demonstrates the protocol, not a shared
//! implementation.
//!
//! Usage:
//!
//! ```text
//! workloop-fixture-selector select --sources <file.json> [--label <name>]...
//! ```
//!
//! `--sources` is required: the fixture reads scripted source pages
//! only (the same file format the reference executable's `--sources`
//! backend accepts), plus one fixture-only extension — an optional
//! `"group"` string on a fact. Facts sharing a group name are emitted
//! as **one** candidate carrying every member, exercising the
//! protocol's grouped-sources capability end to end. The reference
//! adapter ignores the field and emits the same facts as singleton
//! candidates; grouping is fixture surface area, not protocol.
//!
//! Diagnostics (skipped facts, truncation, source outcomes) go to
//! stderr, bounded; stdout carries exactly one JSON envelope. Exit
//! status is 0 for every protocol-conforming outcome — `status:
//! "ok"` and `status: "error"` are both answers; only usage errors
//! exit nonzero.
//!
//! Scope of the claim: this binary proves wire-protocol interchange
//! for a scripted file backend. It is a test fixture — it makes no
//! claim about production selection quality, live source coverage, or
//! Worker execution.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod strict;

// --- Protocol constants -----------------------------------------------------
//
// Duplicated from docs/selector-protocol.md by necessity: an
// independent implementation cannot import the reference's table.
// The shared contract suite is the drift detector — a protocol change
// that updates only one side fails it.

const PROTOCOL_VERSION: &str = "1";
const CAPABILITY_GROUPED_SOURCES: &str = "grouped_sources_v1";
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_ID_BYTES: usize = 512;
const MAX_ADAPTER_BYTES: usize = 64;
const MAX_INTENT_BYTES: usize = 16_384;
const MAX_CONSTRAINT_BYTES: usize = 4_096;
const MAX_ARGV_ITEM_BYTES: usize = 4_096;
const MAX_ARGV_TOTAL_BYTES: usize = 16_384;
const MAX_PERMISSION_BYTES: usize = 128;
const MAX_MESSAGE_BYTES: usize = 256;
const MAX_REPOSITORIES: usize = 64;
const MAX_EXCLUSIONS: usize = 4_096;
const MAX_CANDIDATES: u32 = 32;
const MAX_GROUP_MEMBERS: usize = 32;
const MAX_TOTAL_SOURCES: usize = 1_024;
const MAX_CONSTRAINTS: usize = 64;
const MAX_ARGV_ITEMS: usize = 64;
const MAX_PERMISSIONS: usize = 32;
/// The only selector semantics version this fixture implements.
const SELECTOR_VERSION_SUPPORTED: u32 = 1;
/// Digest tag prefixing every canonical proposal payload
/// (`proposal-v1` — part of proposal identity).
const PROPOSAL_TAG: &str = "proposal-v1";

// Collection bounds — the same shape the reference executable applies
// to its file backend.
const PAGE_SIZE: usize = 100;
const MAX_PAGES_PER_REPOSITORY: usize = 10;
const MAX_ISSUES_PER_REPOSITORY: usize = 1_000;
const MAX_CALLS: usize = 256;
const MAX_TITLE_BYTES: usize = 16 * 1024;
const MAX_LABEL_BYTES: usize = 256;
const MAX_GROUP_NAME_BYTES: usize = 256;

const USAGE: &str =
    "usage: workloop-fixture-selector select --sources <file.json> [--label <name>]...";

// --- Identity helpers ---------------------------------------------------------

/// The protocol `id` shape: 1–512 bytes, no control characters.
fn is_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

/// The adapter-id shape: lowercase ASCII lead, then lowercase
/// alphanumerics and dashes, at most 64 bytes.
fn is_adapter_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ADAPTER_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Nonempty, byte-bounded, control-free — the schema's string shape.
fn is_wire_string(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn sorted_distinct(items: &[String]) -> bool {
    items.windows(2).all(|w| w[0] < w[1])
}

// --- Canonical JSON + digest --------------------------------------------------
//
// Independent re-implementation of the canonical encoding the
// proposal key digests: object keys sorted recursively, arrays in
// order, compact separators, primitives via serde_json.

fn canonical_json(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(_) | Value::String(_) => {
            out.extend_from_slice(
                serde_json::to_string(value)
                    .expect("primitive json")
                    .as_bytes(),
            );
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical_json(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key).expect("key json").as_bytes());
                out.push(b':');
                canonical_json(&map[*key], out);
            }
            out.push(b'}');
        }
    }
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    canonical_json(value, &mut out);
    out
}

/// `proposal-v1` identity: SHA-256 over `tag:canonical-bytes`,
/// lowercase hex — the same construction the reference computes.
fn proposal_key(
    selector: &str,
    selector_version: u32,
    policy_revision: u64,
    sources: &[SourceBinding],
    task: &TaskInput,
    required_permissions: &[String],
) -> String {
    let payload = json!({
        "schema": PROPOSAL_TAG,
        "selector": selector,
        "selector_version": selector_version,
        "policy_revision": policy_revision,
        "sources": sources.iter().map(|s| json!({
            "host": s.issue.host,
            "repository": s.issue.repository,
            "issue": s.issue.issue,
            "revision": s.revision,
            "content_digest": s.content_digest,
        })).collect::<Vec<_>>(),
        "task": {
            "intent": task.intent,
            "constraints": task.constraints,
            "verification_argv": task.verification_argv,
        },
        "required_permissions": required_permissions,
    });
    let mut h = Sha256::new();
    h.update(PROPOSAL_TAG.as_bytes());
    h.update(b":");
    h.update(canonical_bytes(&payload));
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// --- Domain shapes ------------------------------------------------------------

/// Scoped issue identity. Equality and ordering ignore
/// `display_number` — presentation metadata, never identity.
#[derive(Debug, Clone)]
struct IssueRef {
    host: String,
    repository: String,
    issue: String,
    display_number: Option<u64>,
}

impl PartialEq for IssueRef {
    fn eq(&self, other: &Self) -> bool {
        (
            self.host.as_str(),
            self.repository.as_str(),
            self.issue.as_str(),
        ) == (
            other.host.as_str(),
            other.repository.as_str(),
            other.issue.as_str(),
        )
    }
}
impl Eq for IssueRef {}
impl PartialOrd for IssueRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IssueRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.host.as_str(),
            self.repository.as_str(),
            self.issue.as_str(),
        )
            .cmp(&(
                other.host.as_str(),
                other.repository.as_str(),
                other.issue.as_str(),
            ))
    }
}

/// One collected source fact — the fixture's view of a source Issue.
/// `group` is the fixture-only extension: facts sharing a group name
/// are proposed as one multi-source candidate.
#[derive(Debug, Clone)]
struct Fact {
    issue: IssueRef,
    state: IssueState,
    labels: BTreeSet<String>,
    created_unix_seconds: i64,
    title: String,
    revision: String,
    content_digest: String,
    group: Option<String>,
}

/// Fact equality for contradiction detection mirrors the reference:
/// identity plus every content field — `display_number` and the
/// fixture-only `group` marker take no part.
impl PartialEq for Fact {
    fn eq(&self, other: &Self) -> bool {
        self.issue == other.issue
            && self.state == other.state
            && self.labels == other.labels
            && self.created_unix_seconds == other.created_unix_seconds
            && self.title == other.title
            && self.revision == other.revision
            && self.content_digest == other.content_digest
    }
}
impl Eq for Fact {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone)]
struct SourceBinding {
    issue: IssueRef,
    revision: String,
    content_digest: String,
}

#[derive(Debug, Clone)]
struct TaskInput {
    intent: String,
    constraints: Vec<String>,
    verification_argv: Vec<String>,
}

/// A validated request — the normalized fields selection and
/// response validation need.
#[derive(Debug)]
struct ParsedRequest {
    request_id: String,
    selector: String,
    selector_version: u32,
    policy_revision: u64,
    repositories: Vec<(String, String)>, // (host, repository), sorted
    excluded_sources: Vec<IssueRef>,
    max_candidates: u32,
    deadline_unix_ms: i64,
    task_constraints: Vec<String>,
    verification_argv: Vec<String>,
    required_permissions: Vec<String>,
}

/// The failure vocabulary — one of nine closed codes.
#[derive(Debug, Clone, Copy)]
enum FailureCode {
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
    fn as_str(self) -> &'static str {
        match self {
            FailureCode::InvalidRequest => "invalid_request",
            FailureCode::UnsupportedVersion => "unsupported_version",
            FailureCode::UnsupportedCapability => "unsupported_capability",
            FailureCode::InputLimit => "input_limit",
            FailureCode::MemberLimit => "member_limit",
            FailureCode::SourceUnavailable => "source_unavailable",
            FailureCode::AuthenticationFailed => "authentication_failed",
            FailureCode::DeadlineExceeded => "deadline_exceeded",
            FailureCode::InternalError => "internal_error",
        }
    }
}

/// Why request parsing failed — the tag selects the failure code
/// exactly as the reference's violation mapping does.
#[derive(Debug, Clone, Copy)]
enum RequestViolation {
    ProtocolVersion,
    Capabilities,
    RepositoriesBound,
    ExclusionsBound,
    MaxCandidates,
    RequestTooLarge,
    Other,
}

impl RequestViolation {
    fn failure_code(self) -> FailureCode {
        match self {
            RequestViolation::ProtocolVersion => FailureCode::UnsupportedVersion,
            RequestViolation::Capabilities => FailureCode::UnsupportedCapability,
            RequestViolation::RepositoriesBound
            | RequestViolation::ExclusionsBound
            | RequestViolation::MaxCandidates
            | RequestViolation::RequestTooLarge => FailureCode::InputLimit,
            _ => FailureCode::InvalidRequest,
        }
    }
}

// --- Request parsing ------------------------------------------------------------

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRepository {
    host: String,
    repository: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExcludedSource {
    repository: WireRepository,
    issue: String,
    display_number: Option<u64>,
}

/// Parse and validate one request payload — the same rules, in the
/// same order, the reference adapter applies.
fn parse_request(bytes: &[u8]) -> Result<ParsedRequest, RequestViolation> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(RequestViolation::RequestTooLarge);
    }
    // The wire contract's framing rules — duplicate keys, the depth
    // bound, the control-free string alphabet, and single-value
    // trailing — are stricter than serde_json's grammar; scan for
    // them before the typed parse, as the reference does.
    strict::scan(bytes).map_err(|_| RequestViolation::Other)?;
    let value: Value = serde_json::from_slice(bytes).map_err(|_| RequestViolation::Other)?;
    // Required-nullable keys must be present before the typed parse —
    // serde would silently accept an omitted `display_number`.
    if let Some(excluded) = value.get("excluded_sources").and_then(|v| v.as_array()) {
        for entry in excluded {
            let present = entry
                .as_object()
                .is_some_and(|map| map.contains_key("display_number"));
            if !present {
                return Err(RequestViolation::Other);
            }
        }
    }
    let wire: WireRequest = serde_json::from_value(value).map_err(|_| RequestViolation::Other)?;
    if wire.protocol_version != PROTOCOL_VERSION {
        return Err(RequestViolation::ProtocolVersion);
    }
    if wire.capabilities != [CAPABILITY_GROUPED_SOURCES] {
        return Err(RequestViolation::Capabilities);
    }
    if !is_id(&wire.request_id) {
        return Err(RequestViolation::Other);
    }
    if !is_adapter_id(&wire.selector) {
        return Err(RequestViolation::Other);
    }
    if wire.selector_version == 0 {
        return Err(RequestViolation::Other);
    }
    if wire.repositories.is_empty() || wire.repositories.len() > MAX_REPOSITORIES {
        return Err(RequestViolation::RepositoriesBound);
    }
    let mut repositories: Vec<(String, String)> = Vec::with_capacity(wire.repositories.len());
    for repo in &wire.repositories {
        if !is_id(&repo.host) || !is_id(&repo.repository) {
            return Err(RequestViolation::Other);
        }
        repositories.push((repo.host.clone(), repo.repository.clone()));
    }
    repositories.sort();
    repositories.dedup();
    if repositories.len() != wire.repositories.len() {
        return Err(RequestViolation::Other);
    }
    if wire.excluded_sources.len() > MAX_EXCLUSIONS {
        return Err(RequestViolation::ExclusionsBound);
    }
    let mut excluded_sources: Vec<IssueRef> = Vec::with_capacity(wire.excluded_sources.len());
    for entry in &wire.excluded_sources {
        if entry.display_number == Some(0) {
            return Err(RequestViolation::Other);
        }
        let issue = IssueRef {
            host: entry.repository.host.clone(),
            repository: entry.repository.repository.clone(),
            issue: entry.issue.clone(),
            display_number: entry.display_number,
        };
        if !is_id(&issue.issue) || !is_id(&issue.host) || !is_id(&issue.repository) {
            return Err(RequestViolation::Other);
        }
        // Exclusions outside `repositories` are invalid input —
        // mirrored from the reference so both ends refuse the same
        // payloads.
        if !repositories
            .iter()
            .any(|(host, repository)| *host == issue.host && *repository == issue.repository)
        {
            return Err(RequestViolation::Other);
        }
        excluded_sources.push(issue);
    }
    excluded_sources.sort();
    for pair in excluded_sources.windows(2) {
        if pair[0] == pair[1] {
            return Err(RequestViolation::Other);
        }
    }
    if !(1..=MAX_CANDIDATES).contains(&wire.max_candidates) {
        return Err(RequestViolation::MaxCandidates);
    }
    if wire.deadline_unix_ms < 1 {
        return Err(RequestViolation::Other);
    }
    if wire.task_constraints.len() > MAX_CONSTRAINTS
        || !wire
            .task_constraints
            .iter()
            .all(|c| is_wire_string(c, MAX_CONSTRAINT_BYTES))
        || !sorted_distinct(&wire.task_constraints)
    {
        return Err(RequestViolation::Other);
    }
    if wire.verification_argv.len() > MAX_ARGV_ITEMS
        || !wire
            .verification_argv
            .iter()
            .all(|a| is_wire_string(a, MAX_ARGV_ITEM_BYTES))
        || wire
            .verification_argv
            .iter()
            .map(|a| a.len())
            .sum::<usize>()
            > MAX_ARGV_TOTAL_BYTES
    {
        return Err(RequestViolation::Other);
    }
    if wire.required_permissions.len() > MAX_PERMISSIONS
        || !wire
            .required_permissions
            .iter()
            .all(|p| is_wire_string(p, MAX_PERMISSION_BYTES))
        || !sorted_distinct(&wire.required_permissions)
    {
        return Err(RequestViolation::Other);
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

// --- Sources file ---------------------------------------------------------------

/// One repository's scripted source: either a page list served in
/// order or a typed read failure replayed on every call.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RepoSource {
    Pages { pages: Vec<FilePage> },
    Failure { error: FileError },
}

#[derive(Debug, Deserialize)]
struct FilePage {
    issues: Vec<FileFact>,
}

#[derive(Debug, Deserialize)]
struct FileError {
    kind: String,
    // Required by the file format for parity with the reference
    // reader; the reason is diagnostic-only and never reaches the
    // wire, so it is parsed but not read.
    #[allow(dead_code)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct FileIssueRef {
    repository: WireRepository,
    issue: String,
    #[serde(default)]
    display_number: Option<u64>,
}

/// The scripted fact shape — the reference's `CollectedIssue` wire
/// form plus the fixture-only optional `group` marker. Extra fields
/// are ignored, matching the reference reader's tolerance.
#[derive(Debug, Deserialize)]
struct FileFact {
    issue: FileIssueRef,
    state: String,
    #[serde(default)]
    labels: BTreeSet<String>,
    created_unix_seconds: i64,
    title: String,
    revision: String,
    content_digest: String,
    #[serde(default)]
    group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SourceFile {
    repositories: BTreeMap<String, RepoSource>,
}

struct Sources {
    repositories: BTreeMap<String, RepoSource>,
}

impl Sources {
    fn open(path: &PathBuf) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read sources file: {e}"))?;
        let file: SourceFile = serde_json::from_str(&text)
            .map_err(|_| "sources file is not valid JSON".to_string())?;
        // Identity fields must satisfy the protocol id shape at parse
        // time — the reference's typed ids reject them during
        // deserialization, so a bad id fails the whole file there;
        // mirror that as an internal_error here.
        for source in file.repositories.values() {
            if let RepoSource::Pages { pages } = source {
                for page in pages {
                    for fact in &page.issues {
                        let issue = &fact.issue;
                        if !is_id(&issue.repository.host)
                            || !is_id(&issue.repository.repository)
                            || !is_id(&issue.issue)
                        {
                            return Err("sources file is not valid JSON".to_string());
                        }
                    }
                }
            }
        }
        Ok(Self {
            repositories: file.repositories,
        })
    }
}

fn fact_from_file(raw: &FileFact) -> Result<Fact, String> {
    let issue = &raw.issue;
    let state = match raw.state.as_str() {
        "open" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return Err(format!("fact {} carries an unknown state", issue.issue)),
    };
    let group = raw.group.clone().filter(|g| !g.is_empty());
    if let Some(name) = &group
        && !is_wire_string(name, MAX_GROUP_NAME_BYTES)
    {
        return Err(format!(
            "fact {} carries a malformed group name",
            issue.issue
        ));
    }
    Ok(Fact {
        issue: IssueRef {
            host: issue.repository.host.clone(),
            repository: issue.repository.repository.clone(),
            issue: issue.issue.clone(),
            display_number: issue.display_number,
        },
        state,
        labels: raw.labels.clone(),
        created_unix_seconds: raw.created_unix_seconds,
        title: raw.title.clone(),
        revision: raw.revision.clone(),
        content_digest: raw.content_digest.clone(),
        group,
    })
}

// --- Collection -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanStatus {
    Complete,
    Truncated,
    DeadlineExceeded,
    Denied,
    Unavailable,
    Incomplete,
    NotAttempted,
}

impl ScanStatus {
    fn describe(self) -> &'static str {
        match self {
            ScanStatus::Complete => "complete",
            ScanStatus::Truncated => "truncated",
            ScanStatus::DeadlineExceeded => "deadline_exceeded",
            ScanStatus::Denied => "denied",
            ScanStatus::Unavailable => "unavailable",
            ScanStatus::Incomplete => "incomplete",
            ScanStatus::NotAttempted => "not_attempted",
        }
    }

    fn failure_code(self) -> FailureCode {
        match self {
            ScanStatus::Denied => FailureCode::AuthenticationFailed,
            ScanStatus::DeadlineExceeded => FailureCode::DeadlineExceeded,
            _ => FailureCode::SourceUnavailable,
        }
    }
}

struct RepoReport {
    host: String,
    repository: String,
    status: ScanStatus,
    calls: usize,
    issues: usize,
}

/// Per-fact contract: scoped to the repository being read, every
/// required field present and bounded.
fn validate_fact(repository: &(String, String), fact: &Fact) -> Result<(), String> {
    if fact.issue.host != repository.0 || fact.issue.repository != repository.1 {
        return Err(format!(
            "fact {} is scoped outside the requested repository",
            fact.issue.issue
        ));
    }
    // The issue id is an opaque identity token under the same bound
    // as revisions and digests; the message names only the
    // repository so a pathological id cannot inflate the diagnostic.
    if !bounded_token(&fact.issue.issue) {
        return Err(format!(
            "a fact in {}/{} carries an out-of-bounds issue id",
            repository.0, repository.1
        ));
    }
    // The title becomes the candidate intent verbatim: bounded,
    // control-free, and carrying at least one non-whitespace
    // character — the protocol's intent shape.
    if fact.title.is_empty()
        || fact.title.len() > MAX_TITLE_BYTES
        || fact.title.chars().any(char::is_control)
        || !fact.title.chars().any(|c| !c.is_whitespace())
    {
        return Err(format!(
            "fact {} title is empty, whitespace-only, control-bearing, \
             or exceeds {MAX_TITLE_BYTES} bytes",
            fact.issue.issue
        ));
    }
    if !bounded_token(&fact.revision) || !bounded_token(&fact.content_digest) {
        return Err(format!(
            "fact {} is missing a bounded revision or content digest",
            fact.issue.issue
        ));
    }
    if fact
        .labels
        .iter()
        .any(|l| l.is_empty() || l.len() > MAX_LABEL_BYTES)
    {
        return Err(format!(
            "fact {} carries a malformed label",
            fact.issue.issue
        ));
    }
    if fact.created_unix_seconds < 0 {
        return Err(format!(
            "fact {} carries a negative creation time",
            fact.issue.issue
        ));
    }
    Ok(())
}

fn bounded_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

/// Scan one repository's scripted pages under the collection bounds.
/// Returns the report plus — meaningful only on `Complete` — the
/// admitted facts.
fn scan_repository(
    sources: &Sources,
    repository: &(String, String),
    deadline_unix_seconds: i64,
    total_calls: &mut usize,
) -> (RepoReport, Vec<Fact>) {
    let key = format!("{}/{}", repository.0, repository.1);
    let mut report = RepoReport {
        host: repository.0.clone(),
        repository: repository.1.clone(),
        status: ScanStatus::Complete,
        calls: 0,
        issues: 0,
    };
    let mut seen: BTreeMap<(String, String, String), Fact> = BTreeMap::new();
    let mut page_index = 0usize;
    let mut pages_served = 0usize;

    let Some(source) = sources.repositories.get(&key) else {
        report.status = ScanStatus::Unavailable;
        return (report, Vec::new());
    };

    loop {
        if *total_calls >= MAX_CALLS {
            report.status = if report.calls == 0 {
                ScanStatus::NotAttempted
            } else {
                ScanStatus::Truncated
            };
            break;
        }
        if now_unix_seconds() >= deadline_unix_seconds {
            report.status = ScanStatus::DeadlineExceeded;
            break;
        }
        if pages_served >= MAX_PAGES_PER_REPOSITORY {
            report.status = ScanStatus::Truncated;
            break;
        }
        *total_calls += 1;
        report.calls += 1;
        pages_served += 1;

        match source {
            RepoSource::Failure { error } => {
                report.status = match error.kind.as_str() {
                    "denied" => ScanStatus::Denied,
                    "malformed" => ScanStatus::Incomplete,
                    _ => ScanStatus::Unavailable,
                };
                break;
            }
            RepoSource::Pages { pages } => {
                let Some(file_page) = pages.get(page_index) else {
                    report.status = ScanStatus::Incomplete;
                    break;
                };
                page_index += 1;
                // The deadline bounds scan completion, not just
                // call initiation: a page landing at or past it
                // admits nothing — even a final page — so the
                // repository reports DeadlineExceeded rather than
                // Complete.
                if now_unix_seconds() >= deadline_unix_seconds {
                    report.status = ScanStatus::DeadlineExceeded;
                    break;
                }
                if file_page.issues.len() > PAGE_SIZE {
                    report.status = ScanStatus::Incomplete;
                    break;
                }
                let mut page_facts = Vec::with_capacity(file_page.issues.len());
                let mut page_error = false;
                for raw in &file_page.issues {
                    match fact_from_file(raw).and_then(|fact| {
                        validate_fact(repository, &fact)?;
                        Ok(fact)
                    }) {
                        Ok(fact) => page_facts.push(fact),
                        Err(_) => {
                            page_error = true;
                            break;
                        }
                    }
                }
                if page_error {
                    report.status = ScanStatus::Incomplete;
                    break;
                }
                // Dedup by scoped identity; identical repeats
                // collapse, any other difference is a contradiction.
                let mut contradiction = false;
                for fact in page_facts {
                    let ident = (
                        fact.issue.host.clone(),
                        fact.issue.repository.clone(),
                        fact.issue.issue.clone(),
                    );
                    match seen.get(&ident) {
                        None => {
                            seen.insert(ident, fact);
                        }
                        Some(existing) if *existing == fact => {}
                        Some(_) => {
                            contradiction = true;
                            break;
                        }
                    }
                }
                if contradiction {
                    report.status = ScanStatus::Incomplete;
                    break;
                }
                if seen.len() > MAX_ISSUES_PER_REPOSITORY {
                    report.status = ScanStatus::Truncated;
                    break;
                }
                if page_index >= pages.len() {
                    break;
                }
            }
        }
    }

    if report.status == ScanStatus::Complete {
        report.issues = seen.len();
        (report, seen.into_values().collect())
    } else {
        report.issues = 0;
        (report, Vec::new())
    }
}

// --- Selection ------------------------------------------------------------------

/// Whether policy excludes this fact's issue: identity match, or —
/// when both sides carry a display number in the same repository —
/// display-number equality. Display numbers never enter identity.
fn excluded(excluded: &[IssueRef], issue: &IssueRef) -> bool {
    excluded.iter().any(|entry| {
        let same_repository = entry.host == issue.host && entry.repository == issue.repository;
        (same_repository && entry.issue == issue.issue)
            || (same_repository
                && matches!(
                    (entry.display_number, issue.display_number),
                    (Some(a), Some(b)) if a == b
                ))
    })
}

/// Whether `title` can serve verbatim as a candidate `task.intent`.
fn conforming_intent(title: &str) -> bool {
    is_wire_string(title, MAX_INTENT_BYTES) && title.chars().any(|c| !c.is_whitespace())
}

struct Candidate {
    sources: Vec<SourceBinding>,
    task: TaskInput,
    /// Earliest member rank — the candidate's position in run order.
    rank: usize,
    /// Eligible facts the candidate represents (for truncation
    /// accounting: a dropped group counts its members).
    member_count: usize,
}

struct Selection {
    candidates: Vec<Candidate>,
    /// (reason, count) pairs for stderr diagnostics.
    skipped: BTreeMap<&'static str, usize>,
    truncated_facts: usize,
}

/// Apply the selection policy: eligibility in the reference's check
/// order, deterministic ranking, group assembly, and the
/// `max_candidates` bound over candidates.
fn select(
    facts: &[Fact],
    request: &ParsedRequest,
    required_labels: &BTreeSet<String>,
) -> Selection {
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut eligible: Vec<&Fact> = Vec::new();

    for fact in facts {
        let in_scope = request.repositories.iter().any(|(host, repository)| {
            *host == fact.issue.host && *repository == fact.issue.repository
        });
        let reason = if !in_scope {
            Some("not_allowlisted")
        } else if excluded(&request.excluded_sources, &fact.issue) {
            Some("excluded")
        } else if fact.state != IssueState::Open {
            Some("not_open")
        } else if !required_labels.is_subset(&fact.labels) {
            Some("missing_labels")
        } else if fact.issue.display_number == Some(0) {
            Some("invalid_issue_ref")
        } else if !conforming_intent(&fact.title) {
            Some("invalid_intent")
        } else {
            None
        };
        match reason {
            Some(reason) => *skipped.entry(reason).or_default() += 1,
            None => eligible.push(fact),
        }
    }

    // Deterministic rank: creation time, then Issue number
    // (unnumbered last), then scoped identity.
    eligible.sort_by(|a, b| {
        a.created_unix_seconds
            .cmp(&b.created_unix_seconds)
            .then_with(|| {
                a.issue
                    .display_number
                    .unwrap_or(u64::MAX)
                    .cmp(&b.issue.display_number.unwrap_or(u64::MAX))
            })
            .then_with(|| a.issue.cmp(&b.issue))
    });

    // Group eligible facts by their fixture marker; singleton facts
    // form one-member candidates implicitly.
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (rank, fact) in eligible.iter().enumerate() {
        if let Some(name) = &fact.group {
            groups.entry(name.clone()).or_default().push(rank);
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut grouped_ranks: BTreeSet<usize> = BTreeSet::new();
    for ranks in groups.values() {
        // Members emit sorted by (host, repository, issue) — the
        // protocol's required member order — regardless of rank.
        let mut members: Vec<&Fact> = ranks.iter().map(|r| eligible[*r]).collect();
        members.sort_by(|a, b| a.issue.cmp(&b.issue));
        let rank = *ranks.iter().min().unwrap();
        for r in ranks {
            grouped_ranks.insert(*r);
        }
        let intent = members
            .iter()
            .map(|f| f.title.as_str())
            .collect::<Vec<_>>()
            .join(" + ");
        candidates.push(Candidate {
            sources: members
                .iter()
                .map(|f| SourceBinding {
                    issue: f.issue.clone(),
                    revision: f.revision.clone(),
                    content_digest: f.content_digest.clone(),
                })
                .collect(),
            task: TaskInput {
                intent,
                constraints: request.task_constraints.clone(),
                verification_argv: request.verification_argv.clone(),
            },
            rank,
            member_count: members.len(),
        });
    }
    for (rank, fact) in eligible.iter().enumerate() {
        if grouped_ranks.contains(&rank) {
            continue;
        }
        candidates.push(Candidate {
            sources: vec![SourceBinding {
                issue: fact.issue.clone(),
                revision: fact.revision.clone(),
                content_digest: fact.content_digest.clone(),
            }],
            task: TaskInput {
                intent: fact.title.clone(),
                constraints: request.task_constraints.clone(),
                verification_argv: request.verification_argv.clone(),
            },
            rank,
            member_count: 1,
        });
    }
    candidates.sort_by_key(|c| c.rank);

    let limit = request.max_candidates as usize;
    let kept: Vec<Candidate> = candidates.into_iter().take(limit).collect();
    // Truncation accounting stays in *facts*: a dropped candidate
    // counts every eligible fact it would have carried.
    let emitted_facts: usize = kept.iter().map(|c| c.member_count).sum();
    let truncated_facts = eligible.len() - emitted_facts;
    Selection {
        candidates: kept,
        skipped,
        truncated_facts,
    }
}

// --- Response emission -----------------------------------------------------------

fn emit(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn fail(request_id: Option<&str>, code: FailureCode, message: &str) -> ! {
    emit(&failure_response(request_id, code, message));
    std::process::exit(0);
}

/// Serialize a conforming failure response — the same envelope shape
/// the reference emits, including the bounded, control-stripped
/// message.
fn failure_response(request_id: Option<&str>, code: FailureCode, message: &str) -> Vec<u8> {
    let filtered: String = message.chars().filter(|c| !c.is_control()).collect();
    let mut bounded = String::new();
    for ch in filtered.chars() {
        if bounded.len() + ch.len_utf8() > MAX_MESSAGE_BYTES {
            break;
        }
        bounded.push(ch);
    }
    let message = if bounded.is_empty() {
        "unspecified".to_string()
    } else {
        bounded
    };
    canonical_bytes(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id,
        "status": "error",
        "error": { "code": code.as_str(), "message": message },
    }))
}

fn source_json(source: &SourceBinding) -> Value {
    json!({
        "issue": {
            "repository": {
                "host": source.issue.host,
                "repository": source.issue.repository,
            },
            "issue": source.issue.issue,
            "display_number": source.issue.display_number,
        },
        "revision": source.revision,
        "content_digest": source.content_digest,
    })
}

fn success_response(
    request_id: &str,
    candidates: &[Candidate],
    request: &ParsedRequest,
    fetched: i64,
) -> Vec<u8> {
    let wire_candidates: Vec<Value> = candidates
        .iter()
        .map(|candidate| {
            let sources: Vec<Value> = candidate.sources.iter().map(source_json).collect();
            let key = proposal_key(
                &request.selector,
                request.selector_version,
                request.policy_revision,
                &candidate.sources,
                &candidate.task,
                &request.required_permissions,
            );
            json!({
                "key": key,
                "selector": request.selector,
                "selector_version": request.selector_version,
                "policy_revision": request.policy_revision,
                "sources": sources,
                "task": {
                    "intent": candidate.task.intent,
                    "constraints": candidate.task.constraints,
                    "verification_argv": candidate.task.verification_argv,
                },
                "required_permissions": request.required_permissions,
                "provenance": {
                    "snapshot_cursor": Value::Null,
                    "fetched_unix_seconds": fetched,
                },
            })
        })
        .collect();
    canonical_bytes(&json!({
        "protocol_version": PROTOCOL_VERSION,
        "capabilities": [CAPABILITY_GROUPED_SOURCES],
        "request_id": request_id,
        "status": "ok",
        "candidates": wire_candidates,
    }))
}

// --- Diagnostics -----------------------------------------------------------------

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn report_diagnostics(selection: &Selection, reports: &[RepoReport]) {
    if !selection.skipped.is_empty() {
        let detail = selection
            .skipped
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("workloop-fixture-selector: skipped facts: {detail}");
    }
    if selection.truncated_facts > 0 {
        eprintln!(
            "workloop-fixture-selector: {} eligible facts truncated by max_candidates",
            selection.truncated_facts
        );
    }
    let calls: usize = reports.iter().map(|r| r.calls).sum();
    let issues: usize = reports.iter().map(|r| r.issues).sum();
    eprintln!(
        "workloop-fixture-selector: collected {issues} issues across {} repositories ({calls} calls)",
        reports.len()
    );
}

// --- Entry point -----------------------------------------------------------------

#[derive(Debug)]
struct Args {
    sources: PathBuf,
    labels: BTreeSet<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(USAGE.to_string());
    };
    if command != "select" {
        return Err(USAGE.to_string());
    }
    let mut sources = None;
    let mut labels = BTreeSet::new();
    let mut it = args.peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sources" => {
                let Some(path) = it.next() else {
                    return Err("--sources requires a path".into());
                };
                sources = Some(PathBuf::from(path));
            }
            "--label" => {
                let Some(label) = it.next() else {
                    return Err("--label requires a name".into());
                };
                if label.is_empty()
                    || label.len() > MAX_LABEL_BYTES
                    || label.chars().any(char::is_control)
                {
                    return Err("--label is empty, oversized, or carries control characters".into());
                }
                labels.insert(label);
            }
            other => return Err(format!("unknown argument '{other}'\n{USAGE}")),
        }
    }
    let Some(sources) = sources else {
        return Err("--sources is required".into());
    };
    Ok(Args { sources, labels })
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("workloop-fixture-selector: {message}");
            std::process::exit(2);
        }
    };

    // One bounded read at the request contract's byte ceiling.
    let mut request_bytes = Vec::new();
    if std::io::stdin()
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut request_bytes)
        .is_err()
        || request_bytes.len() > MAX_REQUEST_BYTES
    {
        fail(
            None,
            FailureCode::InputLimit,
            "request exceeds the input bound",
        );
    }
    let request = match parse_request(&request_bytes) {
        Ok(request) => request,
        Err(violation) => {
            // Best-effort request_id echo: extract it only when it
            // satisfies the id rule — echoing a control-bearing id
            // would make the caller reject this failure as malformed.
            let request_id = serde_json::from_slice::<Value>(&request_bytes)
                .ok()
                .and_then(|v| {
                    v.get("request_id")
                        .and_then(|r| r.as_str())
                        .map(str::to_owned)
                })
                .filter(|id| is_id(id));
            fail(
                request_id.as_deref(),
                violation.failure_code(),
                "request failed protocol validation",
            );
        }
    };
    let request_id = request.request_id.clone();

    // The fixture implements selection semantics version 1 only.
    if request.selector_version != SELECTOR_VERSION_SUPPORTED {
        fail(
            Some(&request_id),
            FailureCode::InvalidRequest,
            "unsupported selector_version",
        );
    }

    let sources = match Sources::open(&args.sources) {
        Ok(sources) => sources,
        Err(message) => fail(Some(&request_id), FailureCode::InternalError, &message),
    };

    let deadline_seconds = request.deadline_unix_ms / 1_000;
    let fetched = now_unix_seconds();

    // Scan every requested repository; a required read that does not
    // complete fails the whole invocation — never an empty "successful"
    // scan over partial coverage. Reports stay in sorted order so the
    // first failure decides, matching the reference.
    let mut reports = Vec::new();
    let mut facts: Vec<Fact> = Vec::new();
    let mut total_calls = 0usize;
    for repository in &request.repositories {
        let (report, admitted) =
            scan_repository(&sources, repository, deadline_seconds, &mut total_calls);
        let complete = report.status == ScanStatus::Complete;
        if complete {
            facts.extend(admitted);
        }
        reports.push(report);
    }
    facts.sort_by(|a, b| a.issue.cmp(&b.issue));

    for report in &reports {
        if report.status != ScanStatus::Complete {
            eprintln!(
                "workloop-fixture-selector: source {}/{} did not complete ({})",
                report.host,
                report.repository,
                report.status.describe()
            );
            fail(
                Some(&request_id),
                report.status.failure_code(),
                "a required source read did not complete",
            );
        }
    }

    // Group/member bounds are checked before emission: a group that
    // cannot fit the member or total-source bounds fails the
    // invocation rather than producing a response the caller must
    // reject wholesale.
    let selection = select(&facts, &request, &args.labels);
    let total_sources: usize = selection.candidates.iter().map(|c| c.sources.len()).sum();
    if selection
        .candidates
        .iter()
        .any(|c| c.sources.len() > MAX_GROUP_MEMBERS)
        || total_sources > MAX_TOTAL_SOURCES
    {
        fail(
            Some(&request_id),
            FailureCode::MemberLimit,
            "selected group cannot fit member or response bounds",
        );
    }
    // A group intent that overflows the wire bound cannot be
    // emitted — member_limit, the protocol's "selected group cannot
    // fit response bounds" code, not a request fault.
    if selection
        .candidates
        .iter()
        .any(|c| !conforming_intent(&c.task.intent))
    {
        fail(
            Some(&request_id),
            FailureCode::MemberLimit,
            "grouped intent exceeds the intent bound",
        );
    }

    report_diagnostics(&selection, &reports);
    emit(&success_response(
        &request_id,
        &selection.candidates,
        &request,
        fetched,
    ));
}
