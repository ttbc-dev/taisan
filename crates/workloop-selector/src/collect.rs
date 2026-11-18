//! Bounded source-fact collection for the reference Selector (#202).
//!
//! The selector stays a pure function over a [`SelectorSnapshot`]; this
//! module is the stage in front of it that *produces* that snapshot
//! from live host reads. Collection is adapter-owned: the selector
//! side owns the bounds, the per-repository outcome taxonomy, and the
//! rule that only a completed scan may feed executable input, while
//! the host adapter implements [`SourceReader`] — the paged issue
//! listing — and resolves its own source-read authentication. The
//! collector itself holds no store handle, no publication credential,
//! and no worker authority, and it performs reads only.
//!
//! Every non-`Complete` repository outcome — truncated, deadline
//! exceeded, denied, unavailable, incomplete, or never attempted —
//! contributes **zero** facts. A partially scanned repository is
//! reported, never quietly approximated: no guessed fact becomes
//! executable input, and a complete scan of zero issues stays
//! distinguishable from every failure mode.

use std::collections::{BTreeMap, BTreeSet};

use workloop_core::identity::{IssueRef, RepositoryRef};

use crate::{IssueFact, IssueState, SelectorSnapshot};

/// One collected source Issue — the full retained fact record the
/// approved reference policy consumes (#214): scoped identity,
/// lifecycle state, labels, creation time, and the exact read
/// (`revision` + `content_digest`) a later admission freshness check
/// compares against.
///
/// Field validity is enforced by [`collect`], not by this type: an
/// adapter may construct a fact with empty strings, and collection
/// refuses the whole repository rather than admit it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CollectedIssue {
    /// Scoped identity: host instance + repository id + opaque issue
    /// id. `display_number` is presentation metadata and takes no part
    /// in identity — a host-side rename or renumber never forks it.
    pub issue: IssueRef,
    pub state: IssueState,
    /// Host-supplied label names. Empty is a legitimate value (the
    /// issue carries no labels); the set itself is never `None`.
    pub labels: BTreeSet<String>,
    /// Host-supplied creation time as Unix seconds — the
    /// creation-order fact deterministic ranking needs. Must be
    /// nonnegative; a missing or malformed value fails the page.
    pub created_unix_seconds: i64,
    /// The semantic task headline carried into proposals.
    pub title: String,
    /// The host-supplied source revision this fact was read at —
    /// never a timestamp.
    pub revision: String,
    /// Digest of the Issue content this fact was read from.
    pub content_digest: String,
}

/// The fact as the existing snapshot selector consumes it: labels and
/// creation time are retained on [`CollectedIssue`] for policy use but
/// are not part of the v0.1 `IssueFact` projection.
impl CollectedIssue {
    pub fn fact(&self) -> IssueFact {
        IssueFact {
            issue: self.issue.clone(),
            state: self.state,
            revision: self.revision.clone(),
            content_digest: self.content_digest.clone(),
            title: self.title.clone(),
        }
    }
}

/// One page of a repository's issue listing, as the adapter's
/// [`SourceReader`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePage {
    /// The page's facts, in the reader's order. Order carries no
    /// meaning — collection sorts admitted facts by `IssueRef`.
    pub issues: Vec<CollectedIssue>,
    /// `Some` when the host has another page; `None` means pagination
    /// is exhausted. The cursor is opaque to the collector.
    pub next_cursor: Option<String>,
}

/// A typed failure of one source read. The class decides the
/// repository's [`ScanStatus`]; the reason is diagnostic text —
/// [`collect`] bounds it before it enters the report, and it must
/// never carry secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceReadError {
    /// The read was refused: the bound actor lacks the capability or
    /// the source-read credential was denied. Distinct from a host
    /// failure — retrying with the same credential cannot help.
    Denied { reason: String },
    /// The read could not complete: transport failure, host error, or
    /// timeout. The repository's contents are unknown, not empty.
    Unavailable { reason: String },
    /// The host answered but the page cannot be trusted: malformed
    /// JSON shape, an out-of-scope fact, or a self-contradictory
    /// listing. Distinct from `Unavailable` — the host replied, the
    /// reply is unusable.
    Malformed { reason: String },
}

/// The paged issue-listing capability a host adapter implements for
/// source collection. One call returns at most `page_size` facts for
/// `repository` after `cursor` (`None` = first page). The implementation
/// owns endpoint mapping, pagination mechanics, and source-read
/// authentication; it must never widen scope beyond the requested
/// repository and must classify every failure into [`SourceReadError`].
pub trait SourceReader {
    fn list_issues(
        &mut self,
        repository: &RepositoryRef,
        cursor: Option<&str>,
        page_size: usize,
    ) -> Result<IssuePage, SourceReadError>;
}

/// The bounds one collection run is held to. Every axis is enforced;
/// exceeding any of them ends that repository's scan with the
/// matching [`ScanStatus`] rather than reading on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionBounds {
    /// Maximum facts requested per `list_issues` call.
    pub page_size: usize,
    /// Pages one repository may consume before the scan is
    /// [`ScanStatus::Truncated`].
    pub max_pages_per_repository: usize,
    /// Unique facts one repository may yield before
    /// [`ScanStatus::Truncated`].
    pub max_issues_per_repository: usize,
    /// Total `list_issues` calls across the whole collection —
    /// repositories not yet reached report [`ScanStatus::NotAttempted`],
    /// a scan already in flight reports [`ScanStatus::Truncated`].
    pub max_calls: usize,
    /// Unix-seconds deadline checked before every call and after
    /// every page returns; a scan still in flight past it reports
    /// [`ScanStatus::DeadlineExceeded`] and admits nothing — even a
    /// final page.
    pub deadline_unix_seconds: i64,
}

/// How one repository's scan ended. `Complete` is the only status
/// whose facts enter the collection; every other variant contributes
/// zero and stays distinguishable from a complete empty repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanStatus {
    /// Pagination exhausted and every listed fact validated.
    Complete,
    /// A page, issue, or call bound stopped the scan before exhaustion —
    /// including the shared call budget running out mid-scan.
    Truncated,
    /// The deadline passed before the scan completed.
    DeadlineExceeded,
    /// The read was refused — denied capability or credential. The
    /// reason is bounded by [`collect`] before it enters the report.
    Denied { reason: String },
    /// The read could not complete — transport or host failure. The
    /// reason is bounded by [`collect`] before it enters the report.
    Unavailable { reason: String },
    /// The host answered but its facts could not be trusted.
    /// The reason is bounded by [`collect`] before it enters the report.
    Incomplete { reason: String },
    /// The call budget was exhausted before this repository's first
    /// call — the repository was never read at all.
    NotAttempted,
}

/// One repository's outcome inside a [`CollectionReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryReport {
    pub repository: RepositoryRef,
    pub status: ScanStatus,
    /// `list_issues` calls this repository consumed.
    pub calls: usize,
    /// Unique facts the scan validated before it ended (admitted only
    /// when `status` is `Complete`).
    pub issues: usize,
}

/// The audit record of one collection run: per-repository outcomes in
/// scan order plus the total call count actually spent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionReport {
    pub repositories: Vec<RepositoryReport>,
    /// Total `list_issues` calls made — including calls that failed.
    pub calls: usize,
}

/// The result of one bounded collection: the admitted facts plus the
/// report that says exactly what was and was not read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Collection {
    /// Facts from `Complete` repositories only, sorted by `IssueRef`.
    /// A fact here is authoritative input; nothing else is.
    pub issues: Vec<CollectedIssue>,
    pub report: CollectionReport,
}

impl Collection {
    /// The v0.1 snapshot projection of the admitted facts — the input
    /// the existing [`crate::propose`] consumes unchanged. Facts the
    /// policy layer needs beyond `IssueFact` (labels, creation time)
    /// remain on [`Collection::issues`].
    pub fn snapshot(&self) -> SelectorSnapshot {
        SelectorSnapshot {
            issues: self.issues.iter().map(CollectedIssue::fact).collect(),
            cursor: None,
            fetched_unix_seconds: None,
        }
    }
}

/// Run one bounded collection over `repositories` — the configured
/// scope, scanned in sorted `RepositoryRef` order so the outcome is
/// deterministic. `now` supplies the Unix-seconds clock the deadline
/// is checked against; injecting it keeps collection testable without
/// a real clock.
///
/// The function is total: reader errors, bound exhaustion, and
/// malformed pages are data in the report, never panics or partial
/// admissions.
pub fn collect<R: SourceReader + ?Sized>(
    reader: &mut R,
    repositories: &BTreeSet<RepositoryRef>,
    bounds: &CollectionBounds,
    now: &dyn Fn() -> i64,
) -> Collection {
    let mut collection = Collection::default();
    for repository in repositories {
        let (report, admitted) = scan_repository(
            reader,
            repository,
            bounds,
            now,
            &mut collection.report.calls,
        );
        // Admitted facts exist only on `Complete`; every other status
        // discards its partial pages whole.
        if report.status == ScanStatus::Complete {
            collection.issues.extend(admitted);
        }
        collection.report.repositories.push(report);
    }
    collection.issues.sort_by(|a, b| a.issue.cmp(&b.issue));
    collection
}

/// The per-repository scan: pages until exhaustion, a bound, a
/// failure, or the deadline. Returns the repository's report plus —
/// meaningful only on `Complete` — the admitted facts.
fn scan_repository<R: SourceReader + ?Sized>(
    reader: &mut R,
    repository: &RepositoryRef,
    bounds: &CollectionBounds,
    now: &dyn Fn() -> i64,
    total_calls: &mut usize,
) -> (RepositoryReport, Vec<CollectedIssue>) {
    let mut report = RepositoryReport {
        repository: repository.clone(),
        status: ScanStatus::Complete,
        calls: 0,
        issues: 0,
    };
    // Unique facts seen so far, keyed by scoped identity. `IssueRef`
    // equality ignores `display_number`, so a renumbered duplicate
    // collapses; any other field difference is a contradiction.
    let mut seen: BTreeMap<IssueRef, CollectedIssue> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;

    loop {
        if *total_calls >= bounds.max_calls {
            // A repository that already spent calls is a partial scan
            // — Truncated, not NotAttempted (which means the budget
            // ran out before this repository's first call).
            report.status = if report.calls == 0 {
                ScanStatus::NotAttempted
            } else {
                ScanStatus::Truncated
            };
            break;
        }
        if now() >= bounds.deadline_unix_seconds {
            report.status = ScanStatus::DeadlineExceeded;
            break;
        }
        if pages >= bounds.max_pages_per_repository {
            report.status = ScanStatus::Truncated;
            break;
        }
        *total_calls += 1;
        report.calls += 1;
        pages += 1;
        match reader.list_issues(repository, cursor.as_deref(), bounds.page_size) {
            Err(error) => {
                report.status = match error {
                    SourceReadError::Denied { reason } => ScanStatus::Denied {
                        reason: bounded_reason(reason),
                    },
                    SourceReadError::Unavailable { reason } => ScanStatus::Unavailable {
                        reason: bounded_reason(reason),
                    },
                    SourceReadError::Malformed { reason } => ScanStatus::Incomplete {
                        reason: bounded_reason(reason),
                    },
                };
                break;
            }
            Ok(page) => {
                // The deadline bounds scan completion, not just call
                // initiation: a page landing at or past it admits
                // nothing — even a final page — so the repository
                // reports DeadlineExceeded rather than Complete.
                if now() >= bounds.deadline_unix_seconds {
                    report.status = ScanStatus::DeadlineExceeded;
                    break;
                }
                // The reader contract is "at most page_size facts";
                // a page that breaks it cannot be trusted — the same
                // failure class as malformed source data. Enforcing
                // it here also bounds what one reply can make the
                // collector clone.
                if page.issues.len() > bounds.page_size {
                    report.status = ScanStatus::Incomplete {
                        reason: bounded_reason(format!(
                            "page carries {} facts, exceeding the requested page size {}",
                            page.issues.len(),
                            bounds.page_size
                        )),
                    };
                    break;
                }
                if let Err(reason) = admit_page(repository, &page, &mut seen) {
                    report.status = ScanStatus::Incomplete {
                        reason: bounded_reason(reason),
                    };
                    break;
                }
                if seen.len() > bounds.max_issues_per_repository {
                    report.status = ScanStatus::Truncated;
                    break;
                }
                match page.next_cursor {
                    None => break,
                    Some(next) => cursor = Some(next),
                }
            }
        }
    }

    if report.status == ScanStatus::Complete {
        report.issues = seen.len();
        (report, seen.into_values().collect())
    } else {
        // A non-Complete scan admits nothing: partial pages are
        // discarded whole, never approximated.
        report.issues = 0;
        (report, Vec::new())
    }
}

/// Validate one page and merge its facts into `seen`. Any defect —
/// an out-of-scope issue, a missing required field, or a fact
/// contradicting an earlier listing of the same identity — fails the
/// entire repository as `Incomplete`.
fn admit_page(
    repository: &RepositoryRef,
    page: &IssuePage,
    seen: &mut BTreeMap<IssueRef, CollectedIssue>,
) -> Result<(), String> {
    for fact in &page.issues {
        validate_fact(repository, fact)?;
        match seen.get(&fact.issue) {
            None => {
                seen.insert(fact.issue.clone(), fact.clone());
            }
            Some(existing) if *existing == *fact => {}
            Some(_) => {
                return Err(format!("contradictory facts for {}", fact.issue));
            }
        }
    }
    Ok(())
}

/// The per-fact contract: scoped to the repository being read, every
/// required field present and bounded, and a usable creation timestamp.
fn validate_fact(repository: &RepositoryRef, fact: &CollectedIssue) -> Result<(), String> {
    if fact.issue.repository != *repository {
        return Err(format!(
            "fact {} is scoped outside the requested repository",
            fact.issue
        ));
    }
    // The issue id is an opaque identity token under the same bound
    // as revisions and digests. The message names only the
    // repository: echoing the id would let a pathological value
    // inflate the diagnostic it just failed.
    if !bounded_token(fact.issue.issue.as_str()) {
        return Err(format!(
            "a fact in {} carries an out-of-bounds issue id",
            fact.issue.repository
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
            fact.issue
        ));
    }
    if !bounded_token(&fact.revision) || !bounded_token(&fact.content_digest) {
        return Err(format!(
            "fact {} is missing a bounded revision or content digest",
            fact.issue
        ));
    }
    if fact
        .labels
        .iter()
        .any(|l| l.is_empty() || l.len() > MAX_LABEL_BYTES)
    {
        return Err(format!("fact {} carries a malformed label", fact.issue));
    }
    if fact.created_unix_seconds < 0 {
        return Err(format!(
            "fact {} carries a negative creation time",
            fact.issue
        ));
    }
    Ok(())
}

/// An opaque identity token — issue id, source revision, or content
/// digest — is bounded: nonempty, at most 512 bytes, no control
/// characters, matching the identity layer's opaque-id discipline.
fn bounded_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

/// Task headlines ride into proposal intents; bound them at the
/// protocol's intent ceiling so a pathological host cannot produce an
/// unbounded fact.
const MAX_TITLE_BYTES: usize = 16 * 1024;
const MAX_LABEL_BYTES: usize = 256;

/// Diagnostic reasons entering [`ScanStatus`] are reader- or
/// fact-supplied text; bound them so a hostile or broken reader
/// cannot inflate the report.
const MAX_DIAGNOSTIC_BYTES: usize = 1024;

fn bounded_reason(reason: String) -> String {
    if reason.len() <= MAX_DIAGNOSTIC_BYTES {
        return reason;
    }
    let mut end = MAX_DIAGNOSTIC_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = reason[..end].to_string();
    truncated.push('…');
    truncated
}
