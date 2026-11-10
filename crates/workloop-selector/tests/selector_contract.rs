//! Contract tests for the reference Selector (U6.1): deterministic
//! single-issue proposals from a snapshot + policy.
//!
//! Every assertion here binds the proposal surface only — keys,
//! sources, task input, provenance, and typed skip reasons. No live
//! host or worker is exercised; the selector is a pure function and
//! these are contract results, not live evidence.

use std::collections::BTreeSet;

use workloop_core::identity::{
    AdapterId, HostInstanceId, IssueId, IssueRef, RepositoryId, RepositoryRef,
};
use workloop_selector::{
    IssueFact, IssueState, Proposal, SelectionPolicy, SelectorSnapshot, SkipReason, SkippedIssue,
    propose,
};

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo(id: &str) -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted(id))
}

fn issue(repo_id: &str, issue_id: &str) -> IssueRef {
    IssueRef {
        repository: repo(repo_id),
        issue: IssueId::trusted(issue_id),
        display_number: None,
    }
}

fn fact(issue: IssueRef, revision: &str, digest: &str, title: &str) -> IssueFact {
    IssueFact {
        issue,
        state: IssueState::Open,
        revision: revision.into(),
        content_digest: digest.into(),
        title: title.into(),
    }
}

fn policy(repos: &[&str]) -> SelectionPolicy {
    SelectionPolicy {
        revision: 7,
        allowlist: repos.iter().map(|r| repo(r)).collect(),
        excluded: BTreeSet::new(),
        max_proposals: 10,
        task_constraints: BTreeSet::from(["keep the diff minimal".to_string()]),
        verification_argv: vec!["cargo".into(), "test".into()],
        required_permissions: BTreeSet::from(["workspace_write".to_string()]),
    }
}

fn selector() -> AdapterId {
    AdapterId::trusted("reference-selector")
}

fn keys(proposals: &[Proposal]) -> Vec<&str> {
    proposals.iter().map(|p| p.key.as_str()).collect()
}

/// The same snapshot + policy produce the same proposal identity on
/// every run — including after a serde round-trip, so identity does
/// not depend on process state or representation quirks.
#[test]
fn same_snapshot_and_policy_yield_the_same_proposal_identity() {
    let snapshot = SelectorSnapshot {
        issues: vec![
            fact(issue("repo-a", "i1"), "rev-1", "digest-1", "fix the parser"),
            fact(
                issue("repo-a", "i2"),
                "rev-2",
                "digest-2",
                "add retry bound",
            ),
        ],
        cursor: Some("cursor-9".into()),
        fetched_unix_seconds: Some(1_700_000_000),
    };
    let policy = policy(&["repo-a"]);

    let first = propose(&selector(), &snapshot, &policy);
    let second = propose(&selector(), &snapshot, &policy);
    assert_eq!(first, second, "a pure run is reproducible");
    assert_eq!(first.proposals.len(), 2);

    // Identity survives representation: serialize and re-read the same
    // inputs, then re-run — the keys must not move.
    let snapshot: SelectorSnapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
    let policy: SelectionPolicy =
        serde_json::from_str(&serde_json::to_string(&policy).unwrap()).unwrap();
    let third = propose(&selector(), &snapshot, &policy);
    assert_eq!(keys(&first.proposals), keys(&third.proposals));
}

/// Reordering the snapshot's fact list changes neither the proposal
/// identities nor the deterministic proposal order.
#[test]
fn input_order_never_changes_proposal_identity_or_order() {
    let i1 = issue("repo-a", "i1");
    let i2 = issue("repo-a", "i2");
    let i3 = issue("repo-b", "i9");
    let forward = SelectorSnapshot {
        issues: vec![
            fact(i1.clone(), "r1", "d1", "first"),
            fact(i2.clone(), "r2", "d2", "second"),
            fact(i3.clone(), "r3", "d3", "third"),
        ],
        ..SelectorSnapshot::default()
    };
    let reversed = SelectorSnapshot {
        issues: vec![
            fact(i3, "r3", "d3", "third"),
            fact(i2, "r2", "d2", "second"),
            fact(i1, "r1", "d1", "first"),
        ],
        ..SelectorSnapshot::default()
    };
    let policy = policy(&["repo-a", "repo-b"]);
    let a = propose(&selector(), &forward, &policy);
    let b = propose(&selector(), &reversed, &policy);
    assert_eq!(
        a.proposals, b.proposals,
        "proposal order is sorted IssueRef order, not input order"
    );
}

/// New Issue content — a new content digest — produces a new proposal
/// identity; the old decision is not replayed for changed work.
#[test]
fn new_issue_content_yields_a_new_proposal_identity() {
    let base = SelectorSnapshot {
        issues: vec![fact(issue("repo-a", "i1"), "rev-1", "digest-1", "t")],
        ..SelectorSnapshot::default()
    };
    let edited = SelectorSnapshot {
        issues: vec![fact(issue("repo-a", "i1"), "rev-1", "digest-2", "t")],
        ..SelectorSnapshot::default()
    };
    let pol = policy(&["repo-a"]);
    let a = propose(&selector(), &base, &pol).proposals[0].key.clone();
    let b = propose(&selector(), &edited, &pol).proposals[0].key.clone();
    assert_ne!(a, b, "changed content must mint a new identity");

    // A policy change likewise mints new identities.
    let mut new_policy = policy(&["repo-a"]);
    new_policy.revision += 1;
    let c = propose(&selector(), &base, &new_policy).proposals[0]
        .key
        .clone();
    assert_ne!(a, c, "a different policy revision re-identifies proposals");
}

/// Bookkeeping — snapshot cursor, fetch timestamp, run timestamps, and
/// display numbers — never enters proposal identity.
#[test]
fn bookkeeping_never_enters_the_key() {
    let i = issue("repo-a", "i1");
    let mut with_number = i.clone();
    with_number.display_number = Some(42);
    let snapshot_a = SelectorSnapshot {
        issues: vec![fact(i, "rev-1", "digest-1", "t")],
        cursor: Some("cursor-a".into()),
        fetched_unix_seconds: Some(1_700_000_000),
    };
    let snapshot_b = SelectorSnapshot {
        issues: vec![fact(with_number, "rev-1", "digest-1", "t")],
        cursor: Some("cursor-b".into()),
        fetched_unix_seconds: Some(1_700_009_999),
    };
    let policy = policy(&["repo-a"]);
    let a = &propose(&selector(), &snapshot_a, &policy).proposals[0];
    let b = &propose(&selector(), &snapshot_b, &policy).proposals[0];
    assert_eq!(
        a.key, b.key,
        "bookkeeping fields are excluded from identity"
    );
    // …but they still flow into provenance for audit.
    assert_eq!(b.provenance.snapshot_cursor.as_deref(), Some("cursor-b"));
    assert_eq!(b.provenance.fetched_unix_seconds, Some(1_700_009_999));
}

/// Sources outside the configured allowlist are refused with a typed
/// reason — never absorbed into a proposal.
#[test]
fn non_allowlisted_source_is_refused() {
    let snapshot = SelectorSnapshot {
        issues: vec![
            fact(issue("repo-a", "i1"), "r1", "d1", "in scope"),
            fact(issue("repo-x", "i7"), "r7", "d7", "out of scope"),
        ],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &snapshot, &policy(&["repo-a"]));
    assert_eq!(out.proposals.len(), 1);
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].issue, issue("repo-x", "i7"));
    assert_eq!(out.skipped[0].reason, SkipReason::NotAllowlisted);
}

/// An excluded issue is refused even though it is otherwise eligible.
#[test]
fn excluded_issue_is_refused() {
    let excluded_issue = issue("repo-a", "i2");
    let mut policy = policy(&["repo-a"]);
    policy.excluded.insert(excluded_issue.clone());
    let snapshot = SelectorSnapshot {
        issues: vec![
            fact(issue("repo-a", "i1"), "r1", "d1", "keep"),
            fact(excluded_issue, "r2", "d2", "skip me"),
        ],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &snapshot, &policy);
    assert_eq!(out.proposals.len(), 1);
    assert_eq!(out.skipped[0].reason, SkipReason::Excluded);
}

/// A closed issue is not proposed.
#[test]
fn closed_issue_is_not_proposed() {
    let mut f = fact(issue("repo-a", "i1"), "r1", "d1", "done");
    f.state = IssueState::Closed;
    let snapshot = SelectorSnapshot {
        issues: vec![f],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &snapshot, &policy(&["repo-a"]));
    assert!(out.proposals.is_empty());
    assert_eq!(out.skipped[0].reason, SkipReason::NotOpen);
}

/// Identical duplicate facts collapse into one proposal; two facts
/// claiming one Issue identity with different content conflict and are
/// refused — never silently picked between.
#[test]
fn duplicates_collapse_and_conflicting_duplicates_are_refused() {
    let i = issue("repo-a", "i1");
    let identical = SelectorSnapshot {
        issues: vec![
            fact(i.clone(), "r1", "d1", "t"),
            fact(i.clone(), "r1", "d1", "t"),
        ],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &identical, &policy(&["repo-a"]));
    assert_eq!(out.proposals.len(), 1, "a repeated fact is one candidate");
    assert!(out.skipped.is_empty());

    let conflicting = SelectorSnapshot {
        issues: vec![
            fact(i.clone(), "r1", "d1", "t"),
            fact(i.clone(), "r1", "d2", "t"),
        ],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &conflicting, &policy(&["repo-a"]));
    assert!(out.proposals.is_empty());
    assert_eq!(out.skipped[0].reason, SkipReason::ConflictingFacts);
}

/// A third fact matching an earlier one must not re-admit an Issue
/// already recorded as conflicting — the conflict is a property of the
/// snapshot read, not of pairwise comparison order.
#[test]
fn a_later_matching_fact_never_re_admits_a_conflicted_issue() {
    let i = issue("repo-a", "i1");
    let snapshot = SelectorSnapshot {
        issues: vec![
            fact(i.clone(), "r1", "d1", "t"),
            fact(i.clone(), "r1", "d2", "t"),
            fact(i.clone(), "r1", "d1", "t"),
        ],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &snapshot, &policy(&["repo-a"]));
    assert!(
        out.proposals.is_empty(),
        "a conflicted issue is ineligible no matter which later fact matches: {out:?}"
    );
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].reason, SkipReason::ConflictingFacts);
}

/// The proposal bound truncates deterministically: the lowest-sorted
/// eligible issues win, input order cannot change the cut, and every
/// eligible issue past the bound is reported as skipped — distinguishable
/// by typed reason from an issue absent from the snapshot.
#[test]
fn the_bound_truncates_deterministically() {
    let snapshot = SelectorSnapshot {
        issues: vec![
            fact(issue("repo-a", "i3"), "r", "d", "c"),
            fact(issue("repo-a", "i1"), "r", "d", "a"),
            fact(issue("repo-a", "i2"), "r", "d", "b"),
            fact(issue("repo-x", "i7"), "r", "d", "out of scope"),
        ],
        ..SelectorSnapshot::default()
    };
    let mut policy = policy(&["repo-a"]);
    policy.max_proposals = 2;
    let out = propose(&selector(), &snapshot, &policy);
    let proposed: Vec<&IssueRef> = out.proposals.iter().map(|p| &p.sources[0].issue).collect();
    assert_eq!(
        proposed,
        vec![&issue("repo-a", "i1"), &issue("repo-a", "i2")],
        "the bound keeps the sorted head, not the input head"
    );
    assert_eq!(
        out.skipped,
        vec![
            SkippedIssue {
                issue: issue("repo-a", "i3"),
                reason: SkipReason::TruncatedByBound,
            },
            SkippedIssue {
                issue: issue("repo-x", "i7"),
                reason: SkipReason::NotAllowlisted,
            },
        ],
        "a bound-dropped eligible issue is typed data, not silence, and \
         keeps its reason distinct from a policy refusal"
    );
    assert_eq!(
        out.proposals.len() + out.skipped.len(),
        4,
        "every distinct snapshot issue accounts for exactly one outcome"
    );
}

/// A proposal carries the exact source read it was built from — issue
/// identity, revision, and content digest — so a stale source can be
/// detected at admission without re-running selection.
#[test]
fn proposals_bind_their_source_read() {
    let snapshot = SelectorSnapshot {
        issues: vec![fact(issue("repo-a", "i1"), "rev-9", "digest-9", "t")],
        ..SelectorSnapshot::default()
    };
    let out = propose(&selector(), &snapshot, &policy(&["repo-a"]));
    let source = &out.proposals[0].sources[0];
    assert_eq!(source.issue, issue("repo-a", "i1"));
    assert_eq!(source.revision, "rev-9");
    assert_eq!(source.content_digest, "digest-9");
    assert_eq!(out.proposals[0].policy_revision, 7);
    assert_eq!(out.proposals[0].selector, selector());
}

/// An empty or all-refused snapshot yields zero proposals — never an
/// error and never a synthesized candidate. Selector outage is simply
/// the absence of proposals; manual `submit` and existing Work hold no
/// dependency on this function (it takes no store, clock, or I/O).
#[test]
fn zero_proposals_is_data_not_a_failure() {
    let policy = policy(&["repo-a"]);
    let out = propose(&selector(), &SelectorSnapshot::default(), &policy);
    assert!(out.proposals.is_empty());
    assert!(out.skipped.is_empty());
}

/// A proposal is data: serializing and re-reading it preserves the key
/// exactly, so a replayed proposal re-presents the same identity to
/// admission.
#[test]
fn proposal_identity_survives_transport() {
    let snapshot = SelectorSnapshot {
        issues: vec![fact(issue("repo-a", "i1"), "r1", "d1", "t")],
        ..SelectorSnapshot::default()
    };
    let p = propose(&selector(), &snapshot, &policy(&["repo-a"])).proposals[0].clone();
    let wire = serde_json::to_string(&p).unwrap();
    let back: Proposal = serde_json::from_str(&wire).unwrap();
    assert_eq!(back.key, p.key);
    assert_eq!(back, p);
}
