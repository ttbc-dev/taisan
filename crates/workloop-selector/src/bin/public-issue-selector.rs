//! `public-issue-selector` — the packaged stateless reference Selector
//! executable (#214). One JSON request object on stdin, one JSON
//! response object on stdout, protocol version 1
//! (`docs/selector-protocol.md`). No cursor, no ledger, no daemon:
//! every invocation re-reads the requested repository scope and
//! answers from explicit configuration plus request data.
//!
//! Usage:
//!
//! ```text
//! public-issue-selector select [--sources <file.json>] [--label <name>]...
//! ```
//!
//! Source backends:
//!
//! - default — `gh api` against github.com using the ambient `gh`
//!   authentication (the launcher passes `HOME` through, never token
//!   environment variables). Repository ids must be `owner/name`,
//!   and every requested repository's host instance must be declared
//!   with `--github-endpoint <id>` — the installation profile's
//!   `[hosts.*].endpoint` value for github.com. A repository scoped
//!   to any other host instance fails closed rather than being
//!   silently served from github.com.
//! - `--sources <file>` — a JSON file describing scripted source
//!   pages; the deterministic backend contract tests and offline
//!   installations use. Format:
//!
//! ```json
//! {"repositories": {"<host>/<repository-id>":
//!   {"pages": [{"issues": [<CollectedIssue>...]}]}
//!   | {"error": {"kind": "denied|unavailable|malformed", "reason": "..."}}
//! }}
//! ```
//!
//! `--label` is repeatable; an Issue must carry every configured label
//! to be eligible. Diagnostics (skipped facts, truncation, source
//! outcomes) go to stderr, bounded; stdout carries exactly one JSON
//! envelope. Exit status is 0 for every protocol-conforming outcome —
//! `status: "ok"` and `status: "error"` are both answers; only usage
//! errors exit nonzero.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use workloop_core::identity::{
    AdapterId, IssueId, IssueRef, RepositoryRef, canonical_host_instance,
};
use workloop_selector::collect::{
    self, CollectedIssue, CollectionBounds, IssuePage, RepositoryReport, ScanStatus,
    SourceReadError, SourceReader,
};
use workloop_selector::protocol::{self, FailureCode};
use workloop_selector::reference;
use workloop_selector::{IssueState, SELECTOR_VERSION};

/// Collection bounds for one invocation — generous enough for real
/// repositories, small enough that a hostile or broken source cannot
/// pin the process.
const PAGE_SIZE: usize = 100;
const MAX_PAGES_PER_REPOSITORY: usize = 10;
const MAX_ISSUES_PER_REPOSITORY: usize = 1_000;
const MAX_CALLS: usize = 256;
/// gh's `per_page` ceiling.
const GH_MAX_PER_PAGE: usize = 100;

const USAGE: &str = "usage: public-issue-selector select [--sources <file.json>] [--label <name>]... [--github-endpoint <id>]...";

#[derive(Debug)]
struct Args {
    sources: Option<PathBuf>,
    labels: BTreeSet<String>,
    /// Host-instance ids (profile `[hosts.*].endpoint` values) this
    /// invocation's `gh` backend may serve. Empty means the `gh`
    /// backend serves nothing — it cannot guess which scoped host
    /// instance is github.com.
    github_endpoints: BTreeSet<String>,
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
    let mut github_endpoints = BTreeSet::new();
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
                if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
                    return Err("--label is empty, oversized, or carries control characters".into());
                }
                labels.insert(label);
            }
            "--github-endpoint" => {
                let Some(endpoint) = it.next() else {
                    return Err("--github-endpoint requires an id".into());
                };
                if endpoint.is_empty()
                    || endpoint.len() > protocol::MAX_ID_BYTES
                    || endpoint.chars().any(char::is_control)
                {
                    return Err(
                        "--github-endpoint is empty, oversized, or carries control characters"
                            .into(),
                    );
                }
                github_endpoints.insert(endpoint);
            }
            other => return Err(format!("unknown argument '{other}'\n{USAGE}")),
        }
    }
    Ok(Args {
        sources,
        labels,
        github_endpoints,
    })
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn emit(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn fail(request_id: Option<&str>, code: FailureCode, message: &str) -> ! {
    emit(&protocol::failure_response(request_id, code, message));
    std::process::exit(0);
}

/// Map a request-side violation onto the protocol's failure codes:
/// version/capability mismatches get their own codes, bound
/// violations (oversized bytes, lists, or counts) report
/// `input_limit`, and every other contract violation is an invalid
/// request shape, scope, or identity.
fn request_violation_code(violation: &protocol::Violation) -> FailureCode {
    match violation {
        protocol::Violation::Contract("protocol_version") => FailureCode::UnsupportedVersion,
        protocol::Violation::Contract("capabilities") => FailureCode::UnsupportedCapability,
        protocol::Violation::Contract(
            "repositories_bound" | "exclusions_bound" | "max_candidates" | "request_too_large",
        ) => FailureCode::InputLimit,
        _ => FailureCode::InvalidRequest,
    }
}

/// Map a non-`Complete` repository scan onto the failure vocabulary.
/// The first failing repository decides; its bounded reason stays on
/// stderr diagnostics, never in the wire message.
fn scan_failure_code(status: &ScanStatus) -> FailureCode {
    match status {
        ScanStatus::Denied { .. } => FailureCode::AuthenticationFailed,
        ScanStatus::DeadlineExceeded => FailureCode::DeadlineExceeded,
        _ => FailureCode::SourceUnavailable,
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("public-issue-selector: {message}");
            std::process::exit(2);
        }
    };

    // One bounded read: the request contract's own byte ceiling plus
    // slack for the single JSON value's framing.
    let mut request_bytes = Vec::new();
    if std::io::stdin()
        .take(protocol::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut request_bytes)
        .is_err()
        || request_bytes.len() > protocol::MAX_REQUEST_BYTES
    {
        fail(
            None,
            FailureCode::InputLimit,
            "request exceeds the input bound",
        );
    }
    let request = match protocol::parse_request(&request_bytes) {
        Ok(request) => request,
        Err(violation) => {
            // The request id is untrusted until parsed; extract it
            // best-effort for correlation under the failure envelope's
            // own `is_id` rule — echoing a control-bearing id would
            // make the caller reject this failure as malformed.
            let request_id = serde_json::from_slice::<serde_json::Value>(&request_bytes)
                .ok()
                .and_then(|v| {
                    v.get("request_id")
                        .and_then(|r| r.as_str())
                        .map(str::to_owned)
                })
                .filter(|id| protocol::is_id(id));
            fail(
                request_id.as_deref(),
                request_violation_code(&violation),
                "request failed protocol validation",
            );
        }
    };
    let request_id = request.request_id.clone();

    // The reference policy implements selection semantics version 1
    // only; any other declared version is a request this executable
    // cannot honour.
    if request.selector_version != SELECTOR_VERSION {
        fail(
            Some(&request_id),
            FailureCode::InvalidRequest,
            "unsupported selector_version",
        );
    }
    let selector = match AdapterId::new(request.selector.clone()) {
        Ok(selector) => selector,
        Err(_) => fail(
            Some(&request_id),
            FailureCode::InvalidRequest,
            "invalid selector id",
        ),
    };

    let mut reader: Box<dyn SourceReader> = match &args.sources {
        Some(path) => match FileSourceReader::open(path) {
            Ok(reader) => Box::new(reader),
            Err(message) => fail(Some(&request_id), FailureCode::InternalError, &message),
        },
        None => Box::new(GhSourceReader::new(&args.github_endpoints)),
    };

    let repositories: BTreeSet<RepositoryRef> = request.repositories.iter().cloned().collect();
    let deadline_seconds = request.deadline_unix_ms / 1_000;
    let bounds = CollectionBounds {
        page_size: PAGE_SIZE,
        max_pages_per_repository: MAX_PAGES_PER_REPOSITORY,
        max_issues_per_repository: MAX_ISSUES_PER_REPOSITORY,
        max_calls: MAX_CALLS,
        deadline_unix_seconds: deadline_seconds,
    };
    let fetched = now_unix_seconds();
    let collection = collect::collect(&mut *reader, &repositories, &bounds, &now_unix_seconds);

    // A required source read that did not complete fails the whole
    // invocation — never an empty "successful" scan over partial
    // coverage.
    for report in &collection.report.repositories {
        if report.status != ScanStatus::Complete {
            eprintln!(
                "public-issue-selector: source {} did not complete ({})",
                describe_status(&report.status),
                repository_key(&report.repository)
            );
            fail(
                Some(&request_id),
                scan_failure_code(&report.status),
                "a required source read did not complete",
            );
        }
    }

    let selection = reference::select_reference(
        &collection,
        &request,
        &selector,
        &args.labels,
        Some(fetched),
    );
    report_diagnostics(&selection, &collection.report.repositories);
    emit(&protocol::success_response(
        &request_id,
        &selection.proposals,
    ));
}

fn repository_key(repository: &RepositoryRef) -> String {
    format!("{}/{}", repository.host, repository.repository)
}

fn describe_status(status: &ScanStatus) -> &'static str {
    match status {
        ScanStatus::Complete => "complete",
        ScanStatus::Truncated => "truncated",
        ScanStatus::DeadlineExceeded => "deadline_exceeded",
        ScanStatus::Denied { .. } => "denied",
        ScanStatus::Unavailable { .. } => "unavailable",
        ScanStatus::Incomplete { .. } => "incomplete",
        ScanStatus::NotAttempted => "not_attempted",
    }
}

/// Bounded stderr diagnostics: counts by skip reason and the
/// truncation count — never fact payloads.
fn report_diagnostics(
    selection: &reference::ReferenceSelection,
    repositories: &[RepositoryReport],
) {
    let mut by_reason: BTreeMap<&'static str, usize> = BTreeMap::new();
    for skipped in &selection.skipped {
        let reason = match skipped.reason {
            workloop_selector::SkipReason::NotAllowlisted => "not_allowlisted",
            workloop_selector::SkipReason::Excluded => "excluded",
            workloop_selector::SkipReason::NotOpen => "not_open",
            workloop_selector::SkipReason::ConflictingFacts => "conflicting_facts",
            workloop_selector::SkipReason::TruncatedByBound => "truncated_by_bound",
            workloop_selector::SkipReason::MissingLabels => "missing_labels",
            workloop_selector::SkipReason::InvalidIntent => "invalid_intent",
            workloop_selector::SkipReason::InvalidIssueRef => "invalid_issue_ref",
        };
        *by_reason.entry(reason).or_default() += 1;
    }
    if !by_reason.is_empty() {
        let detail = by_reason
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("public-issue-selector: skipped facts: {detail}");
    }
    if selection.truncated > 0 {
        eprintln!(
            "public-issue-selector: {} eligible facts truncated by max_candidates",
            selection.truncated
        );
    }
    let calls: usize = repositories.iter().map(|r| r.calls).sum();
    eprintln!(
        "public-issue-selector: collected {} issues across {} repositories ({} calls)",
        collection_issue_count(repositories),
        repositories.len(),
        calls
    );
}

fn collection_issue_count(repositories: &[RepositoryReport]) -> usize {
    repositories.iter().map(|r| r.issues).sum()
}

// --- File backend ---------------------------------------------------------

/// One repository's scripted source: either a page list served in
/// order or a typed read failure replayed on every call.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RepoSource {
    Pages { pages: Vec<FilePage> },
    Failure { error: FileError },
}

#[derive(Debug, serde::Deserialize)]
struct FilePage {
    issues: Vec<CollectedIssue>,
}

#[derive(Debug, serde::Deserialize)]
struct FileError {
    kind: String,
    reason: String,
}

#[derive(Debug, serde::Deserialize)]
struct SourceFile {
    repositories: BTreeMap<String, RepoSource>,
}

/// A [`SourceReader`] over a JSON file of scripted pages — the
/// deterministic backend for tests and offline use. Repository keys
/// are `<host>/<repository-id>`; the reader serves each repository's
/// pages in order and replays its declared error on every call.
struct FileSourceReader {
    sources: BTreeMap<String, RepoSource>,
    positions: BTreeMap<String, usize>,
}

impl FileSourceReader {
    fn open(path: &PathBuf) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read sources file: {e}"))?;
        let file: SourceFile = serde_json::from_str(&text)
            .map_err(|e| format!("sources file is not valid JSON: {e}"))?;
        Ok(Self {
            sources: file.repositories,
            positions: BTreeMap::new(),
        })
    }
}

impl SourceReader for FileSourceReader {
    fn list_issues(
        &mut self,
        repository: &RepositoryRef,
        cursor: Option<&str>,
        _page_size: usize,
    ) -> Result<IssuePage, SourceReadError> {
        let key = repository_key(repository);
        let Some(source) = self.sources.get(&key) else {
            return Err(SourceReadError::Unavailable {
                reason: format!("no scripted source for {key}"),
            });
        };
        match source {
            RepoSource::Failure { error } => Err(match error.kind.as_str() {
                "denied" => SourceReadError::Denied {
                    reason: error.reason.clone(),
                },
                "malformed" => SourceReadError::Malformed {
                    reason: error.reason.clone(),
                },
                _ => SourceReadError::Unavailable {
                    reason: error.reason.clone(),
                },
            }),
            RepoSource::Pages { pages } => {
                let index = match cursor {
                    None => *self.positions.get(&key).unwrap_or(&0),
                    Some(cursor) => {
                        cursor
                            .parse::<usize>()
                            .map_err(|_| SourceReadError::Malformed {
                                reason: "unparseable page cursor".into(),
                            })?
                    }
                };
                let Some(page) = pages.get(index) else {
                    return Err(SourceReadError::Malformed {
                        reason: format!("page cursor {index} out of range"),
                    });
                };
                self.positions.insert(key, index + 1);
                Ok(IssuePage {
                    issues: page.issues.clone(),
                    next_cursor: (index + 1 < pages.len()).then(|| (index + 1).to_string()),
                })
            }
        }
    }
}

// --- gh backend ------------------------------------------------------------

/// A [`SourceReader`] over `gh api` for github.com-shaped repository
/// ids (`owner/name`). Authentication is the ambient `gh` login —
/// the launcher passes `HOME` through and never token environment
/// variables, so this backend holds no credential of its own.
///
/// `gh api` always resolves against github.com, so the backend can
/// only serve repositories whose host instance the operator declared
/// as a github.com endpoint — anything else fails closed instead of
/// silently scanning github.com under a foreign scope. The comparison
/// is over canonical host instances: `https://api.github.com` and
/// `github.com` name the same instance.
struct GhSourceReader {
    endpoints: BTreeSet<String>,
}

impl GhSourceReader {
    fn new(endpoints: &BTreeSet<String>) -> Self {
        Self {
            endpoints: endpoints.clone(),
        }
    }
}

/// `owner/name` shape check: exactly one `/`, both halves nonempty
/// and free of whitespace or control characters.
fn gh_repository_path(repository: &str) -> Option<String> {
    let mut parts = repository.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    if [owner, name]
        .iter()
        .any(|p| p.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        return None;
    }
    Some(format!("repos/{owner}/{name}/issues"))
}

impl SourceReader for GhSourceReader {
    fn list_issues(
        &mut self,
        repository: &RepositoryRef,
        cursor: Option<&str>,
        page_size: usize,
    ) -> Result<IssuePage, SourceReadError> {
        // Scope is an identity comparison, not a transport string
        // match: `canonical_host_instance` reduces a profile endpoint
        // (`https://api.github.com`) and the installation-side identity
        // (`github.com`) to the same host instance.
        let requested = canonical_host_instance(repository.host.as_str());
        let declared = self
            .endpoints
            .iter()
            .any(|e| canonical_host_instance(e) == requested);
        if !declared {
            return Err(SourceReadError::Unavailable {
                reason: "repository host is not a declared github.com endpoint".into(),
            });
        }
        let Some(path) = gh_repository_path(repository.repository.as_str()) else {
            return Err(SourceReadError::Unavailable {
                reason: "repository id is not owner/name".into(),
            });
        };
        let page = match cursor {
            None => 1,
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| SourceReadError::Malformed {
                    reason: "unparseable page cursor".into(),
                })?,
        };
        let per_page = page_size.min(GH_MAX_PER_PAGE);
        let endpoint = format!("{path}?state=open&per_page={per_page}&page={page}");
        let output = Command::new("gh")
            .args(["api", &endpoint])
            .output()
            .map_err(|e| SourceReadError::Unavailable {
                reason: format!("gh unavailable: {e}"),
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lower = stderr.to_lowercase();
            let denied = lower.contains("401")
                || lower.contains("403")
                || lower.contains("bad credentials")
                || lower.contains("authentication");
            let reason = stderr
                .lines()
                .next()
                .unwrap_or("gh api failed")
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>();
            return Err(if denied {
                SourceReadError::Denied { reason }
            } else {
                SourceReadError::Unavailable { reason }
            });
        }
        let raw: Vec<serde_json::Value> =
            serde_json::from_slice(&output.stdout).map_err(|e| SourceReadError::Malformed {
                reason: format!("gh api returned invalid JSON: {e}"),
            })?;
        // The issues endpoint also returns pull requests; only real
        // issues feed collection.
        let issues_raw: Vec<&serde_json::Value> = raw
            .iter()
            .filter(|v| !v.get("pull_request").is_some_and(|p| !p.is_null()))
            .collect();
        let mut issues = Vec::with_capacity(issues_raw.len());
        for value in issues_raw {
            issues.push(gh_issue(repository, value)?);
        }
        let has_more = raw.len() == per_page;
        Ok(IssuePage {
            issues,
            next_cursor: has_more.then(|| (page + 1).to_string()),
        })
    }
}

/// Map one `gh api` issue object onto a [`CollectedIssue`] scoped to
/// the requested repository. Missing or malformed required fields
/// fail the page as `Malformed` — a partial fact is never admitted.
fn gh_issue(
    repository: &RepositoryRef,
    value: &serde_json::Value,
) -> Result<CollectedIssue, SourceReadError> {
    let malformed = |reason: &str| SourceReadError::Malformed {
        reason: reason.to_string(),
    };
    let object = value
        .as_object()
        .ok_or_else(|| malformed("issue is not an object"))?;
    let title = object
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| malformed("issue has no title"))?
        .to_string();
    // `body` is required task content but nullable on the wire: an
    // absent or null body digests as the empty string, matching the
    // authoritative re-read on the bridge side.
    let body = object
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let created_at = object
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339)
        .ok_or_else(|| malformed("issue has no usable created_at"))?;
    let revision = object
        .get("updated_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| malformed("issue has no updated_at"))?
        .to_string();
    // The issue number is the repository-scoped identity — the same
    // value the manual `submit --issue repo:number` path stores as the
    // IssueId. Deriving identity from `node_id` or the database `id`
    // would give one issue two scoped identities, letting selector
    // intake bypass claims and exclusions held under the number.
    let display_number = object
        .get("number")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| malformed("issue has no number"))?;
    let mut labels = BTreeSet::new();
    if let Some(label_values) = object.get("labels").and_then(|v| v.as_array()) {
        for label in label_values {
            if let Some(name) = label.get("name").and_then(|v| v.as_str()) {
                labels.insert(name.to_string());
            }
        }
    }
    let issue = IssueRef {
        repository: repository.clone(),
        issue: IssueId::new(display_number.to_string())
            .map_err(|_| malformed("issue id is invalid"))?,
        display_number: Some(display_number),
    };
    // Map only the two known lifecycle states. A missing or
    // unrecognized value is a partial source fact, not a closed
    // Issue: guessing would silently drop a still-open Issue from
    // selection.
    let issue_state = match object.get("state").and_then(|v| v.as_str()) {
        Some("open") => IssueState::Open,
        Some("closed") => IssueState::Closed,
        Some(other) => {
            return Err(malformed(&format!("issue has unknown state '{other}'")));
        }
        None => return Err(malformed("issue has no state")),
    };
    // The digest binds the exact read: a content change produces a
    // different digest and therefore a different proposal identity.
    // It is the shared `issue-content-v1` convention so the
    // authoritative re-read at execution computes the same value
    // for an unchanged Issue — a selector-specific digest here
    // would make every admitted Work look `Changed` at the source
    // gate.
    let content_digest = workloop_selector::issue_content_digest(&title, &body);
    Ok(CollectedIssue {
        issue,
        state: issue_state,
        labels,
        created_unix_seconds: created_at,
        title,
        revision,
        content_digest,
    })
}

/// Parse the RFC 3339 timestamps `gh api` emits (`YYYY-MM-DDTHH:MM:SSZ`
/// or with a `±HH:MM` offset, optional fractional seconds) into Unix
/// seconds. Returns `None` on any deviation — a timestamp the adapter
/// cannot read is malformed source data, never guessed.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Offset: 'Z' or ±HH:MM starting at index 19 (fractional seconds
    // may intervene).
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
    }
    let offset_seconds = match bytes.get(index) {
        Some(b'Z') if index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            let hh = num(index + 1..index + 3)?;
            if bytes.get(index + 3) != Some(&b':') {
                return None;
            }
            let mm = num(index + 4..index + 6)?;
            if index + 6 != bytes.len() || hh > 23 || mm > 59 {
                return None;
            }
            let total = hh * 3600 + mm * 60;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds)
}
