//! TUI contract tests: the TUI is a second interface adapter over the
//! same Command/Query surface as the CLI — it renders only what the
//! Queries return, sends every state change through `submit` with an
//! idempotency key and the displayed Work version, uses `preview` as an
//! advisory only, handles stale head/version rejections by re-querying,
//! and recovers lost submit responses without ever mutating a pending
//! request.

use workloop_core::command::{
    ActionPreview, CommandBody, CommandDecision, CommandRequest, FindingChoice,
};
use workloop_core::domain::*;
use workloop_core::observation::{FindingRef, Observation, ObservationFact, ReviewVerdict};
use workloop_core::projection::{
    AttentionView, CapacityView, CommandResultLookup, QueryResult, WorkDetailView, WorkListView,
};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{WorkloopConfig, handle_command};
use workloop_store::sqlite::SqliteStore;
use workloop_tui::{Contracts, Key, Screen, SqliteContracts, Tui};

const NOW: i64 = 1_700_000_000;

fn fixed_now() -> i64 {
    NOW
}

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

fn cli_req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit_work(store: &SqliteStore, key: &str, issue_number: u64) -> WorkId {
    let d = handle_command(
        store,
        &config(),
        &cli_req(
            key,
            CommandBody::Submit {
                intent: "do the thing".into(),
                issue_refs: vec![GitHubIssueRef {
                    repository: "local/repo".into(),
                    number: issue_number,
                }],
                constraints: vec![],
                verification_argv: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Accepted { work_id, .. } => work_id.unwrap(),
        other => panic!("expected accepted, got {other:?}"),
    }
}

fn apply_effects(store: &SqliteStore, effects: &[Effect]) {
    store
        .transact(|tx| {
            for e in effects {
                tx.apply(e)?;
            }
            Ok(())
        })
        .expect("setup effects apply");
}

fn seed_pr_head(store: &SqliteStore, work_id: &WorkId, sha: &str, observed_at: i64) {
    apply_effects(
        store,
        &[Effect::InsertObservation {
            observation: Observation {
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
        }],
    );
}

fn seed_pr_review(store: &SqliteStore, work_id: &WorkId, sha: &str, finding_ids: &[&str]) {
    apply_effects(
        store,
        &[Effect::InsertObservation {
            observation: Observation {
                source_event_id: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::PrReview {
                    repository: "local/repo".into(),
                    head_sha: sha.into(),
                    reviewer: "rev".into(),
                    verdict: ReviewVerdict::ChangesRequested,
                    findings: finding_ids
                        .iter()
                        .map(|id| FindingRef {
                            finding_id: (*id).into(),
                            reviewed_revision: sha.into(),
                            summary: format!("finding {id}"),
                            location: None,
                            evidence: vec![],
                        })
                        .collect(),
                },
                observed_unix_seconds: NOW,
            },
        }],
    );
}

/// How a simulated lost response behaves — the response is what is lost;
/// whether the Command committed is exactly what the TUI must not assume.
#[derive(Clone, Copy)]
enum DropMode {
    /// Committed on the store, response lost — `command_result` finds it.
    AfterCommit,
    /// Never reached the store — only identical replay can settle it.
    BeforeCommit,
}

/// `Contracts` over a real store that records every request the TUI
/// issues and can drop submit responses — the lost-response fault.
struct Recording<'a> {
    store: &'a SqliteStore,
    config: &'a WorkloopConfig,
    submitted: Vec<CommandRequest>,
    drop_count: usize,
    drop_mode: DropMode,
}

impl<'a> Recording<'a> {
    fn new(store: &'a SqliteStore, config: &'a WorkloopConfig) -> Self {
        Self {
            store,
            config,
            submitted: vec![],
            drop_count: 0,
            drop_mode: DropMode::AfterCommit,
        }
    }

    fn inner(&self) -> SqliteContracts<'a> {
        SqliteContracts {
            store: self.store,
            config: self.config,
            now: fixed_now,
        }
    }
}

impl Contracts for Recording<'_> {
    fn preview(&mut self, req: &CommandRequest) -> Result<ActionPreview, String> {
        self.inner().preview(req)
    }

    fn submit(&mut self, req: &CommandRequest) -> Result<CommandDecision, String> {
        self.submitted.push(req.clone());
        if self.drop_count > 0 {
            self.drop_count -= 1;
            if matches!(self.drop_mode, DropMode::AfterCommit) {
                let _ = self.inner().submit(req);
            }
            return Err("response lost".into());
        }
        self.inner().submit(req)
    }

    fn work_list(&mut self) -> Result<QueryResult<WorkListView>, String> {
        self.inner().work_list()
    }

    fn work_detail(&mut self, work_id: &WorkId) -> Result<QueryResult<WorkDetailView>, String> {
        self.inner().work_detail(work_id)
    }

    fn attention(
        &mut self,
        work_id: Option<&WorkId>,
    ) -> Result<QueryResult<Vec<AttentionView>>, String> {
        self.inner().attention(work_id)
    }

    fn capacity(&mut self) -> Result<QueryResult<CapacityView>, String> {
        self.inner().capacity()
    }

    fn command_result(
        &mut self,
        lookup: &CommandResultLookup,
    ) -> Result<QueryResult<CommandDecision>, String> {
        self.inner().command_result(lookup)
    }
}

fn tui<'a>(store: &'a SqliteStore, config: &'a WorkloopConfig) -> Tui<Recording<'a>> {
    Tui::new(Recording::new(store, config), actor(), Origin("tui".into()))
}

/// Open the detail screen for `work_id` through the board path — the
/// same navigation the interactive driver performs.
fn open_detail<C: Contracts>(tui: &mut Tui<C>, work_id: &WorkId) {
    tui.refresh_board();
    let idx = tui
        .board()
        .items
        .iter()
        .position(|i| i.work_id == *work_id)
        .expect("work appears on the board");
    for _ in 0..idx {
        tui.handle(Key::Down);
    }
    tui.handle(Key::Enter);
    assert_eq!(tui.screen(), Screen::Detail);
    assert_eq!(tui.detail().unwrap().work_id, *work_id);
}

/// Fill every displayed finding with `choice` and return to the judge
/// screen's decision set — the "one decision per displayed finding" the
/// Command requires.
fn decide_all<C: Contracts>(tui: &mut Tui<C>, choice: char) {
    let n = tui.judge().unwrap().findings.len();
    for i in 0..n {
        while tui.judge().unwrap().cursor < i {
            tui.handle(Key::Down);
        }
        tui.handle(Key::Char(choice));
    }
}

#[test]
fn board_is_derived_from_queries_only() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w1 = submit_work(&store, "s1", 1);
    let w2 = submit_work(&store, "s2", 2);
    let mut tui = tui(&store, &cfg);
    tui.refresh_board();
    // The board shows exactly what work_list returns — no card state of
    // its own, and work submitted through the CLI surface appears here.
    let ids: Vec<_> = tui
        .board()
        .items
        .iter()
        .map(|i| i.work_id.clone())
        .collect();
    assert_eq!(ids, vec![w1.clone(), w2]);
    assert!(tui.board().items.iter().all(|i| i.attention_count == 0));
    assert_eq!(tui.board().capacity.as_ref().unwrap().limit, 1);
    let rendered = tui.render(160).join("\n");
    assert!(rendered.contains("queued"));
    assert!(rendered.contains(&w1.0));
}

#[test]
fn every_command_carries_key_and_displayed_version() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    let displayed_version = tui.detail().unwrap().version;
    tui.handle(Key::Char('x')); // cancel
    // The only state change went through submit — carrying an
    // idempotency key and the version the operator was looking at.
    let req = &tui.contracts().submitted[0];
    assert!(req.idempotency_key.starts_with("tui-"));
    assert_eq!(req.origin, Origin("tui".into()));
    match &req.body {
        CommandBody::Cancel {
            work_id,
            expected_work_version,
            ..
        } => {
            assert_eq!(work_id, &w);
            assert_eq!(*expected_work_version, displayed_version);
        }
        other => panic!("expected cancel, got {other:?}"),
    }
}

#[test]
fn judge_submits_exactly_one_decision_per_displayed_finding() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1", "f2"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    assert_eq!(tui.screen(), Screen::Judge);
    let judge = tui.judge().unwrap();
    assert_eq!(judge.pr_head, "h1");
    assert_eq!(judge.work_version, 1);
    assert_eq!(
        judge
            .findings
            .iter()
            .map(|f| f.finding_id.as_str())
            .collect::<Vec<_>>(),
        vec!["f1", "f2"]
    );
}

#[test]
fn incomplete_choices_are_never_submitted() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1", "f2"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    // Decide only f1 — the submit key must not issue a request.
    tui.handle(Key::Char('f'));
    tui.handle(Key::Char('s'));
    assert!(tui.status().contains("decide every displayed finding"));
    assert!(tui.screen() == Screen::Judge);
    assert!(tui.contracts().submitted.is_empty());
}

#[test]
fn judge_submit_carries_displayed_version_head_and_decisions() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1", "f2"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    decide_all(&mut tui, 'a');
    tui.handle(Key::Char('s'));
    let req = &tui.contracts().submitted[0];
    match &req.body {
        CommandBody::JudgeFindings {
            work_id,
            expected_work_version,
            expected_pr_head,
            decisions,
        } => {
            assert_eq!(work_id, &w);
            assert_eq!(*expected_work_version, 1);
            assert_eq!(expected_pr_head, "h1");
            let decided: Vec<_> = decisions
                .iter()
                .map(|d| (d.finding_id.as_str(), &d.choice))
                .collect();
            assert_eq!(
                decided,
                vec![
                    ("f1", &FindingChoice::Accept),
                    ("f2", &FindingChoice::Accept)
                ]
            );
        }
        other => panic!("expected judge_findings, got {other:?}"),
    }
    // The decision was accepted by the shared command path.
    assert!(tui.status().starts_with("accepted"));
    assert!(!tui.pending_unknown());
}

#[test]
fn stale_pr_head_requeries_and_clears_choices() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    decide_all(&mut tui, 'd');
    // A newer head lands between display and submit — the displayed
    // choices are bound to h1 and must not be reissued for h2.
    seed_pr_head(&store, &w, "h2", NOW + 10);
    seed_pr_review(&store, &w, "h2", &["f3"]);
    tui.handle(Key::Char('s'));
    // Exactly one request was issued; the TUI did not rewrite the
    // expected head nor resubmit the old choices.
    assert_eq!(tui.contracts().submitted.len(), 1);
    match &tui.contracts().submitted[0].body {
        CommandBody::JudgeFindings {
            expected_pr_head, ..
        } => assert_eq!(expected_pr_head, "h1"),
        other => panic!("expected judge_findings, got {other:?}"),
    }
    let judge = tui.judge().expect("judge reloads against the new head");
    assert_eq!(judge.pr_head, "h2");
    assert!(judge.choices.is_empty());
    assert_eq!(
        judge
            .findings
            .iter()
            .map(|f| f.finding_id.as_str())
            .collect::<Vec<_>>(),
        vec!["f3"]
    );
    assert!(tui.status().contains("stale head/version"));
    // The operator re-decides on the new head and resubmits — now with
    // the new displayed version and head.
    decide_all(&mut tui, 'a');
    tui.handle(Key::Char('s'));
    assert_eq!(tui.contracts().submitted.len(), 2);
    match &tui.contracts().submitted[1].body {
        CommandBody::JudgeFindings {
            expected_pr_head,
            decisions,
            ..
        } => {
            assert_eq!(expected_pr_head, "h2");
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0].finding_id, "f3");
        }
        other => panic!("expected judge_findings, got {other:?}"),
    }
    assert!(tui.status().starts_with("accepted"));
}

#[test]
fn stale_work_version_refreshes_displayed_state() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    assert_eq!(tui.detail().unwrap().version, 1);
    // Another surface moves the Work forward — v1 is now stale.
    let d = handle_command(
        &store,
        &cfg,
        &cli_req(
            "other",
            CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }));
    // The TUI's cancel is still built on the stale displayed version.
    tui.handle(Key::Char('x'));
    assert!(tui.status().contains("stale head/version"));
    // Re-queried: the displayed version is the new truth — a follow-up
    // command is issued deliberately, on the fresh version.
    assert_eq!(tui.detail().unwrap().version, 2);
    assert_eq!(tui.contracts().submitted.len(), 1);
}

#[test]
fn lost_response_committed_recovers_via_command_result() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    decide_all(&mut tui, 'a');
    // The next submit commits but its response is lost — the TUI sees
    // only Err and must render the result as unknown.
    tui.contracts_mut().drop_mode = DropMode::AfterCommit;
    tui.contracts_mut().drop_count = 1;
    tui.handle(Key::Char('s'));
    assert!(tui.pending_unknown());
    assert!(tui.status().contains("unknown"));
    // While the result is unknown no mutated request may be issued —
    // even deliberate keys are refused until the operator resolves it.
    tui.handle(Key::Char('s'));
    assert_eq!(tui.contracts().submitted.len(), 1);
    // Read-only navigation still passes through: leaving the judge
    // screen does not settle or mutate the outstanding request.
    tui.handle(Key::Esc);
    assert!(tui.pending_unknown());
    assert_eq!(tui.screen(), Screen::Detail);
    // command_result by replay key finds the durably stored decision.
    tui.handle(Key::Char('l'));
    assert!(!tui.pending_unknown());
    assert!(tui.status().starts_with("accepted"));
}

#[test]
fn lost_response_uncommitted_replays_identical_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    decide_all(&mut tui, 'a');
    // The request never reached the store — command_result finds
    // nothing, so the identical key and payload are replayed.
    tui.contracts_mut().drop_mode = DropMode::BeforeCommit;
    tui.contracts_mut().drop_count = 1;
    tui.handle(Key::Char('s'));
    assert!(tui.pending_unknown());
    tui.handle(Key::Char('l'));
    assert!(tui.pending_unknown());
    assert!(tui.status().contains("still unknown"));
    tui.handle(Key::Char('r'));
    assert!(!tui.pending_unknown());
    assert!(tui.status().starts_with("accepted"));
    // The replay was byte-identical: same idempotency key, same payload.
    let submitted = &tui.contracts().submitted;
    assert_eq!(submitted.len(), 2);
    assert_eq!(
        serde_json::to_string(&submitted[0]).unwrap(),
        serde_json::to_string(&submitted[1]).unwrap()
    );
}

#[test]
fn preview_is_advisory_only() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1"]);
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('j'));
    decide_all(&mut tui, 'a');
    tui.handle(Key::Char('p'));
    // The preview is advisory: no request was submitted, no decision was
    // persisted, and the Work version did not move.
    assert!(tui.contracts().submitted.is_empty());
    let preview = tui.judge().unwrap().last_preview.as_ref().unwrap();
    assert!(matches!(
        preview.availability,
        workloop_core::command::PreviewAvailability::Available
    ));
    let detail = match tui.contracts_mut().work_detail(&w).unwrap() {
        QueryResult::Found { value, .. } => value,
        _ => panic!("work exists"),
    };
    assert_eq!(detail.version, 1);
    // The same request then goes through submit — the judgment lands.
    tui.handle(Key::Char('s'));
    assert!(tui.status().starts_with("accepted"));
}

#[test]
fn disconnect_changes_no_execution_truth() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    let w = submit_work(&store, "s1", 1);
    seed_pr_head(&store, &w, "h1", NOW);
    seed_pr_review(&store, &w, "h1", &["f1"]);
    let pending_key;
    {
        let mut tui = tui(&store, &cfg);
        open_detail(&mut tui, &w);
        tui.handle(Key::Char('j'));
        decide_all(&mut tui, 'a');
        // Commit a judgment, lose the response, then disconnect the TUI.
        tui.contracts_mut().drop_mode = DropMode::AfterCommit;
        tui.contracts_mut().drop_count = 1;
        tui.handle(Key::Char('s'));
        assert!(tui.pending_unknown());
        pending_key = tui.contracts().submitted[0].idempotency_key.clone();
        tui.handle(Key::Char('q'));
    } // TUI dropped — disconnected with an outstanding unknown Command.
    // The outstanding Command's decision is durably stored and
    // queryable through the same surface the CLI uses.
    let found = store
        .transact(|tx| {
            workloop_core::projection::command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: Origin("tui".into()),
                    actor: actor(),
                    idempotency_key: pending_key,
                },
            )
        })
        .unwrap();
    assert!(matches!(found, QueryResult::Found { .. }));
    // The recorded judgment is part of execution truth, readable through
    // work_detail exactly as before the disconnect.
    let view = store
        .transact(|tx| workloop_core::projection::work_detail(tx, &w, NOW))
        .unwrap();
    match view {
        QueryResult::Found { value, .. } => assert_eq!(value.version, 2),
        _ => panic!("work exists"),
    }
}

#[test]
fn cli_submitted_work_is_controllable_from_the_tui() {
    let store = SqliteStore::in_memory(1).unwrap();
    let cfg = config();
    // The Work enters the queue through the CLI surface…
    let w = submit_work(&store, "s1", 1);
    // …and the TUI controls the same Work through the same contracts.
    let mut tui = tui(&store, &cfg);
    open_detail(&mut tui, &w);
    tui.handle(Key::Char('x')); // cancel
    assert!(tui.status().starts_with("accepted"));
    let view = store
        .transact(|tx| workloop_core::projection::work_detail(tx, &w, NOW))
        .unwrap();
    match view {
        QueryResult::Found { value, .. } => {
            assert_eq!(
                value.outcome.as_ref().unwrap().state,
                ExecutionState::Cancelled
            );
        }
        _ => panic!("work exists"),
    }
}
