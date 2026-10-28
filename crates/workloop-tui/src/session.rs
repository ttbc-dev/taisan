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
use workloop_core::projection::{
    self, Availability, CommandResultLookup, QueryResult, SourceStatus, WorkDetailView,
};
use workloop_core::store::Store;
use workloop_core::work::{ActionPreview, WorkloopConfig, handle_command, preview};

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

/// The TUI session over one installation's store — generic over the
/// `Store` port: the adapter is coupled to the Command/Query contract,
/// not to the SQLite implementation behind it.
pub struct TuiSession<'a, S: Store> {
    store: &'a S,
    config: WorkloopConfig,
    actor: Actor,
    /// Last `refresh` result — a render snapshot, re-derived each pass.
    board: Option<Board>,
    /// The open Work's last `work_detail` result.
    detail: Option<WorkDetailView>,
    /// The `sources` the open detail arrived with — an empty finding
    /// list under a not-known review source proves nothing, and the
    /// session must say so rather than reporting "no findings".
    detail_sources: Vec<SourceStatus>,
    /// The in-progress judgment over the displayed findings.
    draft: Option<JudgmentDraft>,
    /// The request the operator armed for preview/submit.
    staged: Option<CommandRequest>,
    /// A request whose response never arrived; rendered unknown until
    /// `recover` settles it. Never silently dropped or mutated.
    pending: Option<CommandRequest>,
    key_counter: u64,
}

impl<'a, S: Store> TuiSession<'a, S> {
    pub fn new(store: &'a S, config: WorkloopConfig, actor: Actor) -> Self {
        Self {
            store,
            config,
            actor,
            board: None,
            detail: None,
            detail_sources: vec![],
            draft: None,
            staged: None,
            pending: None,
            key_counter: 0,
        }
    }

    /// Access to the shared store for other surfaces in tests/tools —
    /// the session itself never uses it outside the Command/Query ports.
    pub fn store(&self) -> &'a S {
        self.store
    }

    pub fn board(&self) -> Option<&Board> {
        self.board.as_ref()
    }

    pub fn detail(&self) -> Option<&WorkDetailView> {
        self.detail.as_ref()
    }

    /// Source availability reported with the open detail snapshot.
    pub fn detail_sources(&self) -> &[SourceStatus] {
        &self.detail_sources
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
    /// `attention` Queries — the only reads the console performs. The
    /// transactions begin deferred: pure reads never take the writer
    /// lock, so a board refresh cannot block or stall on a Command.
    pub fn refresh(&mut self, now_unix_seconds: i64) -> Result<&Board, String> {
        let list = match self
            .store
            .transact_deferred(|tx| projection::work_list(tx, now_unix_seconds))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => value,
            other => return Err(format!("work_list: {other:?}")),
        };
        let capacity = match self
            .store
            .transact_deferred(|tx| projection::capacity(tx))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => Some(value),
            QueryResult::StoreUnavailable => None,
            other => return Err(format!("capacity: {other:?}")),
        };
        let attention = match self
            .store
            .transact_deferred(|tx| projection::attention(tx, None, now_unix_seconds))
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
    /// already made only when the version and head still match. A
    /// re-query that finds the version or head moved also drops the
    /// armed request — it was built against the old observation, so it
    /// must be re-armed from the snapshot the operator now sees rather
    /// than submitted under it.
    pub fn open(&mut self, work_id: &WorkId, now_unix_seconds: i64) -> Result<(), String> {
        let (detail, sources) = match self
            .store
            .transact_deferred(|tx| projection::work_detail(tx, work_id, now_unix_seconds))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, sources, .. } => (value, sources),
            other => return Err(format!("work_detail {work_id:?}: {other:?}")),
        };
        let same = self.draft.as_ref().is_some_and(|d| {
            d.work_id == detail.work_id
                && d.work_version == detail.version
                && detail.current_pr.as_ref().and_then(|(_, h)| h.as_ref()) == Some(&d.pr_head)
                && d.covers(&detail.findings)
        });
        if !same {
            // The displayed version/head/finding set moved under the
            // draft: choices tied to the old snapshot are discarded,
            // never retargeted.
            self.draft = None;
        }
        let moved = self.detail.as_ref().is_some_and(|prev| {
            prev.version != detail.version
                || prev.current_pr.as_ref().and_then(|(_, h)| h.as_ref())
                    != detail.current_pr.as_ref().and_then(|(_, h)| h.as_ref())
                || prev
                    .findings
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    != detail
                        .findings
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
        });
        if moved
            || self
                .staged
                .as_ref()
                .and_then(|r| r.body.work_id())
                .is_some_and(|w| w != &detail.work_id)
        {
            self.staged = None;
        }
        self.detail = Some(detail);
        self.detail_sources = sources;
        Ok(())
    }

    /// Begin the judgment draft over the findings the projection scoped
    /// to the open Work's current head. Starting a new draft drops any
    /// armed request — a previously staged Command belongs to the old
    /// draft, never the fresh one. When the detail reports no findings
    /// but the review source was not actually read, the failure names
    /// the unavailable source instead of claiming review was clean.
    pub fn begin_judgment(&mut self) -> Result<&JudgmentDraft, String> {
        let detail = self.detail.as_ref().ok_or("open a work first")?;
        let Some(draft) = JudgmentDraft::from_detail(detail) else {
            let review_unread = self
                .detail_sources
                .iter()
                .any(|s| s.source == "github:review" && s.availability != Availability::Known);
            if detail.findings.is_empty()
                && detail.current_pr.as_ref().is_some_and(|(_, h)| h.is_some())
                && review_unread
            {
                return Err("review source not read — an empty finding list proves nothing".into());
            }
            return Err("no head-scoped findings to judge on the open work".into());
        };
        self.staged = None;
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
                self.on_decision(&decision, now_unix_seconds);
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
            .transact_deferred(|tx| projection::command_result(tx, &lookup))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => {
                self.pending = None;
                self.on_decision(&value, now_unix_seconds);
                return Ok(RecoverOutcome::Found(value));
            }
            QueryResult::NotFound => {}
            other => return Err(format!("command_result: {other:?}")),
        }
        match handle_command(self.store, &self.config, &req, now_unix_seconds) {
            Ok(decision) => {
                self.pending = None;
                self.on_decision(&decision, now_unix_seconds);
                Ok(RecoverOutcome::Replayed(decision))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Recover a pending request by command id — the id the operator
    /// learned from another surface (the CLI's `command_result`, for
    /// example). A CommandDecision carries no origin or idempotency
    /// key, so the named decision settles the pending request only when
    /// it is proven to be the pending request's own decision: the
    /// replay-key lookup must return the same command id. Naming an
    /// unrelated command never releases the unknown-result guard, and
    /// the pending request is never mutated.
    pub fn recover_by_id(
        &mut self,
        command_id: &CommandId,
        now_unix_seconds: i64,
    ) -> Result<RecoverOutcome, String> {
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
            .transact_deferred(|tx| projection::command_result(tx, &lookup))
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { value, .. } => {
                if value.command_id() != command_id {
                    return Err(format!(
                        "the pending request's recorded decision is {:?}, not {command_id:?} — the pending request stays unknown",
                        value.command_id()
                    ));
                }
                self.pending = None;
                self.on_decision(&value, now_unix_seconds);
                return Ok(RecoverOutcome::Found(value));
            }
            QueryResult::NotFound => {}
            other => return Err(format!("command_result: {other:?}")),
        }
        // The pending request has no recorded decision under its own
        // key: a decision named by id cannot be proven to be it, so the
        // pending request stays unknown either way.
        match self
            .store
            .transact_deferred(|tx| {
                projection::command_result(
                    tx,
                    &CommandResultLookup::Id {
                        command_id: command_id.clone(),
                    },
                )
            })
            .map_err(|e| e.to_string())?
        {
            QueryResult::Found { .. } => Err(format!(
                "{command_id:?} is a recorded decision but not the pending request's — recover by replay"
            )),
            QueryResult::NotFound => Err(format!("no recorded decision for {command_id:?}")),
            other => Err(format!("command_result: {other:?}")),
        }
    }

    /// Keep the selection snapshot honest after a decision. An accepted
    /// Command moved the durable version, so the Work is re-queried and
    /// the next armed action carries the post-Command version; a stale
    /// rejection re-queries too and clears every choice tied to the old
    /// snapshot — the expected head/version is never rewritten to
    /// resubmit the old request. A failed re-query drops the snapshot
    /// rather than leaving a stale version armed — the decision itself
    /// is always returned to the caller.
    fn on_decision(&mut self, decision: &CommandDecision, now_unix_seconds: i64) {
        match decision {
            CommandDecision::Accepted {
                work_id: Some(work_id),
                ..
            } => {
                if self.open(work_id, now_unix_seconds).is_err() {
                    self.detail = None;
                }
            }
            CommandDecision::Rejected {
                reason: RejectionCode::StalePrHead | RejectionCode::StaleWorkVersion,
                current_work_id,
                ..
            } => {
                self.draft = None;
                if let Some(work_id) = current_work_id {
                    match self.open(work_id, now_unix_seconds) {
                        Ok(()) => {
                            // Rebuild a clean draft over the refreshed
                            // findings — the cleared choices are gone,
                            // not retargeted.
                            if let Some(draft) =
                                self.detail.as_ref().and_then(JudgmentDraft::from_detail)
                            {
                                self.draft = Some(draft);
                            }
                        }
                        Err(_) => self.detail = None,
                    }
                }
            }
            _ => {}
        }
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
