//! Remote lifecycle reconciliation fault matrix (#378,
//! `docs/executor-transport-contract.md` §14): the coordinator-side
//! reconcile driver proven against a scripted `RemoteTransport` —
//! coordinator crash between commit and send, lost start answers,
//! endpoint re-incarnation, transport partitions, ignored signals,
//! conflicting replay, and ledger corruption. No live deployment and
//! no spawned process: every scenario is durable rows plus a scripted
//! transport, which is exactly the boundary the contract draws.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

use serde_json::{Value, json};
use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::executor::{
    EXECUTOR_PROTOCOL_V1, ExecutorBinding, ExecutorBindingFields, ExecutorCommandV1,
    ExecutorIdentity, ExecutorLimits, ExecutorOperation, ExecutorReceiptV1, ExecutorReconcile,
    OperationKind,
};
use workloop_core::identity::*;
use workloop_core::observation::RemoteFenceClass;
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_exec::remote_reconcile::{
    RemoteTransport, StartOutcome, TransportError, ensure_started, reconcile_once,
};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;
const INCARNATION: &str = "boot-1";

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-reconcile-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config() -> WorkloopConfig {
    let repository = RepositoryRef {
        host: HostInstanceId::trusted("fixture"),
        repository: RepositoryId::trusted("repo"),
    };
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repository.clone(),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: HostActorRef {
                    host: HostInstanceId::trusted("fixture"),
                    actor: HostActorId::trusted("pub"),
                },
                credential_ref: CredentialRef::trusted("cred"),
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
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
        lanes: LanePolicy::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
        // Backoff of zero keeps the bounded-retry tests on consecutive
        // passes; the held-by-backoff case seeds its own cursor.
        policy: WorkPolicy {
            transport_max_retries: 3,
            transport_backoff_seconds: vec![0],
            ..WorkPolicy::default()
        },
    }
}

// ---------------------------------------------------------------------
// Durable-row fixtures
// ---------------------------------------------------------------------

/// The durable identities of one seeded remote request.
struct Seeded {
    request_id: String,
    invocation_id: String,
    work_id: WorkId,
    execution_id: ExecutionId,
    request_digest: String,
}

/// Work + execution (queued) + request + running invocation + binding
/// — the re-adoption set a coordinator restart replays against.
fn seed(store: &SqliteStore, tag: &str, incarnation: &str) -> Seeded {
    let (work_id, execution_id) = match handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("reconcile-faults".into()),
            idempotency_key: format!("remote-reconcile-{tag}"),
            body: CommandBody::Submit {
                intent: "remote work".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap()
    {
        CommandDecision::Accepted {
            work_id: Some(work_id),
            effect:
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id },
            ..
        } => (work_id, execution_id),
        other => panic!("expected accepted submit, got {other:?}"),
    };
    let request_id = format!("{}:inv:0", execution_id.0);
    let invocation_id = store.transact(|tx| tx.alloc_id("inv")).unwrap();
    let request_digest = "a".repeat(64);
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: request_id.clone(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    spec_revision: 1,
                    payload: json!({}),
                    adapter_binding: None,
                    placement: None,
                },
            })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(invocation_id.clone()),
                    request_id: request_id.clone(),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertExecutorBinding {
                binding: ExecutorBinding::new(ExecutorBindingFields {
                    installation_id: "install-a".into(),
                    endpoint_id: "executor-a".into(),
                    endpoint_identity_digest: "e".repeat(64),
                    bound_incarnation_id: incarnation.into(),
                    protocol_version: EXECUTOR_PROTOCOL_V1.into(),
                    capabilities_digest: "c".repeat(64),
                    launch_config_digest: "d".repeat(64),
                    request_id: request_id.clone(),
                    invocation_id: invocation_id.clone(),
                    request_digest: request_digest.clone(),
                    deadline_unix_ms: (NOW + 3600) * 1000,
                    limits: ExecutorLimits {
                        max_input_bytes: 64 * 1024 * 1024,
                        max_output_bytes: 64 * 1024 * 1024,
                        max_item_count: 32,
                        operation_timeout_ms: 3_600_000,
                    },
                })
                .unwrap(),
            })?;
            Ok(())
        })
        .unwrap();
    Seeded {
        request_id,
        invocation_id,
        work_id,
        execution_id,
        request_digest,
    }
}

fn set_exec_state(store: &SqliteStore, seeded: &Seeded, to: ExecutionState) {
    let mut execution = store
        .transact(|tx| tx.execution(&seeded.execution_id))
        .unwrap()
        .unwrap();
    let expected = execution.state.clone();
    execution.state = to;
    store
        .transact(|tx| {
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: expected,
            })
        })
        .unwrap();
}

// ---------------------------------------------------------------------
// Command/receipt fixtures
// ---------------------------------------------------------------------

fn identity(seeded: &Seeded) -> ExecutorIdentity {
    ExecutorIdentity {
        installation_id: "install-a".into(),
        endpoint_id: "executor-a".into(),
        endpoint_identity_digest: "e".repeat(64),
        bound_incarnation_id: INCARNATION.into(),
        request_id: seeded.request_id.clone(),
        invocation_id: seeded.invocation_id.clone(),
        request_digest: seeded.request_digest.clone(),
    }
}

fn command_identity(command: &ExecutorCommandV1) -> ExecutorIdentity {
    ExecutorIdentity {
        installation_id: command.installation_id.clone(),
        endpoint_id: command.endpoint_id.clone(),
        endpoint_identity_digest: command.endpoint_identity_digest.clone(),
        bound_incarnation_id: command.bound_incarnation_id.clone(),
        request_id: command.request_id.clone(),
        invocation_id: command.invocation_id.clone(),
        request_digest: command.request_digest.clone(),
    }
}

/// Commit a command to the operation ledger — the persist-half of
/// persist-before-send.
fn record_command(
    store: &SqliteStore,
    seeded: &Seeded,
    kind: OperationKind,
    ordinal: usize,
    payload: Value,
) -> ExecutorCommandV1 {
    let command = ExecutorCommandV1::new(
        identity(seeded),
        format!("{}:{}:{}", seeded.request_id, kind.as_str(), ordinal),
        kind,
        payload,
    )
    .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorCommand {
                operation: ExecutorOperation::new(&command).unwrap(),
            })
        })
        .unwrap();
    command
}

/// Attach a receipt to its recorded command — the endpoint's answer,
/// durable.
fn attach_receipt(
    store: &SqliteStore,
    command: &ExecutorCommandV1,
    responder_incarnation: &str,
    result: Value,
) -> ExecutorReceiptV1 {
    let receipt = ExecutorReceiptV1::new(
        command_identity(command),
        responder_incarnation,
        command.operation_id.clone(),
        command.operation_kind,
        command.payload_digest.clone(),
        command.command_digest.clone(),
        result,
    )
    .unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id: command.request_id.clone(),
                operation_id: command.operation_id.clone(),
                receipt_body: receipt.to_canonical_bytes().unwrap(),
            })
        })
        .unwrap();
    receipt
}

fn seeded_start(store: &SqliteStore, seeded: &Seeded, result: Option<Value>) -> ExecutorCommandV1 {
    let command = record_command(store, seeded, OperationKind::Start, 1, json!({}));
    if let Some(result) = result {
        attach_receipt(store, &command, INCARNATION, result);
    }
    command
}

/// A recorded live writer: `start` answered `started`.
fn live_writer(store: &SqliteStore, seeded: &Seeded) {
    seeded_start(
        store,
        seeded,
        Some(json!({
            "status": "started",
            "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)}
        })),
    );
}

fn prepared(store: &SqliteStore, seeded: &Seeded) {
    let command = record_command(store, seeded, OperationKind::Prepare, 1, json!({}));
    attach_receipt(
        store,
        &command,
        INCARNATION,
        json!({"status": "prepared", "prepared_id": "prep-1"}),
    );
}

// ---------------------------------------------------------------------
// The scripted transport
// ---------------------------------------------------------------------

enum Step {
    /// Answer the issued command with this result value — the fake
    /// builds the receipt from the command's own identity, as the
    /// endpoint does. A script that runs out of answers faults: an
    /// exhausted script is a transport that stopped answering.
    Answer(Value),
}

struct FakeTransport {
    responder_incarnation: String,
    script: VecDeque<Step>,
    /// Every command that reached the wire — the verbatim-replay and
    /// persist-before-send assertions read it.
    sent: Vec<ExecutorCommandV1>,
}

impl FakeTransport {
    fn new(incarnation: &str, script: Vec<Step>) -> Self {
        Self {
            responder_incarnation: incarnation.into(),
            script: script.into(),
            sent: Vec::new(),
        }
    }
}

impl RemoteTransport for FakeTransport {
    fn session_incarnation(&self) -> &str {
        &self.responder_incarnation
    }

    fn issue_recorded(
        &mut self,
        command: &ExecutorCommandV1,
    ) -> Result<ExecutorReceiptV1, TransportError> {
        self.sent.push(command.clone());
        match self.script.pop_front() {
            Some(Step::Answer(result)) => ExecutorReceiptV1::new(
                command_identity(command),
                self.responder_incarnation.clone(),
                command.operation_id.clone(),
                command.operation_kind,
                command.payload_digest.clone(),
                command.command_digest.clone(),
                result,
            )
            .map_err(|e| TransportError {
                detail: format!("scripted receipt cannot seal: {e}"),
            }),
            _ => Err(TransportError {
                detail: "scripted transport fault".into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------
// Result builders
// ---------------------------------------------------------------------

fn running() -> Value {
    json!({
        "status": "running",
        "sequence": 1,
        "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)},
    })
}

fn stopped(incarnation: &str) -> Value {
    json!({
        "status": "stopped",
        "sequence": 1,
        "proof": {
            "incarnation_id": incarnation,
            "manner": "signal",
            "proof_digest": "p".repeat(64),
            "snapshot_digest": "s".repeat(64),
            "proven_at_unix_seconds": NOW,
        },
    })
}

fn unknown(reason: &str) -> Value {
    json!({"status": "unknown", "reason": reason})
}

fn refused(reason: &str) -> Value {
    json!({"status": "refused", "reason": reason})
}

// ---------------------------------------------------------------------
// Store readers
// ---------------------------------------------------------------------

fn ops(store: &SqliteStore, request_id: &str) -> Vec<ExecutorOperation> {
    store
        .transact(|tx| tx.executor_operations_for_request(request_id))
        .unwrap()
}

fn cursor(store: &SqliteStore, request_id: &str) -> Option<ExecutorReconcile> {
    store
        .transact(|tx| tx.executor_reconcile(request_id))
        .unwrap()
}

fn inv_state(store: &SqliteStore, seeded: &Seeded) -> InvocationState {
    store
        .transact(|tx| {
            Ok(tx
                .invocations_for_execution(&seeded.execution_id)?
                .into_iter()
                .find(|i| i.id.0 == seeded.invocation_id)
                .map(|i| i.state))
        })
        .unwrap()
        .unwrap()
}

fn exec_state(store: &SqliteStore, execution_id: &ExecutionId) -> ExecutionState {
    store
        .transact(|tx| tx.execution(execution_id))
        .unwrap()
        .unwrap()
        .state
}

fn attention_reasons(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    match store
        .transact(|tx| workloop_core::projection::work_detail(tx, work_id, NOW))
        .unwrap()
    {
        workloop_core::projection::QueryResult::Found { value: view, .. } => {
            view.attention.iter().map(|a| a.reason.clone()).collect()
        }
        _ => vec![],
    }
}

fn fence_decisions(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.recovery_decisions(work_id))
        .unwrap()
        .into_iter()
        .filter(|d| d.classification == "remote_fence")
        .map(|d| d.detail)
        .collect()
}

fn rig(tag: &str) -> SqliteStore {
    SqliteStore::open(&tmp_root(tag).join("store.db"), 1).unwrap()
}

// =====================================================================
// The fault matrix
// =====================================================================

/// Coordinator crash between commit and send: the `start` command is
/// durable but unanswered. The restart replays the recorded bytes
/// verbatim — the endpoint answers under the original identity and the
/// writer is adopted. Never a second start, never a replacement
/// dispatch.
#[test]
fn lost_start_answer_replays_verbatim_and_adopts() {
    let store = rig("lost-start");
    let seeded = seed(&store, "lost-start", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Ready);
    prepared(&store, &seeded);
    let start = seeded_start(&store, &seeded, None); // unreceipted

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![
            // The replayed start answers.
            Step::Answer(json!({
                "status": "started",
                "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)}
            })),
            // The reconcile-minted observe.
            Step::Answer(running()),
        ],
    );
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    // The replayed command is byte-identical to the durable record —
    // a crash between commit and send leaves the same bytes on the
    // wire a first send would have produced.
    assert_eq!(transport.sent.len(), 2);
    assert_eq!(
        transport.sent[0].command_digest, start.command_digest,
        "the start was replayed verbatim, not re-minted"
    );
    assert_eq!(transport.sent[1].operation_kind, OperationKind::Observe);
    assert_eq!(ops(&store, &seeded.request_id).len(), 3);
    assert_eq!(report.adopted, vec![seeded.request_id.clone()]);
    assert!(report.fenced.is_empty());
    assert_eq!(inv_state(&store, &seeded), InvocationState::Running);
}

/// The coordinator restart cannot reach the endpoint at all: every
/// issue faults. Faults persist in the reconcile cursor; at
/// `transport_max_retries` the writer fences `transport_exhausted` —
/// the invocation stays an unresolved writer and the named Attention
/// carries the class.
#[test]
fn transport_partition_bounds_retry_then_fences() {
    let store = rig("partition");
    let seeded = seed(&store, "partition", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    for pass in 0..3 {
        let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();
        if pass < 2 {
            assert!(
                report.fenced.is_empty(),
                "pass {pass} must count, not fence"
            );
        }
    }
    assert_eq!(
        cursor(&store, &seeded.request_id)
            .unwrap()
            .fenced_class
            .as_deref(),
        Some("transport_exhausted")
    );
    // The fence marked the writer unknown — never released — and the
    // named Attention carries the failure class.
    assert_eq!(inv_state(&store, &seeded), InvocationState::Unknown);
    assert!(
        fence_decisions(&store, &seeded.work_id)
            .iter()
            .any(|d| d.contains("transport_exhausted")),
        "a remote_fence decision names the class"
    );
    assert_eq!(
        attention_reasons(&store, &seeded.work_id),
        vec!["remote_fence".to_string()]
    );
}

/// A fenced request is frozen: the next pass mints nothing, sends
/// nothing, and reports only the standing fence.
#[test]
fn fenced_request_mints_nothing() {
    let store = rig("fenced");
    let seeded = seed(&store, "fenced", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);
    store
        .transact(|tx| {
            tx.apply(&Effect::UpsertExecutorReconcile {
                cursor: ExecutorReconcile {
                    request_id: seeded.request_id.clone(),
                    consecutive_faults: 0,
                    fenced_class: Some("incarnation_changed".into()),
                    next_attempt_unix_ms: 0,
                    updated_seq: 1,
                },
                expected_seq: 0,
            })
        })
        .unwrap();

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(transport.sent.is_empty(), "no wire contact under a fence");
    assert_eq!(ops(&store, &seeded.request_id).len(), 1);
    assert_eq!(
        report.fenced,
        vec![(
            seeded.request_id.clone(),
            RemoteFenceClass::IncarnationChanged
        )]
    );
}

/// The endpoint rebooted: the responder incarnation differs from the
/// binding's recorded incarnation while a writer may still be live.
/// The recorded-identity probe cannot prove the writer gone — the
/// reconcile fails closed into an `incarnation_changed` fence.
#[test]
fn incarnation_change_fences_a_possibly_live_writer() {
    let store = rig("incarnation");
    let seeded = seed(&store, "incarnation", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new(
        "boot-2",
        vec![Step::Answer(unknown("incarnation_mismatch"))],
    );
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(
        report.fenced,
        vec![(
            seeded.request_id.clone(),
            RemoteFenceClass::IncarnationChanged
        )]
    );
    assert_eq!(inv_state(&store, &seeded), InvocationState::Unknown);
    assert_eq!(
        attention_reasons(&store, &seeded.work_id),
        vec!["remote_fence".to_string()]
    );
}

/// The same reboot with durable proof: the endpoint committed the
/// stop proof before it restarted, and the probe under the recorded
/// binding still carries it — the terminal record is adoptable under
/// any responder because only its proof binding is trusted.
#[test]
fn incarnation_change_still_adopts_a_bound_stop_proof() {
    let store = rig("incarnation-proof");
    let seeded = seed(&store, "incarnation-proof", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new("boot-2", vec![Step::Answer(stopped(INCARNATION))]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(report.proven, vec![seeded.request_id.clone()]);
    assert!(report.fenced.is_empty());
    assert_eq!(inv_state(&store, &seeded), InvocationState::Done);
}

/// No release without proof: a `stopped` answer whose proof names a
/// different incarnation is not the recorded writer's death — it
/// counts against the bounded budget like any unverifiable answer.
#[test]
fn foreign_incarnation_proof_does_not_release() {
    let store = rig("foreign-proof");
    let seeded = seed(&store, "foreign-proof", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new(INCARNATION, vec![Step::Answer(stopped("boot-9"))]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(report.proven.is_empty());
    assert_eq!(report.faults.len(), 1);
    assert_eq!(inv_state(&store, &seeded), InvocationState::Running);
}

/// `exited` names a leader exit with descendants unproven — the
/// reservation holds, the writer stays unresolved, and no exit fact
/// is minted.
#[test]
fn leader_exit_never_releases_the_reservation() {
    let store = rig("leader-exit");
    let seeded = seed(&store, "leader-exit", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![Step::Answer(json!({
            "status": "exited",
            "sequence": 1,
            "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)},
        }))],
    );
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(report.adopted, vec![seeded.request_id.clone()]);
    assert!(report.proven.is_empty());
    assert_eq!(inv_state(&store, &seeded), InvocationState::Running);
}

/// The durable stop intent: intent → signal → inspect. `request_stop`
/// is recorded before the wire sees it; the accepted-but-unproven
/// answer leads into a fresh observe, whose bound proof settles the
/// writer.
#[test]
fn stop_intent_drives_signal_and_inspect_to_proof() {
    let store = rig("stop-drive");
    let seeded = seed(&store, "stop-drive", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);
    set_exec_state(&store, &seeded, ExecutionState::Stopping);

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![
            Step::Answer(json!({"status": "stop_requested"})),
            Step::Answer(stopped(INCARNATION)),
        ],
    );
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(
        transport
            .sent
            .iter()
            .map(|c| c.operation_kind)
            .collect::<Vec<_>>(),
        vec![OperationKind::RequestStop, OperationKind::Observe]
    );
    assert_eq!(report.proven, vec![seeded.request_id.clone()]);
    assert_eq!(inv_state(&store, &seeded), InvocationState::Done);
}

/// A restart mid-stop: the `request_stop` command is durable but its
/// answer never arrived — the crash window between the endpoint's
/// stop and the coordinator's receipt. The replay stage re-issues the
/// recorded intent verbatim; the proof it carries settles the writer.
#[test]
fn unreceipted_stop_intent_replays_verbatim() {
    let store = rig("stop-replay");
    let seeded = seed(&store, "stop-replay", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);
    set_exec_state(&store, &seeded, ExecutionState::Stopping);
    let stop = record_command(&store, &seeded, OperationKind::RequestStop, 1, json!({}));

    let mut transport = FakeTransport::new(INCARNATION, vec![Step::Answer(stopped(INCARNATION))]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(
        transport.sent[0].command_digest, stop.command_digest,
        "the durable intent was replayed, not re-minted"
    );
    assert_eq!(report.proven, vec![seeded.request_id.clone()]);
    assert_eq!(inv_state(&store, &seeded), InvocationState::Done);
}

/// Ignored signals: the endpoint accepts every intent but the writer
/// stays provably alive — `stop_requested` then `running`, pass after
/// pass. Unanswerable liveness is bounded like every other fault and
/// fences `unanswerable` at the retry bound.
#[test]
fn ignored_signals_fence_unanswerable() {
    let store = rig("ignored");
    let seeded = seed(&store, "ignored", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);
    set_exec_state(&store, &seeded, ExecutionState::Stopping);

    let mut script = Vec::new();
    for _ in 0..3 {
        script.push(Step::Answer(json!({"status": "stop_requested"})));
        script.push(Step::Answer(running()));
    }
    let mut transport = FakeTransport::new(INCARNATION, script);
    for _ in 0..3 {
        reconcile_once(&store, &config(), &mut transport, NOW).unwrap();
    }
    assert_eq!(
        cursor(&store, &seeded.request_id)
            .unwrap()
            .fenced_class
            .as_deref(),
        Some("unanswerable")
    );
}

/// Unverifiable liveness under the running drive: `unknown` answers
/// bound against `transport_max_retries` and fence `unanswerable`.
#[test]
fn unknown_answers_bound_to_unanswerable_fence() {
    let store = rig("unknown");
    let seeded = seed(&store, "unknown", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![
            Step::Answer(unknown("process_identity_unavailable")),
            Step::Answer(unknown("process_identity_unavailable")),
            Step::Answer(unknown("process_identity_unavailable")),
        ],
    );
    for _ in 0..3 {
        reconcile_once(&store, &config(), &mut transport, NOW).unwrap();
    }
    assert_eq!(
        cursor(&store, &seeded.request_id)
            .unwrap()
            .fenced_class
            .as_deref(),
        Some("unanswerable")
    );
}

/// The endpoint's replay fence refuses a recorded command id with
/// different bytes: the two ledgers disagree, so the reconcile fails
/// closed into `conflicting_replay` — nothing proceeds under
/// disagreement. (A `refused` result can only travel on the ops that
/// admit it; the replayed `start` is the carry here.)
#[test]
fn conflicting_replay_fences() {
    let store = rig("conflict");
    let seeded = seed(&store, "conflict", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    prepared(&store, &seeded);
    seeded_start(&store, &seeded, None); // unreceipted — replayed below

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![Step::Answer(refused("conflicting_replay"))],
    );
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert_eq!(
        report.fenced,
        vec![(
            seeded.request_id.clone(),
            RemoteFenceClass::ConflictingReplay
        )]
    );
}

/// A corrupt durable row is never silently replayed: a binding whose
/// stored body no longer matches its digest fails verification and
/// fences `record_corrupt`.
#[test]
fn corrupt_binding_fences_record_corrupt() {
    let db = tmp_root("corrupt").join("store.db");
    let store = SqliteStore::open(&db, 1).unwrap();
    let seeded = seed(&store, "corrupt", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    drop(store);
    // Tamper with the durable binding body — the digest no longer
    // covers it, so `fields()` refuses the row on read.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("UPDATE executor_bindings SET binding_body=X'7b7d';")
        .unwrap();
    let store = SqliteStore::open(&db, 1).unwrap();

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(
        transport.sent.is_empty(),
        "a corrupt row is fenced before any wire contact"
    );
    assert_eq!(
        report.fenced,
        vec![(seeded.request_id.clone(), RemoteFenceClass::RecordCorrupt)]
    );
}

/// A durable `refused` start needs no wire contact at all: the
/// recorded answer resolves the writer as a launch refusal and the
/// invocation abandons.
#[test]
fn refused_start_reports_launch_refused() {
    let store = rig("refused");
    let seeded = seed(&store, "refused", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Ready);
    seeded_start(&store, &seeded, Some(refused("request_identity_mismatch")));

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(transport.sent.is_empty());
    assert_eq!(report.refused, vec![seeded.request_id.clone()]);
    assert_eq!(inv_state(&store, &seeded), InvocationState::Abandoned);
}

/// A bound request that was never started is dispatch's problem, not
/// reconciliation's — the pass leaves it untouched.
#[test]
fn bound_never_started_awaits_dispatch() {
    let store = rig("awaiting");
    let seeded = seed(&store, "awaiting", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Ready);

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(transport.sent.is_empty());
    assert_eq!(report.awaiting_dispatch, vec![seeded.request_id.clone()]);
}

/// The persisted backoff slot holds a request until its time — a pass
/// inside the window sends nothing.
#[test]
fn backoff_gate_holds_until_the_slot() {
    let store = rig("backoff");
    let seeded = seed(&store, "backoff", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Running);
    live_writer(&store, &seeded);
    store
        .transact(|tx| {
            tx.apply(&Effect::UpsertExecutorReconcile {
                cursor: ExecutorReconcile {
                    request_id: seeded.request_id.clone(),
                    consecutive_faults: 1,
                    fenced_class: None,
                    next_attempt_unix_ms: (NOW + 600) * 1000,
                    updated_seq: 1,
                },
                expected_seq: 0,
            })
        })
        .unwrap();

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let report = reconcile_once(&store, &config(), &mut transport, NOW).unwrap();

    assert!(transport.sent.is_empty());
    assert_eq!(report.held, vec![seeded.request_id.clone()]);
}

// =====================================================================
// ensure_started — the idempotent start primitive
// =====================================================================

/// `ensure_started` mints the one start a request admits — persist
/// before send — and a second call returns the recorded result
/// without touching the wire.
#[test]
fn ensure_started_mints_once_and_replays() {
    let store = rig("ensure");
    let seeded = seed(&store, "ensure", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Ready);
    prepared(&store, &seeded);

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![Step::Answer(json!({
            "status": "started",
            "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)}
        }))],
    );
    let first = ensure_started(&store, &config(), &mut transport, &seeded.request_id, NOW).unwrap();
    assert!(matches!(first, StartOutcome::Started { .. }));
    assert_eq!(transport.sent.len(), 1);

    // The second call is the replayed record — no wire, no second
    // start row.
    let second =
        ensure_started(&store, &config(), &mut transport, &seeded.request_id, NOW).unwrap();
    assert!(matches!(second, StartOutcome::Started { .. }));
    assert_eq!(transport.sent.len(), 1, "the recorded result replays");
    assert_eq!(
        ops(&store, &seeded.request_id)
            .iter()
            .filter(|o| o.operation_kind == OperationKind::Start)
            .count(),
        1
    );
}

/// A start committed before its answer was lost replays the recorded
/// bytes — `ensure_started` never mints a replacement.
#[test]
fn ensure_started_replays_an_unreceipted_start() {
    let store = rig("ensure-replay");
    let seeded = seed(&store, "ensure-replay", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Ready);
    prepared(&store, &seeded);
    let start = seeded_start(&store, &seeded, None);

    let mut transport = FakeTransport::new(
        INCARNATION,
        vec![Step::Answer(json!({
            "status": "started",
            "process": {"evidence_ref": "ev-1", "identity_digest": "f".repeat(64)}
        }))],
    );
    let outcome =
        ensure_started(&store, &config(), &mut transport, &seeded.request_id, NOW).unwrap();

    assert!(matches!(outcome, StartOutcome::Started { .. }));
    assert_eq!(transport.sent[0].command_digest, start.command_digest);
    assert_eq!(transport.sent.len(), 1);
}

/// A start under a foreign incarnation is refused before any send —
/// the reconcile pass owns the fencing decision.
#[test]
fn ensure_started_refuses_a_foreign_incarnation() {
    let store = rig("ensure-foreign");
    let seeded = seed(&store, "ensure-foreign", INCARNATION);
    prepared(&store, &seeded);

    let mut transport = FakeTransport::new("boot-2", vec![]);
    let outcome =
        ensure_started(&store, &config(), &mut transport, &seeded.request_id, NOW).unwrap();

    assert_eq!(outcome, StartOutcome::ForeignIncarnation);
    assert!(transport.sent.is_empty());
}

/// No `prepared` receipt — no start may be minted.
#[test]
fn ensure_started_requires_prepared() {
    let store = rig("ensure-unprepared");
    let seeded = seed(&store, "ensure-unprepared", INCARNATION);

    let mut transport = FakeTransport::new(INCARNATION, vec![]);
    let outcome =
        ensure_started(&store, &config(), &mut transport, &seeded.request_id, NOW).unwrap();

    assert_eq!(outcome, StartOutcome::Unprepared);
    assert!(transport.sent.is_empty());
}

// =====================================================================
// The remote-bound boundary in the local loops
// =====================================================================

/// A remote-bound writer blocks the local stop-settle: with every
/// unresolved writer remote, `drive_stops` neither signals nor mints
/// the all-resolved exit fact — the reconcile pass owns the proof.
#[test]
fn remote_bound_writer_blocks_local_settle() {
    let store = rig("local-guard");
    let seeded = seed(&store, "local-guard", INCARNATION);
    set_exec_state(&store, &seeded, ExecutionState::Stopping);

    let sup = Supervisor::new(
        PosixProcessControl,
        tmp_root("local-guard-ws"),
        vec![],
        config(),
    );
    sup.drive_stops_scoped(&store, NOW, None, &mut || NOW, None)
        .unwrap();

    assert_eq!(
        inv_state(&store, &seeded),
        InvocationState::Running,
        "the local settle must not release a remote writer"
    );
    assert_eq!(
        exec_state(&store, &seeded.execution_id),
        ExecutionState::Stopping
    );
}
