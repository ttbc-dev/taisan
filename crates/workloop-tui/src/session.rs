//! The console's contract-facing state machine. Every read goes through
//! the `projection` Queries and every write through `work::handle_command`
//! under `origin: "tui"`; `work::preview` provides the advisory preflight.
//! The session holds only selection state — the last Query snapshots, the
//! open Work, the judgment draft, the staged request, and at most one
//! issued-but-unresolved request — and persists nothing of its own.

use workloop_core::command::{
    CommandBody, CommandDecision, CommandId, CommandRequest, FindingChoice, RejectionCode,
};
use workloop_core::domain::{Actor, ExecutionId, Origin, WorkId};
use workloop_core::projection::{self, CommandResultLookup, QueryResult, WorkDetailView};
use workloop_core::store::Store;
use workloop_core::work::{ActionPreview, WorkloopConfig, handle_command, preview};
use workloop_store::sqlite::SqliteStore;

use crate::board::Board;
use crate::judgment::JudgmentDraft;

/// The origin every TUI request carries — the idempotency namespace is
/// (origin, actor, key), so TUI keys never collide with CLI ones.
pub const ORIGIN: &str = "tui";

/// What a `submit` produced from the operator's point of view.
#[derive(Debug)]
pub enum SubmitOutcome {
    /// A durable decision came back — accepted or rejected.
    Decided(CommandDecision),
    /// The call returned no decision: the result is unknown and the
    /// request is tracked until `recover` settles it.
    Unknown,
}

/// How a pending request settled.
#[derive(Debug)]
pub enum RecoverOutcome {
    /// The durable decision was already on record (`command_result` by
    /// the caller's replay key).
    Found(CommandDecision),
    /// Nothing was recorded, so the identical key and payload replayed.
    Replayed(CommandDecision),
}

/// Management Commands the operator can arm against the open Work —
/// each carries the displayed Work version.
#[derive(Debug, Clone)]
pub enum Action {
    Retry,
    Resume,
    Cancel,
    ReleaseJudgment,
    Acknowledge { attention_id: String },
}

/// The TUI session over one installation's store.
pub struct TuiSession<'a> {
    store: &'a SqliteStore,
    config: WorkloopConfig,
    actor: Actor,
    /// Last `refresh` result — a render snapshot, re-derived each pass.
    board: Option<Board>,
    /// The open Work's last `work_detail` result.
    detail: Option<WorkDetailView>,
    /// The in-progress judgment over the displayed findings.
    draft: Option<JudgmentDraft>,
    /// The request the operator armed for preview/submit.
    staged: Option<CommandRequest>,
    /// A request whose response never arrived; rendered unknown until
    /// `recover` settles it. Never silently dropped or mutated.
    pending: Option<CommandRequest>,
    key_counter: u64,
}

impl<'a> TuiSession<'a> {
    pub fn new(store: &'a SqliteStore, config: WorkloopConfig, actor: Actor) -> Self {
        Self {
            store,
            config,
            actor,
            board: None,
            detail: None,
            draft: None,
            staged: None,
            pending: None,
            key_counter: 0,
        }
    }

    /// Access to the shared store for other surfaces in tests/tools —
    /// the session itself never uses it outside the Command/Query ports.
    pub fn store(&self) -> &'a SqliteStore {
        self.store
    }

    pub fn board(&self) -> Option<&Board> {
        self.board.as_ref()
    }

    pub fn detail(&self) -> Option<&WorkDetailView> {
        self.detail.as_ref()
    }

    pub fn draft(&self) -> Option<&JudgmentDraft> {
        self.draft.as_ref()
    }

    pub fn staged(&self) -> Option<&CommandRequest> {
        self.staged.as_ref()
    }

    /// The request whose result is unknown, if any.
    pub fn pending(&self) -> Option<&CommandRequest> {
        self.pending.as_ref()
    }

    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    fn next_key(&mut self) -> String {
        self.key_counter += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("tui-{}-{nanos:x}", self.key_counter)
    }

    /// Re-derive the board from the `work_list`, `capacity`, and
    /// `attention` Queries — the only reads the console performs.
    pub fn refresh(&mut self, now_unix_seconds: i64) -> Result<&Board, String> {
        let list = match self
            .store
            .transact(|tx| projection::work_list(tx, now_unix_seconds))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => value,
            other => return Err(format!("work_list: {other:?}")),
        };
        let capacity = match self
            .store
            .transact(|tx| projection::capacity(tx))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => Some(value),
            QueryResult::StoreUnavailable => None,
            other => return Err(format!("capacity: {other:?}")),
        };
        let attention = match self
            .store
            .transact(|tx| projection::attention(tx, None, now_unix_seconds))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => value,
            other => return Err(format!("attention: {other:?}")),
        };
        self.board = Some(Board::from_queries(list, capacity, attention));
        Ok(self.board.as_ref().expect("just set"))
    }

    /// Open a Work: query `work_detail` and its `attention`, then hold the
    /// snapshot as selection state. Opening a different Work drops the
    /// previous draft; an unchanged re-open keeps choices the operator
    /// already made only when the version and head still match.
    pub fn open(&mut self, work_id: &WorkId, now_unix_seconds: i64) -> Result<(), String> {
        let detail = match self
            .store
            .transact(|tx| projection::work_detail(tx, work_id, now_unix_seconds))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => value,
            other => return Err(format!("work_detail {work_id:?}: {other:?}")),
        };
        let same = self.draft.as_ref().is_some_and(|d| {
            d.work_id == detail.work_id
                && d.work_version == detail.version
                && detail.current_pr.as_ref().and_then(|(_, h)| h.as_ref()) == Some(&d.pr_head)
        });
        if !same {
            // The displayed version/head moved under the draft: choices
            // tied to the old snapshot are discarded, never retargeted.
            self.draft = None;
        }
        if self
            .staged
            .as_ref()
            .and_then(|r| r.body.work_id())
            .is_some_and(|w| w != &detail.work_id)
        {
            self.staged = None;
        }
        self.detail = Some(detail);
        Ok(())
    }

    /// Begin the judgment draft over the findings the projection scoped
    /// to the open Work's current head.
    pub fn begin_judgment(&mut self) -> Result<&JudgmentDraft, String> {
        let detail = self.detail.as_ref().ok_or("open a work first")?;
        let draft = JudgmentDraft::from_detail(detail)
            .ok_or("no head-scoped findings to judge on the open work")?;
        self.draft = Some(draft);
        Ok(self.draft.as_ref().expect("just set"))
    }

    /// Record a choice for one displayed finding id. Any change invalidates
    /// the staged request — a new one is armed with a fresh key.
    pub fn choose(&mut self, finding_id: &str, choice: FindingChoice) -> Result<(), String> {
        let draft = self.draft.as_mut().ok_or("no judgment in progress")?;
        draft.choose(finding_id, choice)?;
        self.staged = None;
        Ok(())
    }

    fn request(&mut self, body: CommandBody) -> CommandRequest {
        CommandRequest {
            actor: self.actor.clone(),
            origin: Origin(ORIGIN.into()),
            idempotency_key: self.next_key(),
            body,
        }
    }

    /// Arm the judgment Command once every displayed finding id carries a
    /// decision — the request carries the displayed Work version and head.
    pub fn stage_judgment(&mut self) -> Result<&CommandRequest, String> {
        let body = self
            .draft
            .as_ref()
            .ok_or("no judgment in progress")?
            .command_body()?;
        self.staged = Some(self.request(body));
        Ok(self.staged.as_ref().expect("just set"))
    }

    /// Arm a management Command against the open Work with the displayed
    /// version — every state-changing action is a typed Command.
    pub fn stage_action(&mut self, action: &Action) -> Result<&CommandRequest, String> {
        let detail = self.detail.as_ref().ok_or("open a work first")?;
        let work_id = detail.work_id.clone();
        let expected_work_version = detail.version;
        let body = match action {
            Action::Retry => CommandBody::Retry {
                work_id,
                expected_work_version,
                expected_execution_id: detail
                    .current_execution_id
                    .clone()
                    .ok_or("no current execution to retry")?,
            },
            Action::Resume => CommandBody::Resume {
                work_id,
                expected_work_version,
                execution_id: detail
                    .current_execution_id
                    .clone()
                    .ok_or("no current execution to resume")?,
            },
            Action::Cancel => CommandBody::Cancel {
                work_id,
                expected_work_version,
                execution_id: None,
            },
            Action::ReleaseJudgment => CommandBody::ReleaseJudgment {
                work_id,
                expected_work_version,
                expected_pr_head: detail
                    .current_pr
                    .as_ref()
                    .and_then(|(_, h)| h.clone())
                    .ok_or("no observed PR head to release against")?,
            },
            Action::Acknowledge { attention_id } => CommandBody::Acknowledge {
                work_id,
                expected_work_version,
                attention_id: attention_id.clone(),
            },
        };
        self.staged = Some(self.request(body));
        Ok(self.staged.as_ref().expect("just set"))
    }

    /// Ensure a staged request exists, building the judgment one when a
    /// complete draft is waiting.
    fn ensure_staged(&mut self) -> Result<(), String> {
        if self.staged.is_some() {
            return Ok(());
        }
        if self.draft.is_some() {
            self.stage_judgment()?;
            return Ok(());
        }
        Err("nothing staged — arm an action or finish a judgment first".into())
    }

    /// Advisory preflight of the staged request through `work::preview`.
    /// Preflight is advisory only — it reserves nothing.
    pub fn preview(&mut self, now_unix_seconds: i64) -> Result<ActionPreview, String> {
        self.ensure_staged()?;
        let req = self.staged.as_ref().expect("checked");
        preview(self.store, &self.config, req, now_unix_seconds).map_err(|e| e.to_string())
    }

    /// Submit the staged request through `work::handle_command`. A stale
    /// rejection re-queries the Work and clears the choices tied to the
    /// old head. An error leaves the request pending — its result is
    /// unknown, never assumed.
    pub fn submit(&mut self, now_unix_seconds: i64) -> Result<SubmitOutcome, String> {
        if self.pending.is_some() {
            return Err("a submitted request's result is still unknown — run recover first".into());
        }
        self.ensure_staged()?;
        let req = self.staged.clone().expect("checked");
        match handle_command(self.store, &self.config, &req, now_unix_seconds) {
            Ok(decision) => {
                self.staged = None;
                self.on_decision(&decision, now_unix_seconds)?;
                Ok(SubmitOutcome::Decided(decision))
            }
            Err(_) => {
                // No decision from the caller's point of view: render it
                // unknown and keep the identical request for recovery.
                self.staged = None;
                self.pending = Some(req);
                Ok(SubmitOutcome::Unknown)
            }
        }
    }

    /// Record that a request was issued and its response is unavailable —
    /// the state a lost response leaves. `recover` settles it.
    pub fn note_lost(&mut self, request: CommandRequest) {
        self.pending = Some(request);
    }

    /// Recover a pending request: first `command_result` by the caller's
    /// replay key, then a replay of the identical key and payload — never
    /// a mutated request while the first result is unknown.
    pub fn recover(&mut self, now_unix_seconds: i64) -> Result<RecoverOutcome, String> {
        let Some(req) = self.pending.clone() else {
            return Err("no pending request".into());
        };
        let lookup = CommandResultLookup::ReplayKey {
            origin: req.origin.clone(),
            actor: req.actor.clone(),
            idempotency_key: req.idempotency_key.clone(),
        };
        match self
            .store
            .transact(|tx| projection::command_result(tx, &lookup))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => {
                self.pending = None;
                self.on_decision(&value, now_unix_seconds)?;
                return Ok(RecoverOutcome::Found(value));
            }
            QueryResult::NotFound => {}
            other => return Err(format!("command_result: {other:?}")),
        }
        match handle_command(self.store, &self.config, &req, now_unix_seconds) {
            Ok(decision) => {
                self.pending = None;
                self.on_decision(&decision, now_unix_seconds)?;
                Ok(RecoverOutcome::Replayed(decision))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Recover a pending request by command id — the id the operator
    /// learned from another surface (the CLI's `command_result`, for
    /// example). The named durable decision settles the pending request:
    /// it is a recorded fact either way, and the pending request is
    /// never mutated by naming it.
    pub fn recover_by_id(
        &mut self,
        command_id: &CommandId,
        now_unix_seconds: i64,
    ) -> Result<RecoverOutcome, String> {
        if self.pending.is_none() {
            return Err("no pending request".into());
        }
        match self
            .store
            .transact(|tx| {
                projection::command_result(
                    tx,
                    &CommandResultLookup::Id {
                        command_id: command_id.clone(),
                    },
                )
            })
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => {
                self.pending = None;
                self.on_decision(&value, now_unix_seconds)?;
                Ok(RecoverOutcome::Found(value))
            }
            QueryResult::NotFound => Err(format!("no recorded decision for {command_id:?}")),
            other => Err(format!("command_result: {other:?}")),
        }
    }

    /// On a stale rejection, re-query the Work and clear every choice
    /// tied to the old snapshot — the expected head/version is never
    /// rewritten to resubmit the old request.
    fn on_decision(
        &mut self,
        decision: &CommandDecision,
        now_unix_seconds: i64,
    ) -> Result<(), String> {
        let CommandDecision::Rejected {
            reason,
            current_work_id,
            ..
        } = decision
        else {
            return Ok(());
        };
        if !matches!(
            reason,
            RejectionCode::StalePrHead | RejectionCode::StaleWorkVersion
        ) {
            return Ok(());
        }
        self.draft = None;
        if let Some(work_id) = current_work_id {
            self.open(work_id, now_unix_seconds)?;
            // Rebuild a clean draft over the refreshed findings — the
            // cleared choices are gone, not retargeted.
            if let Some(draft) = self.detail.as_ref().and_then(JudgmentDraft::from_detail) {
                self.draft = Some(draft);
            }
        }
        Ok(())
    }

    /// Build a request without staging it — used when a caller composes
    /// its own Command through the same TUI origin/actor.
    pub fn compose(&mut self, body: CommandBody) -> CommandRequest {
        self.request(body)
    }

    /// The currently open work's Execution id for callers that need it.
    pub fn open_execution(&self) -> Option<ExecutionId> {
        self.detail
            .as_ref()
            .and_then(|d| d.current_execution_id.clone())
    }
}
