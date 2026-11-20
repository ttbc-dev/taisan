//! Contract tests for the TUI → `work` `preview` port
//! (docs/module-contracts.md): an advisory preflight over saved facts.
//! It reserves neither capacity nor execution ownership, persists no
//! decision, and never substitutes for the Command-time precondition
//! check — `submit` still rechecks.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::HostActionStatus;
use workloop_core::observation::{Completeness, Observation, ObservationFact, OccurrenceInput};
use workloop_core::projection::{self, CommandResultLookup, QueryResult};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{
    HostTemplate, PreviewAvailability, WorkloopConfig, handle_command, handle_observation, preview,
};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn rev(object: &str) -> CommitRevision {
    CommitRevision::new(repo(), object).unwrap()
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn target(object: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(object),
        base: None,
        target_revision: None,
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: host(),
                    actor: HostActorId::trusted("pub-agent"),
                },
                credential_ref: CredentialRef::trusted("cred-publication"),
            }],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
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
            sources: vec![],
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

fn emit(store: &SqliteStore, work_id: &WorkId, fact: ObservationFact, at: i64) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact,
            observed_unix_seconds: at,
        },
        at,
    )
    .unwrap();
}

/// The applied intent the open receipt reconciles against — only a
/// receipt naming a recorded applied intent validates the opened change.
fn open_intent(store: &SqliteStore, work_id: &WorkId) {
    store
        .transact(|tx| {
            if tx
                .action_intents()?
                .iter()
                .any(|i| i.action_id == "act-open")
            {
                return Ok(());
            }
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: "act-open".into(),
                    work_id: work_id.clone(),
                    host: host(),
                    repository: repo(),
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: host(),
                        actor: HostActorId::trusted("pub-agent"),
                    },
                    action: HostAction::OpenChangeRequest {
                        source_branch: format!("work/{}", work_id.0),
                        target_branch: "main".into(),
                        title: "seed".into(),
                        body: String::new(),
                    },
                    payload_digest: "test".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })
        })
        .unwrap();
}

/// The change request the Work opened plus its observed head — the two
/// facts `current_target` derives the judgeable diff target from.
fn open_change(store: &SqliteStore, work_id: &WorkId, object: &str, at: i64) {
    open_intent(store, work_id);
    emit(
        store,
        work_id,
        ObservationFact::HostActionResult {
            action_id: "act-open".into(),
            authenticated_actor: None,
            status: workloop_core::observation::HostActionStatus::Applied,
            effects: vec![
                workloop_core::observation::NormalizedEffectFact::ChangeOpened {
                    change: change(),
                    head: rev(object),
                    source_branch: format!("work/{}", work_id.0),
                    target_branch: "main".into(),
                },
            ],
        },
        at,
    );
    emit(
        store,
        work_id,
        ObservationFact::ChangeHead {
            change: change(),
            head: rev(object),
        },
        at,
    );
}

/// A complete review over `source_keys` on `object`'s target — seals an
/// immutable round and bumps the Work's version.
fn seal_review(store: &SqliteStore, work_id: &WorkId, object: &str, source_keys: &[&str], at: i64) {
    emit(
        store,
        work_id,
        ObservationFact::Review {
            target: target(object),
            source: "reviewer".into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: source_keys
                .iter()
                .map(|key| OccurrenceInput {
                    source_key: (*key).into(),
                    content_digest: None,
                    severity: None,
                    position: None,
                    thread_id: None,
                    summary: None,
                })
                .collect(),
        },
        at,
    );
}

/// The occurrence ids a `JudgeFindings` must cover exactly.
fn occurrence_ids(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| {
            let round = tx.latest_round(work_id)?.expect("a sealed round");
            Ok(tx
                .occurrences(&round.id)?
                .into_iter()
                .map(|o| o.id)
                .collect::<Vec<_>>())
        })
        .unwrap()
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

    // A judgment with no observed change target cannot be evaluated →
    // unknown.
    let p = preview(
        &store,
        &config(),
        &req(
            "pm-judge",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 1,
                expected_head: rev("h1"),
                decisions: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(p.availability, PreviewAvailability::Unknown);
    assert_eq!(p.reasons, vec![RejectionCode::SourceUnavailable]);
    assert_eq!(p.observed_head, None);

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
    open_change(&store, &work_id, "h9", NOW);
    seal_review(&store, &work_id, "h9", &["f1"], NOW);
    let ids = occurrence_ids(&store, &work_id);
    let p = preview(
        &store,
        &config(),
        &req(
            "ph-judge",
            CommandBody::JudgeFindings {
                work_id: work_id.clone(),
                expected_work_version: 2,
                expected_head: rev("h9"),
                decisions: ids
                    .iter()
                    .map(|occurrence_id| FindingDecision {
                        occurrence_id: occurrence_id.clone(),
                        choice: FindingChoice::Dismiss,
                        rationale: Some("not a defect".into()),
                    })
                    .collect(),
            },
        ),
        NOW,
    )
    .unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    assert_eq!(p.observed_head, Some(rev("h9")));
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
/// submit rejects with, never `available`. Re-judging a round that
/// already has a judgment passes `decide` but fails the `judgments`
/// uniqueness constraint at apply time.
#[test]
fn preview_mirrors_apply_time_rejection() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = seed_work(&store, "pj-seed");
    open_change(&store, &work_id, "h9", NOW);
    seal_review(&store, &work_id, "h9", &["f1"], NOW);
    let ids = occurrence_ids(&store, &work_id);
    let decisions = |choice: FindingChoice| {
        ids.iter()
            .map(|occurrence_id| FindingDecision {
                occurrence_id: occurrence_id.clone(),
                choice: choice.clone(),
                rationale: Some("rationale".into()),
            })
            .collect::<Vec<_>>()
    };

    let judge = CommandBody::JudgeFindings {
        work_id: work_id.clone(),
        expected_work_version: 2,
        expected_head: rev("h9"),
        decisions: decisions(FindingChoice::Fix),
    };
    let d = handle_command(&store, &config(), &req("pj-first", judge), NOW).unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    // Re-judging the same round: decide passes (head and version match),
    // but the round_judgments uniqueness constraint rejects it at apply
    // time.
    let rejudge = req(
        "pj-again",
        CommandBody::JudgeFindings {
            work_id: work_id.clone(),
            expected_work_version: 3,
            expected_head: rev("h9"),
            decisions: decisions(FindingChoice::Dismiss),
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
