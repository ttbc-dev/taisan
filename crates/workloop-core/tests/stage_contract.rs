//! Stage-contract tests: the `converge` stage classifies only on
//! publication evidence, and the executor × reviewer matrix refuses —
//! never silently degrades — at configuration-validation time.

use workloop_core::stage::{
    ConvergeClassification, ConvergeRecord, Executor, MatrixConfigError, Reviewer,
    validate_review_matrix,
};

fn record(head_before: &str, head_after: &str, pushed: Option<&str>) -> ConvergeRecord {
    ConvergeRecord {
        head_before: head_before.into(),
        head_after: head_after.into(),
        pushed_sha: pushed.map(str::to_string),
        replies_posted: vec!["comment-7".into()],
        threads_addressed: vec!["thread-3".into()],
    }
}

#[test]
fn pushed_head_equal_to_live_head_classifies_fixed() {
    let record = record("aaa", "bbb", Some("bbb"));
    assert_eq!(
        record.classify("bbb", false),
        ConvergeClassification::Fixed {
            pushed_sha: "bbb".into()
        }
    );
}

#[test]
fn pushed_head_verified_as_ancestor_of_live_head_classifies_fixed() {
    // The live head moved on after the push; the pushed head is still an
    // ancestor of it, so publication holds.
    let record = record("aaa", "bbb", Some("bbb"));
    assert_eq!(
        record.classify("ccc", true),
        ConvergeClassification::Fixed {
            pushed_sha: "bbb".into()
        }
    );
}

#[test]
fn claimed_head_without_a_push_is_a_failure_that_preserves_evidence() {
    // hermes-setup #445: local commits existed (`head_after` moved) but
    // nothing reached the PR. Never a success — and the whole record
    // survives as evidence. An ancestry flag about a nonexistent push
    // cannot manufacture a success either.
    let record = record("aaa", "bbb", None);
    for pushed_is_ancestor_of_live in [false, true] {
        assert_eq!(
            record.classify("aaa", pushed_is_ancestor_of_live),
            ConvergeClassification::UnpublishedFailure {
                live_head: "aaa".into(),
                record: record.clone(),
            },
            "a fix claim without publication must never classify as fixed"
        );
    }
    // The preserved record keeps the reply/thread evidence, not just the heads.
    let ConvergeClassification::UnpublishedFailure {
        record: preserved, ..
    } = record.classify("aaa", false)
    else {
        panic!("expected an UnpublishedFailure");
    };
    assert_eq!(preserved.replies_posted, vec!["comment-7".to_string()]);
    assert_eq!(preserved.threads_addressed, vec!["thread-3".to_string()]);
}

#[test]
fn pushed_head_that_does_not_reach_the_live_head_is_a_failure() {
    // Diverged from the live head (or pushed somewhere the PR cannot
    // see): publication does not verify, so the claim fails with the
    // record preserved.
    let record = record("aaa", "bbb", Some("bbb"));
    assert_eq!(
        record.classify("ccc", false),
        ConvergeClassification::UnpublishedFailure {
            live_head: "ccc".into(),
            record: record.clone(),
        }
    );
}

#[test]
fn no_change_and_no_push_classifies_nothing_to_fix() {
    let record = record("aaa", "aaa", None);
    assert_eq!(
        record.classify("aaa", false),
        ConvergeClassification::NothingToFix
    );
    // No pushed head exists, so an ancestry claim is irrelevant here too.
    assert_eq!(
        record.classify("bbb", true),
        ConvergeClassification::NothingToFix
    );
}

#[test]
fn converge_record_serializes_with_the_contract_shape() {
    let record = record("aaa", "bbb", Some("bbb"));
    let value = serde_json::to_value(&record).unwrap();
    let mut keys: Vec<String> = value
        .as_object()
        .expect("record serializes as an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "head_after",
            "head_before",
            "pushed_sha",
            "replies_posted",
            "threads_addressed"
        ]
    );
    assert_eq!(value["pushed_sha"], "bbb");
    let round_tripped: ConvergeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(round_tripped, record);

    // A record with no push omits/nulls `pushed_sha` and still parses:
    // absence of publication is representable, not a decode failure.
    let without_push = serde_json::json!({
        "head_before": "aaa",
        "head_after": "aaa",
        "replies_posted": [],
        "threads_addressed": [],
    });
    let parsed: ConvergeRecord = serde_json::from_value(without_push).unwrap();
    assert_eq!(parsed.pushed_sha, None);
}

#[test]
fn every_supported_matrix_cell_preserves_the_reviewer_axis() {
    let cases = [
        ("local", "devin", Executor::Local, Reviewer::Devin),
        ("local", "shadow", Executor::Local, Reviewer::Shadow),
        ("local", "both", Executor::Local, Reviewer::Both),
        ("remote", "devin", Executor::Remote, Reviewer::Devin),
        ("remote", "shadow", Executor::Remote, Reviewer::Shadow),
        ("remote", "both", Executor::Remote, Reviewer::Both),
    ];
    for (executor, reviewer, want_executor, want_reviewer) in cases {
        let validated = validate_review_matrix(executor, reviewer).unwrap_or_else(|e| {
            panic!("{executor} x {reviewer} must be a supported cell, refused: {e:?}")
        });
        assert_eq!(
            validated,
            (want_executor, want_reviewer),
            "the reviewer axis must come back exactly as configured for {executor}"
        );
    }
    // The hermes-setup #446 regression cell: a remote lane's shadow
    // review must survive validation unchanged — never silently
    // downgraded to the Devin path.
    assert_eq!(
        validate_review_matrix("remote", "shadow").unwrap(),
        (Executor::Remote, Reviewer::Shadow)
    );
}

#[test]
fn unknown_executor_is_refused_at_configuration_validation() {
    assert_eq!(
        validate_review_matrix("container", "shadow"),
        Err(MatrixConfigError::UnsupportedExecutor {
            value: "container".into()
        })
    );
}

#[test]
fn unknown_reviewer_is_refused_at_configuration_validation() {
    assert_eq!(
        validate_review_matrix("remote", "manual"),
        Err(MatrixConfigError::UnsupportedReviewer {
            value: "manual".into()
        })
    );
}
