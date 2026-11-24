//! Remote transfer/collection contract (#376,
//! `docs/executor-transport-contract.md` §6): the bounded immutable
//! input delivery and quarantined artifact collection between
//! independent coordinator and executor filesystems, driven end to
//! end with real git repositories and a real store — export →
//! chunked transfer → executor prepare → produced work → collection
//! assembly → chunked transfer → quarantined collect → acknowledge.
//! Fixtures prove contracts only: no network, no credentials, no
//! shared storage shortcut — the two sides share nothing but wire
//! bytes staged through two disjoint roots.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::executor::{
    EXECUTOR_PROTOCOL_V1, ExecutorBinding, ExecutorBindingFields, ExecutorCollectionState,
    ExecutorIdentity, ExecutorLimits, ExecutorTransferV1, RemoteHandoffV1, TransferItemV1,
    item_content_digest,
};
use workloop_core::identity::*;
use workloop_core::observation::ArtifactFact;
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_core::worker_v2::*;
use workloop_exec::bodies::{BodyStore, CAS_CATEGORY};
use workloop_exec::handoff;
use workloop_exec::remote_collect::{self, RemoteCollectLayout};
use workloop_exec::remote_custody::{self, AckTombstone, CustodyLayout};
use workloop_exec::remote_export::{self, ExportedTransfer};
use workloop_exec::remote_prepare::{self, PrepareError, RemotePrepareLayout};
use workloop_exec::remote_transfer::{self, TransferError, TransferStore, item_chunks};
use workloop_exec::workspace::RepositorySource;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;
const CHUNK: u64 = 64 * 1024;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-remote-{tag}-{}-{}",
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

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("credential.helper=")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A coordinator-side source mirror with a couple of commits.
fn source_repo(root: &Path) -> (RepositoryRef, PathBuf, String) {
    let repo = root.join("source");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "fixture@example"]);
    git(&repo, &["config", "user.name", "fixture"]);
    std::fs::write(repo.join("README.md"), "# source\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "one"]);
    std::fs::write(repo.join("src.rs"), "fn main() {}\n").unwrap();
    // A body large enough that its compressed bundle still spans
    // several wire chunks — incompressible bytes, not a pattern zlib
    // can fold.
    let mut blob = Vec::with_capacity(256 * 1024);
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").unwrap(), {
        blob.resize(256 * 1024, 0);
        &mut blob
    })
    .unwrap();
    std::fs::write(repo.join("blob.bin"), &blob).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "two"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    (
        RepositoryRef {
            host: HostInstanceId::trusted("fixture"),
            repository: RepositoryId::trusted("repo"),
        },
        repo,
        base,
    )
}

fn config(repository: &RepositoryRef) -> WorkloopConfig {
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

fn limits() -> ExecutorLimits {
    ExecutorLimits {
        max_input_bytes: 64 * 1024 * 1024,
        max_output_bytes: 64 * 1024 * 1024,
        max_item_count: 32,
        operation_timeout_ms: 3_600_000,
    }
}

/// The durable identities of one seeded remote request: work +
/// execution + request + invocation + durable v2 request body +
/// executor binding + retained goal body.
struct Seeded {
    request_id: String,
    invocation_id: String,
    work_id: WorkId,
    request: WorkerRequestV2,
    goal_bytes: Vec<u8>,
}

fn seed_remote_request(
    store: &SqliteStore,
    bodies: &BodyStore,
    repository: &RepositoryRef,
    base: &str,
    required_artifacts: Vec<RequiredArtifact>,
    max_report_bytes: u64,
    max_log_bytes: u64,
) -> Seeded {
    let (work_id, execution_id) = match handle_command(
        store,
        &config(repository),
        &CommandRequest {
            actor: Actor {
                kind: ActorKind::Human,
                external_id: "op".into(),
            },
            origin: Origin("cli".into()),
            idempotency_key: format!("remote-{}", &base[..8]),
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
            work_id: Some(work_id),
            effect:
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id },
            ..
        } => (work_id, execution_id),
        other => panic!("expected accepted submit, got {other:?}"),
    };
    let request_id = format!("{}:inv:0", execution_id.0);
    // Real invocation ids are `alloc_id("inv")` — `inv-1` — never the
    // request id (artifact refs embed them verbatim).
    let invocation_id = store.transact(|tx| tx.alloc_id("inv")).unwrap();

    // The retained goal body the request's ref names.
    let goal_bytes = goal_body_bytes(InvocationRole::Coding, "do the thing", &[]);
    let goal_digest = goal_body_digest(&goal_bytes);
    let goal_path = format!("bodies/goal/{goal_digest}");
    let catalog = store.body_catalog();
    bodies
        .retain_at(catalog, CAS_CATEGORY, &goal_path, &goal_bytes, NOW, None)
        .unwrap();

    let wire_repo = WireRepository {
        host: repository.host.as_str().to_string(),
        repository: repository.repository.as_str().to_string(),
    };
    let mut request = WorkerRequestV2 {
        format_version: WORKER_REQUEST_V2_FORMAT.into(),
        request_id: request_id.clone(),
        invocation_id: invocation_id.clone(),
        request_digest: String::new(),
        store_id: store.transact(|tx| tx.store_id()).unwrap().0,
        work_id: work_id.0.clone(),
        execution_id: execution_id.0.clone(),
        role: InvocationRole::Coding,
        repository: wire_repo.clone(),
        base: RequestBase {
            repository: wire_repo,
            object: base.into(),
        },
        spec: SpecRef {
            revision: 1,
            digest: "a".repeat(64),
        },
        workspace: WorkspaceGrant {
            workspace_id: format!("ws-{}", work_id.0),
            mount: "/workspace".into(),
            read_only: false,
        },
        budget: RequestBudget {
            deadline_unix_seconds: NOW + 3600,
            max_runtime_seconds: 1800,
            max_turns: 200,
            max_report_bytes,
            max_log_bytes,
            max_verification_commands: 8,
            max_continuation_refs: 4,
        },
        required_capabilities: RequiredCapabilities {
            role: InvocationRole::Coding,
            protocol_versions: vec![],
            capabilities: vec![],
            resume: "unsupported".into(),
            verification_egress_hosts: vec![],
        },
        continuation: vec![],
        task: TaskBody::Coding {
            goal: GoalRef {
                body_ref: goal_path,
                media_type: "text/markdown".into(),
                bytes: goal_bytes.len() as u64,
                digest: goal_digest,
            },
            issues: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    request.request_digest = request.digest();

    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: request_id.clone(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    spec_revision: 1,
                    payload: serde_json::json!({}),
                    adapter_binding: None,
                },
            })?;
            tx.apply(&Effect::InsertWorkerRequestBody {
                body: WorkerRequestBody {
                    request_id: request_id.clone(),
                    format_version: WORKER_REQUEST_V2_FORMAT.into(),
                    canonical_body: request.to_canonical_bytes(),
                    body_digest: request.digest(),
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
                    required_artifacts: required_artifacts.clone(),
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::InsertExecutorBinding {
                binding: ExecutorBinding::new(ExecutorBindingFields {
                    installation_id: "install-a".into(),
                    endpoint_id: "executor-a".into(),
                    endpoint_identity_digest: "e".repeat(64),
                    bound_incarnation_id: "boot-1".into(),
                    protocol_version: EXECUTOR_PROTOCOL_V1.into(),
                    capabilities_digest: "c".repeat(64),
                    launch_config_digest: "d".repeat(64),
                    request_id: request_id.clone(),
                    invocation_id: invocation_id.clone(),
                    request_digest: request.digest(),
                    limits: limits(),
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
        request,
        goal_bytes,
    }
}

fn identity(seeded: &Seeded) -> ExecutorIdentity {
    ExecutorIdentity {
        installation_id: "install-a".into(),
        endpoint_id: "executor-a".into(),
        endpoint_identity_digest: "e".repeat(64),
        bound_incarnation_id: "boot-1".into(),
        request_id: seeded.request_id.clone(),
        invocation_id: seeded.invocation_id.clone(),
        request_digest: seeded.request.digest(),
    }
}

/// Both sides' disjoint roots — coordinator and executor share no
/// filesystem. `exec_transfers`/`coord_transfers` are the two
/// TransferStores wire bytes pass through.
struct Rig {
    root: PathBuf,
    store: SqliteStore,
    bodies: BodyStore,
    source: RepositorySource,
    handoff_repo: PathBuf,
    exec_transfers: TransferStore,
    coord_transfers: TransferStore,
    exec_prepare: RemotePrepareLayout,
    custody: CustodyLayout,
    collect_layout: RemoteCollectLayout,
}

fn rig(tag: &str) -> (Rig, String) {
    let root = tmp_root(tag);
    let (repository, source_path, base) = source_repo(&root.join("coord"));
    let store = SqliteStore::open(&root.join("coord/store.db"), 1).unwrap();
    let bodies = BodyStore::new(root.join("coord/state"));
    let handoff_repo = root.join("coord/artifacts.git");
    git(&root.join("coord"), &["init", "--bare", "artifacts.git"]);
    // The executor's io tree and staged bodies live under the custody
    // root so the post-ack cleanup's scope check admits them; the
    // managed workspace root is the second scoped root.
    let custody_root = root.join("exec/custody");
    let exec_transfers = TransferStore::new(custody_root.join("transfers"));
    let rig = Rig {
        exec_prepare: RemotePrepareLayout {
            workspace_root: root.join("exec/ws"),
            io_root: custody_root.join("io"),
            scratch: root.join("exec/scratch"),
        },
        custody: CustodyLayout {
            workspace_root: root.join("exec/ws"),
            custody_root: custody_root.clone(),
            transfers: exec_transfers.clone(),
        },
        collect_layout: RemoteCollectLayout {
            repository: handoff_repo.clone(),
            bodies: BodyStore::new(root.join("coord/state")),
        },
        coord_transfers: TransferStore::new(root.join("coord/transfers")),
        exec_transfers,
        source: RepositorySource {
            repository,
            path: source_path,
        },
        store,
        bodies,
        handoff_repo,
        root,
    };
    (rig, base)
}

/// Move every item of a declared transfer through the wire — chunked
/// frames through the receiver's staging.
fn ship(sender: &ExportedTransfer, receiver: &TransferStore) -> PathBuf {
    receiver.declare(&sender.manifest).unwrap();
    for item in &sender.items {
        for (chunk, body) in item_chunks(&sender.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&sender.manifest, &chunk, body)
                .unwrap();
        }
    }
    receiver.finalize(&sender.manifest).unwrap()
}

/// Ship an assembled collection the same way.
fn ship_collection(
    assembled: &remote_custody::AssembledCollection,
    receiver: &TransferStore,
) -> PathBuf {
    receiver.declare(&assembled.manifest).unwrap();
    for item in &assembled.items {
        for (chunk, body) in item_chunks(&assembled.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&assembled.manifest, &chunk, body)
                .unwrap();
        }
    }
    receiver.finalize(&assembled.manifest).unwrap()
}

/// The worker's contribution: a produced commit carrying the required
/// artifact, plus the out-directory whitelist files.
fn worker_produce(
    _rig: &Rig,
    prepared: &remote_prepare::PreparedRemote,
    artifact_path: &str,
    artifact_bytes: &[u8],
) -> String {
    let ws = &prepared.workspace;
    std::fs::create_dir_all(ws.join(artifact_path).parent().unwrap()).unwrap();
    std::fs::write(ws.join(artifact_path), artifact_bytes).unwrap();
    git(ws, &["config", "user.email", "worker@example"]);
    git(ws, &["config", "user.name", "worker"]);
    git(ws, &["add", "."]);
    git(ws, &["commit", "-qm", "produced"]);
    let produced = git(ws, &["rev-parse", "HEAD"]);
    let request = &prepared.request;
    let handoff = RemoteHandoffV1 {
        format_version: workloop_core::executor::REMOTE_HANDOFF_V1_FORMAT.into(),
        request_id: request.request_id.clone(),
        invocation_id: request.invocation_id.clone(),
        repository: workloop_core::executor::HandoffRepository {
            host: request.repository.host.clone(),
            repository: request.repository.repository.clone(),
        },
        base_commit: request.base.object.clone(),
        produced_commit: produced.clone(),
        artifacts: vec![workloop_core::executor::HandoffArtifact {
            path: artifact_path.into(),
            bytes: artifact_bytes.len() as u64,
            claimed_sha256: sha256_hex("artifact-v1", artifact_bytes),
        }],
    };
    std::fs::write(
        prepared.out_dir.join("handoff.json"),
        serde_json::to_vec(&handoff).unwrap(),
    )
    .unwrap();
    std::fs::write(prepared.out_dir.join("report.json"), b"{\"status\":\"ok\"}").unwrap();
    std::fs::write(prepared.out_dir.join("runtime.log"), b"log line\n").unwrap();
    produced
}

/// The full happy path: export → transfer → prepare → produce →
/// assemble → transfer → collect → artifact ref + durable row.
#[test]
fn a_coding_request_roundtrips_coordinator_to_executor_and_back() {
    let (rig, base) = rig("roundtrip");
    let required = vec![RequiredArtifact {
        kind: "result".into(),
        path: "dist/result.json".into(),
        expected_digest: None,
    }];
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        required.clone(),
        1 << 20,
        1 << 20,
    );

    // Coordinator export: only the request, the goal body, and the
    // approved object closure leave coordinator storage.
    let scratch = rig.root.join("coord/export-scratch");
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &scratch,
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let item_paths: Vec<&str> = exported.items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(
        item_paths,
        vec!["request.json", "input/goal.md", "source.bundle"],
        "exactly the approved surface exports"
    );
    let transfer_dir = ship(&exported, &rig.exec_transfers);

    // Executor prepare: digests, object closure, and identity all
    // validate before the managed workspace exists.
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &transfer_dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    assert_eq!(prepared.head, base);
    assert_eq!(git(&prepared.workspace, &["rev-parse", "HEAD"]), base);
    assert_eq!(prepared.branch, format!("work/{}", seeded.work_id.0));
    // Input materialized read-only and byte-exact.
    let goal = std::fs::read(prepared.input_dir.join("goal.md")).unwrap();
    assert_eq!(goal, seeded.goal_bytes);
    assert!(
        std::fs::metadata(prepared.input_dir.join("goal.md"))
            .unwrap()
            .permissions()
            .readonly(),
        "input is immutable for the Worker"
    );

    // The Worker produces; the executor assembles the collection.
    let produced = worker_produce(&rig, &prepared, "dist/result.json", b"RESULT-42");
    let collection_id = format!("col-{}", seeded.request_id);
    let assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    assert_eq!(assembled.produced, produced);
    let collect_dir = ship_collection(&assembled, &rig.coord_transfers);

    // Coordinator quarantine import: produced objects verified, the
    // artifact ref established under the expected-absent guard, the
    // durable collection row committed.
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &collect_dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof-digest",
        NOW,
    )
    .unwrap();
    assert_eq!(
        collected.collection.state,
        ExecutorCollectionState::Collected,
        "invalid_reason: {:?}",
        collected.collection.invalid_reason
    );
    assert_eq!(
        collected.artifacts,
        vec![(
            required[0].clone(),
            ArtifactFact::Present {
                digest: sha256_hex("artifact-v1", b"RESULT-42")
            }
        )]
    );
    let artifact_id = format!("{}/result", seeded.invocation_id);
    assert_eq!(
        git(
            &rig.handoff_repo,
            &["rev-parse", &handoff::artifact_ref(&artifact_id)]
        ),
        produced,
        "the acknowledged artifact ref pins the exact produced commit"
    );
    let row = rig
        .store
        .transact(|tx| tx.artifact_handoff(&artifact_id))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(row.produced.as_deref(), Some(produced.as_str()));

    // The durable collection record: digest over the exact retained
    // set, stop proof bound, no truncation.
    let record = collected.collection.record().unwrap();
    assert_eq!(record.stop_proof, "stop-proof-digest");
    assert!(!record.report_truncated);
    assert_eq!(record.artifact_ids, vec![artifact_id]);
}

/// An interrupted transfer resumes at the journaled offset; a chunk
/// replayed identically is idempotent and a different digest at the
/// occupied offset is `conflicting_replay`.
#[test]
fn interruption_resume_and_conflicting_replay() {
    let (rig, base) = rig("interrupt");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let receiver = &rig.exec_transfers;
    receiver.declare(&exported.manifest).unwrap();

    // Send only part of the bundle, then "crash" — re-declare and
    // resume; the pending set reports the journaled offsets.
    let bundle_item = exported
        .items
        .iter()
        .find(|i| i.item_id == "source-bundle")
        .unwrap();
    let frames = item_chunks(&exported.manifest, "source-bundle", &bundle_item.body);
    assert!(frames.len() > 2, "the bundle spans several chunks");
    receiver
        .accept_chunk(&exported.manifest, &frames[0].0, frames[0].1)
        .unwrap();
    let progress = receiver.progress(&exported.manifest).unwrap();
    let bundle_offset = progress
        .iter()
        .find(|(id, _)| id == "source-bundle")
        .map(|(_, off)| *off)
        .unwrap();
    assert_eq!(bundle_offset, CHUNK);

    // A gap is reported with next_offset — the pending receipt's
    // payload.
    let err = receiver
        .accept_chunk(&exported.manifest, &frames[2].0, frames[2].1)
        .unwrap_err();
    assert!(matches!(err, TransferError::OutOfOrder { next_offset } if next_offset == CHUNK));

    // Identical replay of the occupied chunk is idempotent.
    let replayed = receiver
        .accept_chunk(&exported.manifest, &frames[0].0, frames[0].1)
        .unwrap();
    assert!(matches!(
        replayed,
        remote_transfer::ChunkOutcome::Replayed { next_offset } if next_offset == CHUNK
    ));

    // A different digest at the occupied offset conflicts.
    let mut forged = frames[0].0.clone();
    let bad = vec![b'x'; frames[0].1.len()];
    forged.chunk_digest = workloop_core::executor::chunk_body_digest(&bad);
    let err = receiver
        .accept_chunk(&exported.manifest, &forged, &bad)
        .unwrap_err();
    assert!(matches!(err, TransferError::ConflictingReplay { .. }));

    // Resume completes and finalizes; prepare still validates fully.
    receiver.declare(&exported.manifest).unwrap();
    for item in &exported.items {
        for (chunk, body) in item_chunks(&exported.manifest, &item.item_id, &item.body) {
            let _ = receiver.accept_chunk(&exported.manifest, &chunk, body);
        }
    }
    let dir = receiver.finalize(&exported.manifest).unwrap();
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    assert_eq!(prepared.head, base);
}

/// A corrupt or missing member is refused: the item digest check at
/// finalize catches torn bytes, and prepare re-verifies before the
/// workspace exists.
#[test]
fn missing_and_corrupt_items_are_refused() {
    let (rig, base) = rig("corrupt");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let receiver = &rig.exec_transfers;
    receiver.declare(&exported.manifest).unwrap();

    // Ship everything except the request item — finalize refuses
    // incomplete.
    for item in exported.items.iter().filter(|i| i.item_id != "request") {
        for (chunk, body) in item_chunks(&exported.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&exported.manifest, &chunk, body)
                .unwrap();
        }
    }
    let err = receiver.finalize(&exported.manifest).unwrap_err();
    assert!(matches!(err, TransferError::Incomplete { .. }));

    // Complete the transfer honestly, then corrupt the finalized
    // item — prepare's whole-item re-verification is what refuses it.
    let good = exported
        .items
        .iter()
        .find(|i| i.item_id == "request")
        .unwrap();
    for (chunk, body) in item_chunks(&exported.manifest, "request", &good.body) {
        receiver
            .accept_chunk(&exported.manifest, &chunk, body)
            .unwrap();
    }
    let dir = receiver.finalize(&exported.manifest).unwrap();
    std::fs::write(dir.join("items/request"), b"{\"forged\":true}").unwrap();
    let err = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap_err();
    assert!(
        matches!(err, PrepareError::DigestMismatch { .. }),
        "prepare re-verifies the item digest, got {err:?}"
    );
}

/// A traversal path is refused at manifest validation and a symlink
/// planted inside the finalized set is refused at prepare — no wire
/// path ever reaches a filesystem open unfiltered.
#[test]
fn traversal_paths_and_symlinks_are_refused() {
    // A manifest naming an escaping path fails its own validation —
    // the receiver never sees it.
    let bad = vec![TransferItemV1 {
        item_id: "evil".into(),
        path: "../escape".into(),
        bytes: 1,
        digest: item_content_digest(b"x"),
    }];
    assert!(ExecutorTransferV1::declared("t", "c", CHUNK, bad).is_err());

    let (rig, base) = rig("symlink");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let dir = ship(&exported, &rig.exec_transfers);
    // Plant a symlink where the staged item sat — prepare's
    // symlink_metadata check refuses it, never follows it.
    std::fs::remove_file(dir.join("items/request")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.join("items/request")).unwrap();
    let err = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        PrepareError::Invalid { .. } | PrepareError::DigestMismatch { .. }
    ));

    // And a collection carrying a symlinked out member is refused at
    // assembly — the executor never ships a link. Restore the staged
    // item and prepare honestly first.
    let request_body = exported
        .items
        .iter()
        .find(|i| i.item_id == "request")
        .unwrap()
        .body
        .clone();
    std::fs::remove_file(dir.join("items/request")).unwrap();
    std::fs::write(dir.join("items/request"), &request_body).unwrap();
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", prepared.out_dir.join("report.json")).unwrap();
    let err = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        "col-x",
        CHUNK,
    )
    .unwrap_err();
    assert!(matches!(err, remote_custody::CustodyError::Invalid { .. }));
}

/// A foreign request — a request body naming a different identity
/// than the bound record — is refused at prepare, and a foreign
/// handoff manifest at collect.
#[test]
fn a_foreign_request_is_refused() {
    let (rig, base) = rig("foreign");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let dir = ship(&exported, &rig.exec_transfers);
    // A binding for a different request refuses the whole transfer —
    // `request_identity_mismatch`, never a prepared foreign workspace.
    let mut foreign = identity(&seeded);
    foreign.request_id = "e-other:inv:0".into();
    let err = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &foreign,
    )
    .unwrap_err();
    assert!(matches!(err, PrepareError::Foreign { .. }));
}

/// A repeated collection replays the durable record — no duplicate
/// artifact refs, no second durable row — and a different collection
/// digest for the same request conflicts.
#[test]
fn repeated_collection_replays_without_duplicating() {
    let (rig, base) = rig("repeat");
    let required = vec![RequiredArtifact {
        kind: "result".into(),
        path: "dist/result.json".into(),
        expected_digest: None,
    }];
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        required,
        1 << 20,
        1 << 20,
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    let produced = worker_produce(&rig, &prepared, "dist/result.json", b"RESULT");
    let collection_id = format!("col-{}", seeded.request_id);
    let assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    let dir = ship_collection(&assembled, &rig.coord_transfers);

    let first = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(first.collection.state, ExecutorCollectionState::Collected);

    // The same collection again: replays the durable row, leaves the
    // artifact ref alone, duplicates nothing.
    let second = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert!(second.replayed, "a repeated collection replays");
    assert_eq!(
        second.collection.collection_digest,
        first.collection.collection_digest
    );
    let artifact_id = format!("{}/result", seeded.invocation_id);
    assert_eq!(
        git(
            &rig.handoff_repo,
            &["rev-parse", &handoff::artifact_ref(&artifact_id)]
        ),
        produced
    );

    // A different collection digest for the same request is a
    // conflict — the durable row refuses it, never overwrites.
    let err = upsert_conflicting(&rig, &seeded, &collection_id);
    assert!(err, "a different digest under one request conflicts");
}

fn upsert_conflicting(rig: &Rig, seeded: &Seeded, collection_id: &str) -> bool {
    use workloop_core::executor::{
        CollectionEntry, ExecutorCollection, ExecutorCollectionRecord, collection_digest,
    };
    let entries = vec![CollectionEntry {
        item_id: "report".into(),
        bytes: 1,
        content_digest: item_content_digest(b"F"),
        source: "worker".into(),
    }];
    let record = ExecutorCollectionRecord {
        request_id: seeded.request_id.clone(),
        invocation_id: seeded.invocation_id.clone(),
        collection_id: collection_id.into(),
        collection_digest: collection_digest(&entries).unwrap(),
        stop_proof: "other".into(),
        entries,
        artifact_ids: vec![],
        report_truncated: false,
        log_truncated: false,
    };
    let mut forged =
        ExecutorCollection::new(record, ExecutorCollectionState::Collected, None, 0).unwrap();
    rig.store
        .transact(|tx| {
            forged.recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertExecutorCollection {
                collection: forged.clone(),
                expected_seq: 0,
            })
        })
        .is_err()
}

fn prepare_side(
    rig: &Rig,
    seeded: &Seeded,
    transfer_id: &str,
) -> (remote_prepare::PreparedRemote, ExportedTransfer) {
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &rig.source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        transfer_id,
        CHUNK,
    )
    .unwrap();
    let dir = ship(&exported, &rig.exec_transfers);
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &exported.manifest,
        &limits(),
        &identity(seeded),
    )
    .unwrap();
    (prepared, exported)
}

/// A lost acknowledgement response never requires another
/// collection: the tombstone replays the same receipt after the
/// bodies are reclaimed, and cleanup is scoped, bounded, and
/// retryable.
#[test]
fn lost_acknowledgement_replays_the_tombstone() {
    let (rig, base) = rig("lost-ack");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![RequiredArtifact {
            kind: "result".into(),
            path: "dist/result.json".into(),
            expected_digest: None,
        }],
        1 << 20,
        1 << 20,
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    worker_produce(&rig, &prepared, "dist/result.json", b"RESULT");
    let collection_id = format!("col-{}", seeded.request_id);
    let assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(
        collected.collection.state,
        ExecutorCollectionState::Collected
    );

    // The acknowledge commits the tombstone and the durable row's
    // acknowledged state; remote WIP is then reclaimed.
    let tombstone = AckTombstone {
        request_id: seeded.request_id.clone(),
        invocation_id: seeded.invocation_id.clone(),
        collection_id: collection_id.clone(),
        collection_digest: collected.collection.collection_digest.clone(),
        bound_incarnation_id: "boot-1".into(),
        responder_incarnation_id: "boot-1".into(),
        stop_proof: "stop-proof".into(),
        receipt_body: b"{\"status\":\"acknowledged\"}".to_vec(),
    };
    remote_custody::commit_tombstone(&rig.custody, &tombstone).unwrap();
    let mut acked = collected.collection.clone();
    acked.state = ExecutorCollectionState::Acknowledged;
    rig.store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            acked.recorded_seq = seq;
            tx.apply(&Effect::UpsertExecutorCollection {
                collection: acked.clone(),
                expected_seq: collected.collection.recorded_seq,
            })
        })
        .unwrap();

    let ws = prepared.workspace.clone();
    let io = prepared.io_dir.clone();
    let staging = rig.exec_transfers.staging_dir("tr-out-1");
    let outcome = remote_custody::cleanup_after_ack(
        &rig.custody,
        &seeded.request_id,
        &collection_id,
        &[ws.clone(), io.clone(), staging],
    )
    .unwrap();
    assert_eq!(outcome, remote_custody::CleanupOutcome::Complete);
    assert!(
        !ws.exists() && !io.exists(),
        "remote WIP is reclaimed after ack"
    );

    // The lost-ack case: the acknowledge replays — the tombstone
    // returns the identical receipt body, no re-collection needed.
    let replay = remote_custody::read_tombstone(&rig.custody, &collection_id)
        .unwrap()
        .expect("tombstone survives cleanup");
    assert_eq!(replay.receipt_body, tombstone.receipt_body);
    assert_eq!(
        replay.collection_digest,
        collected.collection.collection_digest
    );

    // A repeated collect on the coordinator side replays the durable
    // acknowledged row — never another artifact commit.
    let replayed = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert!(replayed.replayed);
    assert_eq!(
        replayed.collection.state,
        ExecutorCollectionState::Acknowledged
    );
}

/// An oversized report or log is retained only to the Invocation
/// bound and the durable record flags the truncation — nothing is
/// silently whole.
#[test]
fn oversized_report_and_log_truncate_honestly() {
    let (rig, base) = rig("truncate");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        64, // 64-byte report bound
        32, // 32-byte log bound
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    let report = vec![b'r'; 200];
    let log = vec![b'l'; 100];
    std::fs::write(prepared.out_dir.join("report.json"), &report).unwrap();
    std::fs::write(prepared.out_dir.join("runtime.log"), &log).unwrap();
    let collection_id = format!("col-{}", seeded.request_id);
    let assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    let record = collected.collection.record().unwrap();
    assert!(record.report_truncated && record.log_truncated);
    let retained = collected.report.unwrap();
    assert_eq!(retained.bytes.len(), 64);
    assert!(retained.truncated);
    let retained_log = collected.log.unwrap();
    assert_eq!(retained_log.bytes.len(), 32);
    assert!(retained_log.truncated);
    // The collection-digest entries record the retained prefix, not
    // the untruncated claim.
    let report_entry = record
        .entries
        .iter()
        .find(|e| e.item_id == "report")
        .unwrap();
    assert_eq!(report_entry.bytes, 64);
    assert_eq!(
        report_entry.content_digest,
        item_content_digest(&retained.bytes)
    );
}

/// A foreign handoff manifest — a Worker naming a different request
/// or invocation — is an invalid collection, never Present evidence.
#[test]
fn a_foreign_handoff_manifest_is_invalid() {
    let (rig, base) = rig("foreign-manifest");
    let required = vec![RequiredArtifact {
        kind: "result".into(),
        path: "dist/result.json".into(),
        expected_digest: None,
    }];
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        required,
        1 << 20,
        1 << 20,
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    let produced = worker_produce(&rig, &prepared, "dist/result.json", b"RESULT");
    // Forge the manifest's invocation identity.
    let foreign = RemoteHandoffV1 {
        format_version: workloop_core::executor::REMOTE_HANDOFF_V1_FORMAT.into(),
        request_id: seeded.request_id.clone(),
        invocation_id: "inv-someone-else".into(),
        repository: workloop_core::executor::HandoffRepository {
            host: "fixture".into(),
            repository: "repo".into(),
        },
        base_commit: base.clone(),
        produced_commit: produced,
        artifacts: vec![workloop_core::executor::HandoffArtifact {
            path: "dist/result.json".into(),
            bytes: 6,
            claimed_sha256: sha256_hex("artifact-v1", b"RESULT"),
        }],
    };
    std::fs::write(
        prepared.out_dir.join("handoff.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    let collection_id = format!("col-{}", seeded.request_id);
    let assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("collection_invalid")
    );
    // No artifact ref and no handoff row exists — invalid never
    // becomes Present evidence.
    let artifact_id = format!("{}/result", seeded.invocation_id);
    let ref_missing = std::process::Command::new("git")
        .arg("-C")
        .arg(&rig.handoff_repo)
        .args([
            "rev-parse",
            "--verify",
            &handoff::artifact_ref(&artifact_id),
        ])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(false);
    assert!(ref_missing, "a foreign manifest produces no artifact ref");
    assert!(
        rig.store
            .transact(|tx| tx.artifact_handoff(&artifact_id))
            .unwrap()
            .is_none()
    );
}
