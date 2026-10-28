//! TUI surface contract tests (issue #15): the console is the second
//! interface adapter. It reaches `work` only through `preview` and
//! `submit(CommandRequest)` and `projection` only through Query; the
//! board is derived from saved records; preview is advisory; stale
//! rejections re-query and clear; a lost response recovers by
//! command_result or identical replay — never a mutated request; and
//! disconnecting changes no execution truth.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::observation::{Observation, ObservationFact, ProcessStateFact, ReviewVerdict};
use workloop_core::projection::{self, CommandResultLookup, QueryResult, WorkPhase};
use workloop_core::store::{Effect, Store, StoreError, TxView};
use workloop_core::work::{
    PreviewAvailability, WorkloopConfig, handle_command, handle_observation,
};
use workloop_store::sqlite::SqliteStore;
use workloop_tui::render::board_frame;
use workloop_tui::session::{Action, ORIGIN, RecoverOutcome, SubmitOutcome, TuiSession};

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
        external_id: "op-tui".into(),
    }
}

/// Attach a session to `store` — the fixture stands in for the process
/// boundary: the session reaches the store only through the Command/
/// Query ports while the test inspects the same records the CLI sees.
fn session(store: &SqliteStore) -> TuiSession<'_, SqliteStore> {
    TuiSession::new(store, config(), actor())
}

fn cli_req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit_work(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        &config(),
        &cli_req(
            key,
            CommandBody::Submit {
                intent: format!("intent-{key}"),
                issue_refs: vec![GitHubIssueRef {
                    repository: "local/repo".into(),
                    number: 1,
                }],
                constraints: vec![],
                verification_argv: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    match d {
        CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("submit {key}: {other:?}"),
    }
}

fn observe(store: &SqliteStore, work_id: &WorkId, fact: ObservationFact, at: i64) {
    handle_observation(
        store,
        &config(),
        &Observation {
            source_event_id: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact,
            observed_unix_seconds: at,
        },
        at,
    )
    .unwrap();
}

fn pr_head(sha: &str) -> ObservationFact {
    ObservationFact::PrHead {
        repository: "local/repo".into(),
        number: 1,
        head_sha: sha.into(),
    }
}

fn pr_review(sha: &str) -> ObservationFact {
    ObservationFact::PrReview {
        repository: "local/repo".into(),
        head_sha: sha.into(),
        reviewer: "rev".into(),
        verdict: ReviewVerdict::ChangesRequested,
    }
}

fn process_state() -> ObservationFact {
    ObservationFact::ProcessState {
        invocation_id: InvocationId("inv-1".into()),
        process_identity: "pid-1".into(),
        state: ProcessStateFact::Running,
    }
}

fn last_seq(store: &SqliteStore) -> i64 {
    store.transact(|tx| tx.last_seq()).unwrap()
}

fn stored_decision(store: &SqliteStore, key: &str) -> Option<CommandDecision> {
    match store
        .transact(|tx| {
            projection::command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: Origin(ORIGIN.into()),
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

/// AC: columns derive from `work_list`/`attention`/`capacity` — phase,
/// outcome, and attention counts come from saved records, never from
/// state the TUI persists.
#[test]
fn board_columns_derive_from_queries() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (w1, _e1) = submit_work(&store, "b-queued");
    let (w2, e2) = submit_work(&store, "b-paused");
    // Park w2 paused so it lands in the PAUSED column with attention —
    // the fixture equivalent of prior history.
    store
        .transact(|tx| {
            let mut e = tx.execution(&e2)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair: needs human".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();

    session.refresh(NOW).unwrap();
    let board = session.board().expect("board");
    let phase_of = |w: &WorkId| {
        board
            .columns
            .iter()
            .find(|c| c.cards.iter().any(|card| &card.work_id == w))
            .map(|c| c.phase.clone())
    };
    assert_eq!(phase_of(&w1), Some(WorkPhase::Queued));
    assert_eq!(phase_of(&w2), Some(WorkPhase::Paused));
    let w2_card = board
        .columns
        .iter()
        .flat_map(|c| &c.cards)
        .find(|card| card.work_id == w2)
        .unwrap();
    assert_eq!(w2_card.attention_count, 1, "attention derives from records");
    assert_eq!(board.attention.len(), 1);
    assert_eq!(board.capacity.as_ref().unwrap().limit, 1);
    // The board holds no card state of its own: order is the projection's.
    assert_eq!(board.order(), vec![w1, w2]);
}

/// AC: TUI submissions go through `handle_command` under origin "tui"
/// and are recoverable by their own replay key — a namespace distinct
/// from the CLI's.
#[test]
fn tui_commands_record_tui_origin() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "o-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    let key = session.staged().unwrap().idempotency_key.clone();
    let SubmitOutcome::Decided(decision) = session.submit(NOW).unwrap() else {
        panic!("expected decided")
    };
    assert!(matches!(decision, CommandDecision::Accepted { .. }));
    let stored = stored_decision(&store, &key).expect("decision persisted");
    assert_eq!(
        serde_json::to_string(&stored).unwrap(),
        serde_json::to_string(&decision).unwrap(),
        "command_result recovers the TUI decision by its replay key"
    );
    // The CLI reads the same truth; its origin is a different namespace
    // so a TUI key is not visible under origin "cli".
    let cli_view = store
        .transact(|tx| {
            projection::command_result(
                tx,
                &CommandResultLookup::ReplayKey {
                    origin: Origin("cli".into()),
                    actor: actor(),
                    idempotency_key: key.clone(),
                },
            )
        })
        .unwrap();
    assert!(matches!(cli_view, QueryResult::NotFound));
}

/// AC: preview is advisory — it persists nothing and never substitutes
/// for the submit-time precondition check.
#[test]
fn session_preview_is_advisory() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "pv-seed");
    let before = last_seq(&store);
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    let key = session.staged().unwrap().idempotency_key.clone();
    let p = session.preview(NOW).unwrap();
    assert_eq!(p.availability, PreviewAvailability::Available);
    assert_eq!(p.observed_work_version, Some(1));
    assert_eq!(last_seq(&store), before, "preview wrote nothing");
    assert!(stored_decision(&store, &key).is_none());
    // The real submit still runs the full precondition check.
    let SubmitOutcome::Decided(d) = session.submit(NOW).unwrap() else {
        panic!("expected decided")
    };
    assert!(matches!(d, CommandDecision::Accepted { .. }));
}

/// AC: the judgment flow displays head-scoped findings, collects one
/// supported decision per displayed finding id, and submits the
/// displayed Work version and PR head.
#[test]
fn judge_findings_flow_end_to_end() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "j-seed");
    observe(&store, &work_id, pr_head("h1"), NOW);
    observe(&store, &work_id, pr_review("h1"), NOW);

    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.begin_judgment().unwrap();
    let finding = session.draft().unwrap().findings[0].finding_id.clone();
    assert!(finding.contains("h1"), "displayed finding is head-scoped");
    // Exactly one decision per displayed id: a re-pick replaces.
    session.choose(&finding, FindingChoice::Fix).unwrap();
    session.choose(&finding, FindingChoice::Dismiss).unwrap();

    let preview = session.preview(NOW).unwrap();
    assert_eq!(preview.availability, PreviewAvailability::Available);
    assert_eq!(preview.observed_pr_head.as_deref(), Some("h1"));

    let SubmitOutcome::Decided(d) = session.submit(NOW).unwrap() else {
        panic!("expected decided")
    };
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let (head, decisions) = store
        .transact(|tx| tx.latest_judgment(&work_id))
        .unwrap()
        .expect("judgment recorded");
    assert_eq!(head, "h1");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].finding_id, finding);
    assert_eq!(decisions[0].choice, FindingChoice::Dismiss);
    // The accepted Command bumped the version: the submitted draft is
    // gone — its choices belong to the pre-Command snapshot.
    assert!(session.draft().is_none());
    assert_eq!(session.detail().unwrap().version, 2);
    // A pick for an id outside the displayed set is refused locally.
    session.begin_judgment().unwrap();
    assert!(
        session
            .choose("no-such-finding", FindingChoice::Fix)
            .is_err()
    );
}

/// A draft that is not complete cannot arm a request — the Command is
/// built only once every displayed finding carries a decision.
#[test]
fn incomplete_draft_cannot_submit() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "ic-seed");
    observe(&store, &work_id, pr_head("h1"), NOW);
    observe(&store, &work_id, pr_review("h1"), NOW);
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.begin_judgment().unwrap();
    // No choices made — nothing may reach `work`.
    assert!(session.stage_judgment().is_err());
    assert!(session.submit(NOW).is_err());
    assert!(stored_decision(&store, "tui-1").is_none());
}

/// AC: a stale-head rejection re-queries the findings and clears the
/// old choices; the expected head is never rewritten to resubmit them.
#[test]
fn stale_head_rejection_requeries_and_clears_choices() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "s-seed");
    observe(&store, &work_id, pr_head("h-old"), NOW);
    observe(&store, &work_id, pr_review("h-old"), NOW);

    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.begin_judgment().unwrap();
    let old_finding = session.draft().unwrap().findings[0].finding_id.clone();
    session.choose(&old_finding, FindingChoice::Fix).unwrap();
    session.stage_judgment().unwrap();
    let submitted_key = session.staged().unwrap().idempotency_key.clone();

    // The head moves after the operator's snapshot.
    observe(&store, &work_id, pr_head("h-new"), NOW + 10);
    observe(&store, &work_id, pr_review("h-new"), NOW + 10);

    let SubmitOutcome::Decided(d) = session.submit(NOW + 20).unwrap() else {
        panic!("expected decided")
    };
    match &d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(*reason, RejectionCode::StalePrHead)
        }
        other => panic!("expected stale_pr_head, got {other:?}"),
    }

    // The session re-queried: the detail shows the new head and the
    // rebuilt draft has no choices left — nothing retargeted or
    // resubmitted the old set.
    let detail = session.detail().expect("re-queried detail");
    assert_eq!(
        detail.current_pr.as_ref().and_then(|(_, h)| h.as_deref()),
        Some("h-new")
    );
    let draft = session.draft().expect("fresh draft for the new head");
    assert_eq!(draft.pr_head, "h-new");
    assert!(draft.findings.iter().all(|f| f.choice.is_none()));
    assert!(session.staged().is_none());
    // Exactly one request was ever submitted under its own key.
    assert!(matches!(
        stored_decision(&store, &submitted_key),
        Some(CommandDecision::Rejected { .. })
    ));
}

/// A stale Work version behaves the same way: the snapshot is
/// re-queried and the stale choices are cleared.
#[test]
fn stale_work_version_rejection_requeries() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "sv-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    // Another surface bumps the version.
    let _ = handle_command(
        &store,
        &config(),
        &cli_req(
            "sv-other",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    let SubmitOutcome::Decided(d) = session.submit(NOW).unwrap() else {
        panic!("expected decided")
    };
    match &d {
        CommandDecision::Rejected { reason, .. } => {
            assert_eq!(*reason, RejectionCode::StaleWorkVersion)
        }
        other => panic!("expected stale_work_version, got {other:?}"),
    }
    assert_eq!(session.detail().unwrap().version, 2);
    assert!(session.staged().is_none());
}

/// AC: a lost response is unknown, recovered via `command_result` by
/// the caller's replay key when the decision did land.
#[test]
fn lost_submit_recovers_recorded_decision() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "l-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    let req = session.staged().unwrap().clone();

    // The command landed but the response never arrived.
    let landed = handle_command(&store, &config(), &req, NOW).unwrap();
    session.note_lost(req.clone());
    assert!(session.pending().is_some());
    match session.recover(NOW).unwrap() {
        RecoverOutcome::Found(d) => {
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                serde_json::to_string(&landed).unwrap()
            );
        }
        other => panic!("expected found, got {other:?}"),
    }
    assert!(session.pending().is_none());
}

/// AC: when nothing landed, recovery replays the identical key and
/// payload — the request is decided once, never mutated.
#[test]
fn lost_submit_replays_identical_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "l2-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    // A composed-but-never-issued request whose response is "lost".
    let req = session.compose(CommandBody::Cancel {
        work_id: work_id.clone(),
        expected_work_version: 1,
        execution_id: None,
    });
    session.note_lost(req.clone());
    match session.recover(NOW).unwrap() {
        RecoverOutcome::Replayed(d) => {
            assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
        }
        other => panic!("expected replayed, got {other:?}"),
    }
    assert!(session.pending().is_none());
    // Replaying again under the same key returns that exact decision.
    let again = handle_command(&store, &config(), &req, NOW).unwrap();
    assert_eq!(
        serde_json::to_string(&again).unwrap(),
        serde_json::to_string(&stored_decision(&store, &req.idempotency_key).unwrap()).unwrap()
    );
}

/// While a result is unknown, no new mutating request may be issued —
/// the session refuses rather than risking an unordered mutation.
#[test]
fn submit_refused_while_result_unknown() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "sr-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    let lost = session.compose(CommandBody::Cancel {
        work_id: work_id.clone(),
        expected_work_version: 1,
        execution_id: None,
    });
    session.note_lost(lost);
    session.stage_action(&Action::Retry).ok();
    assert!(session.submit(NOW).is_err());
    // Recovery clears the way; the pending request settles first.
    assert!(matches!(
        session.recover(NOW).unwrap(),
        RecoverOutcome::Replayed(_)
    ));
}

/// AC: disconnecting the TUI changes no execution truth — refresh,
/// open, stage, and preview all leave the store byte-identical, and a
/// fresh session sees the same truth the CLI does.
#[test]
fn disconnect_changes_no_execution_truth() {
    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = submit_work(&store, "d-seed");
    let before_seq = last_seq(&store);
    let before_works = store.transact(|tx| tx.works()).unwrap();
    {
        let mut session = session(&store);
        session.refresh(NOW).unwrap();
        session.open(&work_id, NOW).unwrap();
        session.stage_action(&Action::Cancel).unwrap();
        session.preview(NOW).unwrap();
        // Disconnect: the session — and all its selection state — goes
        // away when this scope ends.
    }
    assert_eq!(last_seq(&store), before_seq, "UI actions wrote nothing");
    assert_eq!(
        serde_json::to_string(&store.transact(|tx| tx.works()).unwrap()).unwrap(),
        serde_json::to_string(&before_works).unwrap()
    );

    // A new session sees the same truth the CLI does — the Work is still
    // queued and controllable through either surface.
    let mut session2 = session(&store);
    session2.refresh(NOW).unwrap();
    assert_eq!(session2.board().unwrap().order(), vec![work_id.clone()]);
    let d = handle_command(
        &store,
        &config(),
        &cli_req(
            "d-cli-cancel",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }));
}

/// Local refusals never reach `work`: verbs without the required
/// session state are rejected at the adapter boundary, and no decision
/// row appears for them.
#[test]
fn local_refusals_never_reach_work() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "lr-seed");

    // submit with nothing staged; judge with nothing open; choose with
    // no draft — all refused locally.
    assert!(session.submit(NOW).is_err());
    assert!(session.preview(NOW).is_err());
    assert!(session.begin_judgment().is_err());
    assert!(session.choose("f", FindingChoice::Fix).is_err());
    assert!(session.stage_action(&Action::Cancel).is_err());
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    // No observed head → no draft can begin.
    assert!(session.begin_judgment().is_err());
    // Nothing the console refused ever persisted a decision.
    assert!(last_seq(&store) > 0, "seeding wrote the work");
    let decision_count = store
        .transact(|tx| {
            tx.stored_decision(&Origin(ORIGIN.into()), &actor(), "tui-1")
                .map(|d| d.is_some())
        })
        .unwrap();
    assert!(!decision_count, "a refused path persisted a TUI decision");
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workloop-tui"))
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-tui-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Drive the real binary over piped stdin — the console process itself.
fn run_console(root: &Path, input: &str) -> (bool, String, String) {
    let mut child = Command::new(bin())
        .args([
            "--db",
            &root.join("workloop.db").to_string_lossy(),
            "--config",
            &root.join("config.json").to_string_lossy(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("workloop-tui spawns");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// INV-009: EOF and malformed input never produce a Command — the
/// console refuses bad verbs locally and exits cleanly on EOF/quit.
#[test]
fn console_eof_and_malformed_input() {
    let dir = root("console");
    let db_path = dir.join("workloop.db");
    let store = SqliteStore::open(&db_path, 1).unwrap();
    let (work_id, _e) = submit_work(&store, "c-seed");
    drop(store);

    // Malformed input is refused locally; EOF ends the session.
    let (ok, stdout, stderr) = run_console(&dir, "bogus verb here\nquit\n");
    assert!(ok, "console exited nonzero: {stderr}");
    assert!(stdout.contains("unknown verb"), "{stdout}");

    // EOF alone disconnects cleanly.
    let (ok, _, stderr) = run_console(&dir, "");
    assert!(ok, "EOF exit failed: {stderr}");

    // A full drive: open the work, arm a cancel, preview it, submit.
    let (ok, stdout, stderr) =
        run_console(&dir, "refresh\nopen 1\ncancel\npreview\nsubmit\nquit\n");
    assert!(ok, "console exited nonzero: {stderr}");
    assert!(stdout.contains("availability Available"), "{stdout}");
    assert!(stdout.contains("accepted"), "{stdout}");

    // The store holds the truth the console produced — a cancelled Work.
    let store = SqliteStore::open(&db_path, 1).unwrap();
    let detail = store
        .transact(|tx| projection::work_detail(tx, &work_id, NOW + 100))
        .unwrap();
    match detail {
        QueryResult::Found { value, .. } => {
            assert_eq!(value.version, 2, "cancel bumped the version");
            assert!(
                value
                    .executions
                    .iter()
                    .all(|e| e.state == ExecutionState::Cancelled),
                "{:?}",
                value.executions
            );
        }
        other => panic!("{other:?}"),
    }
}

/// AC: a lost result also settles by command id — the id the operator
/// obtained from another surface (the CLI's command_result, say).
#[test]
fn lost_submit_recovers_by_command_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "li-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    let req = session.staged().unwrap().clone();

    // The command landed but the response never arrived; the operator
    // learns its command id from the other surface.
    let landed = handle_command(&store, &config(), &req, NOW).unwrap();
    let command_id = landed.command_id().clone();
    session.note_lost(req);
    match session.recover_by_id(&command_id, NOW).unwrap() {
        RecoverOutcome::Found(d) => {
            assert_eq!(d.command_id(), &command_id);
        }
        other => panic!("expected found, got {other:?}"),
    }
    assert!(session.pending().is_none());
}

/// The card number `board_frame` prints is `Board::order()`'s —
/// column-major. When an earlier column holds more rows, the nth card
/// on screen is still the nth `open <n>` resolves.
#[test]
fn board_numbers_match_open_order() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (w1, _e1) = submit_work(&store, "bn-1");
    let (w2, _e2) = submit_work(&store, "bn-2");
    let (w3, e3) = submit_work(&store, "bn-3");
    // Park w3 paused: QUEUED holds two rows, PAUSED one — row-major
    // reading order would number w3 as 2, but order() numbers it 3.
    store
        .transact(|tx| {
            let mut e = tx.execution(&e3)?.unwrap();
            e.state = ExecutionState::Paused;
            e.pause_reason = Some("repair".into());
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Queued,
            })
        })
        .unwrap();

    session.refresh(NOW).unwrap();
    let board = session.board().unwrap();
    assert_eq!(board.order(), vec![w1.clone(), w2.clone(), w3.clone()]);
    let frame = board_frame(board, None);
    for (i, w) in [w1, w2, w3].iter().enumerate() {
        assert!(
            frame.contains(&format!("{}. {}", i + 1, w.0)),
            "card #{} ({}) missing its printed number:\n{frame}",
            i + 1,
            w.0
        );
    }
}

/// Identical finding strings are the same finding id: the draft keeps
/// one slot per distinct id and the Command carries exactly one
/// decision for it — two displayed rows can never fight over one key.
#[test]
fn duplicate_findings_share_one_decision_slot() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "df-seed");
    observe(&store, &work_id, pr_head("h1"), NOW);
    // The same review recorded twice — the projection displays one
    // identical finding string per observation.
    observe(&store, &work_id, pr_review("h1"), NOW);
    observe(&store, &work_id, pr_review("h1"), NOW + 1);

    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.begin_judgment().unwrap();
    let draft = session.draft().unwrap();
    let distinct: std::collections::HashSet<_> = draft
        .findings
        .iter()
        .map(|f| f.finding_id.clone())
        .collect();
    assert_eq!(draft.findings.len(), distinct.len(), "slots repeat an id");
    assert_eq!(draft.findings.len(), 1, "duplicate findings not deduped");

    let finding = draft.findings[0].finding_id.clone();
    session.choose(&finding, FindingChoice::Fix).unwrap();
    let req = session.stage_judgment().unwrap().clone();
    let CommandBody::JudgeFindings { decisions, .. } = &req.body else {
        panic!("expected judge_findings")
    };
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].finding_id, finding);
}

/// The detail pane shows what the projection recorded — evidence from
/// ProcessState/Artifact/PrActionResult observations is not dropped.
#[test]
fn detail_frame_shows_recorded_evidence() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "ev-seed");
    observe(&store, &work_id, process_state(), NOW);
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    let detail = session.detail().unwrap();
    assert!(
        !detail.evidence.is_empty(),
        "projection recorded no evidence for the observation"
    );
    let frame = workloop_tui::render::detail_frame(detail);
    assert!(
        frame.contains("evidence process:Running"),
        "evidence missing from detail pane:\n{frame}"
    );
}

/// An accepted Command moved the durable version: the session
/// re-queries so the next armed action carries the post-Command
/// version instead of a stale one.
#[test]
fn accepted_submit_refreshes_open_detail() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "ar-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    let SubmitOutcome::Decided(d) = session.submit(NOW).unwrap() else {
        panic!("expected decided")
    };
    assert!(matches!(d, CommandDecision::Accepted { .. }));
    assert_eq!(
        session.detail().unwrap().version,
        2,
        "detail snapshot kept the pre-Command version"
    );
    // The next armed action carries the fresh version — not v1, which
    // the handler would reject as stale.
    let req = session.stage_action(&Action::Cancel).unwrap().clone();
    let CommandBody::Cancel {
        expected_work_version,
        ..
    } = &req.body
    else {
        panic!("expected cancel")
    };
    assert_eq!(*expected_work_version, 2);
}

/// A command id cannot settle the unknown-result guard unless it is
/// proven to be the pending request's own decision.
#[test]
fn recover_by_id_rejects_unrelated_command_id() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "ri-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();

    // A composed-but-never-issued request is "lost".
    let lost = session.compose(CommandBody::Cancel {
        work_id: work_id.clone(),
        expected_work_version: 1,
        execution_id: None,
    });
    session.note_lost(lost);

    // An unrelated durable decision exists — submitted through the CLI.
    let other = handle_command(
        &store,
        &config(),
        &cli_req(
            "ri-other",
            CommandBody::Acknowledge {
                work_id: work_id.clone(),
                expected_work_version: 1,
                attention_id: "att-x".into(),
            },
        ),
        NOW,
    )
    .unwrap();
    let other_id = other.command_id().clone();
    assert!(
        session.recover_by_id(&other_id, NOW).is_err(),
        "an unrelated command id released the unknown-result guard"
    );
    assert!(session.pending().is_some());
    // The honest recovery still works: identical replay.
    assert!(matches!(
        session.recover(NOW).unwrap(),
        RecoverOutcome::Replayed(_)
    ));
    assert!(session.pending().is_none());
}

/// Starting a new judgment drops any armed request — the previously
/// staged Command belongs to the old draft and must never submit under
/// the new one's display.
#[test]
fn new_judgment_clears_staged_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "nj-seed");
    observe(&store, &work_id, pr_head("h1"), NOW);
    observe(&store, &work_id, pr_review("h1"), NOW);
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.begin_judgment().unwrap();
    let finding = session.draft().unwrap().findings[0].finding_id.clone();
    session.choose(&finding, FindingChoice::Fix).unwrap();
    session.stage_judgment().unwrap();
    assert!(session.staged().is_some());
    // A fresh `judge` discards the armed request tied to the old draft.
    session.begin_judgment().unwrap();
    assert!(
        session.staged().is_none(),
        "the stale armed request survived a new judgment"
    );
    assert!(session.preview(NOW).is_err(), "nothing to preview anymore");
}

/// An empty finding list under an un-read review source is not "no
/// findings": the console names the unavailable source instead.
#[test]
fn empty_findings_under_unread_source_is_not_clean() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "us-seed");
    // A head is observed but no review ever arrived — github:review is
    // Unknown, so empty findings prove nothing.
    observe(&store, &work_id, pr_head("h1"), NOW);
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    let err = session.begin_judgment().unwrap_err();
    assert!(
        err.contains("review source"),
        "expected the unavailable-source error, got: {err}"
    );
    // The detail render surfaces the un-read source too.
    let frame = workloop_tui::render::sources_frame(session.detail_sources());
    assert!(frame.contains("github:review"), "{frame}");
}

/// A re-query that finds the version or head moved discards the armed
/// request: it was built against the old observation and must be
/// re-armed from the snapshot the operator now sees — never submitted
/// with a stale expected version under the new display.
#[test]
fn moved_snapshot_clears_staged_request() {
    let store = SqliteStore::in_memory(1).unwrap();
    let mut session = session(&store);
    let (work_id, _e) = submit_work(&store, "mv-seed");
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    session.stage_action(&Action::Cancel).unwrap();
    assert!(session.staged().is_some());

    // Another surface moves the durable version under the armed request.
    let _ = handle_command(
        &store,
        &config(),
        &cli_req(
            "mv-other",
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: 1,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();

    session.open(&work_id, NOW).unwrap();
    assert!(
        session.staged().is_none(),
        "an armed request built on the pre-move snapshot survived the re-query"
    );
    // Re-arming carries the observed version — not the stale one the
    // discarded request held.
    let req = session.stage_action(&Action::Cancel).unwrap().clone();
    let CommandBody::Cancel {
        expected_work_version,
        ..
    } = &req.body
    else {
        panic!("expected cancel")
    };
    assert_eq!(*expected_work_version, 2);
}

/// The session is bound to the `Store` port, not the SQLite type: any
/// implementation backing the Command/Query contract can drive the
/// adapter.
#[test]
fn session_is_generic_over_the_store_port() {
    /// A `Store` that is not `SqliteStore`: delegates to one, but the
    /// session sees only the port.
    struct Delegating<'a>(&'a SqliteStore);
    impl Store for Delegating<'_> {
        fn transact<R>(
            &self,
            f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
        ) -> Result<R, StoreError> {
            self.0.transact(f)
        }
        fn transact_deferred<R>(
            &self,
            f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
        ) -> Result<R, StoreError> {
            self.0.transact_deferred(f)
        }
    }

    let store = SqliteStore::in_memory(1).unwrap();
    let (work_id, _e) = submit_work(&store, "gs-seed");
    let adapter = Delegating(&store);
    let mut session = TuiSession::new(&adapter, config(), actor());
    session.refresh(NOW).unwrap();
    session.open(&work_id, NOW).unwrap();
    assert_eq!(session.detail().unwrap().work_id, work_id);
    assert_eq!(session.board().unwrap().order(), vec![work_id]);
}

/// INV-007: store faults surface as errors — a db path that cannot be
/// opened fails the process with a message, never a silent rejection.
#[test]
fn store_faults_surface_as_errors() {
    let dir = root("fault");
    let output = Command::new(bin())
        .args([
            "--db",
            &dir.join("no-such-dir/nested.db").to_string_lossy(),
            "--config",
            &dir.join("config.json").to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("workloop-tui spawns");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("workloop-tui:"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
