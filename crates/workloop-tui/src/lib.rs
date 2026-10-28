//! `workloop-tui` — the management TUI: the second interface adapter
//! after the CLI. It reaches `work` only through `preview` and `submit`,
//! and `projection` only through the shared Queries — the same contract
//! surface as the CLI, with no TUI-only commands or queries and no
//! separate state model. Kanban columns derive phase, outcome, and
//! attention counts from `work_list`/`work_detail`/`attention`/`capacity`
//! results; the TUI stores selection state only and never persists a
//! card status or card ID of its own.
//!
//! The interactive terminal driver lives in `main.rs`; everything here
//! is a headless state machine over the `Contracts` port so the contract
//! behaviours (preflight, stale rejection handling, lost-response
//! recovery) are testable without a terminal.

use std::collections::BTreeMap;

use workloop_core::command::{
    ActionPreview, CommandBody, CommandDecision, CommandRequest, FindingChoice, FindingDecision,
    PreviewAvailability, RejectionCode,
};
use workloop_core::domain::{Actor, Origin, WorkId};
use workloop_core::observation::FindingRef;
use workloop_core::projection::{
    AttentionView, CapacityView, CommandResultLookup, QueryResult, WorkDetailView, WorkListView,
    WorkPhase, WorkSummary,
};
use workloop_core::store::Store;

/// The TUI's entire reach into the core: `work.preview`, `work.submit`,
/// and the projection Queries. Nothing else — no store access, no
/// adapter-private reads.
pub trait Contracts {
    /// Advisory preflight over saved facts; reserves nothing.
    fn preview(&mut self, req: &CommandRequest) -> Result<ActionPreview, String>;
    /// The state-changing Command path; an `Err` is a lost response —
    /// the Command may or may not have committed.
    fn submit(&mut self, req: &CommandRequest) -> Result<CommandDecision, String>;
    fn work_list(&mut self) -> Result<QueryResult<WorkListView>, String>;
    fn work_detail(&mut self, work_id: &WorkId) -> Result<QueryResult<WorkDetailView>, String>;
    fn attention(
        &mut self,
        work_id: Option<&WorkId>,
    ) -> Result<QueryResult<Vec<AttentionView>>, String>;
    fn capacity(&mut self) -> Result<QueryResult<CapacityView>, String>;
    /// Recover a lost submit response by command ID or replay key.
    fn command_result(
        &mut self,
        lookup: &CommandResultLookup,
    ) -> Result<QueryResult<CommandDecision>, String>;
}

/// `Contracts` over the SQLite store — the production adapter the binary
/// composes. It wires the shared contract entry points and nothing else.
/// `now` is sampled per call: a session may run for hours and the
/// projections' thresholds (uncertain attention, reservation expiry)
/// must see real elapsed time, like the CLI sampling `now()` per command.
pub struct SqliteContracts<'a> {
    pub store: &'a workloop_store::sqlite::SqliteStore,
    pub config: &'a workloop_core::work::WorkloopConfig,
    pub now: fn() -> i64,
}

impl Contracts for SqliteContracts<'_> {
    fn preview(&mut self, req: &CommandRequest) -> Result<ActionPreview, String> {
        workloop_core::work::preview(self.store, self.config, req, (self.now)())
            .map_err(|e| e.to_string())
    }

    fn submit(&mut self, req: &CommandRequest) -> Result<CommandDecision, String> {
        workloop_core::work::handle_command(self.store, self.config, req, (self.now)())
            .map_err(|e| e.to_string())
    }

    fn work_list(&mut self) -> Result<QueryResult<WorkListView>, String> {
        self.store
            .transact(|tx| workloop_core::projection::work_list(tx, (self.now)()))
            .map_err(|e| e.to_string())
    }

    fn work_detail(&mut self, work_id: &WorkId) -> Result<QueryResult<WorkDetailView>, String> {
        self.store
            .transact(|tx| workloop_core::projection::work_detail(tx, work_id, (self.now)()))
            .map_err(|e| e.to_string())
    }

    fn attention(
        &mut self,
        work_id: Option<&WorkId>,
    ) -> Result<QueryResult<Vec<AttentionView>>, String> {
        self.store
            .transact(|tx| workloop_core::projection::attention(tx, work_id, (self.now)()))
            .map_err(|e| e.to_string())
    }

    fn capacity(&mut self) -> Result<QueryResult<CapacityView>, String> {
        self.store
            .transact(|tx| workloop_core::projection::capacity(tx))
            .map_err(|e| e.to_string())
    }

    fn command_result(
        &mut self,
        lookup: &CommandResultLookup,
    ) -> Result<QueryResult<CommandDecision>, String> {
        self.store
            .transact(|tx| workloop_core::projection::command_result(tx, lookup))
            .map_err(|e| e.to_string())
    }
}

/// Input to the state machine. `main.rs` maps terminal bytes to this;
/// tests drive it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Char(char),
    /// Periodic wake — refreshes the read-only board view.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    Detail,
    Judge,
}

/// The kanban model, rebuilt from Queries on every refresh — derived
/// state, never persisted card state.
pub struct Board {
    pub items: Vec<WorkSummary>,
    pub attention: Vec<AttentionView>,
    pub capacity: Option<CapacityView>,
}

impl Board {
    fn empty() -> Self {
        Self {
            items: vec![],
            attention: vec![],
            capacity: None,
        }
    }
}

/// The operator's working set for one `judge_findings` Command: the
/// displayed Work version, the displayed PR head, the displayed findings,
/// and one choice per finding ID. Selection state only.
pub struct JudgeState {
    pub work_id: WorkId,
    pub work_version: u64,
    pub pr_head: String,
    pub findings: Vec<FindingRef>,
    /// choice per displayed finding ID — the pending decisions.
    pub choices: BTreeMap<String, FindingChoice>,
    pub cursor: usize,
    pub last_preview: Option<ActionPreview>,
}

/// A submit whose response was lost: the Command may or may not have
/// committed. Recovery is `command_result` by replay key, or replaying
/// the identical key and payload — never a mutated request.
struct PendingSubmit {
    request: CommandRequest,
}

pub struct Tui<C: Contracts> {
    contracts: C,
    actor: Actor,
    origin: Origin,
    key_seq: u64,
    screen: Screen,
    board: Board,
    selection: usize,
    detail: Option<WorkDetailView>,
    judge: Option<JudgeState>,
    pending: Option<PendingSubmit>,
    status: String,
    quit: bool,
}

impl<C: Contracts> Tui<C> {
    pub fn new(contracts: C, actor: Actor, origin: Origin) -> Self {
        Self {
            contracts,
            actor,
            origin,
            key_seq: 0,
            screen: Screen::Board,
            board: Board::empty(),
            selection: 0,
            detail: None,
            judge: None,
            pending: None,
            status: String::new(),
            quit: false,
        }
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    pub fn detail(&self) -> Option<&WorkDetailView> {
        self.detail.as_ref()
    }

    pub fn judge(&self) -> Option<&JudgeState> {
        self.judge.as_ref()
    }

    /// A lost-response submit is still unresolved.
    pub fn pending_unknown(&self) -> bool {
        self.pending.is_some()
    }

    /// Read access to the adapter — tests inspect what the TUI issued.
    pub fn contracts(&self) -> &C {
        &self.contracts
    }

    /// Mutable access to the adapter — tests arm the fault injection.
    pub fn contracts_mut(&mut self) -> &mut C {
        &mut self.contracts
    }

    /// Rebuild the board from `work_list`, `attention`, and `capacity` —
    /// the only source of the kanban view.
    pub fn refresh_board(&mut self) {
        match self.contracts.work_list() {
            Ok(QueryResult::Found { value, .. }) => self.board.items = value.items,
            Ok(_) => self.board.items = vec![],
            Err(e) => self.status = format!("work_list: {e}"),
        }
        match self.contracts.attention(None) {
            Ok(QueryResult::Found { value, .. }) => self.board.attention = value,
            _ => self.board.attention = vec![],
        }
        match self.contracts.capacity() {
            Ok(QueryResult::Found { value, .. }) => self.board.capacity = Some(value),
            _ => self.board.capacity = None,
        }
        if self.selection >= self.board.items.len() {
            self.selection = self.board.items.len().saturating_sub(1);
        }
    }

    /// Re-query the open Work detail; keeps the displayed version honest.
    pub fn refresh_detail(&mut self) {
        let Some(work_id) = self.detail.as_ref().map(|d| d.work_id.clone()) else {
            return;
        };
        match self.contracts.work_detail(&work_id) {
            Ok(QueryResult::Found { value, .. }) => self.detail = Some(value),
            Ok(QueryResult::NotFound) => {
                self.detail = None;
                self.judge = None;
                self.screen = Screen::Board;
                self.status = "work no longer exists".into();
            }
            Ok(_) => self.status = "work_detail: unavailable".into(),
            Err(e) => self.status = format!("work_detail: {e}"),
        }
    }

    fn request(&mut self, body: CommandBody) -> CommandRequest {
        self.key_seq += 1;
        CommandRequest {
            actor: self.actor.clone(),
            origin: self.origin.clone(),
            idempotency_key: format!("tui-{}", self.key_seq),
            body,
        }
    }

    /// Every state-changing action funnels here: `submit` carries the
    /// idempotency key and the displayed Work version; a lost response is
    /// recorded as pending-unknown, never retried with a mutated request.
    fn submit_and_apply(&mut self, request: CommandRequest) {
        match self.contracts.submit(&request) {
            Ok(decision) => self.apply_decision(decision),
            Err(e) => {
                self.pending = Some(PendingSubmit { request });
                self.status =
                    format!("submit response lost ({e}) — result unknown; l=lookup, r=replay");
            }
        }
    }

    fn apply_decision(&mut self, decision: CommandDecision) {
        self.pending = None;
        match &decision {
            CommandDecision::Accepted { command_id, .. } => {
                self.status = format!("accepted {}", command_id.0);
                self.after_accept();
            }
            CommandDecision::Rejected {
                reason: RejectionCode::StalePrHead | RejectionCode::StaleWorkVersion,
                ..
            } => {
                // The world moved: re-query and drop every choice bound
                // to the old head — never rewrite the expected head and
                // resubmit the old choices.
                self.reload_after_stale();
            }
            CommandDecision::Rejected {
                reason, evidence, ..
            } => {
                self.status = format!("rejected {reason:?}: {}", evidence.join("; "));
            }
        }
    }

    /// A state change landed: refresh the views the operator is looking
    /// at so the next command is built on the new displayed version.
    fn after_accept(&mut self) {
        match self.screen {
            Screen::Board => self.refresh_board(),
            Screen::Detail | Screen::Judge => {
                self.refresh_detail();
                if self.screen == Screen::Judge {
                    // The accepted judgment may move the gate; the judge
                    // screen re-reads head and findings like a stale
                    // rejection does.
                    self.reload_judge_from_detail();
                }
            }
        }
    }

    fn reload_after_stale(&mut self) {
        self.status = "stale head/version — re-queried; old choices cleared".into();
        match self.screen {
            Screen::Judge => self.reload_judge_from_detail(),
            _ => self.refresh_detail(),
        }
    }

    /// Rebuild the judge working set from a fresh `work_detail`: new
    /// displayed version, head, and finding set; choices tied to the old
    /// head are cleared.
    fn reload_judge_from_detail(&mut self) {
        let Some(judge) = self.judge.as_ref() else {
            return;
        };
        let work_id = judge.work_id.clone();
        match self.contracts.work_detail(&work_id) {
            Ok(QueryResult::Found { value, .. }) => {
                self.detail = Some(value.clone());
                let Some((_, Some(head))) = value.current_pr.clone() else {
                    self.judge = None;
                    self.screen = Screen::Detail;
                    self.status = "no current PR head — judgment no longer applies".into();
                    return;
                };
                if value.findings.is_empty() {
                    self.judge = None;
                    self.screen = Screen::Detail;
                    self.status = format!("no judgeable findings for head {head}");
                    return;
                }
                self.judge = Some(JudgeState {
                    work_id: value.work_id.clone(),
                    work_version: value.version,
                    pr_head: head,
                    findings: value.findings.clone(),
                    choices: BTreeMap::new(),
                    cursor: 0,
                    last_preview: None,
                });
            }
            Ok(QueryResult::NotFound) => {
                self.judge = None;
                self.screen = Screen::Board;
                self.status = "work no longer exists".into();
            }
            Ok(_) => self.status = "work_detail: unavailable".into(),
            Err(e) => self.status = format!("work_detail: {e}"),
        }
    }

    /// The judge Command built from the displayed state — one decision
    /// per displayed finding ID, the displayed version, the displayed
    /// head, and a fresh idempotency key per issued Command.
    fn judge_request(&mut self) -> Result<CommandRequest, String> {
        let (work_id, work_version, pr_head, decisions) = {
            let judge = self.judge.as_ref().expect("judge screen active");
            if judge.choices.len() != judge.findings.len() {
                return Err(format!(
                    "decide every displayed finding first ({}/{} chosen)",
                    judge.choices.len(),
                    judge.findings.len()
                ));
            }
            let decisions: Vec<FindingDecision> = judge
                .findings
                .iter()
                .map(|f| FindingDecision {
                    finding_id: f.finding_id.clone(),
                    choice: judge.choices[&f.finding_id].clone(),
                })
                .collect();
            (
                judge.work_id.clone(),
                judge.work_version,
                judge.pr_head.clone(),
                decisions,
            )
        };
        Ok(self.request(CommandBody::JudgeFindings {
            work_id,
            expected_work_version: work_version,
            expected_pr_head: pr_head,
            decisions,
        }))
    }

    /// `command_result` by replay key — the first recovery path for a
    /// lost response. If that comes back empty, replaying the identical
    /// request ('r') is the other; both are safe because the store keys
    /// the decision on (origin, actor, idempotency key).
    fn recover_pending(&mut self) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let lookup = CommandResultLookup::ReplayKey {
            origin: pending.request.origin.clone(),
            actor: pending.request.actor.clone(),
            idempotency_key: pending.request.idempotency_key.clone(),
        };
        match self.contracts.command_result(&lookup) {
            Ok(QueryResult::Found { value, .. }) => self.apply_decision(value),
            Ok(_) => {
                self.status = "result still unknown — 'r' replays the identical request".into()
            }
            Err(e) => self.status = format!("command_result: {e}"),
        }
    }

    /// Replay the pending request verbatim — identical idempotency key
    /// and payload; a mutation is never issued while the first result is
    /// unknown.
    fn replay_pending(&mut self) {
        let Some(request) = self.pending.as_ref().map(|p| p.request.clone()) else {
            return;
        };
        match self.contracts.submit(&request) {
            Ok(decision) => self.apply_decision(decision),
            Err(e) => self.status = format!("response lost again ({e}) — still unknown"),
        }
    }

    pub fn handle(&mut self, key: Key) {
        match key {
            Key::Char('q') => {
                // Disconnecting the TUI changes no execution truth —
                // quitting only ends this adapter's session.
                self.quit = true;
                return;
            }
            Key::Tick => {
                if self.screen == Screen::Board {
                    self.refresh_board();
                }
                return;
            }
            _ => {}
        }
        // While a submit result is unknown, only read-only navigation
        // passes through — a mutated request is never issued in place of
        // the pending one.
        if self.pending.is_some() {
            match key {
                Key::Char('l') => self.recover_pending(),
                Key::Char('r') => self.replay_pending(),
                Key::Up | Key::Down | Key::Esc | Key::Char('b') => match self.screen {
                    Screen::Board => self.handle_board(key),
                    Screen::Detail => self.handle_detail(key),
                    Screen::Judge => self.handle_judge(key),
                },
                _ => {
                    self.status =
                        "submit result unknown — resolve it first: l=lookup, r=replay".into()
                }
            }
            return;
        }
        match self.screen {
            Screen::Board => self.handle_board(key),
            Screen::Detail => self.handle_detail(key),
            Screen::Judge => self.handle_judge(key),
        }
    }

    fn handle_board(&mut self, key: Key) {
        match key {
            Key::Up => self.selection = self.selection.saturating_sub(1),
            Key::Down => {
                if self.selection + 1 < self.board.items.len() {
                    self.selection += 1;
                }
            }
            Key::Enter => {
                let Some(item) = self.board.items.get(self.selection) else {
                    return;
                };
                match self.contracts.work_detail(&item.work_id) {
                    Ok(QueryResult::Found { value, .. }) => {
                        self.detail = Some(value);
                        self.screen = Screen::Detail;
                    }
                    Ok(_) => self.status = "work_detail: unavailable".into(),
                    Err(e) => self.status = format!("work_detail: {e}"),
                }
            }
            Key::Char('r') => self.refresh_board(),
            _ => {}
        }
    }

    fn handle_detail(&mut self, key: Key) {
        let Some(detail) = self.detail.clone() else {
            self.screen = Screen::Board;
            return;
        };
        match key {
            Key::Esc | Key::Char('b') => {
                self.screen = Screen::Board;
                self.refresh_board();
            }
            Key::Char('r') => self.refresh_detail(),
            Key::Char('j') => self.open_judge(&detail),
            Key::Char('x') => {
                let request = self.request(CommandBody::Cancel {
                    work_id: detail.work_id.clone(),
                    expected_work_version: detail.version,
                    execution_id: detail.current_execution_id.clone(),
                });
                self.submit_and_apply(request);
            }
            Key::Char('t') => {
                let Some(execution_id) = detail.current_execution_id.clone() else {
                    self.status = "no execution to retry".into();
                    return;
                };
                let request = self.request(CommandBody::Retry {
                    work_id: detail.work_id.clone(),
                    expected_work_version: detail.version,
                    expected_execution_id: execution_id,
                });
                self.submit_and_apply(request);
            }
            Key::Char('u') => {
                let Some(execution_id) = detail.current_execution_id.clone() else {
                    self.status = "no execution to resume".into();
                    return;
                };
                let request = self.request(CommandBody::Resume {
                    work_id: detail.work_id.clone(),
                    expected_work_version: detail.version,
                    execution_id,
                });
                self.submit_and_apply(request);
            }
            Key::Char('a') => {
                let Some(attention_id) = detail.attention.first().map(|a| a.attention_id.clone())
                else {
                    self.status = "no attention to acknowledge".into();
                    return;
                };
                let request = self.request(CommandBody::Acknowledge {
                    work_id: detail.work_id.clone(),
                    expected_work_version: detail.version,
                    attention_id,
                });
                self.submit_and_apply(request);
            }
            Key::Char('g') => {
                let Some((_, Some(head))) = detail.current_pr.clone() else {
                    self.status = "no observed PR head".into();
                    return;
                };
                let request = self.request(CommandBody::ReleaseJudgment {
                    work_id: detail.work_id.clone(),
                    expected_work_version: detail.version,
                    expected_pr_head: head,
                });
                self.submit_and_apply(request);
            }
            _ => {}
        }
    }

    /// Enter the judge flow with the displayed version, head, and
    /// finding set — the exact set `work` validates `judge_findings`
    /// against, because both come from `work_detail`.
    fn open_judge(&mut self, detail: &WorkDetailView) {
        let Some((_, Some(head))) = detail.current_pr.clone() else {
            self.status = "no observed PR head to judge".into();
            return;
        };
        if detail.findings.is_empty() {
            self.status = "no judgeable findings for the current head".into();
            return;
        }
        self.judge = Some(JudgeState {
            work_id: detail.work_id.clone(),
            work_version: detail.version,
            pr_head: head,
            findings: detail.findings.clone(),
            choices: BTreeMap::new(),
            cursor: 0,
            last_preview: None,
        });
        self.screen = Screen::Judge;
    }

    fn handle_judge(&mut self, key: Key) {
        match key {
            Key::Esc | Key::Char('b') => {
                self.judge = None;
                self.screen = Screen::Detail;
            }
            Key::Up => {
                if let Some(j) = self.judge.as_mut() {
                    j.cursor = j.cursor.saturating_sub(1);
                }
            }
            Key::Down => {
                if let Some(j) = self.judge.as_mut()
                    && j.cursor + 1 < j.findings.len()
                {
                    j.cursor += 1;
                }
            }
            Key::Char(choice @ ('f' | 'd' | 'a' | 'w')) => {
                let Some(j) = self.judge.as_mut() else { return };
                let Some(finding) = j.findings.get(j.cursor) else {
                    return;
                };
                let choice = match choice {
                    'f' => FindingChoice::Fix,
                    'd' => FindingChoice::Dismiss,
                    'a' => FindingChoice::Accept,
                    _ => FindingChoice::Wait,
                };
                j.choices.insert(finding.finding_id.clone(), choice);
                j.last_preview = None;
            }
            Key::Char('p') => match self.judge_request() {
                Ok(request) => match self.contracts.preview(&request) {
                    Ok(preview) => {
                        self.status = preview_status(&preview);
                        if let Some(j) = self.judge.as_mut() {
                            j.last_preview = Some(preview);
                        }
                    }
                    Err(e) => self.status = format!("preview: {e}"),
                },
                Err(e) => self.status = e,
            },
            Key::Char('s') | Key::Enter => match self.judge_request() {
                Ok(request) => self.submit_and_apply(request),
                Err(e) => self.status = e,
            },
            _ => {}
        }
    }

    /// Render the current screen to text lines — the only view the
    /// terminal driver and the tests share.
    pub fn render(&self, width: usize) -> Vec<String> {
        let mut lines = match self.screen {
            Screen::Board => self.render_board(width),
            Screen::Detail => self.render_detail(),
            Screen::Judge => self.render_judge(),
        };
        if self.pending.is_some() {
            lines.push("PENDING: submit result unknown — l=lookup command_result, r=replay".into());
        }
        if !self.status.is_empty() {
            lines.push(format!("status: {}", self.status));
        }
        lines
    }

    fn render_board(&self, width: usize) -> Vec<String> {
        let columns: [WorkPhase; 10] = [
            WorkPhase::Queued,
            WorkPhase::Implementing,
            WorkPhase::Reviewing,
            WorkPhase::Converging,
            WorkPhase::Waiting,
            WorkPhase::Paused,
            WorkPhase::Stopping,
            WorkPhase::Completed,
            WorkPhase::Stopped,
            WorkPhase::Unknown,
        ];
        let col_width = (width / columns.len()).max(12);
        let mut out = Vec::new();
        let capacity = self
            .board
            .capacity
            .as_ref()
            .map(|c| format!("capacity {}/{}", c.used, c.limit))
            .unwrap_or_else(|| "capacity ?".into());
        out.push(format!(
            "workloop — {} work item(s), {} attention — {capacity}",
            self.board.items.len(),
            self.board.attention.len()
        ));
        out.push(
            columns
                .iter()
                .map(|p| pad(&phase_name(p), col_width))
                .collect::<String>(),
        );
        let height = columns
            .iter()
            .map(|p| self.board.items.iter().filter(|i| &i.phase == p).count())
            .max()
            .unwrap_or(0);
        for row in 0..height {
            let mut line = String::new();
            for p in &columns {
                let cell = self
                    .board
                    .items
                    .iter()
                    .filter(|i| &i.phase == p)
                    .nth(row)
                    .map(|i| {
                        let cursor = if self
                            .board
                            .items
                            .get(self.selection)
                            .is_some_and(|s| s.work_id == i.work_id)
                        {
                            ">"
                        } else {
                            " "
                        };
                        format!(
                            "{cursor}{} v{} !{}{}",
                            i.work_id.0,
                            i.version,
                            i.attention_count,
                            i.outcome
                                .as_ref()
                                .and_then(|o| o.code.clone())
                                .map(|c| format!(" [{c}]"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                line.push_str(&pad(&cell, col_width));
            }
            out.push(line);
        }
        out.push("enter=detail  r=refresh  q=quit".into());
        out
    }

    fn render_detail(&self) -> Vec<String> {
        let Some(d) = self.detail.as_ref() else {
            return vec!["no work selected".into()];
        };
        let mut out = vec![
            format!("work {}  v{}", d.work_id.0, d.version),
            format!(
                "executions: {}",
                if d.executions.is_empty() {
                    "none".into()
                } else {
                    d.executions
                        .iter()
                        .map(|e| format!("{} {:?} ({})", e.execution_id.0, e.state, e.ownership))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        ];
        match &d.current_pr {
            Some((pr, head)) => out.push(format!(
                "pr {}#{} head {}",
                pr.repository,
                pr.number,
                head.as_deref().unwrap_or("?")
            )),
            None => out.push("pr: none observed".into()),
        }
        if let Some(outcome) = &d.outcome {
            out.push(format!(
                "outcome: {:?} {}",
                outcome.state,
                outcome.code.as_deref().unwrap_or("")
            ));
        }
        out.push(format!("findings ({}):", d.findings.len()));
        for f in &d.findings {
            out.push(format!("  {}  {}", f.finding_id, f.summary));
        }
        out.push(format!("attention ({}):", d.attention.len()));
        for a in &d.attention {
            out.push(format!(
                "  {}  {} -> {}",
                a.attention_id, a.reason, a.next_action
            ));
        }
        if !d.evidence.is_empty() {
            out.push(format!("evidence: {}", d.evidence.join(", ")));
        }
        out.push(
            "j=judge  x=cancel  t=retry  u=resume  a=ack  g=release  r=refresh  b=board  q=quit"
                .into(),
        );
        out
    }

    fn render_judge(&self) -> Vec<String> {
        let Some(j) = self.judge.as_ref() else {
            return vec!["judge closed".into()];
        };
        let mut out = vec![format!(
            "judge_findings {} v{} @ {}",
            j.work_id.0, j.work_version, j.pr_head
        )];
        for (i, f) in j.findings.iter().enumerate() {
            let cursor = if i == j.cursor { ">" } else { " " };
            let choice = j.choices.get(&f.finding_id).map(choice_name).unwrap_or("·");
            out.push(format!(
                "{cursor}[{choice}] {}  {}",
                f.finding_id, f.summary
            ));
        }
        if let Some(p) = &j.last_preview {
            out.push(format!("preview: {}", preview_status(p)));
        }
        out.push(format!(
            "f=fix d=dismiss a=accept w=wait  p=preview  s=submit ({}/{})  b=back",
            j.choices.len(),
            j.findings.len()
        ));
        out
    }
}

fn preview_status(p: &ActionPreview) -> String {
    match p.availability {
        PreviewAvailability::Available => format!(
            "available (version {:?}, head {:?})",
            p.observed_work_version, p.observed_pr_head
        ),
        PreviewAvailability::Blocked => {
            format!("blocked: {:?} {}", p.reasons, p.evidence.join("; "))
        }
        PreviewAvailability::Unknown => format!("unknown: {}", p.evidence.join("; ")),
    }
}

fn phase_name(p: &WorkPhase) -> String {
    format!("{p:?}").to_lowercase()
}

fn choice_name(c: &FindingChoice) -> &'static str {
    match c {
        FindingChoice::Fix => "fix",
        FindingChoice::Dismiss => "dismiss",
        FindingChoice::Accept => "accept",
        FindingChoice::Wait => "wait",
    }
}

fn pad(s: &str, width: usize) -> String {
    let mut s = s.chars().take(width.saturating_sub(1)).collect::<String>();
    while s.chars().count() < width {
        s.push(' ');
    }
    s
}
