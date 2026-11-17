//! Fixture-host source collection contract (#202): the synthetic
//! remote's paged issue listing driven through the real
//! [`workloop_selector::collect::collect`] bounds engine. Every fact
//! planted is retained verbatim — identity, state, labels, creation
//! time, revision and digest — and every failure class surfaces as
//! typed data, never a guessed fact. Collection is read-only: no
//! Issue, label, pull-request, comment, or Work write is issued.

use std::collections::BTreeSet;

use workloop_core::identity::{
    CredentialRef, HostActorId, HostActorRef, HostInstanceId, IssueId, IssueRef, RepositoryId,
    RepositoryRef,
};
use workloop_github::fixture::{CredentialBook, FixtureGithub};
use workloop_selector::IssueState;
use workloop_selector::collect::{CollectedIssue, CollectionBounds, ScanStatus, collect};

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo(id: &str) -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted(id))
}

fn actor(name: &str) -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted(name),
    }
}

fn reader_actor() -> HostActorRef {
    actor("source-reader")
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
        created_unix_seconds: 1_600_000_000,
        title: format!("task for {id}"),
        revision: format!("rev-{id}"),
        content_digest: format!("digest-{id}"),
    }
}

/// A fixture with one seated source-read credential authenticating as
/// `reader_actor` — no publication credential, no store.
fn seated_fixture() -> FixtureGithub {
    let mut book = CredentialBook::new();
    book.seat_env("SRC_READ_CRED", "src-token");
    book.seat_actor("src-token", reader_actor());
    FixtureGithub::new().with_credentials(book)
}

fn source_ref() -> CredentialRef {
    CredentialRef::trusted("env:SRC_READ_CRED")
}

fn bounds() -> CollectionBounds {
    CollectionBounds {
        page_size: 100,
        max_pages_per_repository: 8,
        max_issues_per_repository: 256,
        max_calls: 64,
        deadline_unix_seconds: NOW + 3600,
    }
}

fn scope(repos: &[RepositoryRef]) -> BTreeSet<RepositoryRef> {
    repos.iter().cloned().collect()
}

fn status_of<'a>(
    collection: &'a workloop_selector::collect::Collection,
    repository: &RepositoryRef,
) -> &'a ScanStatus {
    &collection
        .report
        .repositories
        .iter()
        .find(|r| r.repository == *repository)
        .expect("every scoped repository is reported")
        .status
}

/// Fixture host reads collect every retained field verbatim: scoped
/// identity, open/closed state, labels, creation time, source
/// revision and content digest — across a paged listing.
#[test]
fn fixture_reads_collect_exact_retained_facts() {
    let target = repo("repo-a");
    let mut fixture = seated_fixture();
    let mut rich = fact(&target, "issue-1", Some(42));
    rich.state = IssueState::Closed;
    rich.labels = BTreeSet::from(["bug".to_string(), "p1".to_string()]);
    rich.created_unix_seconds = 1_550_000_000;
    rich.title = "fix the flaky test".into();
    rich.revision = "updated-at-2026-01-01T00:00:00Z+etag".into();
    rich.content_digest = "sha256:abc123".into();
    fixture.plant_source_issue(&target, rich);
    fixture.plant_source_issue(&target, fact(&target, "issue-2", Some(43)));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .expect("seated credential authenticates");
    let collection = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &bounds(),
        &|| NOW,
    );

    assert_eq!(status_of(&collection, &target), &ScanStatus::Complete);
    assert_eq!(collection.issues.len(), 2);
    let first = &collection.issues[0];
    assert_eq!(first.issue, issue(&target, "issue-1", Some(42)));
    assert_eq!(first.state, IssueState::Closed);
    assert_eq!(
        first.labels,
        BTreeSet::from(["bug".to_string(), "p1".to_string()])
    );
    assert_eq!(first.created_unix_seconds, 1_550_000_000);
    assert_eq!(first.title, "fix the flaky test");
    assert_eq!(first.revision, "updated-at-2026-01-01T00:00:00Z+etag");
    assert_eq!(first.content_digest, "sha256:abc123");
    // Identity ignores the display number: the same scoped id is one fact.
    assert_eq!(
        collection.issues[1].issue,
        issue(&target, "issue-2", Some(43))
    );
}

/// A listing larger than one page is followed through cursors to
/// exhaustion; every planted fact is retained.
#[test]
fn pagination_collects_every_page() {
    let target = repo("repo-paged");
    let mut fixture = seated_fixture();
    for i in 0..5 {
        fixture.plant_source_issue(&target, fact(&target, &format!("i{i}"), Some(i + 1)));
    }
    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let mut b = bounds();
    b.page_size = 2;
    let collection = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &b,
        &|| NOW,
    );
    assert_eq!(status_of(&collection, &target), &ScanStatus::Complete);
    assert_eq!(collection.issues.len(), 5);
    let report = collection
        .report
        .repositories
        .iter()
        .find(|r| r.repository == target)
        .unwrap();
    assert_eq!(report.calls, 3, "pages of 2,2,1 plus the empty tail check");
}

/// A scan cut by the page bound contributes zero facts — distinguishable
/// from the complete empty repository beside it.
#[test]
fn truncated_scan_contributes_nothing() {
    let full = repo("repo-full");
    let cut = repo("repo-cut");
    let empty = repo("repo-empty");
    let mut fixture = seated_fixture();
    for i in 0..5 {
        fixture.plant_source_issue(&cut, fact(&cut, &format!("i{i}"), None));
    }
    fixture.plant_source_issue(&full, fact(&full, "i1", None));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let mut b = bounds();
    b.page_size = 2;
    b.max_pages_per_repository = 1;
    let collection = collect(
        &mut reader,
        &scope(&[full.clone(), cut.clone(), empty.clone()]),
        &b,
        &|| NOW,
    );

    assert_eq!(collection.issues.len(), 1);
    assert_eq!(collection.issues[0].issue.repository, full);
    assert_eq!(status_of(&collection, &cut), &ScanStatus::Truncated);
    assert_eq!(status_of(&collection, &empty), &ScanStatus::Complete);
    assert!(
        collection
            .report
            .repositories
            .iter()
            .find(|r| r.repository == empty)
            .unwrap()
            .issues
            == 0
    );
}

/// Denied and unavailable reads surface as their typed statuses —
/// never an empty successful scan.
#[test]
fn denied_and_unavailable_reads_are_typed() {
    let denied_repo = repo("repo-denied");
    let down_repo = repo("repo-down");
    let mut fixture = seated_fixture();
    fixture.deny_source_reads(&denied_repo);
    fixture.unavailable_source_reads(&down_repo);
    fixture.plant_source_issue(&denied_repo, fact(&denied_repo, "i1", None));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let collection = collect(
        &mut reader,
        &scope(&[denied_repo.clone(), down_repo.clone()]),
        &bounds(),
        &|| NOW,
    );

    assert!(matches!(
        status_of(&collection, &denied_repo),
        ScanStatus::Denied { .. }
    ));
    assert!(matches!(
        status_of(&collection, &down_repo),
        ScanStatus::Unavailable { .. }
    ));
    assert!(collection.issues.is_empty());
}

/// A planted fact missing required fields fails its whole repository
/// as Incomplete — the collection never admits a guessed fact.
#[test]
fn malformed_facts_fail_the_repository() {
    let target = repo("repo-bad");
    let mut fixture = seated_fixture();
    let mut bad = fact(&target, "i1", None);
    bad.revision = String::new();
    fixture.plant_source_issue(&target, bad);
    fixture.plant_source_issue(&target, fact(&target, "i2", None));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let collection = collect(
        &mut reader,
        &scope(std::slice::from_ref(&target)),
        &bounds(),
        &|| NOW,
    );
    assert!(matches!(
        status_of(&collection, &target),
        ScanStatus::Incomplete { .. }
    ));
    assert!(collection.issues.is_empty(), "no partial page is admitted");
}

/// Source IDs stay collision-safe across repositories and display
/// renames: identical opaque ids and display numbers in different
/// scopes remain distinct facts.
#[test]
fn source_ids_stay_scoped_across_repositories() {
    let repo_a = repo("repo-a");
    let repo_b = repo("repo-b");
    let mut fixture = seated_fixture();
    // Same opaque id and same display number in two repositories —
    // two distinct facts; a renumbered plant in repo-a is the same
    // identity, not a second fact.
    fixture.plant_source_issue(&repo_a, fact(&repo_a, "i1", Some(7)));
    fixture.plant_source_issue(&repo_a, fact(&repo_a, "i1", Some(9)));
    fixture.plant_source_issue(&repo_b, fact(&repo_b, "i1", Some(7)));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let collection = collect(
        &mut reader,
        &scope(&[repo_a.clone(), repo_b.clone()]),
        &bounds(),
        &|| NOW,
    );
    assert_eq!(collection.issues.len(), 2);
    let repos: BTreeSet<String> = collection
        .issues
        .iter()
        .map(|i| i.issue.repository.to_string())
        .collect();
    assert_eq!(
        repos,
        BTreeSet::from([
            "test-host/repo-a".to_string(),
            "test-host/repo-b".to_string()
        ])
    );
}

/// Credential failures surface before any read: a missing credential
/// environment, an unresolvable slot, a denied credential, and an
/// actor mismatch each fail `source_reader` construction with a typed
/// `CredentialError` — no reader exists, so no listing can be
/// bypassed or silently skipped.
#[test]
fn credential_failures_fail_before_any_read() {
    // No credential environment installed at all.
    let bare = FixtureGithub::new();
    let error = bare
        .source_reader(Some(&source_ref()), &reader_actor())
        .expect_err("no book must fail");
    assert_eq!(error.code(), "credential.missing");

    // A slot the book does not seat.
    let fixture = seated_fixture();
    let unknown = CredentialRef::trusted("env:NOT_SEATED");
    let error = fixture
        .source_reader(Some(&unknown), &reader_actor())
        .expect_err("unseated slot must fail");
    assert_eq!(error.code(), "credential.unresolvable");

    // The host denies the resolved credential.
    let mut book = CredentialBook::new();
    book.seat_env("SRC_READ_CRED", "denied-token");
    book.seat_denied("denied-token");
    let fixture = FixtureGithub::new().with_credentials(book);
    let error = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .expect_err("denied credential must fail");
    assert_eq!(error.code(), "credential.unresolvable");

    // The credential authenticates as a different actor than bound.
    let mut book = CredentialBook::new();
    book.seat_env("SRC_READ_CRED", "other-token");
    book.seat_actor("other-token", actor("intruder"));
    let fixture = FixtureGithub::new().with_credentials(book);
    let error = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .expect_err("actor mismatch must fail");
    assert_eq!(error.code(), "credential.actor_mismatch");

    // An undefined slot is credential.missing, not a silent read.
    let fixture = seated_fixture();
    let error = fixture
        .source_reader(None, &reader_actor())
        .expect_err("undefined slot must fail");
    assert_eq!(error.code(), "credential.missing");

    // Every failure happened at construction: no reader exists, so no
    // listing call was ever issued.
}

/// Collection is read-only: a full pass over planted repositories
/// issues no writes — the fixture's applied-write history stays empty
/// and no store was ever involved (the reader holds none).
#[test]
fn collection_writes_nothing() {
    let target = repo("repo-a");
    let mut fixture = seated_fixture();
    fixture.plant_source_issue(&target, fact(&target, "i1", None));
    fixture.plant_source_issue(&target, fact(&target, "i2", None));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let collection = collect(&mut reader, &scope(&[target]), &bounds(), &|| NOW);
    assert_eq!(collection.issues.len(), 2);
    assert!(
        fixture.applied().is_empty(),
        "collection must not record host writes: {:?}",
        fixture.applied()
    );
}

/// The snapshot projection feeds the existing selector unchanged:
/// admitted facts map to `IssueFact` while labels and creation time
/// stay on the collected record.
#[test]
fn collection_feeds_the_snapshot_selector() {
    let target = repo("repo-a");
    let mut fixture = seated_fixture();
    fixture.plant_source_issue(&target, fact(&target, "i1", Some(11)));

    let mut reader = fixture
        .source_reader(Some(&source_ref()), &reader_actor())
        .unwrap();
    let collection = collect(&mut reader, &scope(&[target]), &bounds(), &|| NOW);
    let snapshot = collection.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].title, "task for i1");
    assert_eq!(snapshot.issues[0].revision, "rev-i1");
}
