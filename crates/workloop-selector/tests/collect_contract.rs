//! Contract tests for bounded source-fact collection (#202): the
//! adapter-owned stage in front of the snapshot selector. A stub
//! [`SourceReader`] scripts pages and failures; every assertion binds
//! the collection contract — per-repository statuses, bounds,
//! fact validation, scoped identity, and determinism. No host, store,
//! or worker is exercised.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use workloop_core::identity::{HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef};
use workloop_selector::IssueState;
use workloop_selector::collect::{
    CollectedIssue, Collection, CollectionBounds, IssuePage, ScanStatus, SourceReadError,
    SourceReader, collect,
};

fn host(name: &str) -> HostInstanceId {
    HostInstanceId::trusted(name)
}

fn repo(host_name: &str, id: &str) -> RepositoryRef {
    RepositoryRef::new(host(host_name), RepositoryId::trusted(id))
}

fn issue(repo: &RepositoryRef, id: &str, number: Option<u64>) -> IssueRef {
    IssueRef {
        repository: repo.clone(),
        issue: IssueId::trusted(id),
        display_number: number,
    }
}

fn fact(repo: &RepositoryRef, id: &str, number: Option<u64>) -> CollectedIssue {
    CollectedIssue {
        issue: issue(repo, id, number),
        state: IssueState::Open,
        labels: BTreeSet::from(["ready".to_string()]),
        created_unix_seconds: 1_700_000_000,
        title: format!("task for {id}"),
        revision: format!("rev-{id}"),
        content_digest: format!("digest-{id}"),
    }
}

fn page(facts: Vec<CollectedIssue>, next: Option<&str>) -> IssuePage {
    IssuePage {
        issues: facts,
        next_cursor: next.map(str::to_string),
    }
}

/// One scripted reader step: a page answer or a typed failure.
enum Step {
    Page(IssuePage),
    Fail(SourceReadError),
}

/// A stub reader: per-repository scripted steps consumed in call
/// order; an exhausted script answers a complete empty page, so a
/// repository with no script reads as a complete empty repository.
struct StubReader {
    scripts: BTreeMap<String, VecDeque<Step>>,
    calls: Vec<(String, Option<String>, usize)>,
}

impl StubReader {
    fn new() -> Self {
        Self {
            scripts: BTreeMap::new(),
            calls: Vec::new(),
        }
    }

    fn script(&mut self, repository: &RepositoryRef, steps: Vec<Step>) -> &mut Self {
        self.scripts
            .insert(repository.to_string(), steps.into_iter().collect());
        self
    }

    fn script_pages(&mut self, repository: &RepositoryRef, pages: Vec<IssuePage>) -> &mut Self {
        self.script(repository, pages.into_iter().map(Step::Page).collect())
    }
}

impl SourceReader for StubReader {
    fn list_issues(
        &mut self,
        repository: &RepositoryRef,
        cursor: Option<&str>,
        page_size: usize,
    ) -> Result<IssuePage, SourceReadError> {
        self.calls.push((
            repository.to_string(),
            cursor.map(str::to_string),
            page_size,
        ));
        let key = repository.to_string();
        match self.scripts.get_mut(&key).and_then(VecDeque::pop_front) {
            Some(Step::Page(page)) => Ok(page),
            Some(Step::Fail(error)) => Err(error),
            None => Ok(IssuePage {
                issues: Vec::new(),
                next_cursor: None,
            }),
        }
    }
}

fn bounds() -> CollectionBounds {
    CollectionBounds {
        page_size: 2,
        max_pages_per_repository: 8,
        max_issues_per_repository: 16,
        max_calls: 64,
        deadline_unix_seconds: 1_800_000_000,
    }
}

fn clock_at(now: i64) -> impl Fn() -> i64 {
    move || now
}

fn scope(repos: &[RepositoryRef]) -> BTreeSet<RepositoryRef> {
    repos.iter().cloned().collect()
}

fn status_of<'a>(collection: &'a Collection, repository: &RepositoryRef) -> &'a ScanStatus {
    &collection
        .report
        .repositories
        .iter()
        .find(|r| r.repository == *repository)
        .expect("every scoped repository is reported")
        .status
}

/// A completed scan contributes its facts; a scan cut by any bound or
/// failure contributes zero — and the two stay distinguishable.
#[test]
fn only_complete_scans_contribute_facts() {
    let full = repo("h", "repo-full");
    let empty = repo("h", "repo-empty");
    let cut = repo("h", "repo-cut");
    let mut reader = StubReader::new();
    reader.script_pages(&full, vec![page(vec![fact(&full, "i1", Some(1))], None)]);
    // `empty` has no script: a complete scan of zero issues.
    reader.script_pages(
        &cut,
        vec![
            page(vec![fact(&cut, "i1", None)], Some("c1")),
            page(vec![fact(&cut, "i2", None)], Some("c2")),
        ],
    );

    let mut tight = bounds();
    tight.max_pages_per_repository = 1;
    let collection = collect(
        &mut reader,
        &scope(&[full.clone(), empty.clone(), cut.clone()]),
        &tight,
        &clock_at(1_700_000_001),
    );

    assert_eq!(collection.issues.len(), 1, "only the complete repo admits");
    assert_eq!(collection.issues[0].issue, issue(&full, "i1", Some(1)));
    assert_eq!(status_of(&collection, &full), &ScanStatus::Complete);
    assert_eq!(status_of(&collection, &empty), &ScanStatus::Complete);
    assert_eq!(status_of(&collection, &cut), &ScanStatus::Truncated);
    // The empty repository is Complete-with-zero — never confused with
    // the truncated one.
    let empty_report = collection
        .report
        .repositories
        .iter()
        .find(|r| r.repository == empty)
        .unwrap();
    assert_eq!(empty_report.issues, 0);
}

/// Each bound axis stops the scan with its own typed status.
#[test]
fn bounds_stop_the_scan_with_typed_statuses() {
    // Page bound.
    let paged = repo("h", "repo-paged");
    let mut reader = StubReader::new();
    reader.script_pages(
        &paged,
        vec![
            page(vec![fact(&paged, "i1", None)], Some("c1")),
            page(vec![fact(&paged, "i2", None)], Some("c2")),
            page(vec![fact(&paged, "i3", None)], None),
        ],
    );
    let mut b = bounds();
    b.max_pages_per_repository = 2;
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&paged)),
        &b,
        &clock_at(1),
    );
    assert_eq!(status_of(&c, &paged), &ScanStatus::Truncated);
    assert!(c.issues.is_empty());

    // Issue bound.
    let many = repo("h", "repo-many");
    let mut reader = StubReader::new();
    reader.script_pages(
        &many,
        vec![page(
            vec![
                fact(&many, "i1", None),
                fact(&many, "i2", None),
                fact(&many, "i3", None),
            ],
            None,
        )],
    );
    let mut b = bounds();
    b.max_issues_per_repository = 2;
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&many)),
        &b,
        &clock_at(1),
    );
    assert_eq!(status_of(&c, &many), &ScanStatus::Truncated);
    assert!(c.issues.is_empty());

    // Call budget: the second repository is never read.
    let first = repo("h", "repo-first");
    let second = repo("h", "repo-second");
    let mut reader = StubReader::new();
    reader.script_pages(&first, vec![page(vec![fact(&first, "i1", None)], None)]);
    let mut b = bounds();
    b.max_calls = 1;
    let c = collect(
        &mut reader,
        &scope(&[first.clone(), second.clone()]),
        &b,
        &clock_at(1),
    );
    assert_eq!(status_of(&c, &first), &ScanStatus::Complete);
    assert_eq!(status_of(&c, &second), &ScanStatus::NotAttempted);
    assert_eq!(c.report.calls, 1);
    assert_eq!(c.issues.len(), 1);

    // Deadline: already past before the first call.
    let late = repo("h", "repo-late");
    let mut reader = StubReader::new();
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&late)),
        &bounds(),
        &clock_at(9_999_999_999),
    );
    assert_eq!(status_of(&c, &late), &ScanStatus::DeadlineExceeded);
    assert!(reader.calls.is_empty(), "no call is made past the deadline");
}

/// A mid-scan deadline stops further pages but keeps earlier calls
/// counted.
#[test]
fn the_deadline_is_checked_between_calls() {
    let repo = repo("h", "repo-slow");
    let mut reader = StubReader::new();
    reader.script_pages(
        &repo,
        vec![
            page(vec![fact(&repo, "i1", None)], Some("c1")),
            page(vec![fact(&repo, "i2", None)], None),
        ],
    );
    // The clock advances one step per read: first call sees 1, second 2.
    let ticks = std::cell::Cell::new(0i64);
    let now = || {
        ticks.set(ticks.get() + 1);
        ticks.get()
    };
    let mut b = bounds();
    b.deadline_unix_seconds = 2;
    let c = collect(&mut reader, &scope(std::slice::from_ref(&repo)), &b, &now);
    assert_eq!(status_of(&c, &repo), &ScanStatus::DeadlineExceeded);
    assert_eq!(c.report.calls, 1, "the second page was never requested");
    assert!(c.issues.is_empty());
}

/// Typed read failures map to their repository statuses; the reason is
/// carried as bounded diagnostic data.
#[test]
fn read_failures_become_typed_statuses() {
    for (error, expected) in [
        (
            SourceReadError::Denied {
                reason: "capability issue_list denied".into(),
            },
            ScanStatus::Denied {
                reason: "capability issue_list denied".into(),
            },
        ),
        (
            SourceReadError::Unavailable {
                reason: "host timed out".into(),
            },
            ScanStatus::Unavailable {
                reason: "host timed out".into(),
            },
        ),
        (
            SourceReadError::Malformed {
                reason: "page is not a list".into(),
            },
            ScanStatus::Incomplete {
                reason: "page is not a list".into(),
            },
        ),
    ] {
        let target = repo("h", "repo-fail");
        let mut reader = StubReader::new();
        reader.script(&target, vec![Step::Fail(error)]);
        let c = collect(
            &mut reader,
            &scope(std::slice::from_ref(&target)),
            &bounds(),
            &clock_at(1),
        );
        assert_eq!(status_of(&c, &target), &expected);
        assert!(c.issues.is_empty());
    }
}

/// Malformed facts — out-of-scope identity, empty required fields, a
/// contradictory duplicate — each fail the whole repository as
/// Incomplete with zero admitted facts.
#[test]
fn malformed_facts_fail_the_repository() {
    let target = repo("h", "repo-bad");
    let other = repo("h", "repo-other");

    // A fact scoped to a different repository than the one read.
    let mut foreign = fact(&other, "i9", None);
    foreign.issue.repository = other.clone();
    let mut reader = StubReader::new();
    reader.script_pages(&target, vec![page(vec![foreign], None)]);
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &bounds(),
        &clock_at(1),
    );
    assert!(matches!(
        status_of(&c, &target),
        ScanStatus::Incomplete { .. }
    ));
    assert!(c.issues.is_empty());

    // Empty revision / digest / title each fail independently.
    for mutate in [
        (|f: &mut CollectedIssue| f.revision = String::new()) as fn(&mut CollectedIssue),
        |f| f.content_digest = String::new(),
        |f| f.title = String::new(),
        |f| f.created_unix_seconds = -1,
        |f| {
            f.labels.insert(String::new());
        },
    ] {
        let mut bad = fact(&target, "i1", None);
        mutate(&mut bad);
        let mut reader = StubReader::new();
        reader.script_pages(&target, vec![page(vec![bad], None)]);
        let c = collect(
            &mut reader,
            &scope(std::slice::from_ref(&target)),
            &bounds(),
            &clock_at(1),
        );
        assert!(
            matches!(status_of(&c, &target), ScanStatus::Incomplete { .. }),
            "malformed fact must fail the repository"
        );
        assert!(c.issues.is_empty());
    }

    // The same identity listed twice with different content is a
    // contradiction — never silently picked between.
    let mut first = fact(&target, "i1", None);
    first.title = "version a".into();
    let mut second = fact(&target, "i1", None);
    second.title = "version b".into();
    let mut reader = StubReader::new();
    reader.script_pages(
        &target,
        vec![page(vec![first], Some("c1")), page(vec![second], None)],
    );
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &bounds(),
        &clock_at(1),
    );
    assert!(matches!(
        status_of(&c, &target),
        ScanStatus::Incomplete { .. }
    ));
    assert!(c.issues.is_empty());
}

/// Scoped identity survives display metadata: the same opaque issue id
/// on two repositories or host instances is two distinct facts, a
/// renumbered duplicate collapses, and identical repeats are one fact.
#[test]
fn scoped_identity_survives_display_metadata() {
    let repo_a = repo("host-1", "repo-a");
    let repo_b = repo("host-1", "repo-b");
    let repo_c = repo("host-2", "repo-a");
    let mut reader = StubReader::new();
    // Same opaque id "i1" in three scopes; repo-a lists it twice with a
    // different display number — one identity, one fact.
    reader.script_pages(
        &repo_a,
        vec![page(
            vec![fact(&repo_a, "i1", Some(7)), fact(&repo_a, "i1", Some(9))],
            None,
        )],
    );
    reader.script_pages(
        &repo_b,
        vec![page(vec![fact(&repo_b, "i1", Some(7))], None)],
    );
    reader.script_pages(
        &repo_c,
        vec![page(vec![fact(&repo_c, "i1", Some(7))], None)],
    );

    let c = collect(
        &mut reader,
        &scope(&[repo_a.clone(), repo_b.clone(), repo_c.clone()]),
        &bounds(),
        &clock_at(1),
    );
    assert_eq!(c.issues.len(), 3, "one fact per scoped identity");
    let ids: Vec<(String, String)> = c
        .issues
        .iter()
        .map(|i| (i.issue.repository.to_string(), i.issue.issue.to_string()))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("host-1/repo-a".to_string(), "i1".to_string()),
            ("host-1/repo-b".to_string(), "i1".to_string()),
            ("host-2/repo-a".to_string(), "i1".to_string()),
        ]
    );
}

/// Repositories scan in sorted order, admitted facts sort by IssueRef,
/// and an identical run reproduces the identical collection.
#[test]
fn collection_is_deterministic() {
    let z = repo("h", "repo-z");
    let a = repo("h", "repo-a");
    let m = repo("h", "repo-m");
    let build = || {
        let mut reader = StubReader::new();
        reader.script_pages(&z, vec![page(vec![fact(&z, "i2", None)], None)]);
        reader.script_pages(
            &a,
            vec![page(vec![fact(&a, "i9", None), fact(&a, "i1", None)], None)],
        );
        reader.script_pages(&m, vec![page(vec![fact(&m, "i5", None)], None)]);
        reader
    };
    let mut reader = build();
    // Deliberately unsorted scope insertion still scans sorted.
    let c1 = collect(
        &mut reader,
        &scope(&[z.clone(), a.clone(), m.clone()]),
        &bounds(),
        &clock_at(1),
    );
    let order: Vec<String> = c1
        .report
        .repositories
        .iter()
        .map(|r| r.repository.to_string())
        .collect();
    assert_eq!(order, vec!["h/repo-a", "h/repo-m", "h/repo-z"]);
    let fact_order: Vec<String> = c1
        .issues
        .iter()
        .map(|i| i.issue.issue.to_string())
        .collect();
    // Facts sort by scoped IssueRef: repo-a's i1 before i9, then
    // repo-m, then repo-z — input order never matters.
    assert_eq!(fact_order, vec!["i1", "i9", "i5", "i2"]);

    let mut reader2 = build();
    let c2 = collect(
        &mut reader2,
        &scope(&[m.clone(), z.clone(), a.clone()]),
        &bounds(),
        &clock_at(1),
    );
    assert_eq!(c1, c2, "identical inputs produce identical collections");
}

/// Pagination is followed through opaque cursors until the host says
/// exhaustion; page_size is passed to every call.
#[test]
fn pagination_follows_cursors_to_exhaustion() {
    let target = repo("h", "repo-pages");
    let mut reader = StubReader::new();
    reader.script_pages(
        &target,
        vec![
            page(vec![fact(&target, "i1", None)], Some("cursor-1")),
            page(vec![fact(&target, "i2", None)], Some("cursor-2")),
            page(vec![fact(&target, "i3", None)], None),
        ],
    );
    let c = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &bounds(),
        &clock_at(1),
    );
    assert_eq!(status_of(&c, &target), &ScanStatus::Complete);
    assert_eq!(c.issues.len(), 3);
    assert_eq!(
        reader
            .calls
            .iter()
            .map(|(_, cur, _)| cur.clone())
            .collect::<Vec<_>>(),
        vec![
            None,
            Some("cursor-1".to_string()),
            Some("cursor-2".to_string())
        ],
        "each call continues from the previous cursor"
    );
    assert!(
        reader.calls.iter().all(|(_, _, size)| *size == 2),
        "page_size reaches every call"
    );
}

/// The snapshot projection carries exactly the admitted facts in the
/// v0.1 IssueFact shape — labels and creation time stay on the
/// collected record for policy use.
#[test]
fn snapshot_projects_admitted_facts() {
    let target = repo("h", "repo-snap");
    let mut rich = fact(&target, "i1", Some(42));
    rich.labels = BTreeSet::from(["bug".to_string(), "p1".to_string()]);
    rich.created_unix_seconds = 1_600_000_000;
    rich.state = IssueState::Closed;
    let mut reader = StubReader::new();
    reader.script_pages(&target, vec![page(vec![rich], None)]);
    let c = collect(&mut reader, &scope(&[target]), &bounds(), &clock_at(1));
    let snapshot = c.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    let projected = &snapshot.issues[0];
    assert_eq!(
        projected.issue,
        issue(&repo("h", "repo-snap"), "i1", Some(42))
    );
    assert_eq!(projected.state, IssueState::Closed);
    assert_eq!(projected.revision, "rev-i1");
    assert_eq!(projected.content_digest, "digest-i1");
    assert_eq!(projected.title, "task for i1");
    // The retained record keeps what the projection drops.
    assert_eq!(c.issues[0].labels.len(), 2);
    assert_eq!(c.issues[0].created_unix_seconds, 1_600_000_000);
}
