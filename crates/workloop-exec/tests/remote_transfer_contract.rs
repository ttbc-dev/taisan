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
use workloop_core::store::{Effect, Store, StoreError};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command};
use workloop_core::worker_v2::*;
use workloop_exec::bodies::{BodyStore, CAS_CATEGORY, content_id};
use workloop_exec::handoff;
use workloop_exec::remote_collect::{self, RemoteCollectLayout};
use workloop_exec::remote_custody::{self, AckTombstone, CustodyLayout};
use workloop_exec::remote_export::{self, ExportedItem, ExportedTransfer};
use workloop_exec::remote_prepare::{self, PrepareError, RemotePrepareLayout};
use workloop_exec::remote_transfer::{
    self, TransferBound, TransferError, TransferStore, item_chunks,
};
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

/// The receiver-side bound each direction stages under — the same
/// immutable `ExecutorLimits` the bound request carries.
fn in_bound() -> TransferBound {
    TransferBound::input(&limits())
}

fn out_bound() -> TransferBound {
    TransferBound::output(&limits())
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
    receiver.declare(&sender.manifest, &in_bound()).unwrap();
    for item in &sender.items {
        for (chunk, body) in item_chunks(&sender.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&sender.manifest, &in_bound(), &chunk, body)
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
    receiver.declare(&assembled.manifest, &out_bound()).unwrap();
    for item in &assembled.items {
        for (chunk, body) in item_chunks(&assembled.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&assembled.manifest, &out_bound(), &chunk, body)
                .unwrap();
        }
    }
    receiver.finalize(&assembled.manifest).unwrap()
}

/// The worker's contribution: a produced commit carrying the required
/// artifact, plus the out-directory whitelist files.
fn worker_produce(
    rig: &Rig,
    prepared: &remote_prepare::PreparedRemote,
    artifact_path: &str,
    artifact_bytes: &[u8],
) -> String {
    worker_produce_many(rig, prepared, &[(artifact_path, artifact_bytes)])
}

/// The multi-artifact form: one produced commit carrying every named
/// output, and a `handoff.json` describing the whole set — the
/// manifest's descriptor list is sorted by UTF-8 path bytes as the
/// contract requires.
fn worker_produce_many(
    _rig: &Rig,
    prepared: &remote_prepare::PreparedRemote,
    outputs: &[(&str, &[u8])],
) -> String {
    let ws = &prepared.workspace;
    let mut descriptors = Vec::with_capacity(outputs.len());
    for (artifact_path, artifact_bytes) in outputs {
        std::fs::create_dir_all(ws.join(artifact_path).parent().unwrap()).unwrap();
        std::fs::write(ws.join(artifact_path), artifact_bytes).unwrap();
        descriptors.push(workloop_core::executor::HandoffArtifact {
            path: (*artifact_path).into(),
            bytes: artifact_bytes.len() as u64,
            claimed_sha256: sha256_hex("artifact-v1", artifact_bytes),
        });
    }
    descriptors.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
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
        artifacts: descriptors,
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

/// Seed a CodeReview request: same durable plumbing as a Coding
/// request but the read-only review role, a `review-source` body
/// retained under the path the export resolves, and the exact diff
/// target the request binds.
fn seed_review_request(
    store: &SqliteStore,
    bodies: &BodyStore,
    repository: &RepositoryRef,
    base: &str,
    target: workloop_core::worker_v2::DiffTarget,
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
            idempotency_key: format!("remote-review-{}", &base[..8]),
            body: CommandBody::Submit {
                intent: "review the thing".into(),
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

    let review_body = ReviewSourceBody::new(
        work_id.0.clone(),
        serde_json::to_value(&target).unwrap(),
        None,
    );
    let review_bytes = review_body.canonical_bytes();
    let review_digest = review_body.wire_digest();
    let review_path = format!("bodies/review-source/{review_digest}");
    let catalog = store.body_catalog();
    bodies
        .retain_at(
            catalog,
            CAS_CATEGORY,
            &review_path,
            &review_bytes,
            NOW,
            None,
        )
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
        role: InvocationRole::Review,
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
            read_only: true,
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
            role: InvocationRole::Review,
            protocol_versions: vec![],
            capabilities: vec![],
            resume: "unsupported".into(),
            verification_egress_hosts: vec![],
        },
        continuation: vec![],
        task: TaskBody::CodeReview {
            target,
            review_source: ReviewSourceRef {
                source: "review-1".into(),
                bytes: review_bytes.len() as u64,
                digest: review_digest,
                source_revision: content_id(&review_bytes),
            },
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
                    role: InvocationRole::Review,
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
                    role: InvocationRole::Review,
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
        goal_bytes: Vec::new(),
    }
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
        &limits(),
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

    // The durable collection record: the digest is one identity both
    // sides agree on — the coordinator's durable row equals the
    // executor's assembled digest, so an `acknowledge` can match it.
    let record = collected.collection.record().unwrap();
    assert_eq!(record.collection_digest, assembled.digest);
    assert_eq!(record.stop_proof, "stop-proof-digest");
    assert!(!record.report_truncated);
    assert_eq!(record.artifact_ids, vec![artifact_id]);
}

/// A zero-byte required artifact proves no bytes: remote verification
/// reports it `Missing`, exactly as the local supervisor does, and
/// establishes no artifact ref or acknowledged handoff row. An empty
/// blob must never become `Present` just because its declared size and
/// claimed digest agree at zero.
#[test]
fn an_empty_required_artifact_is_missing_not_present() {
    let (rig, base) = rig("empty-artifact");
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
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    // The Worker commits an empty artifact and honestly describes its
    // zero size and empty digest — the claim is internally consistent,
    // yet there is no artifact evidence.
    let produced = worker_produce(&rig, &prepared, "dist/result.json", b"");
    assert_ne!(produced, base, "the work branch moved past its start");
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
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
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
        vec![(required[0].clone(), ArtifactFact::Missing)],
        "an empty blob is Missing, never Present"
    );
    assert!(
        collected
            .collection
            .record()
            .unwrap()
            .artifact_ids
            .is_empty(),
        "no artifact id is recorded for empty bytes"
    );
    let artifact_id = format!("{}/result", seeded.invocation_id);
    let ref_status = std::process::Command::new("git")
        .arg("-C")
        .arg(&rig.handoff_repo)
        .args([
            "rev-parse",
            "--verify",
            &handoff::artifact_ref(&artifact_id),
        ])
        .output()
        .unwrap()
        .status;
    assert!(
        !ref_status.success(),
        "an empty artifact establishes no artifact ref"
    );
    assert!(
        rig.store
            .transact(|tx| tx.artifact_handoff(&artifact_id))
            .unwrap()
            .is_none(),
        "an empty artifact records no acknowledged handoff row"
    );
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
    receiver.declare(&exported.manifest, &in_bound()).unwrap();

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
        .accept_chunk(&exported.manifest, &in_bound(), &frames[0].0, frames[0].1)
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
        .accept_chunk(&exported.manifest, &in_bound(), &frames[2].0, frames[2].1)
        .unwrap_err();
    assert!(matches!(err, TransferError::OutOfOrder { next_offset } if next_offset == CHUNK));

    // Identical replay of the occupied chunk is idempotent.
    let replayed = receiver
        .accept_chunk(&exported.manifest, &in_bound(), &frames[0].0, frames[0].1)
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
        .accept_chunk(&exported.manifest, &in_bound(), &forged, &bad)
        .unwrap_err();
    assert!(matches!(err, TransferError::ConflictingReplay { .. }));

    // Resume completes and finalizes; prepare still validates fully.
    receiver.declare(&exported.manifest, &in_bound()).unwrap();
    for item in &exported.items {
        for (chunk, body) in item_chunks(&exported.manifest, &item.item_id, &item.body) {
            let _ = receiver.accept_chunk(&exported.manifest, &in_bound(), &chunk, body);
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
    receiver.declare(&exported.manifest, &in_bound()).unwrap();

    // Ship everything except the request item — finalize refuses
    // incomplete.
    for item in exported.items.iter().filter(|i| i.item_id != "request") {
        for (chunk, body) in item_chunks(&exported.manifest, &item.item_id, &item.body) {
            receiver
                .accept_chunk(&exported.manifest, &in_bound(), &chunk, body)
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
            .accept_chunk(&exported.manifest, &in_bound(), &chunk, body)
            .unwrap();
    }
    let dir = receiver.finalize(&exported.manifest).unwrap();
    std::fs::write(
        remote_transfer::item_path(&dir, "request"),
        b"{\"forged\":true}",
    )
    .unwrap();
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
    std::fs::remove_file(remote_transfer::item_path(&dir, "request")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", remote_transfer::item_path(&dir, "request")).unwrap();
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
    std::fs::remove_file(remote_transfer::item_path(&dir, "request")).unwrap();
    std::fs::write(remote_transfer::item_path(&dir, "request"), &request_body).unwrap();
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
        &limits(),
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
        &limits(),
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
        &limits(),
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
        &limits(),
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
        &limits(),
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
    // The collection digest binds the full transferred bodies — the
    // same digest the executor assembled — so the coordinator's
    // acknowledge can match it; the record's truncation flags carry
    // the honest account of what retention kept.
    assert_eq!(record.collection_digest, assembled.digest);
    let report_entry = record
        .entries
        .iter()
        .find(|e| e.item_id == "report")
        .unwrap();
    assert_eq!(report_entry.bytes, 200);
    assert_eq!(report_entry.content_digest, item_content_digest(&report));
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
        &limits(),
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

/// A repeated `collect` binds the incoming collection to the durable
/// row: a different body under the same collection identity, or a
/// different collection identity under the same request, is the
/// contract's `conflicting_replay` — refused, never a silent replay
/// of whatever row happens to be durable.
#[test]
fn a_conflicting_collection_does_not_replay_the_durable_row() {
    let (rig, base) = rig("collect-conflict");
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
    let first = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(first.collection.state, ExecutorCollectionState::Collected);

    // A different body under the same collection identity: the
    // recomputed digest no longer matches the durable row — conflict,
    // not a replay.
    std::fs::write(
        prepared.out_dir.join("report.json"),
        b"{\"status\":\"changed\"}",
    )
    .unwrap();
    let changed = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-2",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    let dir2 = ship_collection(&changed, &rig.coord_transfers);
    let err = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir2,
        &changed.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            remote_collect::CollectError::Store(StoreError::ConstraintConflict { .. })
        ),
        "a different digest under one request conflicts, got {err:?}"
    );

    // Identical content under a different collection identity: the
    // collection_id alone mismatches the durable row — conflict too.
    std::fs::write(prepared.out_dir.join("report.json"), b"{\"status\":\"ok\"}").unwrap();
    let renamed = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-3",
        "col-other",
        CHUNK,
    )
    .unwrap();
    let dir3 = ship_collection(&renamed, &rig.coord_transfers);
    let err = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir3,
        &renamed.manifest,
        &limits(),
        &seeded.request_id,
        "col-other",
        "stop-proof",
        NOW,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            remote_collect::CollectError::Store(StoreError::ConstraintConflict { .. })
        ),
        "a different collection id under one request conflicts, got {err:?}"
    );

    // The durable row is untouched by either conflicting attempt.
    let row = rig
        .store
        .transact(|tx| tx.executor_collection(&seeded.request_id))
        .unwrap()
        .unwrap();
    assert_eq!(row.collection_id, collection_id);
    assert_eq!(row.collection_digest, first.collection.collection_digest);
}

/// A transient infrastructure fault inside produced validation —
/// here the quarantine scratch path is blocked — propagates as a
/// retryable fault and persists nothing: a retryable git/io failure
/// never poisons the record with a terminal `invalid` row.
#[test]
fn a_transient_fault_does_not_poison_the_collection() {
    let (rig, base) = rig("transient-fault");
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

    // Block the quarantine scratch path — a filesystem fault, not a
    // verdict on the collection bytes.
    std::fs::write(dir.join("scratch"), b"blocked").unwrap();
    let err = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap_err();
    assert!(
        matches!(err, remote_collect::CollectError::Fault(_)),
        "a transient io fault propagates, got {err:?}"
    );
    assert!(
        rig.store
            .transact(|tx| tx.executor_collection(&seeded.request_id))
            .unwrap()
            .is_none(),
        "a retryable fault never persists a collection row"
    );

    // Clear the fault and the same collection collects cleanly — the
    // retry is the contract's recovery path.
    std::fs::remove_file(dir.join("scratch")).unwrap();
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
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
}

/// Wire-supplied identifiers that join managed paths are validated
/// before any filesystem effect: a `collection_id` carrying `..` is
/// refused at assembly, tombstone commit/read, and cleanup — a
/// sentinel outside the scoped dir proves no remove or create escaped
/// — and a `work_id` carrying a traversal is refused when the wire
/// request parses, before prepare can join it into a workspace or
/// receipt path.
#[test]
fn traversal_identifiers_are_refused_before_any_filesystem_effect() {
    let (rig, base) = rig("traversal-ids");
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

    // `scratch/../escaped` resolves to `custody_root/escaped` — the
    // sentinel that remove_dir_all would have deleted before the fix.
    let escaped = rig.custody.custody_root.join("escaped");
    std::fs::create_dir_all(&escaped).unwrap();
    std::fs::write(escaped.join("sentinel"), b"keep").unwrap();

    let err = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-bad",
        "../escaped",
        CHUNK,
    )
    .unwrap_err();
    assert!(
        matches!(err, remote_custody::CustodyError::Invalid { .. }),
        "a traversal collection_id refuses at assembly, got {err:?}"
    );
    assert!(
        escaped.join("sentinel").exists(),
        "no remove or create escaped the custody root"
    );

    // The tombstone and journal constructors carry the same guard —
    // `ack/../escaped.json` is the write that must never happen.
    let tombstone = AckTombstone {
        request_id: seeded.request_id.clone(),
        invocation_id: seeded.invocation_id.clone(),
        collection_id: "../escaped".into(),
        collection_digest: "d".repeat(64),
        bound_incarnation_id: "boot-1".into(),
        responder_incarnation_id: "boot-1".into(),
        stop_proof: "s".into(),
        receipt_body: b"{}".to_vec(),
    };
    assert!(
        matches!(
            remote_custody::commit_tombstone(&rig.custody, &tombstone),
            Err(remote_custody::CustodyError::Invalid { .. })
        ),
        "a traversal collection_id refuses at tombstone commit"
    );
    assert!(
        matches!(
            remote_custody::read_tombstone(&rig.custody, "../escaped"),
            Err(remote_custody::CustodyError::Invalid { .. })
        ),
        "a traversal collection_id refuses at tombstone read"
    );
    assert!(
        matches!(
            remote_custody::cleanup_after_ack(
                &rig.custody,
                &seeded.request_id,
                "../escaped",
                std::slice::from_ref(&escaped),
            ),
            Err(remote_custody::CustodyError::Invalid { .. })
        ),
        "a traversal collection_id refuses at cleanup"
    );
    assert!(
        !rig.custody.custody_root.join("escaped.json").exists(),
        "no tombstone or journal write escaped the managed dirs"
    );
    assert!(escaped.join("sentinel").exists());

    // A traversal `work_id` is refused when the wire request parses:
    // the digest is recomputed so `validate` — not the digest check —
    // is what refuses it, and prepare never sees the id.
    let mut forged = seeded.request.clone();
    forged.work_id = "w/../../escaped".into();
    forged.workspace.workspace_id = format!("ws-{}", forged.work_id);
    forged.request_digest = forged.digest();
    assert!(
        WorkerRequestV2::parse(&forged.to_canonical_bytes()).is_err(),
        "a traversal work_id fails request validation"
    );

    // The same forged body inside a finalized transfer is refused at
    // the same boundary — the request item's parse is the gate.
    let forged_items = vec![ExportedItem {
        item_id: "request".into(),
        path: "request.json".into(),
        body: forged.to_canonical_bytes(),
    }];
    let forged_transfer = ExportedTransfer {
        manifest: ExecutorTransferV1::declared(
            "tr-in-bad",
            &seeded.request_id,
            CHUNK,
            forged_items
                .iter()
                .map(|i| TransferItemV1 {
                    item_id: i.item_id.clone(),
                    path: i.path.clone(),
                    bytes: i.body.len() as u64,
                    digest: item_content_digest(&i.body),
                })
                .collect(),
        )
        .unwrap(),
        items: forged_items,
        head: base.clone(),
    };
    let dir = ship(&forged_transfer, &rig.exec_transfers);
    let err = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &forged_transfer.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap_err();
    assert!(
        matches!(err, PrepareError::Invalid { .. }),
        "a traversal work_id refuses at prepare, got {err:?}"
    );
    assert!(
        escaped.join("sentinel").exists(),
        "no workspace or receipt path escaped the managed roots"
    );
}

/// A finalized collection that omits the required report member
/// records the deterministic `collection_incomplete` invalid outcome
/// — the row persists, a replay returns it, and no artifact ref is
/// ever established for the invalid collection.
#[test]
fn a_collection_without_a_report_records_incomplete() {
    let (rig, base) = rig("no-report");
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
    // The worker produces the artifact and the handoff manifest but
    // never writes report.json — the collection assembles without a
    // report member.
    worker_produce(&rig, &prepared, "dist/result.json", b"RESULT");
    std::fs::remove_file(prepared.out_dir.join("report.json")).unwrap();
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
    assert!(
        assembled.manifest.item("report").is_none(),
        "the collection carries no report item"
    );
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("collection_incomplete"),
        "a missing report is the closed incomplete outcome, not a retry"
    );
    assert!(
        collected.report.is_none(),
        "no report body is retained for the invalid collection"
    );
    // Invalid never becomes Present evidence — no artifact ref, and
    // the artifact verdicts are empty.
    assert!(collected.artifacts.is_empty());
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
    assert!(ref_missing, "an incomplete collection pins no artifact ref");

    // The outcome is durable: the replay returns the recorded row
    // instead of retrying the validation forever.
    let replayed = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.collection.state, ExecutorCollectionState::Invalid);
}

/// The receiver enforces the run's immutable output limits on the
/// declared collection (contract §6/§8): an over-`max_output_bytes`
/// total records the closed `limit_exceeded` invalid outcome and no
/// item body is ever read into the record.
#[test]
fn an_over_limit_collection_is_refused() {
    let (rig, base) = rig("over-limit");
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

    let tight = ExecutorLimits {
        max_output_bytes: assembled.manifest.total_bytes - 1,
        ..limits()
    };
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &tight,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("limit_exceeded"),
        "an over-limit collection is refused, never retried"
    );
    assert!(collected.artifacts.is_empty() && collected.report.is_none());
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
    assert!(ref_missing, "an over-limit collection pins no artifact ref");
}

/// The item-count bound is enforced the same way — a declared set
/// past `max_item_count` records `limit_exceeded` and persists.
#[test]
fn an_over_item_count_collection_is_refused() {
    let (rig, base) = rig("over-count");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    std::fs::write(prepared.out_dir.join("report.json"), b"{}").unwrap();
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

    let tight = ExecutorLimits {
        max_item_count: assembled.manifest.item_count - 1,
        ..limits()
    };
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &tight,
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("limit_exceeded")
    );
}

/// A finalized transfer that simply omits a body the request's refs
/// require is refused at prepare — the required set is checked, not
/// only the declared set the manifest happens to carry.
#[test]
fn a_transfer_missing_a_required_input_is_refused() {
    let (rig, base) = rig("missing-input");
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
    // Rebuild the transfer without the goal item — the request body
    // still declares the goal ref, so prepare must refuse, not
    // materialize a partial input set.
    let items: Vec<ExportedItem> = exported
        .items
        .into_iter()
        .filter(|i| i.item_id != "input-goal.md")
        .collect();
    let short = ExportedTransfer {
        manifest: ExecutorTransferV1::declared(
            "tr-in-2",
            &seeded.request_id,
            CHUNK,
            items
                .iter()
                .map(|i| TransferItemV1 {
                    item_id: i.item_id.clone(),
                    path: i.path.clone(),
                    bytes: i.body.len() as u64,
                    digest: item_content_digest(&i.body),
                })
                .collect(),
        )
        .unwrap(),
        items,
        head: exported.head,
    };
    let dir = ship(&short, &rig.exec_transfers);
    let err = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &dir,
        &short.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap_err();
    assert!(
        matches!(err, PrepareError::Invalid { .. }),
        "a missing required input refuses prepare, got {err:?}"
    );
}

/// A zero-byte output file — an empty `runtime.log` — declares an
/// item that emits no chunk frame; the transfer still finalizes and
/// the collection imports it whole.
#[test]
fn an_empty_output_file_finalizes_and_collects() {
    let (rig, base) = rig("empty-output");
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
    worker_produce(&rig, &prepared, "dist/result.json", b"RESULT");
    // The worker leaves an empty runtime.log — the item exists with
    // zero bytes and crosses the wire as no frames at all.
    std::fs::write(prepared.out_dir.join("runtime.log"), b"").unwrap();
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
    assert_eq!(assembled.manifest.item("log").unwrap().bytes, 0);
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    assert!(
        remote_transfer::item_path(&dir, "log").exists(),
        "the zero-byte item materialized in the finalized set"
    );
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
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
        collected.log.as_ref().map(|l| l.bytes.as_slice()),
        Some(b"".as_slice()),
        "the empty log body is retained whole"
    );
}

/// An `Invalid` collection never becomes `Present` evidence: when a
/// later required artifact's ref is already held by a different
/// commit (`ref_conflict`), the deterministic conflict is decided
/// before any durable write — the earlier artifact's ref and handoff
/// row stay absent (contract §6: "rejection never becomes Present
/// evidence").
#[test]
fn an_invalid_collection_leaves_no_artifact_evidence() {
    let (rig, base) = rig("invalid-no-evidence");
    let required = vec![
        RequiredArtifact {
            kind: "aa".into(),
            path: "dist/aa.json".into(),
            expected_digest: None,
        },
        RequiredArtifact {
            kind: "bb".into(),
            path: "dist/bb.json".into(),
            expected_digest: None,
        },
    ];
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        required.clone(),
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
    let transfer_dir = ship(&exported, &rig.exec_transfers);
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &transfer_dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    worker_produce_many(
        &rig,
        &prepared,
        &[
            ("dist/aa.json", b"AA".as_slice()),
            ("dist/bb.json", b"BB".as_slice()),
        ],
    );

    // The second artifact's ref is already held by a different commit
    // — a `ref_conflict` the collection must record as invalid.
    let src = rig.source.path.to_string_lossy().into_owned();
    git(&rig.handoff_repo, &["fetch", "--no-tags", &src, &base]);
    let conflicting = format!("{}/bb", seeded.invocation_id);
    git(
        &rig.handoff_repo,
        &["update-ref", &handoff::artifact_ref(&conflicting), &base],
    );

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
    let collect_dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &collect_dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("ref_conflict")
    );
    assert!(
        collected
            .collection
            .record()
            .unwrap()
            .artifact_ids
            .is_empty(),
        "an invalid collection names no present artifacts"
    );

    // The first artifact verified Present but its ref and handoff row
    // must not be durable: the verdict was decided before any write.
    let clean = format!("{}/aa", seeded.invocation_id);
    let ref_status = std::process::Command::new("git")
        .arg("-C")
        .arg(&rig.handoff_repo)
        .args(["rev-parse", "--verify", &handoff::artifact_ref(&clean)])
        .output()
        .unwrap()
        .status;
    assert!(
        !ref_status.success(),
        "no artifact ref survives an invalid collection"
    );
    for artifact_id in [&clean, &conflicting] {
        assert!(
            rig.store
                .transact(|tx| tx.artifact_handoff(artifact_id))
                .unwrap()
                .is_none(),
            "no handoff row survives an invalid collection"
        );
    }
}

/// A durable handoff row already recorded under a different produced
/// commit is a deterministic `ref_conflict` — terminal `Invalid`
/// decided before any write, not a retryable mid-commit race. The
/// foreign row and the absent ref are left exactly as found.
#[test]
fn an_existing_handoff_row_for_another_produced_is_invalid() {
    let (rig, base) = rig("invalid-row-conflict");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![RequiredArtifact {
            kind: "aa".into(),
            path: "dist/aa.json".into(),
            expected_digest: None,
        }],
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
    let transfer_dir = ship(&exported, &rig.exec_transfers);
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &transfer_dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    worker_produce_many(&rig, &prepared, &[("dist/aa.json", b"AA".as_slice())]);

    // A row for the same artifact_id already recorded under a
    // different produced commit — foreign evidence the collect must
    // refuse before writing anything.
    let artifact_id = format!("{}/aa", seeded.invocation_id);
    rig.store
        .transact(|tx| {
            let row = ArtifactHandoff {
                artifact_id: artifact_id.clone(),
                invocation_id: InvocationId(seeded.invocation_id.clone()),
                request_id: seeded.request_id.clone(),
                work_id: seeded.work_id.clone(),
                execution_id: ExecutionId("exec-foreign".into()),
                base: base.clone(),
                produced: Some(base.clone()),
                manifest_digest: None,
                state: HandoffState::Acknowledged,
                invalid_reason: None,
                attempt: 0,
                recorded_seq: tx.alloc_seq()?,
            };
            tx.apply(&Effect::UpsertHandoff {
                handoff: row,
                expected_seq: 0,
            })
        })
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
    let collect_dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &collect_dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("ref_conflict")
    );

    // The foreign row stands untouched and no artifact ref was
    // claimed beside it.
    let row = rig
        .store
        .transact(|tx| tx.artifact_handoff(&artifact_id))
        .unwrap()
        .unwrap();
    assert_eq!(row.produced.as_deref(), Some(base.as_str()));
    let ref_status = std::process::Command::new("git")
        .arg("-C")
        .arg(&rig.handoff_repo)
        .args([
            "rev-parse",
            "--verify",
            &handoff::artifact_ref(&artifact_id),
        ])
        .output()
        .unwrap()
        .status;
    assert!(
        !ref_status.success(),
        "no artifact ref survives an invalid collection"
    );
}

/// A CodeReview whose `target.head == target.base` (both differing
/// from the request base) still exports a bundle pinning
/// `refs/taisan/export/review-head` — ref names are pinned
/// independently, never deduplicated by commit SHA.
#[test]
fn a_review_whose_head_shares_base_sha_still_pins_review_head() {
    let (rig, base) = rig("review-shared-sha");
    let target_sha = git(&rig.source.path, &["rev-parse", "HEAD^"]);
    assert_ne!(target_sha, base, "the fixture needs a two-commit source");
    let target = workloop_core::worker_v2::DiffTarget {
        change: "cr-1".into(),
        base: target_sha.clone(),
        head: target_sha.clone(),
        diff_digest: "d".repeat(64),
        target_revision: None,
    };
    let seeded = seed_review_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        target,
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
    let bundle = exported
        .items
        .iter()
        .find(|i| i.item_id == "source-bundle")
        .unwrap();
    let probe = rig.root.join("probe");
    std::fs::create_dir_all(&probe).unwrap();
    git(&probe, &["init"]);
    let bundle_path = probe.join("source.bundle");
    std::fs::write(&bundle_path, &bundle.body).unwrap();
    let heads = git(
        &probe,
        &["bundle", "list-heads", bundle_path.to_str().unwrap()],
    );
    for name in ["base", "review-base", "review-head"] {
        let expect = format!("{target_sha} refs/taisan/export/{name}");
        let expect = if name == "base" {
            format!("{base} refs/taisan/export/{name}")
        } else {
            expect
        };
        assert!(
            heads.lines().any(|l| l.trim() == expect),
            "bundle pins {expect}\n{heads}"
        );
    }

    // The receiving side re-verifies the pin by name and prepares.
    let transfer_dir = ship(&exported, &rig.exec_transfers);
    let prepared = remote_prepare::prepare_remote(
        &rig.exec_prepare,
        &transfer_dir,
        &exported.manifest,
        &limits(),
        &identity(&seeded),
    )
    .unwrap();
    assert_eq!(prepared.head, target_sha);
}

/// The export fetches pinned commits by raw SHA through transports
/// that negotiate with upload-pack (`file://`), so the fetch carries
/// the same `uploadpack.allowAnySHA1InWant` capability the workspace
/// path grants — a commit no advertised ref names still imports.
/// The pinned base here is `HEAD^`, never advertised as a ref tip,
/// so the want is non-trivial; whether a strict upload-pack would
/// refuse it without the flag is transport-dependent, and the
/// flag's presence itself is pinned by a unit test in
/// `remote_export`.
#[test]
fn export_fetches_pinned_commits_through_upload_pack() {
    let (rig, base) = rig("export-upload-pack");
    // Pin an ancestor that no ref advertises — the fetch must want
    // the commit by raw SHA, not by name.
    let unadvertised = git(&rig.source.path, &["rev-parse", "HEAD^"]);
    assert_ne!(unadvertised, base, "the fixture needs a two-commit source");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &unadvertised,
        vec![],
        1 << 20,
        1 << 20,
    );
    // Address the same mirror by URL so the fetch negotiates through
    // upload-pack rather than direct object access.
    let url_source = RepositorySource {
        repository: rig.source.repository.clone(),
        path: PathBuf::from(format!("file://{}", rig.source.path.display())),
    };
    let exported = remote_export::export_request_transfer(
        &rig.store,
        &rig.bodies,
        &url_source,
        Some(&rig.handoff_repo),
        &rig.root.join("coord/export-scratch"),
        &seeded.request_id,
        "tr-in-1",
        CHUNK,
    )
    .unwrap();
    let bundle = exported
        .items
        .iter()
        .find(|i| i.item_id == "source-bundle")
        .unwrap();
    let probe = rig.root.join("probe");
    std::fs::create_dir_all(&probe).unwrap();
    git(&probe, &["init"]);
    let bundle_path = probe.join("source.bundle");
    std::fs::write(&bundle_path, &bundle.body).unwrap();
    let heads = git(
        &probe,
        &["bundle", "list-heads", bundle_path.to_str().unwrap()],
    );
    let expect = format!("{unadvertised} refs/taisan/export/base");
    assert!(
        heads.lines().any(|l| l.trim() == expect),
        "bundle pins {expect}\n{heads}"
    );
}

/// Seed a Fix request: same durable plumbing as a Coding request but
/// the writer role whose branch starts at the predecessor's produced
/// commit — `base.object` stays the predecessor's fork base, which is
/// exactly the skew the remote `nothing_produced` check must not
/// mistake for the branch start.
fn seed_fix_request(
    store: &SqliteStore,
    bodies: &BodyStore,
    repository: &RepositoryRef,
    base: &str,
    predecessor: ArtifactRef,
    required_artifacts: Vec<RequiredArtifact>,
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
            idempotency_key: format!("remote-fix-{}", &base[..8]),
            body: CommandBody::Submit {
                intent: "fix the thing".into(),
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

    let goal_bytes = goal_body_bytes(InvocationRole::Fix, "fix the thing", &[]);
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
        role: InvocationRole::Fix,
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
            max_report_bytes: 1 << 20,
            max_log_bytes: 1 << 20,
            max_verification_commands: 8,
            max_continuation_refs: 4,
        },
        required_capabilities: RequiredCapabilities {
            role: InvocationRole::Fix,
            protocol_versions: vec![],
            capabilities: vec![],
            resume: "unsupported".into(),
            verification_egress_hosts: vec![],
        },
        continuation: vec![],
        task: TaskBody::Fix {
            goal: GoalRef {
                body_ref: goal_path,
                media_type: "text/markdown".into(),
                bytes: goal_bytes.len() as u64,
                digest: goal_digest,
            },
            issues: vec![],
            predecessor,
            origin: Box::new(FixOrigin::Prepublication {
                failed_evidence: vec![],
            }),
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
                    role: InvocationRole::Fix,
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
                    role: InvocationRole::Fix,
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

/// A handoff manifest that cannot even parse is the same closed
/// `collection_invalid` every semantic refusal records — the durable
/// `invalid_reason` is the contract's vocabulary, never a free-form
/// detail string.
#[test]
fn a_malformed_handoff_manifest_records_collection_invalid() {
    let (rig, base) = rig("malformed-manifest");
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
    // Replace the manifest with bytes no parser accepts.
    std::fs::write(prepared.out_dir.join("handoff.json"), b"{not json").unwrap();
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
        &limits(),
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
}

/// A bundle the quarantine import refuses — `git bundle verify`
/// failing on the declared bytes — records the same closed
/// `collection_invalid` a malformed handoff does. The durable
/// `invalid_reason` is the contract token either way, never a
/// free-form detail.
#[test]
fn a_bundle_the_quarantine_refuses_records_collection_invalid() {
    let (rig, base) = rig("refused-bundle");
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
    let mut assembled = remote_custody::assemble_collection(
        &rig.custody,
        &prepared.request,
        &prepared.io_dir,
        "tr-out-1",
        &collection_id,
        CHUNK,
    )
    .unwrap();
    // The shipped bundle bytes are not a git bundle at all — re-declare
    // the manifest over the refused body so the transfer layer carries
    // it honestly and only the quarantine gate can stop it.
    for item in &mut assembled.items {
        if item.item_id == "export-bundle" {
            item.body = b"not a git bundle".to_vec();
        }
    }
    let items = assembled
        .items
        .iter()
        .map(|i| TransferItemV1 {
            item_id: i.item_id.clone(),
            path: i.path.clone(),
            bytes: i.body.len() as u64,
            digest: item_content_digest(&i.body),
        })
        .collect();
    assembled.manifest =
        ExecutorTransferV1::declared("tr-out-1", &collection_id, CHUNK, items).unwrap();
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
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
}

/// A Fix whose branch never moved past its start — the predecessor's
/// produced commit — is `nothing_produced`, not a collection of the
/// predecessor's own artifacts: the request's `base.object` is the
/// predecessor's fork point, so comparing `produced` to `base` can
/// never see a no-op Fix.
#[test]
fn a_noop_fix_is_nothing_produced() {
    let (rig, base) = rig("noop-fix");
    // The predecessor's produced commit: `dist/result.json` already
    // exists in its tree, so artifact verification alone would find
    // it Present.
    let artifact_bytes = b"RESULT-FROM-PREDECESSOR";
    std::fs::create_dir_all(rig.source.path.join("dist")).unwrap();
    std::fs::write(rig.source.path.join("dist/result.json"), artifact_bytes).unwrap();
    git(&rig.source.path, &["add", "."]);
    git(&rig.source.path, &["commit", "-qm", "produced"]);
    let produced = git(&rig.source.path, &["rev-parse", "HEAD"]);
    assert_ne!(produced, base);
    // Publish it under the artifact ref the Fix request binds — the
    // same authority a real acknowledged predecessor holds.
    let predecessor_ref = handoff::artifact_ref("inv-0/result");
    git(
        &rig.source.path,
        &[
            "push",
            rig.handoff_repo.to_str().unwrap(),
            &format!("{produced}:{predecessor_ref}"),
        ],
    );
    let seeded = seed_fix_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        ArtifactRef {
            artifact_id: "inv-0/result".into(),
            manifest_digest: "b".repeat(64),
            produced: produced.clone(),
        },
        vec![RequiredArtifact {
            kind: "result".into(),
            path: "dist/result.json".into(),
            expected_digest: None,
        }],
    );
    let (prepared, _exported) = prepare_side(&rig, &seeded, "tr-in-1");
    assert_eq!(prepared.head, produced, "a Fix checks out its predecessor");

    // A no-op Worker: out/ carries the handoff naming the unchanged
    // HEAD, but the branch itself never moved.
    let request = &prepared.request;
    let noop_handoff = RemoteHandoffV1 {
        format_version: workloop_core::executor::REMOTE_HANDOFF_V1_FORMAT.into(),
        request_id: request.request_id.clone(),
        invocation_id: request.invocation_id.clone(),
        repository: workloop_core::executor::HandoffRepository {
            host: request.repository.host.clone(),
            repository: request.repository.repository.clone(),
        },
        base_commit: base.clone(),
        produced_commit: produced.clone(),
        artifacts: vec![workloop_core::executor::HandoffArtifact {
            path: "dist/result.json".into(),
            bytes: artifact_bytes.len() as u64,
            claimed_sha256: sha256_hex("artifact-v1", artifact_bytes),
        }],
    };
    std::fs::write(
        prepared.out_dir.join("handoff.json"),
        serde_json::to_vec(&noop_handoff).unwrap(),
    )
    .unwrap();
    std::fs::write(prepared.out_dir.join("report.json"), b"{\"status\":\"ok\"}").unwrap();
    std::fs::write(prepared.out_dir.join("runtime.log"), b"log line\n").unwrap();

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
    let dir = ship_collection(&assembled, &rig.coord_transfers);
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        &collection_id,
        "stop-proof",
        NOW,
    )
    .unwrap();
    assert_eq!(collected.collection.state, ExecutorCollectionState::Invalid);
    assert_eq!(
        collected.collection.invalid_reason.as_deref(),
        Some("nothing_produced")
    );
    // A nothing_produced verdict commits no artifact evidence.
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
    assert!(ref_missing, "a no-op Fix establishes no artifact ref");
    assert!(
        rig.store
            .transact(|tx| tx.artifact_handoff(&artifact_id))
            .unwrap()
            .is_none()
    );
}

/// A crash-orphaned journal temp is never authoritative: resume reads
/// only the `<hex>.offset` journal, and the finalized surface carries
/// neither journal nor temp — the completed set is items + manifest.
#[test]
fn a_stale_journal_temp_does_not_wedge_or_publish() {
    let root = tmp_root("journal-temp");
    let receiver = TransferStore::new(root.join("transfers"));
    let body: Vec<u8> = (0..3 * CHUNK + 17).map(|i| (i % 251) as u8).collect();
    let manifest = ExecutorTransferV1::declared(
        "t-1",
        "col-1",
        CHUNK,
        vec![TransferItemV1 {
            item_id: "item-1".into(),
            path: "input/item-1".into(),
            bytes: body.len() as u64,
            digest: item_content_digest(&body),
        }],
    )
    .unwrap();
    receiver.declare(&manifest, &in_bound()).unwrap();
    let frames = item_chunks(&manifest, "item-1", &body);
    receiver
        .accept_chunk(&manifest, &in_bound(), &frames[0].0, frames[0].1)
        .unwrap();

    // The crash window: the new journal's temp was written but the
    // rename never ran — the durable answer is still the old journal.
    let items_dir = receiver.staging_dir("t-1").join("items");
    let name = sha256_hex("taisan.executor.item-name.v1", b"item-1");
    std::fs::write(items_dir.join(format!("{name}.tmp")), b"torn").unwrap();
    // A `declare` interrupted the same way leaves its own temp at the
    // staging root.
    std::fs::write(
        receiver.staging_dir("t-1").join("manifest.json.tmp"),
        b"torn",
    )
    .unwrap();

    // Resume ignores the orphan: the next chunk lands at the journaled
    // offset and a fresh advance publishes over it.
    let progress = receiver.progress(&manifest).unwrap();
    assert_eq!(progress[0].1, CHUNK);
    for (chunk, chunk_body) in &frames[1..] {
        receiver
            .accept_chunk(&manifest, &in_bound(), chunk, chunk_body)
            .unwrap();
    }
    let dir = receiver.finalize(&manifest).unwrap();
    assert!(
        !dir.join("manifest.json.tmp").exists(),
        "a crashed declare's temp never publishes"
    );
    let leftover: Vec<String> = std::fs::read_dir(dir.join("items"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".offset") || n.ends_with(".tmp"))
        .collect();
    assert!(
        leftover.is_empty(),
        "journals leaked into complete: {leftover:?}"
    );
    assert_eq!(
        std::fs::read(dir.join("items").join(&name)).unwrap(),
        body,
        "the finalized item is byte-exact"
    );
}

/// A deterministic identity mismatch is the contract's terminal
/// `request_identity_mismatch`/`collection_invalid` refusal — never a
/// retryable `Fault` and never a durable row. A manifest naming
/// another collection and a durable request body naming another
/// request both refuse the same way, and the unrecorded refusal
/// leaves the honest collection free to land later.
#[test]
fn an_identity_mismatch_is_a_terminal_refusal() {
    let (rig, base) = rig("identity-mismatch");
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

    // The operation names a different collection than the manifest
    // declares — a foreign transfer, refused without retry or record.
    let err = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
        &seeded.request_id,
        "col-other",
        "stop-proof",
        NOW,
    )
    .unwrap_err();
    assert!(
        matches!(err, remote_collect::CollectError::Foreign { .. }),
        "a collection_id mismatch is terminal, got {err:?}"
    );

    // The durable-body arm is guarded twice: collect's
    // `request.request_id != request_id` refuses Foreign, and the
    // store's own integrity gate refuses to even persist a body whose
    // embedded request_id disagrees with its key — proven here.
    let other_request = "other-request";
    rig.store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: other_request.into(),
                    work_id: seeded.work_id.clone(),
                    execution_id: ExecutionId(seeded.request.execution_id.clone()),
                    role: InvocationRole::Coding,
                    spec_revision: 1,
                    payload: serde_json::json!({}),
                    adapter_binding: None,
                },
            })
        })
        .unwrap();
    let err = rig
        .store
        .transact(|tx| {
            tx.apply(&Effect::InsertWorkerRequestBody {
                body: WorkerRequestBody {
                    request_id: other_request.into(),
                    format_version: WORKER_REQUEST_V2_FORMAT.into(),
                    canonical_body: seeded.request.to_canonical_bytes(),
                    body_digest: seeded.request.digest(),
                },
            })
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintConflict { .. }),
        "a mismatched durable body is unwritable, got {err:?}"
    );

    // Neither refusal recorded a row — the honest collection still
    // lands on the right request.
    assert!(
        rig.store
            .transact(|tx| tx.executor_collection(&seeded.request_id))
            .unwrap()
            .is_none(),
        "a foreign refusal never persists a collection row"
    );
    let collected = remote_collect::collect_remote(
        &rig.store,
        &rig.collect_layout,
        &dir,
        &assembled.manifest,
        &limits(),
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
}

/// A journaled cleanup path that only *lexically* prefixes a managed
/// root — `<custody_root>/../escapee`, or the root itself — is
/// refused before any removal: `Path::starts_with` is a raw prefix
/// match, so normalization is the scope check's job, and a root is
/// never its own descendant.
#[test]
fn cleanup_refuses_paths_escaping_the_managed_roots() {
    let (rig, base) = rig("cleanup-escape");
    let seeded = seed_remote_request(
        &rig.store,
        &rig.bodies,
        &rig.source.repository,
        &base,
        vec![],
        1 << 20,
        1 << 20,
    );

    // A sentinel tree OUTSIDE the managed roots — the sibling the
    // traversal path resolves to.
    let escapee = rig
        .custody
        .custody_root
        .parent()
        .expect("custody root has a parent")
        .join("escapee");
    std::fs::create_dir_all(&escapee).unwrap();
    std::fs::write(escapee.join("sentinel"), b"keep").unwrap();
    let managed = rig.custody.custody_root.join("managed");
    std::fs::create_dir_all(&managed).unwrap();

    // The recorded journal carries the traversal path and the root
    // itself — both must refuse before either removal runs.
    let collection_id = "col-escape";
    let journal_dir = rig.custody.custody_root.join("cleanup");
    std::fs::create_dir_all(&journal_dir).unwrap();
    let journal = serde_json::json!({
        "request_id": seeded.request_id,
        "collection_id": collection_id,
        "pending": [
            rig.custody.custody_root.join("..").join("escapee"),
            rig.custody.custody_root,
        ],
        "attempts": 0,
    });
    std::fs::write(
        journal_dir.join(format!("{collection_id}.json")),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let err =
        remote_custody::cleanup_after_ack(&rig.custody, &seeded.request_id, collection_id, &[])
            .unwrap_err();
    assert!(
        matches!(err, remote_custody::CustodyError::Conflict { .. }),
        "an escaping journaled path refuses, got {err:?}"
    );
    assert!(
        escapee.join("sentinel").exists(),
        "nothing outside was removed"
    );
    assert!(managed.exists(), "the custody root itself was not removed");

    // The root-as-pending-path alone refuses too.
    let journal = serde_json::json!({
        "request_id": seeded.request_id,
        "collection_id": collection_id,
        "pending": [rig.custody.custody_root],
        "attempts": 0,
    });
    std::fs::write(
        journal_dir.join(format!("{collection_id}.json")),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let err =
        remote_custody::cleanup_after_ack(&rig.custody, &seeded.request_id, collection_id, &[])
            .unwrap_err();
    assert!(matches!(err, remote_custody::CustodyError::Conflict { .. }));
    assert!(managed.exists());

    // In-scope paths still clean up: the managed dir is removed and
    // the journal closes.
    let outcome = remote_custody::cleanup_after_ack(
        &rig.custody,
        &seeded.request_id,
        "col-fine",
        std::slice::from_ref(&managed),
    )
    .unwrap();
    assert_eq!(outcome, remote_custody::CleanupOutcome::Complete);
    assert!(!managed.exists());
}
