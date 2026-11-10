//! TUI surface end-to-end: a scripted operator session drives the real
//! TuiSession against a real SqliteStore + workloop_core::work, covering
//! submit → observe → open → judgment → stale → release and the
//! pane-detail/pick-number contract.
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use workloop_core::command::{CommandDecision, CommandRequest, FindingChoice, RejectionCode};
use workloop_core::domain::{
    Actor, ActorKind, ExecutionBudget, LanePolicy, Origin, WorkId, WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, ChangeRequestId, ChangeRequestRef, CommitRevision, CredentialRef,
    DiffTarget, HostActorId, HostActorRef, HostCapability, HostInstanceId, RepositoryId,
    RepositoryRef, RoleBinding,
};
use workloop_core::observation::{
    Completeness, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
    OccurrenceInput,
};
use workloop_core::projection::FindingView;
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{self, HostTemplate, WorkloopConfig};
use workloop_store::sqlite::SqliteStore;

use workloop_tui::render;
use workloop_tui::session::{Action, SubmitOutcome, TuiSession};

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tuisurf-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const NOW: i64 = 1_700_000_000;

fn operator() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "tui-op".into(),
    }
}

fn host_instance() -> HostInstanceId {
    HostInstanceId::trusted("host-local")
}

fn repository() -> RepositoryRef {
    RepositoryRef::new(host_instance(), RepositoryId::trusted("local-repo"))
}

fn change() -> ChangeRequestRef {
    ChangeRequestRef {
        repository: repository(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    }
}

fn rev(sha: &str) -> CommitRevision {
    CommitRevision::new(repository(), sha).unwrap()
}

fn target(sha: &str) -> DiffTarget {
    DiffTarget {
        change: change(),
        head: rev(sha),
        base: None,
        target_revision: None,
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repository(),
            contract_version: 1,
            actors: vec![
                RoleBinding {
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: host_instance(),
                        actor: HostActorId::trusted("pub-agent"),
                    },
                    credential_ref: CredentialRef::trusted("cred-publication"),
                },
                RoleBinding {
                    role: ActorRole::Review,
                    actor: HostActorRef {
                        host: host_instance(),
                        actor: HostActorId::trusted("review-bot"),
                    },
                    credential_ref: CredentialRef::trusted("cred-review"),
                },
            ],
        },
        declared_capabilities: BTreeSet::from([
            HostCapability::RepositoryRead,
            HostCapability::PushRef,
            HostCapability::ChangeRequestCreate,
            HostCapability::ChangeRequestRead,
            HostCapability::ChecksRead,
            HostCapability::ReviewFindingsRead,
            HostCapability::IssueRead,
        ]),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: workloop_core::domain::AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

fn submit_work(store: &SqliteStore) -> WorkId {
    let req = CommandRequest {
        actor: operator(),
        origin: Origin("test".into()),
        idempotency_key: format!("key-{}", workloop_core::domain::new_id("t")),
        body: workloop_core::command::CommandBody::Submit {
            intent: "demo task".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let decision = work::handle_command(store, &config(), &req, NOW).unwrap();
    match decision {
        CommandDecision::Accepted { work_id, .. } => work_id.unwrap(),
        other => panic!("submit rejected: {other:?}"),
    }
}

fn observe(store: &SqliteStore, work_id: &WorkId, fact: ObservationFact, at: i64) {
    let obs = Observation {
        delivery: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: None,
        fact,
        observed_unix_seconds: at,
    };
    work::handle_observation(store, &config(), &obs, at).unwrap();
}

/// Publish the change request and record its head — the applied intent,
/// the outbox receipt, and the head observation together establish the
/// diff target.
fn open_change_at(store: &SqliteStore, work_id: &WorkId, sha: &str, at: i64) {
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
                    host: host_instance(),
                    repository: repository(),
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: host_instance(),
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
    observe(
        store,
        work_id,
        ObservationFact::HostActionResult {
            action_id: "act-open".into(),
            authenticated_actor: None,
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change(),
                head: rev(sha),
                source_branch: format!("work/{}", work_id.0),
                target_branch: "main".into(),
            }],
        },
        at,
    );
    observe(
        store,
        work_id,
        ObservationFact::ChangeHead {
            change: change(),
            head: rev(sha),
        },
        at,
    );
}

/// One complete review delivery seals an immutable round over the exact
/// diff target; each entry is one occurrence the judgment must decide.
fn seal_review(
    store: &SqliteStore,
    work_id: &WorkId,
    sha: &str,
    source: &str,
    source_keys: &[&str],
    at: i64,
) {
    observe(
        store,
        work_id,
        ObservationFact::Review {
            target: target(sha),
            source: source.into(),
            source_revision: None,
            completeness: Completeness::Complete,
            occurrences: source_keys
                .iter()
                .map(|k| OccurrenceInput {
                    source_key: k.to_string(),
                    content_digest: None,
                    severity: None,
                    position: None,
                    thread_id: None,
                })
                .collect(),
        },
        at,
    );
}

/// The standard fixture: a published change at `sha` and one sealed
/// round with one occurrence per source key.
fn review_fixture(store: &SqliteStore, work_id: &WorkId, sha: &str, source_keys: &[&str]) {
    open_change_at(store, work_id, sha, NOW);
    seal_review(store, work_id, sha, "reviewer", source_keys, NOW);
}

#[test]
fn board_open_judge_choose_stage_submit_release() {
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    let mut s = TuiSession::new(&store, config(), operator());

    // Board shows the new Work.
    s.refresh(NOW).unwrap();
    let frame = render::board_frame(s.board().unwrap(), None);
    assert!(frame.contains(&work_id.0), "board: {frame}");

    // Observations publish the change and seal a two-occurrence round.
    review_fixture(&store, &work_id, "h1", &["a", "b"]);

    // Open the work → detail pane shows the findings.
    s.open(&work_id, NOW).unwrap();
    let detail = render::detail_frame(s.detail().unwrap());
    assert!(detail.contains("finding 1: a"), "detail: {detail}");
    assert!(detail.contains("finding 2: b"), "detail: {detail}");

    // Judgment flow: decide every displayed occurrence.
    let draft = s.begin_judgment().unwrap();
    let ids: Vec<String> = draft
        .findings
        .iter()
        .map(|f| f.occurrence_id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    s.choose(&ids[0], FindingChoice::Fix, None).unwrap();
    s.choose(
        &ids[1],
        FindingChoice::Dismiss,
        Some("not-reproducible".into()),
    )
    .unwrap();
    s.stage_judgment().unwrap();
    let outcome = s.submit(NOW).unwrap();
    assert!(
        matches!(
            outcome,
            SubmitOutcome::Decided(CommandDecision::Accepted { .. })
        ),
        "submit: {outcome:?}"
    );

    // The durable judgment exists; releasing it is a valid Command form
    // (the Execution is not paused, so an ineligible rejection is fine).
    let s2 = &mut s;
    let req = s2.stage_action(&Action::ReleaseJudgment).unwrap();
    assert!(matches!(
        req.body,
        workloop_core::command::CommandBody::ReleaseJudgment { .. }
    ));
    let outcome = s2.submit(NOW).unwrap();
    match outcome {
        SubmitOutcome::Decided(CommandDecision::Accepted { .. }) => {}
        SubmitOutcome::Decided(CommandDecision::Rejected {
            reason: RejectionCode::Ineligible,
            ..
        }) => {}
        other => panic!("release submit: {other:?}"),
    }
}

#[test]
fn detail_frame_numbers_match_draft_slots() {
    // AC: "a pick number always selects the occurrence shown at that
    // position" — the detail pane numbers the same list the draft's
    // slots hold, in the same order.
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    // Two occurrences share a source key: identity is the occurrence
    // id, never the display text — three rows, three distinct slots.
    review_fixture(&store, &work_id, "h1", &["a", "a", "rev2"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    let frame = render::detail_frame(s.detail().unwrap());
    let draft = s.begin_judgment().unwrap().clone();
    assert_eq!(draft.findings.len(), 3);
    for (i, slot) in draft.findings.iter().enumerate() {
        assert!(
            frame.contains(&format!("finding {}: {}", i + 1, slot.source_key)),
            "slot {} missing from frame: {frame}",
            i + 1
        );
    }
    // pick 2 selects the second slot — same source key as slot 1, but a
    // distinct occurrence id, so the choice lands on occurrence 2 only.
    let second = draft.findings[1].occurrence_id.clone();
    s.choose(&second, FindingChoice::Fix, None).unwrap();
    let draft = s.draft().unwrap();
    assert!(draft.findings[0].choice.is_none());
    assert_eq!(draft.findings[1].choice, Some(FindingChoice::Fix));
    assert!(draft.findings[2].choice.is_none());
}

#[test]
fn stale_head_rejection_requeries_and_clears_choices() {
    // AC: "a stale-head rejection re-queries the changed head" — stage a
    // judgment on h-old, move the head to h-new, submit → StaleHead →
    // choices cleared and the draft re-read at h-new.
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h-old", &["a"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let occ = s.draft().unwrap().findings[0].occurrence_id.clone();
    s.choose(&occ, FindingChoice::Fix, None).unwrap();
    s.stage_judgment().unwrap();

    // Head moves under the operator; no review for the new head has
    // been delivered, so the staged judgment rejects as StaleHead.
    observe(
        &store,
        &work_id,
        ObservationFact::ChangeHead {
            change: change(),
            head: rev("h-new"),
        },
        NOW + 10,
    );

    let outcome = s.submit(NOW + 11).unwrap();
    let SubmitOutcome::Decided(CommandDecision::Rejected { reason, .. }) = outcome else {
        panic!("expected rejection, got {outcome:?}");
    };
    assert_eq!(reason, RejectionCode::StaleHead);
    // Re-queried: the new head has no sealed round, so there is nothing
    // to decide — the draft and staged request are cleared, never
    // retargeted onto the new head.
    assert!(s.draft().is_none());
    assert!(s.staged().is_none());
    // The detail pane shows the current head.
    let detail = render::detail_frame(s.detail().unwrap());
    assert!(detail.contains("h-new"), "detail: {detail}");
}

#[test]
fn new_round_on_same_head_invalidates_draft_and_staged() {
    // AC: "new findings on the same head invalidate a draft or armed
    // request built on the smaller set" — a new sealed round on the same
    // head replaces the occurrence set; the draft and staged request
    // built on the old set must be discarded.
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h1", &["a"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let occ = s.draft().unwrap().findings[0].occurrence_id.clone();
    s.choose(&occ, FindingChoice::Fix, None).unwrap();
    s.stage_judgment().unwrap();

    // A second complete review on the same head seals a new round — the
    // occurrence ids change, and the Work version bumps.
    seal_review(&store, &work_id, "h1", "reviewer", &["a", "c"], NOW + 5);

    // Re-query (poll tick / next open) sees the new detail.
    s.open(&work_id, NOW + 6).unwrap();
    assert!(s.draft().is_none(), "draft must be discarded");
    assert!(s.staged().is_none(), "staged request must be discarded");
    // Re-building a draft covers the new occurrence set.
    let draft = s.begin_judgment().unwrap();
    let keys: Vec<&str> = draft
        .findings
        .iter()
        .map(|f| f.source_key.as_str())
        .collect();
    assert_eq!(keys, vec!["a", "c"]);
}

#[test]
fn empty_findings_under_unread_source_is_not_clean() {
    // AC: "an empty finding list under a not-known review source proves
    // nothing" — a published change with no sealed round surfaces the
    // review source as unavailable; judgment refuses instead of
    // treating zero findings as a clean review.
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    open_change_at(&store, &work_id, "h1", NOW);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    let err = s.begin_judgment().unwrap_err();
    assert!(err.contains("review source"), "judge error: {err}");
}

#[test]
fn choose_rejects_unknown_occurrence_id() {
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h1", &["a"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let err = s
        .choose("occ-not-displayed", FindingChoice::Fix, None)
        .unwrap_err();
    assert!(err.contains("not in the displayed set"), "err: {err}");
}

#[test]
fn preview_does_not_send() {
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h1", &["a"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let occ = s.draft().unwrap().findings[0].occurrence_id.clone();
    s.choose(&occ, FindingChoice::Fix, None).unwrap();
    let preview = s.preview(NOW).unwrap();
    let frame = render::preview_frame(&preview);
    assert!(frame.contains("h1"), "preview: {frame}");
    // Nothing was sent: no judgment recorded.
    let judged = store.transact(|tx| tx.latest_judgment(&work_id)).unwrap();
    assert!(judged.is_none());
}

#[test]
fn incomplete_draft_cannot_stage() {
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h1", &["a", "b"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let err = s.stage_judgment().unwrap_err();
    assert!(err.contains("no decision yet"), "err: {err}");
}

#[test]
fn judgment_exact_set_rejected_on_omission() {
    // The work-side guard: a judgment whose decisions omit a sealed
    // occurrence is `invalid_request`, even if the draft was tricked
    // into staging it.
    let root = temp_root();
    let _c = Cleanup(root.clone());
    let store = SqliteStore::open(&root.join("t.db"), 4).unwrap();
    let work_id = submit_work(&store);
    review_fixture(&store, &work_id, "h1", &["a", "b"]);

    let mut s = TuiSession::new(&store, config(), operator());
    s.open(&work_id, NOW).unwrap();
    s.begin_judgment().unwrap();
    let detail = s.detail().unwrap().clone();
    let occs: Vec<FindingView> = detail.findings.clone();
    // Compose a request covering only the first occurrence — bypassing
    // the draft's completeness check to prove Core enforces the exact
    // set independently of the TUI.
    let req = s.compose(workloop_core::command::CommandBody::JudgeFindings {
        work_id: work_id.clone(),
        expected_work_version: detail.version,
        expected_head: detail.current_change.clone().unwrap().1.unwrap(),
        decisions: vec![workloop_core::command::FindingDecision {
            occurrence_id: occs[0].occurrence_id.clone(),
            choice: FindingChoice::Fix,
            rationale: None,
        }],
    });
    let decision = work::handle_command(&store, &config(), &req, NOW).unwrap();
    let CommandDecision::Rejected { reason, .. } = decision else {
        panic!("expected rejection, got {decision:?}");
    };
    assert_eq!(reason, RejectionCode::InvalidRequest);
}
