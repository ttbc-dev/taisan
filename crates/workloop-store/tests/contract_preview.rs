//! Contract tests for the TUI → `work` `preview` port
//! (docs/module-contracts.md): an advisory preflight over saved facts.
//! It reserves neither capacity nor execution ownership, persists no
//! decision, and never substitutes for the Command-time precondition
//! check — `submit` still rechecks.

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::{Observation, ObservationFact, ReviewVerdict};
use workloop_core::projection::{self, CommandResultLookup, QueryResult};
use workloop_core::store::Store;
use workloop_core::work::{
    PreviewAvailability, WorkloopConfig, handle_command, handle_observation, preview,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn config() -> WorkloopConfig {
    WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("tui".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit_req(key: &str) -> CommandRequest {
    req(
        key,
        CommandBody::Submit {
            intent: "do the thing".into(),
            issue_refs: vec![GitHubIssueRef {
                repository: "local/repo".into(),
                number: 1,
            }],
            constraints: vec![],
            verification_argv: vec![],
        },
    )
}

fn seed_work(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(store, &config(), &submit_req(key), NOW).unwrap();
    match d {
        CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("submit {key}: {other:?}"),
    }
}

fn seed_pr_head(store: &SqliteStore, work_id: &WorkId, sha: &str, observed_at: i64) {
    handle_observation(
        store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::PrHead {
                repository: "local/repo".into(),
                number: 1,
                head_sha: sha.into(),
            },
            observed_unix_seconds: observed_at,
        },
        observed_at,
    )
    .unwrap();
}

fn seed_pr_review(store: &SqliteStore, work_id: &WorkId, sha: &str, observed_at: i64) {
    handle_observation(
        store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::PrReview {
                repository: "local/repo".into(),
                head_sha: sha.into(),
                reviewer: "rev".into(),
                verdict: ReviewVerdict::ChangesRequested,
            },
            observed_unix_seconds: observed_at,
        },
        observed_at,
    )
    .unwrap();
}

fn last_seq(store: &SqliteStore) -> i64 {
    store.transact(|tx| tx.last_seq()).unwrap()
}

fn stored(store: &SqliteStore, origin: &Origin, key: &str) -> Option<CommandDecision> {
    match store
        .transact(|tx| {
            projection::command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: origin.clone(),
                    actor: actor(),
                    idempotency_key: key.into(),
                },
            )
        })
        .unwrap()
    {
        QueryResult::Found { value, .. } => Some(value),
        _ => None,
    }
}

/// INV-001: preview is strictly read-only — no decision, no event, no
/// allocation, no reservation survives it.
#[test]
fn preview_persists_nothing() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e1) = seed_work(&store, "p-seed");
    let before_seq = last_seq(&store);
    let before_works = store.transact(|tx| tx.works()).unwrap().len();

    // Preview a fresh submit — the decision pass allocates w/e/cmd ids
    // and a sequence; none may leak.
    let p = preview(&store, &config(), &submit_req("p-sub"), NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    // Preview an acceptable cancel and an unacceptable retry.
    let cancel = req(
        "p-cancel",
        CommandBody::Cancel {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: None,
        },
    );
    let p = preview(&store, &config(), &cancel, NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    assert_eq!(p.observed_work_version, Some(1));
    let retry = req(
        "p-retry",
        CommandBody::Retry {
            work_id: work_id.clone(),
            expected_work_version: 1,
            expected_execution_id: ExecutionId("e-99".into()),
        },
    );
    let p = preview(&store, &config(), &retry, NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Blocked);
    assert_eq!(p.reasons, vec![RejectionCode::Ineligible]);

    assert_eq!(last_seq(&store), before_seq, "preview wrote to event_log");
    assert_eq!(
        store.transact(|tx| tx.works()).unwrap().len(),
        before_works,
        "preview created a Work"
    );
    for key in ["p-sub", "p-cancel", "p-retry"] {
        assert!(
            stored(&store, &Origin("tui".into()), key).is_none(),
            "preview persisted a decision under {key}"
        );
    }

    // The next real submit still gets the first free ids — preview burned
    // none.
    let d = handle_command(&store, &config(), &submit_req("p-real"), NOW).unwrap();
    match d {
        CommandDecision::Accepted { work_id, .. } => {
            assert_eq!(work_id.unwrap().0, "w-2");
        }
        other => panic!("{other:?}"),
    }
}

/// INV-011: availability mirrors the Command outcome — accepted is
/// available, evidence-insufficient rejections are unknown, everything
/// else is blocked, and a stored key reports what a submit would return.
#[test]
fn preview_mirrors_decision_outcome() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = seed_work(&store, "pm-seed");

    // Stale version → blocked with the rejection's code and the observed
    // current version for display.
    let p = preview(
        &store,
        &config(),
        &req(
            "pm-stale",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 99,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(p.availability, PreviewAvailability::Blocked);
    assert_eq!(p.reasons, vec![RejectionCode::StaleWorkVersion]);
    assert_eq!(p.observed_work_version, Some(1));

    // A judgment with no observed head cannot be evaluated → unknown.
    let p = preview(
        &store,
        &config(),
        &req(
            "pm-judge",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_pr_head: "h1".into(),
                decisions: vec![FindingDecision {
                    finding_id: "f1".into(),
                    choice: FindingChoice::Fix,
                }],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(p.availability, PreviewAvailability::Unknown);
    assert_eq!(p.reasons, vec![RejectionCode::SourceUnavailable]);
    assert_eq!(p.observed_pr_head, None);

    // A stored accepted decision previews as available under the same
    // key and payload; a changed payload previews idempotency_conflict.
    let stored_req = req(
        "pm-key",
        CommandBody::Cancel {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: None,
        },
    );
    let d = handle_command(&store, &config(), &stored_req, NOW).unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }));
    let p = preview(&store, &config(), &stored_req, NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    let mutated = req(
        "pm-key",
        CommandBody::Cancel {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: Some(ExecutionId("e-1".into())),
        },
    );
    let p = preview(&store, &config(), &mutated, NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Blocked);
    assert_eq!(p.reasons, vec![RejectionCode::IdempotencyConflict]);
}

/// The observed head surfaces for display: a judge preview carries the
/// persisted head the Command would be checked against.
#[test]
fn preview_reports_observed_head() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = seed_work(&store, "ph-seed");
    seed_pr_head(&store, &work_id, "h9", NOW);
    seed_pr_review(&store, &work_id, "h9", NOW);
    let p = preview(
        &store,
        &config(),
        &req(
            "ph-judge",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_pr_head: "h9".into(),
                decisions: vec![FindingDecision {
                    finding_id: "f1".into(),
                    choice: FindingChoice::Dismiss,
                }],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    assert_eq!(p.observed_pr_head.as_deref(), Some("h9"));
}

/// The advisory surface never replaces the acceptance-time check: a
/// preview that was available does not weaken the later real decision.
#[test]
fn preview_never_substitutes_for_submit() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = seed_work(&store, "ps-seed");
    let request = req(
        "ps-cancel",
        CommandBody::Cancel {
            work_id: work_id.clone(),
            expected_work_version: 1,
            execution_id: None,
        },
    );
    let p = preview(&store, &config(), &request, NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    // The world moved: another adapter bumped the version.
    let _ = handle_command(
        &store,
        &config(),
        &req(
            "ps-ack-setup",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    // Rejected at submit despite the earlier available preview — the
    // expected version is now stale and the handler rechecked.
    let d = handle_command(&store, &config(), &request, NOW).unwrap();
    match d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::StaleWorkVersion)
        }
        other => panic!("expected stale rejection, got {other:?}"),
    }
}

/// INV-011: preview runs the apply pass too — a Command whose effects
/// violate a store constraint previews blocked with the same code the
/// submit rejects with, never `available`. Re-judging a head that
/// already has a judgment passes `decide` but fails the `judgments`
/// uniqueness constraint at apply time.
#[test]
fn preview_mirrors_apply_time_rejection() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = seed_work(&store, "pj-seed");
    seed_pr_head(&store, &work_id, "h9", NOW);
    seed_pr_review(&store, &work_id, "h9", NOW);

    let judge = CommandBody::JudgeFindings {
        work_id: work_id.clone(),
        expected_work_version: 1,
        expected_pr_head: "h9".into(),
        decisions: vec![FindingDecision {
            finding_id: "f1".into(),
            choice: FindingChoice::Fix,
        }],
    };
    let d = handle_command(&store, &config(), &req("pj-first", judge), NOW).unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    // Re-judging the same head: decide passes (head and version match),
    // but the judgments uniqueness constraint rejects it at apply time.
    let rejudge = req(
        "pj-again",
        CommandBody::JudgeFindings {
            work_id: work_id.clone(),
            expected_work_version: 2,
            expected_pr_head: "h9".into(),
            decisions: vec![FindingDecision {
                finding_id: "f1".into(),
                choice: FindingChoice::Dismiss,
            }],
        },
    );

    // Preview first — the fresh decide+apply pass must predict the
    // apply-time rejection, not `available`.
    let p = preview(&store, &config(), &rejudge, NOW).unwrap();
    assert_eq!(
        p.availability,
        PreviewAvailability::Blocked,
        "preview must mirror the apply-time rejection"
    );
    assert_eq!(p.reasons, vec![RejectionCode::Ineligible]);
    // And the preview persisted nothing for it.
    assert!(stored(&store, &Origin("tui".into()), "pj-again").is_none());

    // The real submit agrees with the prediction.
    let d = handle_command(&store, &config(), &rejudge, NOW).unwrap();
    match d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(reason, RejectionCode::Ineligible)
        }
        other => panic!("expected the re-judgment rejected, got {other:?}"),
    }
}
