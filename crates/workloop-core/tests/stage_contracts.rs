//! Executor-independence contract coverage (issue #21): converge
//! publication evidence, orthogonal executor×reviewer validation, and the
//! lane-produced exemption from bot filtering.

use workloop_core::ports::{EventProvenance, admit_bot_filtered_event};
use workloop_core::stage::*;

fn record(head_before: &str, head_after: &str, pushed_sha: Option<&str>) -> ConvergeRecord {
    ConvergeRecord {
        head_before: head_before.into(),
        head_after: head_after.into(),
        pushed_sha: pushed_sha.map(str::to_string),
        replies_posted: vec!["reply-1".into()],
        threads_addressed: vec!["thread-9".into()],
    }
}

// ---- INV-002: publication evidence gates the review-fixed verdict ----

#[test]
fn pushed_head_equal_to_live_is_review_fixed() {
    let rec = record("sha-old", "sha-new", Some("sha-new"));
    assert_eq!(
        rec.classify(&"sha-new".to_string(), false),
        ConvergeClassification::ReviewFixed
    );
}

#[test]
fn pushed_head_ancestor_of_live_is_review_fixed() {
    // The PR advanced past the pushed head (e.g. a follow-up commit), but
    // the pushed head is provably on the live head's history.
    let rec = record("sha-old", "sha-new", Some("sha-new"));
    assert_eq!(
        rec.classify(&"sha-newer".to_string(), true),
        ConvergeClassification::ReviewFixed
    );
}

#[test]
fn pushed_head_neither_equal_nor_ancestor_is_publication_unverified() {
    let rec = record("sha-old", "sha-new", Some("sha-new"));
    assert_eq!(
        rec.classify(&"sha-other".to_string(), false),
        ConvergeClassification::PublicationUnverified,
        "a claimed fix whose head never reached the PR is a failure, not a success"
    );
}

#[test]
fn produced_but_unpushed_head_is_publication_unverified() {
    // The hermes-setup#445 defect shape: a worker reported fixed while its
    // commits died with the container — head_after != head_before and
    // nothing was pushed.
    let rec = record("sha-old", "sha-new", None);
    assert_eq!(
        rec.classify(&"sha-old".to_string(), false),
        ConvergeClassification::PublicationUnverified
    );
}

#[test]
fn unchanged_head_without_push_is_nothing_to_fix() {
    let rec = record("sha-old", "sha-old", None);
    assert_eq!(
        rec.classify(&"sha-old".to_string(), false),
        ConvergeClassification::NothingToFix
    );
}

// ---- INV-003/INV-004: orthogonal axes refuse, never degrade ----

#[test]
fn supported_cell_validates_into_a_lane_spec() {
    let matrix = LaneMatrix::new([
        (ExecutorKind::Local, ReviewerKind::Devin),
        (ExecutorKind::Local, ReviewerKind::Shadow),
        (ExecutorKind::Remote, ReviewerKind::Devin),
    ]);
    let spec = matrix
        .validate(ExecutorKind::Local, ReviewerKind::Shadow)
        .expect("declared cell validates");
    assert_eq!(spec.executor(), ExecutorKind::Local);
    assert_eq!(spec.reviewer(), ReviewerKind::Shadow);
}

#[test]
fn unsupported_cell_is_refused_not_degraded() {
    let matrix = LaneMatrix::new([(ExecutorKind::Local, ReviewerKind::Devin)]);
    let err = matrix
        .validate(ExecutorKind::Remote, ReviewerKind::Shadow)
        .expect_err("undeclared cell must be refused at config validation");
    assert_eq!(err.executor, ExecutorKind::Remote);
    assert_eq!(err.reviewer, ReviewerKind::Shadow);
}

#[test]
fn reviewer_axis_is_not_silently_dropped_by_executor() {
    // `Both` is a distinct reviewer value, not a remote-lane degradation:
    // a matrix that never declared it refuses it even where a narrower
    // reviewer on the same executor is supported.
    let matrix = LaneMatrix::new([(ExecutorKind::Remote, ReviewerKind::Devin)]);
    assert!(
        matrix
            .validate(ExecutorKind::Remote, ReviewerKind::Both)
            .is_err()
    );
}

// ---- INV-005: lane-produced events survive bot filtering ----

#[test]
fn lane_managed_bot_authored_event_is_admitted() {
    // The hermes-setup#446 defect shape: a lane-opened PR is authored by
    // the lane's own App — dropping it starves the lane's review loop.
    assert!(admit_bot_filtered_event(EventProvenance::LaneManaged, true));
    assert!(admit_bot_filtered_event(
        EventProvenance::LaneManaged,
        false
    ));
}

#[test]
fn external_bot_event_is_dropped_by_the_filter() {
    assert!(!admit_bot_filtered_event(EventProvenance::External, true));
}

#[test]
fn external_human_event_is_admitted() {
    assert!(admit_bot_filtered_event(EventProvenance::External, false));
}
