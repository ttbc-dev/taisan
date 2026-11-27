//! Contract tests: durable executor endpoint bindings and operation
//! records (`executor_bindings`, `executor_operations`, store format
//! 14, `docs/executor-transport-contract.md` §2/§4). The binding is the
//! immutable first record for a remote request — written before the
//! first operation is sent; the operation ledger holds each command's
//! canonical bytes before any endpoint contact, and a receipt attaches
//! only to its recorded command.

use std::collections::BTreeSet;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::executor::{
    EXECUTOR_PROTOCOL_V1, ExecutorBinding, ExecutorBindingFields, ExecutorCommandV1,
    ExecutorIdentity, ExecutorLimits, ExecutorOperation, ExecutorReceiptV1, OperationKind,
};
use workloop_core::identity::*;
use workloop_core::store::*;
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
        placement: Ok(workloop_core::placement::PlacementPolicy::default()),
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

/// Open a store, admit one Work, and plant the durable request row a
/// binding attaches to.
fn seeded_request(store: &SqliteStore, key: &str) -> String {
    let (work_id, execution_id) = match handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: "do the thing".into(),
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
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id } => execution_id.clone(),
                other => panic!("expected an execution effect, got {other:?}"),
            };
            (work_id.clone().expect("work_id"), eid)
        }
        other => panic!("expected accepted, got {other:?}"),
    };
    let request_id = format!("{key}:inv:0");
    apply_effects(
        store,
        &[Effect::InsertRequest {
            request: InvocationRequest {
                placement: None,
                request_id: request_id.clone(),
                work_id,
                execution_id,
                role: InvocationRole::Coding,
                spec_revision: 1,
                payload: serde_json::json!({}),
                adapter_binding: None,
            },
        }],
    );
    request_id
}

fn identity(request_id: &str) -> ExecutorIdentity {
    ExecutorIdentity {
        installation_id: "install-a".into(),
        endpoint_id: "executor-a".into(),
        endpoint_identity_digest: "a".repeat(64),
        bound_incarnation_id: "boot-7f1c".into(),
        request_id: request_id.into(),
        invocation_id: format!("inv-{request_id}"),
        request_digest: "b".repeat(64),
    }
}

fn binding_fields(request_id: &str) -> ExecutorBindingFields {
    ExecutorBindingFields {
        installation_id: "install-a".into(),
        endpoint_id: "executor-a".into(),
        endpoint_identity_digest: "a".repeat(64),
        bound_incarnation_id: "boot-7f1c".into(),
        protocol_version: EXECUTOR_PROTOCOL_V1.into(),
        capabilities_digest: "e".repeat(64),
        launch_config_digest: "c".repeat(64),
        request_id: request_id.into(),
        invocation_id: format!("inv-{request_id}"),
        request_digest: "b".repeat(64),
        limits: ExecutorLimits {
            max_input_bytes: 4 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_item_count: 64,
            operation_timeout_ms: 3_600_000,
        },
    }
}

fn binding(request_id: &str) -> ExecutorBinding {
    ExecutorBinding::new(binding_fields(request_id)).expect("binding seals")
}

fn command(request_id: &str, operation_id: &str, kind: OperationKind) -> ExecutorCommandV1 {
    ExecutorCommandV1::new(
        identity(request_id),
        operation_id,
        kind,
        serde_json::json!({"prepared_id": format!("prep-{request_id}")}),
    )
    .expect("command seals")
}

fn operation(request_id: &str, operation_id: &str, kind: OperationKind) -> ExecutorOperation {
    ExecutorOperation::new(&command(request_id, operation_id, kind)).expect("operation seals")
}

fn started_receipt(command: &ExecutorCommandV1) -> ExecutorReceiptV1 {
    ExecutorReceiptV1::new(
        ExecutorIdentity {
            installation_id: command.installation_id.clone(),
            endpoint_id: command.endpoint_id.clone(),
            endpoint_identity_digest: command.endpoint_identity_digest.clone(),
            bound_incarnation_id: command.bound_incarnation_id.clone(),
            request_id: command.request_id.clone(),
            invocation_id: command.invocation_id.clone(),
            request_digest: command.request_digest.clone(),
        },
        command.bound_incarnation_id.clone(),
        command.operation_id.clone(),
        command.operation_kind,
        command.payload_digest.clone(),
        command.command_digest.clone(),
        serde_json::json!({
            "status": "started",
            "process": {
                "evidence_ref": "proc-evidence-41",
                "identity_digest": "d".repeat(64),
            }
        }),
    )
    .expect("receipt seals")
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "workloop-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The binding reads back verbatim — the recorded canonical bytes and
/// digest, never reconstructed.
#[test]
fn a_recorded_binding_reads_back_verbatim() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "bind-rt");
    let recorded = binding(&request_id);
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: recorded.clone(),
        }],
    );
    let read = store
        .transact(|tx| tx.executor_binding(&request_id))
        .unwrap()
        .expect("the binding is durable");
    assert_eq!(read, recorded);
    assert_eq!(read.fields().unwrap(), binding_fields(&request_id));
}

/// Re-recording identical binding content is a replay; different
/// content under the same request is the store's `conflicting_replay`
/// — a bound request is never silently moved to another endpoint,
/// incarnation, or launch configuration.
#[test]
fn binding_replay_is_idempotent_and_conflict_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "bind-replay");
    let recorded = binding(&request_id);
    for _ in 0..2 {
        apply_effects(
            &store,
            &[Effect::InsertExecutorBinding {
                binding: recorded.clone(),
            }],
        );
    }
    // A different endpoint is a different binding — refused.
    let mut other = binding_fields(&request_id);
    other.endpoint_id = "executor-b".into();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertExecutorBinding {
                binding: ExecutorBinding::new(other).unwrap(),
            })
        })
        .expect_err("a different binding under one request conflicts");
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "the refusal is a typed conflict: {err:?}"
    );
    // A stale incarnation is likewise a different binding.
    let mut stale = binding_fields(&request_id);
    stale.bound_incarnation_id = "boot-dead".into();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertExecutorBinding {
                binding: ExecutorBinding::new(stale).unwrap(),
            })
        })
        .expect_err("a different incarnation conflicts");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // The original binding survives every refused write.
    let read = store
        .transact(|tx| tx.executor_binding(&request_id))
        .unwrap()
        .expect("the original binding survives");
    assert_eq!(read, recorded);
}

/// A binding without its request row is refused by the foreign key —
/// remote bindings only exist attached to a durable request.
#[test]
fn a_binding_without_a_request_row_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertExecutorBinding {
                binding: binding("req:nonexistent"),
            })
        })
        .expect_err("an orphaned binding is refused");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
}

/// The command record commits inside the transaction — before any
/// endpoint contact — and reads back verbatim for replay.
#[test]
fn a_command_persists_before_send_and_reads_back() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "op-rt");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let recorded = operation(&request_id, "op-start", OperationKind::Start);
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: recorded.clone(),
        }],
    );
    let read = store
        .transact(|tx| tx.executor_operation(&request_id, "op-start"))
        .unwrap()
        .expect("the command is durable");
    assert_eq!(read, recorded);
    assert!(read.receipt_body.is_none());
    let all = store
        .transact(|tx| tx.executor_operations_for_request(&request_id))
        .unwrap();
    assert_eq!(all, vec![recorded]);
}

/// A command naming a request with no binding is refused — the
/// operation ledger can only hold commands under a recorded binding.
#[test]
fn a_command_for_an_unbound_request_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "op-nobind");
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorCommand {
                operation: operation(&request_id, "op-start", OperationKind::Start),
            })
        })
        .expect_err("a command without a binding is refused");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .unwrap()
            .is_none()
    );
}

/// The store's replay fence: identical command bytes replay, differing
/// bytes under the same operation identity conflict — the durable
/// record of what was sent can never silently change.
#[test]
fn command_replay_is_idempotent_and_mismatched_bytes_conflict() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "op-replay");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let recorded = operation(&request_id, "op-start", OperationKind::Start);
    for _ in 0..2 {
        apply_effects(
            &store,
            &[Effect::RecordExecutorCommand {
                operation: recorded.clone(),
            }],
        );
    }
    // The same operation identity with different bytes conflicts —
    // the payload differs under the bound request's own identity.
    let changed = ExecutorCommandV1::new(
        identity(&request_id),
        "op-start",
        OperationKind::Start,
        serde_json::json!({"prepared_id": "prep-different"}),
    )
    .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorCommand {
                operation: ExecutorOperation::new(&changed).unwrap(),
            })
        })
        .expect_err("different bytes under one operation identity conflict");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert_eq!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .unwrap()
            .unwrap(),
        recorded
    );
}

/// A second start under any operation identity conflicts — the store
/// holds at most one durable start record per bound request.
#[test]
fn a_second_start_slot_conflicts() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "op-start2");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: operation(&request_id, "op-start-a", OperationKind::Start),
        }],
    );
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorCommand {
                operation: operation(&request_id, "op-start-b", OperationKind::Start),
            })
        })
        .expect_err("a second start conflicts");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // The original start is untouched.
    assert_eq!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start-a"))
            .unwrap()
            .unwrap()
            .operation_kind,
        OperationKind::Start
    );
    // Other operations on the same request still record.
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: operation(&request_id, "op-observe", OperationKind::Observe),
        }],
    );
}

/// A command whose identity disagrees with the recorded binding —
/// foreign endpoint, stale incarnation, foreign request — is refused
/// before it is ever sent.
#[test]
fn a_command_foreign_to_the_binding_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "op-foreign");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    for (label, mutate) in [
        (
            "foreign endpoint",
            Box::new(|id: &mut ExecutorIdentity| id.endpoint_id = "executor-z".into())
                as Box<dyn FnOnce(&mut ExecutorIdentity)>,
        ),
        (
            "stale incarnation",
            Box::new(|id: &mut ExecutorIdentity| id.bound_incarnation_id = "boot-dead".into())
                as Box<dyn FnOnce(&mut ExecutorIdentity)>,
        ),
        (
            "foreign request",
            Box::new(|id: &mut ExecutorIdentity| id.request_digest = "9".repeat(64))
                as Box<dyn FnOnce(&mut ExecutorIdentity)>,
        ),
    ] {
        let mut id = identity(&request_id);
        mutate(&mut id);
        let changed = ExecutorCommandV1::new(
            id,
            format!("op-{label}"),
            OperationKind::Observe,
            serde_json::json!({}),
        )
        .unwrap();
        let err = store
            .transact(|tx| {
                tx.apply(&Effect::RecordExecutorCommand {
                    operation: ExecutorOperation::new(&changed).unwrap(),
                })
            })
            .expect_err("{label} is refused");
        assert!(
            matches!(err, StoreError::ConstraintConflict { .. }),
            "{label} conflicts: {err:?}"
        );
        assert!(
            store
                .transact(|tx| tx.executor_operation(&request_id, &format!("op-{label}")))
                .unwrap()
                .is_none(),
            "no row is left behind for {label}"
        );
    }
}

/// A receipt attaches to its recorded command: parsed, echoed, and
/// digest-verified. An identical re-record replays; differing bytes
/// conflict; a receipt for an unrecorded operation is refused.
#[test]
fn a_receipt_attaches_only_to_its_recorded_command() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "rcpt-rt");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let cmd = command(&request_id, "op-start", OperationKind::Start);
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: ExecutorOperation::new(&cmd).unwrap(),
        }],
    );
    let receipt = started_receipt(&cmd);
    let receipt_bytes = receipt.to_canonical_bytes().unwrap();
    apply_effects(
        &store,
        &[Effect::RecordExecutorReceipt {
            request_id: request_id.clone(),
            operation_id: "op-start".into(),
            receipt_body: receipt_bytes.clone(),
        }],
    );
    let read = store
        .transact(|tx| tx.executor_operation(&request_id, "op-start"))
        .unwrap()
        .unwrap();
    assert_eq!(read.receipt_body.as_deref(), Some(receipt_bytes.as_slice()));
    assert_eq!(
        read.receipt_digest.as_deref(),
        Some(receipt.receipt_digest.as_str())
    );
    // Replaying the same receipt bytes is idempotent.
    apply_effects(
        &store,
        &[Effect::RecordExecutorReceipt {
            request_id: request_id.clone(),
            operation_id: "op-start".into(),
            receipt_body: receipt_bytes,
        }],
    );
    // Differing bytes under the same receipt identity conflict.
    let other = ExecutorReceiptV1::new(
        ExecutorIdentity {
            installation_id: cmd.installation_id.clone(),
            endpoint_id: cmd.endpoint_id.clone(),
            endpoint_identity_digest: cmd.endpoint_identity_digest.clone(),
            bound_incarnation_id: cmd.bound_incarnation_id.clone(),
            request_id: cmd.request_id.clone(),
            invocation_id: cmd.invocation_id.clone(),
            request_digest: cmd.request_digest.clone(),
        },
        cmd.bound_incarnation_id.clone(),
        cmd.operation_id.clone(),
        cmd.operation_kind,
        cmd.payload_digest.clone(),
        cmd.command_digest.clone(),
        serde_json::json!({
            "status": "known_not_started",
            "reason": "known_not_started",
        }),
    )
    .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id: request_id.clone(),
                operation_id: "op-start".into(),
                receipt_body: other.to_canonical_bytes().unwrap(),
            })
        })
        .expect_err("a different receipt under one operation conflicts");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // The recorded receipt survives.
    assert_eq!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .unwrap()
            .unwrap()
            .receipt_digest
            .as_deref(),
        Some(receipt.receipt_digest.as_str())
    );
}

/// A receipt whose echo fields name a different request, incarnation,
/// operation, or digest is foreign evidence — refused, never recorded.
#[test]
fn a_receipt_with_foreign_echo_is_refused() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "rcpt-foreign");
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let cmd = command(&request_id, "op-start", OperationKind::Start);
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: ExecutorOperation::new(&cmd).unwrap(),
        }],
    );
    // Foreign request_id: the receipt answers a different command.
    let mut foreign_identity = ExecutorIdentity {
        installation_id: cmd.installation_id.clone(),
        endpoint_id: cmd.endpoint_id.clone(),
        endpoint_identity_digest: cmd.endpoint_identity_digest.clone(),
        bound_incarnation_id: cmd.bound_incarnation_id.clone(),
        request_id: "other:inv:9".into(),
        invocation_id: cmd.invocation_id.clone(),
        request_digest: cmd.request_digest.clone(),
    };
    let foreign = ExecutorReceiptV1::new(
        foreign_identity.clone(),
        cmd.bound_incarnation_id.clone(),
        cmd.operation_id.clone(),
        cmd.operation_kind,
        cmd.payload_digest.clone(),
        cmd.command_digest.clone(),
        serde_json::json!({
            "status": "started",
            "process": {"evidence_ref": "p", "identity_digest": "d".repeat(64)},
        }),
    )
    .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id: request_id.clone(),
                operation_id: "op-start".into(),
                receipt_body: foreign.to_canonical_bytes().unwrap(),
            })
        })
        .expect_err("a receipt answering a foreign request is refused");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // Stale responder incarnation bound to the wrong command digest.
    foreign_identity.request_id = request_id.clone();
    let stale = ExecutorReceiptV1::new(
        foreign_identity,
        "boot-dead",
        cmd.operation_id.clone(),
        cmd.operation_kind,
        cmd.payload_digest.clone(),
        "f".repeat(64),
        serde_json::json!({
            "status": "started",
            "process": {"evidence_ref": "p", "identity_digest": "d".repeat(64)},
        }),
    )
    .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id: request_id.clone(),
                operation_id: "op-start".into(),
                receipt_body: stale.to_canonical_bytes().unwrap(),
            })
        })
        .expect_err("a receipt answering a stale command digest is refused");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    // A receipt for an unrecorded operation is refused.
    let orphan = ExecutorReceiptV1::new(
        ExecutorIdentity {
            installation_id: cmd.installation_id.clone(),
            endpoint_id: cmd.endpoint_id.clone(),
            endpoint_identity_digest: cmd.endpoint_identity_digest.clone(),
            bound_incarnation_id: cmd.bound_incarnation_id.clone(),
            request_id: cmd.request_id.clone(),
            invocation_id: cmd.invocation_id.clone(),
            request_digest: cmd.request_digest.clone(),
        },
        cmd.bound_incarnation_id.clone(),
        "op-never-sent",
        OperationKind::Observe,
        cmd.payload_digest.clone(),
        cmd.command_digest.clone(),
        serde_json::json!({"status": "unknown", "reason": "unknown"}),
    )
    .unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id,
                operation_id: "op-never-sent".into(),
                receipt_body: orphan.to_canonical_bytes().unwrap(),
            })
        })
        .expect_err("a receipt without a recorded command is refused");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
}

/// Restart durability: records written on a file-backed store survive
/// a close and reopen byte-for-byte, and replaying the same bytes
/// under the fresh connection is still an idempotent no-op — the
/// persist-before-send guarantee does not depend on the connection.
#[test]
fn records_survive_a_restart_and_replay_still_resolves() {
    let root = temp_root("restart-replay");
    let db = root.join("store.db");
    let (request_id, recorded, receipt_bytes) = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = seeded_request(&store, "restart");
        apply_effects(
            &store,
            &[Effect::InsertExecutorBinding {
                binding: binding(&id),
            }],
        );
        let cmd = command(&id, "op-start", OperationKind::Start);
        let operation = ExecutorOperation::new(&cmd).unwrap();
        apply_effects(
            &store,
            &[Effect::RecordExecutorCommand {
                operation: operation.clone(),
            }],
        );
        let bytes = started_receipt(&cmd).to_canonical_bytes().unwrap();
        apply_effects(
            &store,
            &[Effect::RecordExecutorReceipt {
                request_id: id.clone(),
                operation_id: "op-start".into(),
                receipt_body: bytes.clone(),
            }],
        );
        drop(store);
        (id, operation, bytes)
    };
    let store = SqliteStore::open(&db, 1).unwrap();
    // The whole record — command bytes and receipt — is durable
    // across the restart.
    let read = store
        .transact(|tx| tx.executor_operation(&request_id, "op-start"))
        .unwrap()
        .expect("the operation survives the restart");
    assert!(read.verifies());
    assert_eq!(read.command_body, recorded.command_body);
    assert_eq!(read.receipt_body.as_deref(), Some(receipt_bytes.as_slice()));
    // Replaying the identical command and receipt bytes under the
    // fresh connection is still an idempotent no-op.
    apply_effects(
        &store,
        &[
            Effect::RecordExecutorCommand {
                operation: recorded,
            },
            Effect::RecordExecutorReceipt {
                request_id,
                operation_id: "op-start".into(),
                receipt_body: receipt_bytes,
            },
        ],
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// A failed transaction leaves no rows: binding, command, and receipt
/// are durable only together inside their transaction — there is no
/// path where an operation record exists without its binding.
#[test]
fn a_failed_transaction_leaves_no_executor_rows() {
    let store = SqliteStore::in_memory(1).unwrap();
    let request_id = seeded_request(&store, "tx-fail");
    // Seed a conflicting binding so the second apply inside the
    // transaction fails — proving the first insert rolls back too.
    let mut other = binding_fields(&request_id);
    other.endpoint_id = "executor-b".into();
    let conflicting = ExecutorBinding::new(other).unwrap();
    let err = store
        .transact(|tx| {
            tx.apply(&Effect::InsertExecutorBinding {
                binding: binding(&request_id),
            })?;
            tx.apply(&Effect::InsertExecutorBinding {
                binding: conflicting,
            })
        })
        .expect_err("the conflicting second insert fails the transaction");
    assert!(matches!(err, StoreError::ConstraintConflict { .. }));
    assert!(
        store
            .transact(|tx| tx.executor_binding(&request_id))
            .unwrap()
            .is_none(),
        "no binding survives the rolled-back transaction"
    );
    // Now commit a clean binding + command + receipt in one
    // transaction — the atomic unit the dispatcher relies on.
    let cmd = command(&request_id, "op-start", OperationKind::Start);
    let receipt = started_receipt(&cmd);
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertExecutorBinding {
                binding: binding(&request_id),
            })?;
            tx.apply(&Effect::RecordExecutorCommand {
                operation: ExecutorOperation::new(&cmd).unwrap(),
            })?;
            tx.apply(&Effect::RecordExecutorReceipt {
                request_id: request_id.clone(),
                operation_id: "op-start".into(),
                receipt_body: receipt.to_canonical_bytes().unwrap(),
            })
        })
        .expect("the full chain commits atomically");
    assert!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .unwrap()
            .unwrap()
            .verifies()
    );
}

/// Regression: a v13 store gains `executor_bindings` and
/// `executor_operations` on open, keeps every pre-upgrade row, and
/// the new tables accept writes on the upgraded file.
#[test]
fn a_v13_store_gains_the_executor_tables() {
    let root = temp_root("upgrade-v13");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = seeded_request(&store, "up-v13");
        drop(store);
        id
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE executor_operations; DROP TABLE executor_bindings; \
             PRAGMA user_version=13;",
        )
        .unwrap();
        let has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='executor_bindings')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has, "the reshaped file is a real v13 store");
    }
    let store = SqliteStore::open(&db, 1).expect("a v13 store upgrades in place");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 17, "the upgrade lands on the current format");
    assert!(
        store
            .transact(|tx| tx.request(&request_id))
            .unwrap()
            .is_some(),
        "the pre-upgrade request survives"
    );
    assert!(
        store
            .transact(|tx| tx.executor_binding(&request_id))
            .unwrap()
            .is_none(),
        "the v13 file carried no binding"
    );
    apply_effects(
        &store,
        &[Effect::InsertExecutorBinding {
            binding: binding(&request_id),
        }],
    );
    let cmd = command(&request_id, "op-start", OperationKind::Start);
    apply_effects(
        &store,
        &[Effect::RecordExecutorCommand {
            operation: ExecutorOperation::new(&cmd).unwrap(),
        }],
    );
    assert!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .unwrap()
            .is_some(),
        "the upgraded file records operations"
    );
    drop(store);
    let _ = SqliteStore::open(&db, 1).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// A tampered binding or command row reads as corrupt — an error, never
/// a silently absent record the caller could misread as "not remote".
#[test]
fn a_corrupt_row_reads_as_an_error() {
    let root = temp_root("corrupt-rows");
    let db = root.join("store.db");
    let request_id = {
        let store = SqliteStore::open(&db, 1).unwrap();
        let id = seeded_request(&store, "corrupt");
        apply_effects(
            &store,
            &[Effect::InsertExecutorBinding {
                binding: binding(&id),
            }],
        );
        let cmd = command(&id, "op-start", OperationKind::Start);
        apply_effects(
            &store,
            &[Effect::RecordExecutorCommand {
                operation: ExecutorOperation::new(&cmd).unwrap(),
            }],
        );
        drop(store);
        id
    };
    // Corrupt both stored bodies in place: the verbatim replay path
    // must refuse forged bytes rather than hand them to an endpoint.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "UPDATE executor_bindings SET binding_body=X'7b7d'; \
             UPDATE executor_operations SET command_body=X'7b7d';",
        )
        .unwrap();
    let store = SqliteStore::open(&db, 1).unwrap();
    assert!(
        store
            .transact(|tx| tx.executor_binding(&request_id))
            .is_err(),
        "a forged binding row is an error, never a silently absent binding"
    );
    assert!(
        store
            .transact(|tx| tx.executor_operation(&request_id, "op-start"))
            .is_err(),
        "a forged command row is an error, never replayed to an endpoint"
    );
    let _ = std::fs::remove_dir_all(&root);
}
