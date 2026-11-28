//! Remote lifecycle reconciliation (`docs/executor-transport-contract.md`
//! §14, #378): the coordinator-side driver that re-adopts remote work
//! across restarts, disconnects, and lost answers — never dispatching a
//! replacement writer, never releasing a reservation without a durable
//! stop proof bound to the recorded incarnation.
//!
//! The invariants this module holds:
//!
//! - **Single writer.** A bound request admits one `start` command and
//!   one live writer; the `executor_operations` ledger plus the
//!   `executor_one_start` index make a second writer structurally
//!   refused. An unanswered or `unknown` start is queried under the
//!   recorded identity, never re-dispatched.
//! - **Persist-before-send.** Every command this module sends was first
//!   committed to `executor_operations`; every receipt attaches to its
//!   recorded command. A coordinator restart replays the same bytes.
//! - **Durable stop intent.** A recorded `request_stop` command is the
//!   stop intent; restart re-drives intent → signal → inspect until a
//!   `stopped` receipt carrying a proof bound to
//!   `bound_incarnation_id` is recorded.
//! - **No release without proof.** Writer release requires a `stopped`
//!   receipt whose `proof.incarnation_id` equals the binding's
//!   `bound_incarnation_id`. `running`, `exited` (leader gone,
//!   descendants unproven), `unknown`, `unavailable`, and transport
//!   faults never release the reservation.
//! - **Fail-closed fencing.** An incarnation change under a
//!   possibly-live writer, a ledger disagreement, a corrupt durable
//!   row, or an exhausted bounded budget emits a `RemoteFence` fact —
//!   the Invocation stays an unresolved writer and the named Attention
//!   carries the fence class.
//! - **Bounded retry.** Consecutive unproductive steps persist in the
//!   `executor_reconcile` cursor; `transport_max_retries` bounds them
//!   and the persisted `transport_backoff_seconds` schedule gates
//!   re-drives. A fault that never reached an operation still counts —
//!   that is what the cursor exists for.
//! - **Deadlines carry.** The absolute deadline recorded in the binding
//!   bounds every minted operation's remaining budget; replayed
//!   commands keep their original fields, so a restart can never
//!   refund runtime.

use serde_json::json;
use workloop_core::domain::{
    DesiredAction, Execution, ExecutionState, Invocation, InvocationId, InvocationRequest,
    WorkPolicy,
};
use workloop_core::executor::{
    ExecutorBinding, ExecutorBindingFields, ExecutorCommandV1, ExecutorIdentity, ExecutorOperation,
    ExecutorReceiptV1, ExecutorReconcile, OperationKind,
};
use workloop_core::observation::{
    DeliveryId, DeliveryScope, Observation, ObservationFact, ProcessStateFact, RemoteFenceClass,
};
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig, handle_observation};

use crate::supervisor::ExecError;

/// The replay fan-out one pass admits for one request — a restart
/// behind many unreceipted operations drains them across passes, so a
/// burst cannot starve the other bound requests.
const MAX_OPS_PER_PASS: usize = 8;

/// The transport seam reconciliation drives: one connected endpoint
/// session. The real adapter (`workloop-executor`'s `RemoteExecutor`)
/// and the fault-matrix fixture transports both implement it — the
/// module never constructs a transport itself, so a reconcile pass is
/// provable entirely behind scripted endpoints (the issue's
/// no-live-deployment boundary).
pub trait RemoteTransport {
    /// The responder incarnation the session's hello bound. Commands
    /// still name the *recorded* binding incarnation — this answers
    /// which endpoint boot is actually answering, for the fencing
    /// comparison.
    fn session_incarnation(&self) -> &str;
    /// Send a command already committed to `executor_operations` and
    /// return its verified receipt. The transport sends the recorded
    /// canonical bytes verbatim and verifies the receipt against the
    /// command before returning it.
    fn issue_recorded(
        &mut self,
        command: &ExecutorCommandV1,
    ) -> Result<ExecutorReceiptV1, TransportError>;
}

/// A session-level fault: the transport broke, or a receipt could not
/// be verified against its command (foreign or stale evidence). Neither
/// is an executor answer — both are bounded-retry inputs, never proof.
#[derive(Debug)]
pub struct TransportError {
    pub detail: String,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for TransportError {}

/// What the durable start record proves about the remote writer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WriterState {
    /// No `start` operation was ever recorded — the binding was
    /// committed but dispatch never completed a start (a crash between
    /// bind and start). Dispatch owns the next step.
    BoundOnly,
    /// A `prepared` receipt exists but no `start` was recorded —
    /// the crash window between prepare and start; dispatch owns it.
    PreparedNotStarted,
    /// A `start` command is recorded but unreceipted: it may have
    /// spawned before its answer was lost — a possibly-live writer.
    UnreceiptedStart,
    /// The start answer is recorded `unknown`: the endpoint's own
    /// record could not prove the outcome — a possibly-live writer.
    UnknownStart,
    /// A `started` receipt is recorded: the writer is live until a
    /// bound proof says otherwise.
    Live,
    /// `known_not_started` or a typed `refused`: durable proof no
    /// process exists for this start identity.
    NeverStarted { reason: String },
    /// A `stopped` receipt whose proof binds `bound_incarnation_id`:
    /// the writer is proven gone — the reservation may release.
    Terminal,
}

/// Whether the state admits a possibly-live writer — the cases that
/// keep the reservation and the fencing rules alive.
fn possibly_live(state: &WriterState) -> bool {
    matches!(
        state,
        WriterState::UnreceiptedStart | WriterState::UnknownStart | WriterState::Live
    )
}

/// What one reconcile pass did for each bound request.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Requests whose recorded live writer was (re-)adopted — a
    /// `Running` fact was recorded this pass.
    pub adopted: Vec<String>,
    /// Requests whose writer was proven gone by a bound-incarnation
    /// stop proof — an `Exited` fact was recorded this pass.
    pub proven: Vec<String>,
    /// Requests fenced this pass, with the fence class emitted.
    pub fenced: Vec<(String, RemoteFenceClass)>,
    /// Requests proven never to have started — a `LaunchRefused` fact
    /// was recorded this pass.
    pub refused: Vec<String>,
    /// Requests waiting on their persisted backoff slot.
    pub held: Vec<String>,
    /// Bound requests not yet started — dispatch owns them.
    pub awaiting_dispatch: Vec<String>,
    /// Requests that counted a bounded fault this pass (detail attached).
    pub faults: Vec<(String, String)>,
}

/// The `ensure_started` answer: the recorded start outcome, queried
/// under the original request identity — an already-recorded start is
/// returned, never re-minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// A `started` receipt is on record for the request's start
    /// identity (freshly issued or replayed from the ledger).
    Started { evidence_ref: String },
    /// Durable proof no process exists for the recorded start.
    KnownNotStarted { reason: String },
    /// The endpoint typed a refusal for the recorded start.
    Refused { reason: String },
    /// The start's outcome is unproven — `unknown` or unanswered.
    /// The caller reconciles; a second start is never minted.
    Unknown { reason: String },
    /// The session's responder incarnation differs from the binding's
    /// recorded incarnation — the endpoint restarted; a start under it
    /// is refused, and the reconcile pass fences the writer.
    ForeignIncarnation,
    /// A bound-incarnation `stopped` proof is on record — the writer
    /// is already proven gone, so the start question is closed; the
    /// caller never re-reports liveness.
    Terminal,
    /// The request's reconcile cursor records a fence — a fenced
    /// request mints and sends nothing (contract §14); the recorded
    /// start question waits on the reconcile pass, never a wire send.
    Fenced,
    /// No executor binding exists for the request — not a remote
    /// request at all.
    Unbound,
    /// No `prepared` receipt is on record, so no `start` command may
    /// be minted — prepare belongs to dispatch.
    Unprepared,
}

// ---------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------

/// Run one reconciliation pass over every durable executor binding:
/// replay unreceipted operations verbatim, adopt recorded live
/// writers, drive durable stop intents to proof, and emit `RemoteFence`
/// facts where the bounded budget or a ledger disagreement closes the
/// request. Each bound request is reconciled independently — one
/// fenced or faulting request never blocks the pass for the others.
///
/// `transport` is one connected endpoint session; its negotiated
/// incarnation is compared against each binding's recorded
/// `bound_incarnation_id` for the fencing decision. `now_unix_seconds`
/// anchors both the persisted backoff schedule and the recorded facts.
pub fn reconcile_once(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    now_unix_seconds: i64,
) -> Result<ReconcileReport, ExecError> {
    let mut report = ReconcileReport::default();
    let bindings = store.transact(|tx| tx.executor_bindings())?;
    let session_incarnation = transport.session_incarnation().to_string();
    for binding in bindings {
        reconcile_one(
            store,
            config,
            transport,
            &session_incarnation,
            &binding,
            now_unix_seconds,
            &mut report,
        )?;
    }
    Ok(report)
}

/// The idempotent start primitive (`docs/executor-transport-contract.md`
/// §14): return the recorded start outcome for a bound request —
/// re-issuing the already-recorded start identity replays it, never
/// mints a replacement. A start recorded but unanswered is replayed
/// verbatim; absent any start record, one is minted only against a
/// recorded `prepared` receipt — persist-before-send in both cases.
pub fn ensure_started(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    request_id: &str,
    now_unix_seconds: i64,
) -> Result<StartOutcome, ExecError> {
    let Some(binding) = store.transact(|tx| tx.executor_binding(request_id))? else {
        return Ok(StartOutcome::Unbound);
    };
    let fields = binding.fields().map_err(|e| {
        ExecError::Fault(format!(
            "executor binding {request_id} failed verification: {e}"
        ))
    })?;
    // A fenced request mints and sends nothing (contract §14): the
    // recorded start question waits on the reconcile pass, never a
    // wire send. The gate reads the stored value raw — a class outside
    // the closed vocabulary fails closed, never silently unfences.
    if store
        .transact(|tx| tx.executor_reconcile(request_id))?
        .is_some_and(|c| c.fenced_class.is_some())
    {
        return Ok(StartOutcome::Fenced);
    }
    if transport.session_incarnation() != fields.bound_incarnation_id {
        return Ok(StartOutcome::ForeignIncarnation);
    }
    let ops = store.transact(|tx| tx.executor_operations_for_request(request_id))?;
    // A bound-incarnation terminal record closes the start question:
    // the recorded `started` receipt never out-votes a `stopped` proof
    // (`writer_state` calls the same shape `Terminal`).
    if terminal_receipt(&ops, &fields.bound_incarnation_id).is_some() {
        return Ok(StartOutcome::Terminal);
    }
    let start = ops
        .iter()
        .find(|o| o.operation_kind == OperationKind::Start);
    let receipt = match start {
        Some(op) => match issue_or_replay(store, transport, op)? {
            Answer::Receipt(r) => Some(*r),
            Answer::Fault => {
                return Ok(StartOutcome::Unknown {
                    reason: "start answer could not be delivered".into(),
                });
            }
        },
        None => {
            // Mint the one start this request admits — only against a
            // recorded `prepared` receipt (the staged identity start
            // names). The op id is deterministic so a crashed
            // dispatch re-mints the identical command: identical
            // bytes under the same id replay, different bytes
            // conflict — never a second writer.
            let Some(prepared_id) = find_receipt(&ops, OperationKind::Prepare).and_then(|r| {
                r.result
                    .get("prepared_id")
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
            }) else {
                return Ok(StartOutcome::Unprepared);
            };
            let mut payload = deadline_payload(&fields, now_unix_seconds);
            payload["prepared_id"] = json!(prepared_id);
            payload["launch_config_digest"] = json!(fields.launch_config_digest);
            let command = mint_command(&fields, OperationKind::Start, 0, payload)?;
            let op = ExecutorOperation::new(&command)
                .map_err(|e| ExecError::Fault(format!("start command cannot seal: {e}")))?;
            record_and_issue(store, transport, op)?
        }
    };
    let status = receipt
        .as_ref()
        .and_then(|r| r.result.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let outcome = match status {
        "started" => {
            let evidence_ref = receipt
                .as_ref()
                .and_then(|r| r.result.get("process"))
                .and_then(|p| p.get("evidence_ref"))
                .and_then(|e| e.as_str())
                .unwrap_or_default()
                .to_string();
            // Adopt immediately — a restart between receipt and the
            // next reconcile pass must not leave the proven live
            // writer unreported. The delivery id makes re-reporting
            // under the same receipt digest a recorded duplicate.
            if let Some(request) = store.transact(|tx| tx.request(request_id))?
                && let Some(invocation) =
                    store.transact(|tx| tx.invocation_for_request(request_id))?
                && invocation.state.is_unresolved_writer()
            {
                report_process(
                    store,
                    config,
                    &request,
                    invocation.id.clone(),
                    &remote_identity(&fields),
                    ProcessStateFact::Running,
                    &format!(
                        "rec:{request_id}:running:{}",
                        receipt
                            .as_ref()
                            .map(|r| r.receipt_digest.clone())
                            .unwrap_or_default()
                    ),
                    now_unix_seconds,
                )?;
            }
            StartOutcome::Started { evidence_ref }
        }
        "known_not_started" => StartOutcome::KnownNotStarted {
            reason: receipt_reason(receipt.as_ref()),
        },
        "refused" => StartOutcome::Refused {
            reason: receipt_reason(receipt.as_ref()),
        },
        _ => StartOutcome::Unknown {
            reason: receipt_reason(receipt.as_ref()),
        },
    };
    Ok(outcome)
}

// ---------------------------------------------------------------------
// Operation issue: persist-before-send and verbatim replay
// ---------------------------------------------------------------------

enum Answer {
    Receipt(Box<ExecutorReceiptV1>),
    Fault,
}

/// Issue one operation's recorded command — verbatim whether the row
/// has a receipt or not: a recorded receipt is durable truth returned
/// without touching the wire; an unreceipted one travels the wire as
/// the same bytes a first send would have.
fn issue_or_replay(
    store: &impl Store,
    transport: &mut impl RemoteTransport,
    op: &ExecutorOperation,
) -> Result<Answer, ExecError> {
    if let Some(body) = &op.receipt_body {
        let receipt = ExecutorReceiptV1::parse(body).map_err(|e| {
            ExecError::Fault(format!(
                "recorded receipt {}/{} unparseable: {e}",
                op.request_id, op.operation_id
            ))
        })?;
        return Ok(Answer::Receipt(Box::new(receipt)));
    }
    let command = ExecutorCommandV1::parse(&op.command_body).map_err(|e| {
        ExecError::Fault(format!(
            "recorded command {}/{} unparseable: {e}",
            op.request_id, op.operation_id
        ))
    })?;
    match transport.issue_recorded(&command) {
        Ok(receipt) => {
            attach_receipt(store, &command, &receipt)?;
            Ok(Answer::Receipt(Box::new(receipt)))
        }
        Err(_) => Ok(Answer::Fault),
    }
}

fn attach_receipt(
    store: &impl Store,
    command: &ExecutorCommandV1,
    receipt: &ExecutorReceiptV1,
) -> Result<(), ExecError> {
    let body = receipt
        .to_canonical_bytes()
        .map_err(|e| ExecError::Fault(format!("receipt cannot seal: {e}")))?;
    store.transact(|tx| {
        tx.apply(&Effect::RecordExecutorReceipt {
            request_id: command.request_id.clone(),
            operation_id: command.operation_id.clone(),
            receipt_body: body,
        })
    })?;
    Ok(())
}

/// Commit the command first, then send it — persist-before-send in
/// one ordering a crash can never split. A transport fault leaves the
/// recorded row unreceipted for the next pass to replay.
fn record_and_issue(
    store: &impl Store,
    transport: &mut impl RemoteTransport,
    op: ExecutorOperation,
) -> Result<Option<ExecutorReceiptV1>, ExecError> {
    let command = ExecutorCommandV1::parse(&op.command_body)
        .map_err(|e| ExecError::Fault(format!("minted command unparseable: {e}")))?;
    store.transact(|tx| tx.apply(&Effect::RecordExecutorCommand { operation: op }))?;
    match transport.issue_recorded(&command) {
        Ok(receipt) => {
            attach_receipt(store, &command, &receipt)?;
            Ok(Some(receipt))
        }
        Err(_) => Ok(None),
    }
}

fn mint_command(
    fields: &ExecutorBindingFields,
    kind: OperationKind,
    ordinal: usize,
    payload: serde_json::Value,
) -> Result<ExecutorCommandV1, ExecError> {
    ExecutorCommandV1::new(
        ExecutorIdentity {
            installation_id: fields.installation_id.clone(),
            endpoint_id: fields.endpoint_id.clone(),
            endpoint_identity_digest: fields.endpoint_identity_digest.clone(),
            bound_incarnation_id: fields.bound_incarnation_id.clone(),
            request_id: fields.request_id.clone(),
            invocation_id: fields.invocation_id.clone(),
            request_digest: fields.request_digest.clone(),
        },
        format!("{}:{}:{}", fields.request_id, kind.as_str(), ordinal + 1),
        kind,
        payload,
    )
    .map_err(|e| ExecError::Fault(format!("{kind:?} command cannot seal: {e}")))
}

fn find_receipt(ops: &[ExecutorOperation], kind: OperationKind) -> Option<ExecutorReceiptV1> {
    ops.iter()
        .filter(|o| o.operation_kind == kind)
        .filter_map(|o| o.receipt_body.as_ref())
        .filter_map(|body| ExecutorReceiptV1::parse(body).ok())
        .next_back()
}

/// The recorded start outcome — the single `start` slot's verdict plus
/// the terminal override a bound-incarnation stop proof carries.
fn writer_state(ops: &[ExecutorOperation], bound_incarnation_id: &str) -> WriterState {
    if terminal_receipt(ops, bound_incarnation_id).is_some() {
        return WriterState::Terminal;
    }
    let Some(start) = ops
        .iter()
        .find(|o| o.operation_kind == OperationKind::Start)
    else {
        return if find_receipt(ops, OperationKind::Prepare).is_some() {
            WriterState::PreparedNotStarted
        } else {
            WriterState::BoundOnly
        };
    };
    let Some(body) = &start.receipt_body else {
        return WriterState::UnreceiptedStart;
    };
    let Ok(receipt) = ExecutorReceiptV1::parse(body) else {
        return WriterState::UnknownStart;
    };
    match receipt.result.get("status").and_then(|s| s.as_str()) {
        Some("started") => WriterState::Live,
        Some("known_not_started") | Some("refused") => WriterState::NeverStarted {
            reason: receipt
                .result
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },
        _ => WriterState::UnknownStart,
    }
}

/// The recorded `stopped` receipt whose proof names the bound
/// incarnation — the only release the reservation admits.
fn terminal_receipt(
    ops: &[ExecutorOperation],
    bound_incarnation_id: &str,
) -> Option<ExecutorReceiptV1> {
    ops.iter()
        .filter(|o| {
            matches!(
                o.operation_kind,
                OperationKind::Observe | OperationKind::RequestStop
            )
        })
        .filter_map(|o| o.receipt_body.as_ref())
        .filter_map(|body| ExecutorReceiptV1::parse(body).ok())
        .find(|receipt| {
            receipt.result.get("status").and_then(|s| s.as_str()) == Some("stopped")
                && proof_binds(receipt, bound_incarnation_id)
        })
}

fn proof_binds(receipt: &ExecutorReceiptV1, bound_incarnation_id: &str) -> bool {
    receipt
        .result
        .get("proof")
        .and_then(|p| p.get("incarnation_id"))
        .and_then(|i| i.as_str())
        == Some(bound_incarnation_id)
}

fn wants_stop(state: &ExecutionState, desired: &Option<DesiredAction>) -> bool {
    match state {
        ExecutionState::Stopping | ExecutionState::Paused => true,
        ExecutionState::Uncertain => matches!(
            desired,
            Some(DesiredAction::Cancel) | Some(DesiredAction::Fail) | Some(DesiredAction::Pause)
        ),
        s if s.is_terminal() => true,
        _ => false,
    }
}

fn wants_run(state: &ExecutionState, desired: &Option<DesiredAction>) -> bool {
    match state {
        ExecutionState::Ready | ExecutionState::Running | ExecutionState::Waiting => true,
        ExecutionState::Uncertain => desired.is_none() || *desired == Some(DesiredAction::Continue),
        _ => false,
    }
}

// ---------------------------------------------------------------------
// The per-request pass
// ---------------------------------------------------------------------

/// The conflicts contract §14 maps to `ledger_conflict`: a reconcile
/// cursor CAS loss or a refused write on the executor operation
/// ledger — a mint, a receipt attach, a binding check. A constraint
/// refusal on any other surface — an ingest-side CAS inside
/// `handle_observation`, another table's row check — is transient
/// contention, never the durable ledger disagreement the fence is
/// named for. The surface is named by the store's conflict detail,
/// the same convention `is_dedupe_conflict` uses for the observations
/// dedupe index.
fn is_reconcile_ledger_conflict(detail: &str) -> bool {
    [
        "executor reconcile cursor",
        "executor operation",
        "executor command",
        "executor binding",
        "executor receipt",
        "conflicting_replay",
    ]
    .iter()
    .any(|prefix| detail.starts_with(prefix))
}

#[allow(clippy::too_many_arguments)]
fn reconcile_one(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    session_incarnation: &str,
    binding: &ExecutorBinding,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let request_id = binding.request_id.clone();
    match reconcile_one_inner(
        store,
        config,
        transport,
        session_incarnation,
        binding,
        now_unix_seconds,
        report,
    ) {
        // A reconcile-ledger write refused by the store — a cursor CAS
        // loss or a local replay conflict — is the documented
        // `ledger_conflict` fence (contract §14): the contested
        // request closes, the pass reconciles every other binding.
        Err(ExecError::Store(StoreError::ConstraintConflict { detail }))
            if is_reconcile_ledger_conflict(&detail) =>
        {
            emit_fence(
                store,
                config,
                &request_id,
                RemoteFenceClass::LedgerConflict,
                &format!("reconcile write refused: {detail}"),
                now_unix_seconds,
                report,
            )
        }
        // A constraint refusal on any other surface is transient
        // contention, not a ledger disagreement: the request defers to
        // the next pass as a reported fault — it never takes a
        // permanent fence for a write the ledger did not refuse.
        Err(ExecError::Store(StoreError::ConstraintConflict { detail })) => {
            report.faults.push((
                request_id,
                format!("reconcile write contested outside the ledger: {detail}"),
            ));
            Ok(())
        }
        // A typed per-request fault — a rejected ingest, an
        // unparseable durable row — defers this request the same way;
        // one request's fault never aborts the pass. Store and IO
        // failures still propagate: they are infrastructure, not one
        // request's contention.
        Err(ExecError::Fault(detail)) => {
            report.faults.push((request_id, detail));
            Ok(())
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_one_inner(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    session_incarnation: &str,
    binding: &ExecutorBinding,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let request_id = binding.request_id.clone();
    let now_ms = now_unix_seconds.saturating_mul(1000);

    // --- durable reads --------------------------------------------------
    let fields = match binding.fields() {
        Ok(fields) => fields,
        Err(e) => {
            return emit_fence(
                store,
                config,
                &request_id,
                RemoteFenceClass::RecordCorrupt,
                &format!("executor binding failed verification: {e}"),
                now_unix_seconds,
                report,
            );
        }
    };
    let ops = match store.transact(|tx| tx.executor_operations_for_request(&request_id)) {
        Ok(ops) => ops,
        Err(e) => {
            let detail = e.to_string();
            if detail.contains("integrity") {
                return emit_fence(
                    store,
                    config,
                    &request_id,
                    RemoteFenceClass::RecordCorrupt,
                    &format!("operation ledger failed verification: {detail}"),
                    now_unix_seconds,
                    report,
                );
            }
            return Err(ExecError::Store(e));
        }
    };
    let request = store.transact(|tx| tx.request(&request_id))?;
    let invocation = store.transact(|tx| tx.invocation_for_request(&request_id))?;
    let execution = match &request {
        Some(r) => store.transact(|tx| tx.execution(&r.execution_id))?,
        None => None,
    };
    let prior = store.transact(|tx| tx.executor_reconcile(&request_id))?;
    let policy = match &request {
        Some(r) => store
            .transact(|tx| tx.work(&r.work_id))?
            .map(|w| w.policy)
            .unwrap_or_default(),
        None => WorkPolicy::default(),
    };
    let max_faults = policy.transport_max_retries.max(1);

    // --- adoption: durable terminal records first ------------------------
    // A bound-incarnation proof is adoptable under any responder —
    // the endpoint replays durable evidence across restarts, and the
    // coordinator trusts only the receipt's proof binding, never the
    // session.
    if let Some(receipt) = terminal_receipt(&ops, &fields.bound_incarnation_id) {
        adopt_terminal(
            store,
            config,
            &request,
            &invocation,
            &fields,
            &receipt,
            now_unix_seconds,
            report,
        )?;
        return reset_cursor(store, &request_id, &prior);
    }

    // --- durable fence: frozen, fact at-least-once ------------------------
    // A fenced request mints and sends nothing (contract §14): no
    // replay, no new operation — it waits only on the durable terminal
    // adoption handled above. The RemoteFence fact is re-reported under
    // its recorded delivery id: a crash between the fence commit and
    // the fact write otherwise leaves the named Attention never
    // emitted, and ingest dedup makes a landed fact a recorded
    // duplicate.
    if let Some((stored_class, class)) = stored_fence(prior.as_ref()) {
        if let Some(request) = &request {
            refence_fact(
                store,
                config,
                request,
                invocation.as_ref().map(|i| i.id.clone()),
                &stored_class,
                class,
                &request_id,
                now_unix_seconds,
                report,
            )?;
        }
        report.fenced.push((request_id, class));
        return Ok(());
    }

    // --- persisted backoff gate ------------------------------------------
    if prior
        .as_ref()
        .is_some_and(|c| c.fenced_class.is_none() && now_ms < c.next_attempt_unix_ms)
    {
        report.held.push(request_id);
        return Ok(());
    }

    // --- replay: every unreceipted operation verbatim --------------------
    let mut issued = 0usize;
    for op in ops.iter().filter(|o| o.receipt_body.is_none()) {
        if issued >= MAX_OPS_PER_PASS {
            break;
        }
        issued += 1;
        if let Answer::Fault = issue_or_replay(store, transport, op)? {
            return count_fault(
                store,
                config,
                &request_id,
                &prior,
                &policy,
                max_faults,
                RemoteFenceClass::TransportExhausted,
                &format!("transport fault replaying {}", op.operation_id),
                now_unix_seconds,
                report,
            );
        }
    }
    let ops = if issued > 0 {
        // Re-read after replays — receipts recorded this pass change
        // the durable picture.
        store.transact(|tx| tx.executor_operations_for_request(&request_id))?
    } else {
        ops
    };
    reconcile_driven(
        store,
        config,
        transport,
        session_incarnation,
        &fields,
        &ops,
        request,
        invocation,
        execution,
        prior,
        &policy,
        max_faults,
        now_unix_seconds,
        report,
    )
}

/// The state machine after every unreceipted op has been replayed:
/// fenced bookkeeping, incarnation gating, the stop drive, and the
/// observe drive.
#[allow(clippy::too_many_arguments)]
fn reconcile_driven(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    session_incarnation: &str,
    fields: &ExecutorBindingFields,
    ops: &[ExecutorOperation],
    request: Option<InvocationRequest>,
    invocation: Option<Invocation>,
    execution: Option<Execution>,
    prior: Option<ExecutorReconcile>,
    policy: &WorkPolicy,
    max_faults: u32,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let request_id = fields.request_id.clone();
    let state = writer_state(ops, &fields.bound_incarnation_id);
    let fenced = stored_fence(prior.as_ref());
    if let Some((_stored_class, class)) = fenced {
        // A fenced request only ever waits on durable terminal
        // adoption (handled before this point); no new operation is
        // minted or driven under a fence.
        report.fenced.push((request_id.clone(), class));
        return Ok(());
    }

    // --- terminal record minted inside this pass ---------------------------
    // A receipt the replay stage attached may carry the bound stop
    // proof — adopt it under the same rule the pre-replay check uses.
    if state == WriterState::Terminal {
        let receipt = terminal_receipt(ops, &fields.bound_incarnation_id)
            .expect("terminal state implies a bound proof receipt");
        adopt_terminal(
            store,
            config,
            &request,
            &invocation,
            fields,
            &receipt,
            now_unix_seconds,
            report,
        )?;
        return reset_cursor(store, &request_id, &prior);
    }

    // --- writer proven never started -------------------------------------
    if let WriterState::NeverStarted { reason } = &state {
        // A `refused` answer naming `conflicting_replay` is not a
        // launch refusal — it is the endpoint's replay fence telling
        // us the two ledgers disagree about a recorded operation
        // identity. Nothing proceeds under disagreement.
        if reason == "conflicting_replay" {
            return emit_fence(
                store,
                config,
                &request_id,
                RemoteFenceClass::ConflictingReplay,
                "the endpoint refused a recorded operation as conflicting_replay",
                now_unix_seconds,
                report,
            );
        }
        if let (Some(request), Some(invocation)) = (&request, &invocation)
            && invocation.state.is_unresolved_writer()
        {
            report_fact(
                store,
                config,
                request,
                ObservationFact::LaunchRefused {
                    invocation_id: invocation.id.clone(),
                    reason: format!("remote_{reason}"),
                },
                format!("rec:{request_id}:refused:{reason}"),
                now_unix_seconds,
            )?;
        }
        report.refused.push(request_id.clone());
        return reset_cursor(store, &request_id, &prior);
    }

    // --- no possibly-live writer: nothing to reconcile --------------------
    if !possibly_live(&state) {
        report.awaiting_dispatch.push(request_id.clone());
        return reset_cursor(store, &request_id, &prior);
    }

    // --- incarnation fence -------------------------------------------------
    // A different responder cannot inspect or stop the recorded
    // identities. Only durable terminal records adopt (handled above);
    // everything else possibly-live fences.
    if session_incarnation != fields.bound_incarnation_id {
        // One probe under the recorded binding before fencing: the
        // endpoint may have committed the stop proof just before its
        // restart — a replayed observe still carries it.
        let command = mint_command(
            fields,
            OperationKind::Observe,
            count_kind(ops, OperationKind::Observe),
            deadline_payload(fields, now_unix_seconds),
        )?;
        let op = ExecutorOperation::new(&command)
            .map_err(|e| ExecError::Fault(format!("observe command cannot seal: {e}")))?;
        return match record_and_issue(store, transport, op)? {
            Some(receipt)
                if receipt.result.get("status").and_then(|s| s.as_str()) == Some("stopped")
                    && proof_binds(&receipt, &fields.bound_incarnation_id) =>
            {
                adopt_terminal(
                    store,
                    config,
                    &request,
                    &invocation,
                    fields,
                    &receipt,
                    now_unix_seconds,
                    report,
                )?;
                reset_cursor(store, &request_id, &prior)
            }
            Some(_) | None => emit_fence(
                store,
                config,
                &request_id,
                RemoteFenceClass::IncarnationChanged,
                &format!(
                    "responder incarnation {session_incarnation} differs from bound \
                     {} with a possibly-live writer",
                    fields.bound_incarnation_id
                ),
                now_unix_seconds,
                report,
            ),
        };
    }

    // --- same incarnation: the stop drive ---------------------------------
    let exec_state = execution.as_ref().map(|e| e.state.clone());
    let desired = execution.as_ref().and_then(|e| e.desired_action.clone());
    if exec_state.as_ref().is_some_and(|s| wants_stop(s, &desired)) {
        return drive_stop(
            store,
            config,
            transport,
            fields,
            ops,
            &request,
            &invocation,
            &prior,
            policy,
            max_faults,
            now_unix_seconds,
            report,
        );
    }

    // --- same incarnation: the observe drive -------------------------------
    if exec_state.as_ref().is_some_and(|s| wants_run(s, &desired)) {
        return drive_observe(
            store,
            config,
            transport,
            fields,
            ops,
            &request,
            &invocation,
            &prior,
            policy,
            max_faults,
            now_unix_seconds,
            report,
        );
    }
    // The execution neither wants the writer running nor stopped —
    // `queued` dispatch or a missing execution row; the reservation
    // holds untouched.
    Ok(())
}

/// The contract §8 deadline triple every minted operation carries: the
/// absolute authorized-runtime deadline, the coordinator's send time,
/// and the remaining budget — all measured on the coordinator's
/// monotonic clock. Replayed commands keep their original fields, so
/// the absolute deadline (never the mint-time remainder) is what a
/// restart re-derives a conservative window from.
fn deadline_payload(fields: &ExecutorBindingFields, now_unix_seconds: i64) -> serde_json::Value {
    let sent_unix_ms = now_unix_seconds * 1000;
    json!({
        "deadline_unix_ms": fields.deadline_unix_ms,
        "sent_unix_ms": sent_unix_ms,
        "remaining_ms": fields.deadline_unix_ms - sent_unix_ms,
    })
}

fn count_kind(ops: &[ExecutorOperation], kind: OperationKind) -> usize {
    ops.iter().filter(|o| o.operation_kind == kind).count()
}

/// Drive the durable stop intent: mint the `request_stop` command —
/// the durable intent itself — then a fresh observe closes the proof.
/// Replayed unreceipted intents already ran in the replay stage, so a
/// recorded-but-unanswered stop never reaches here; each pass re-drives
/// the endpoint's bounded escalation with a new intent while unproven.
/// `stop_requested` and still-live answers count as faults — an
/// ignored signal is unanswerable liveness, bounded like every other
/// fault.
#[allow(clippy::too_many_arguments)]
fn drive_stop(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    fields: &ExecutorBindingFields,
    ops: &[ExecutorOperation],
    request: &Option<InvocationRequest>,
    invocation: &Option<Invocation>,
    prior: &Option<ExecutorReconcile>,
    policy: &WorkPolicy,
    max_faults: u32,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let request_id = fields.request_id.clone();
    let stop = mint_command(
        fields,
        OperationKind::RequestStop,
        count_kind(ops, OperationKind::RequestStop),
        deadline_payload(fields, now_unix_seconds),
    )?;
    let op = ExecutorOperation::new(&stop)
        .map_err(|e| ExecError::Fault(format!("request_stop command cannot seal: {e}")))?;
    match record_and_issue(store, transport, op)? {
        Some(receipt)
            if receipt.result.get("status").and_then(|s| s.as_str()) == Some("stopped")
                && proof_binds(&receipt, &fields.bound_incarnation_id) =>
        {
            adopt_terminal(
                store,
                config,
                request,
                invocation,
                fields,
                &receipt,
                now_unix_seconds,
                report,
            )?;
            return reset_cursor(store, &request_id, prior);
        }
        Some(_) => {}
        None => {
            return count_fault(
                store,
                config,
                &request_id,
                prior,
                policy,
                max_faults,
                RemoteFenceClass::TransportExhausted,
                "transport fault issuing request_stop",
                now_unix_seconds,
                report,
            );
        }
    }
    // Inspect for the proof: a fresh observe under the same
    // incarnation answers `stopped` once the recorded identities are
    // gone.
    let observe = mint_command(
        fields,
        OperationKind::Observe,
        count_kind(ops, OperationKind::Observe),
        deadline_payload(fields, now_unix_seconds),
    )?;
    let op = ExecutorOperation::new(&observe)
        .map_err(|e| ExecError::Fault(format!("observe command cannot seal: {e}")))?;
    match record_and_issue(store, transport, op)? {
        Some(receipt)
            if receipt.result.get("status").and_then(|s| s.as_str()) == Some("stopped")
                && proof_binds(&receipt, &fields.bound_incarnation_id) =>
        {
            adopt_terminal(
                store,
                config,
                request,
                invocation,
                fields,
                &receipt,
                now_unix_seconds,
                report,
            )?;
            reset_cursor(store, &request_id, prior)
        }
        Some(_) => count_fault(
            store,
            config,
            &request_id,
            prior,
            policy,
            max_faults,
            RemoteFenceClass::Unanswerable,
            "stop intent accepted but the recorded writer is not provably stopped",
            now_unix_seconds,
            report,
        ),
        None => count_fault(
            store,
            config,
            &request_id,
            prior,
            policy,
            max_faults,
            RemoteFenceClass::TransportExhausted,
            "transport fault issuing observe for stop proof",
            now_unix_seconds,
            report,
        ),
    }
}

/// Drive the observe path for a writer the execution wants live:
/// a fresh observe under the bound incarnation, then adopt or count.
#[allow(clippy::too_many_arguments)]
fn drive_observe(
    store: &impl Store,
    config: &WorkloopConfig,
    transport: &mut impl RemoteTransport,
    fields: &ExecutorBindingFields,
    ops: &[ExecutorOperation],
    request: &Option<InvocationRequest>,
    invocation: &Option<Invocation>,
    prior: &Option<ExecutorReconcile>,
    policy: &WorkPolicy,
    max_faults: u32,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let request_id = fields.request_id.clone();
    let observe = mint_command(
        fields,
        OperationKind::Observe,
        count_kind(ops, OperationKind::Observe),
        deadline_payload(fields, now_unix_seconds),
    )?;
    let op = ExecutorOperation::new(&observe)
        .map_err(|e| ExecError::Fault(format!("observe command cannot seal: {e}")))?;
    match record_and_issue(store, transport, op)? {
        Some(receipt) => {
            let status = receipt
                .result
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            match status {
                "running" | "exited" => {
                    // `exited` names a leader exit with live
                    // descendants — the reservation holds either way.
                    if status == "running"
                        && let (Some(request), Some(invocation)) = (request, invocation)
                        && invocation.state.is_unresolved_writer()
                    {
                        report_process(
                            store,
                            config,
                            request,
                            invocation.id.clone(),
                            &remote_identity(fields),
                            ProcessStateFact::Running,
                            &format!("rec:{request_id}:running:{}", receipt.receipt_digest),
                            now_unix_seconds,
                        )?;
                    }
                    report.adopted.push(request_id.clone());
                    reset_cursor(store, &request_id, prior)
                }
                "stopped" if proof_binds(&receipt, &fields.bound_incarnation_id) => {
                    adopt_terminal(
                        store,
                        config,
                        request,
                        invocation,
                        fields,
                        &receipt,
                        now_unix_seconds,
                        report,
                    )?;
                    reset_cursor(store, &request_id, prior)
                }
                "conflict" | "refused" => {
                    let reason = receipt_reason(Some(&receipt));
                    if reason.contains("conflicting_replay") {
                        emit_fence(
                            store,
                            config,
                            &request_id,
                            RemoteFenceClass::ConflictingReplay,
                            &format!("endpoint refused a recorded command: {reason}"),
                            now_unix_seconds,
                            report,
                        )
                    } else {
                        count_fault(
                            store,
                            config,
                            &request_id,
                            prior,
                            policy,
                            max_faults,
                            RemoteFenceClass::Unanswerable,
                            &format!("observe refused: {reason}"),
                            now_unix_seconds,
                            report,
                        )
                    }
                }
                _ => count_fault(
                    store,
                    config,
                    &request_id,
                    prior,
                    policy,
                    max_faults,
                    RemoteFenceClass::Unanswerable,
                    "observe answered unknown or unavailable",
                    now_unix_seconds,
                    report,
                ),
            }
        }
        None => count_fault(
            store,
            config,
            &request_id,
            prior,
            policy,
            max_faults,
            RemoteFenceClass::TransportExhausted,
            "transport fault issuing observe",
            now_unix_seconds,
            report,
        ),
    }
}

// ---------------------------------------------------------------------
// Facts, cursor bookkeeping
// ---------------------------------------------------------------------

/// The remote process identity string reported into the process-fact
/// pipeline — `remote:<incarnation>:<request>` — stable across the
/// writer's life so the launch-acceptance gate and the stop path
/// compare the same identity a `Running` fact minted.
fn remote_identity(fields: &ExecutorBindingFields) -> String {
    format!(
        "remote:{}:{}",
        fields.bound_incarnation_id, fields.request_id
    )
}

fn receipt_reason(receipt: Option<&ExecutorReceiptV1>) -> String {
    receipt
        .and_then(|r| r.result.get("reason").and_then(|x| x.as_str()))
        .unwrap_or("unknown")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn report_process(
    store: &impl Store,
    config: &WorkloopConfig,
    request: &InvocationRequest,
    invocation_id: InvocationId,
    process_identity: &str,
    state: ProcessStateFact,
    event_id: &str,
    now_unix_seconds: i64,
) -> Result<(), ExecError> {
    report_fact(
        store,
        config,
        request,
        ObservationFact::ProcessState {
            invocation_id,
            process_identity: process_identity.to_string(),
            state,
            exit_code: None,
        },
        event_id.to_string(),
        now_unix_seconds,
    )
}

fn report_fact(
    store: &impl Store,
    config: &WorkloopConfig,
    request: &InvocationRequest,
    fact: ObservationFact,
    event_id: String,
    now_unix_seconds: i64,
) -> Result<(), ExecError> {
    let obs = Observation {
        delivery: Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: "remote_reconcile".into(),
            event_id,
        }),
        subject_work_id: Some(request.work_id.clone()),
        subject_execution_id: Some(request.execution_id.clone()),
        fact,
        observed_unix_seconds: now_unix_seconds,
    };
    match handle_observation(store, config, &obs, now_unix_seconds)? {
        IngestReceipt::Rejected(detail) => Err(ExecError::Fault(format!(
            "reconcile report rejected: {detail}"
        ))),
        _ => Ok(()),
    }
}

/// Record the proven-gone fact for a bound-incarnation stop proof:
/// the `Exited` process fact resolves the Invocation and settles the
/// Execution through the same path a local stop proof takes.
#[allow(clippy::too_many_arguments)]
fn adopt_terminal(
    store: &impl Store,
    config: &WorkloopConfig,
    request: &Option<InvocationRequest>,
    invocation: &Option<Invocation>,
    fields: &ExecutorBindingFields,
    receipt: &ExecutorReceiptV1,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    if let (Some(request), Some(invocation)) = (request, invocation)
        && invocation.state.is_unresolved_writer()
    {
        report_process(
            store,
            config,
            request,
            invocation.id.clone(),
            &remote_identity(fields),
            ProcessStateFact::Exited,
            &format!(
                "rec:{}:exited:{}",
                fields.request_id, receipt.receipt_digest
            ),
            now_unix_seconds,
        )?;
    }
    report.proven.push(fields.request_id.clone());
    Ok(())
}

/// A productive step clears the bounded fault count; a fence survives
/// — the Attention it names stands until a human lifts it.
fn reset_cursor(
    store: &impl Store,
    request_id: &str,
    prior: &Option<ExecutorReconcile>,
) -> Result<(), ExecError> {
    let Some(prior) = prior else {
        return Ok(());
    };
    if prior.consecutive_faults == 0 {
        return Ok(());
    }
    store.transact(|tx| {
        tx.apply(&Effect::UpsertExecutorReconcile {
            cursor: ExecutorReconcile {
                request_id: request_id.to_string(),
                consecutive_faults: 0,
                fenced_class: prior.fenced_class.clone(),
                next_attempt_unix_ms: 0,
                updated_seq: prior.updated_seq + 1,
            },
            expected_seq: prior.updated_seq,
        })
    })?;
    Ok(())
}

/// Count one unproductive step against the durable budget; crossing
/// `transport_max_retries` emits the named fence instead.
#[allow(clippy::too_many_arguments)]
fn count_fault(
    store: &impl Store,
    config: &WorkloopConfig,
    request_id: &str,
    prior: &Option<ExecutorReconcile>,
    policy: &WorkPolicy,
    max_faults: u32,
    fence_class: RemoteFenceClass,
    detail: &str,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    if let Some((_stored_class, class)) = stored_fence(prior.as_ref()) {
        report.fenced.push((request_id.to_string(), class));
        return Ok(());
    }
    let faults = prior.as_ref().map_or(0, |c| c.consecutive_faults) + 1;
    if faults >= max_faults {
        return emit_fence(
            store,
            config,
            request_id,
            fence_class,
            detail,
            now_unix_seconds,
            report,
        );
    }
    let backoff = policy
        .transport_backoff_seconds
        .get(faults.saturating_sub(1) as usize)
        .or_else(|| policy.transport_backoff_seconds.last())
        .copied()
        .unwrap_or(0);
    let next_attempt = now_unix_seconds.saturating_mul(1000) + backoff as i64 * 1000;
    let expected = prior.as_ref().map_or(0, |c| c.updated_seq);
    store.transact(|tx| {
        tx.apply(&Effect::UpsertExecutorReconcile {
            cursor: ExecutorReconcile {
                request_id: request_id.to_string(),
                consecutive_faults: faults,
                fenced_class: None,
                next_attempt_unix_ms: next_attempt,
                updated_seq: expected + 1,
            },
            expected_seq: expected,
        })
    })?;
    report
        .faults
        .push((request_id.to_string(), detail.to_string()));
    Ok(())
}

/// The delivery id a `RemoteFence` fact is reported under — stable so
/// a re-report dedupes against the fact the first fence emission wrote.
fn fence_event_id(request_id: &str, stored_class: &str) -> String {
    format!("rec:{request_id}:fence:{stored_class}")
}

/// A stored `fenced_class` outside the closed vocabulary is still a
/// fence: a corrupt or future-version value fails closed — the request
/// stays frozen — while the class degrades to `record_corrupt` for
/// reporting. The raw stored string rides the fact delivery id so the
/// re-report binds what the cursor actually recorded, and the fact's
/// detail names the stored value an operator must reconcile.
fn stored_fence(cursor: Option<&ExecutorReconcile>) -> Option<(String, RemoteFenceClass)> {
    let raw = cursor?.fenced_class.as_deref()?;
    Some((
        raw.to_string(),
        RemoteFenceClass::parse(raw).unwrap_or(RemoteFenceClass::RecordCorrupt),
    ))
}

/// Re-report the `RemoteFence` fact for an already-fenced request when
/// no observation carries its delivery id — the repair path for a
/// crash between the fence commit and the fact write (INV:
/// fence-fact-at-least-once). A landed fact makes this a no-op; a
/// re-report failure counts against this request, never the pass.
#[allow(clippy::too_many_arguments)]
fn refence_fact(
    store: &impl Store,
    config: &WorkloopConfig,
    request: &InvocationRequest,
    invocation_id: Option<InvocationId>,
    stored_class: &str,
    class: RemoteFenceClass,
    request_id: &str,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let event_id = fence_event_id(request_id, stored_class);
    let delivered = store.transact(|tx| {
        Ok(tx.observations_for_work(&request.work_id)?.iter().any(|o| {
            o.delivery.as_ref().is_some_and(|d| {
                d.scope == DeliveryScope::Local
                    && d.source == "remote_reconcile"
                    && d.event_id == event_id
            })
        }))
    })?;
    if delivered {
        return Ok(());
    }
    let detail = if stored_class == class.as_str() {
        format!(
            "remote fence {} fact re-reported by a later reconcile pass",
            class.as_str()
        )
    } else {
        format!(
            "stored fenced_class '{stored_class}' is outside the closed vocabulary; fact re-reported as {}",
            class.as_str()
        )
    };
    if let Err(e) = report_fact(
        store,
        config,
        request,
        ObservationFact::RemoteFence {
            request_id: request_id.to_string(),
            invocation_id,
            class,
            detail,
        },
        event_id,
        now_unix_seconds,
    ) {
        report.faults.push((
            request_id.to_string(),
            format!("fence fact re-report failed: {e}"),
        ));
    }
    Ok(())
}

/// Emit the named fence: the cursor records the class durably first —
/// a fenced request stays fenced — then the `RemoteFence` fact moves
/// the Execution to its fail-closed hold. A crash between the two
/// leaves the cursor fenced with no fact, repaired by the fenced
/// gate's re-report on every later pass. A cursor CAS loss is a ledger
/// disagreement resolved against the durable row: the stored class
/// wins when the racing write fenced, otherwise the fence is retried
/// once and otherwise deferred to the next pass — the conflict never
/// propagates past the request that raised it, and a fact-write
/// failure counts against this request alone.
fn emit_fence(
    store: &impl Store,
    config: &WorkloopConfig,
    request_id: &str,
    class: RemoteFenceClass,
    detail: &str,
    now_unix_seconds: i64,
    report: &mut ReconcileReport,
) -> Result<(), ExecError> {
    let mut attempts = 0usize;
    loop {
        let prior = store.transact(|tx| tx.executor_reconcile(request_id))?;
        if let Some((_stored_class, fenced)) = stored_fence(prior.as_ref()) {
            // A racing write fenced first — the durable class wins.
            report.fenced.push((request_id.to_string(), fenced));
            return Ok(());
        }
        let expected = prior.as_ref().map_or(0, |c| c.updated_seq);
        match store.transact(|tx| {
            tx.apply(&Effect::UpsertExecutorReconcile {
                cursor: ExecutorReconcile {
                    request_id: request_id.to_string(),
                    consecutive_faults: 0,
                    fenced_class: Some(class.as_str().to_string()),
                    next_attempt_unix_ms: 0,
                    updated_seq: expected + 1,
                },
                expected_seq: expected,
            })
        }) {
            Ok(()) => break,
            Err(StoreError::ConstraintConflict { .. }) => {
                attempts += 1;
                if attempts >= 2 {
                    report.faults.push((
                        request_id.to_string(),
                        "reconcile cursor contested; fence deferred to the next pass".into(),
                    ));
                    return Ok(());
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    let request = store.transact(|tx| tx.request(request_id))?;
    let invocation = store.transact(|tx| tx.invocation_for_request(request_id))?;
    if let Some(request) = request
        && let Err(e) = report_fact(
            store,
            config,
            &request,
            ObservationFact::RemoteFence {
                request_id: request_id.to_string(),
                invocation_id: invocation.map(|i| i.id),
                class,
                detail: detail.to_string(),
            },
            fence_event_id(request_id, class.as_str()),
            now_unix_seconds,
        )
    {
        report.faults.push((
            request_id.to_string(),
            format!("fence fact not reported: {e}"),
        ));
    }
    report.fenced.push((request_id.to_string(), class));
    Ok(())
}
