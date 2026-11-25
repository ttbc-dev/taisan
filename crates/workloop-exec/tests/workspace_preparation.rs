use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::store::{Effect, Store};
use workloop_core::work::{HostTemplate, WorkloopConfig, admit_queued, handle_command};
use workloop_core::worker_v2::*;
use workloop_exec::handoff::{HandoffLayout, artifact_ref};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_exec::workspace::{PreparationError, RepositorySource};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-prepare-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("fixture")
}

fn repo() -> RepositoryRef {
    RepositoryRef {
        host: host(),
        repository: RepositoryId::trusted("repo"),
    }
}

fn config() -> WorkloopConfig {
    WorkloopConfig {
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

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId, String) {
    let req = CommandRequest {
        actor: actor(),
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "produce an artifact".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    };
    let d = handle_command(store, &config(), &req, NOW).unwrap();
    let (work_id, eid) = match d {
        CommandDecision::Accepted {
            work_id, effect, ..
        } => {
            let eid = match effect {
                CommandEffect::WorkCreated { execution_id } => execution_id,
                other => panic!("unexpected effect {other:?}"),
            };
            (work_id.unwrap(), eid)
        }
        other => panic!("submit rejected: {other:?}"),
    };
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_source(root: &Path) -> (PathBuf, String, String) {
    let dir = root.join("source");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("seed.txt"), "base").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("seed.txt"), "newer").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "newer"]);
    let newer = git(&dir, &["rev-parse", "HEAD"]);
    (dir, base, newer)
}

fn spec_digest(store: &SqliteStore, work_id: &WorkId) -> String {
    store
        .transact(|tx| tx.spec(work_id, Some(1)))
        .unwrap()
        .unwrap()
        .digest
}

fn store_id(store: &SqliteStore) -> String {
    store.transact(|tx| tx.store_id()).unwrap().0
}

fn wire_repo() -> WireRepository {
    WireRepository {
        host: "fixture".into(),
        repository: "repo".into(),
    }
}

fn budget() -> RequestBudget {
    RequestBudget {
        deadline_unix_seconds: NOW + 3600,
        max_runtime_seconds: 1800,
        max_turns: 200,
        max_report_bytes: 1 << 20,
        max_log_bytes: 1 << 25,
        max_verification_commands: 8,
        max_continuation_refs: 4,
    }
}

fn capabilities(role: InvocationRole) -> RequiredCapabilities {
    RequiredCapabilities {
        role,
        protocol_versions: vec![WORKER_REQUEST_V2_FORMAT.into()],
        capabilities: vec![],
        resume: "unsupported".into(),
        verification_egress_hosts: vec![],
    }
}

fn coding_task(issues: Vec<GroupedIssue>) -> TaskBody {
    TaskBody::Coding {
        goal: GoalRef {
            body_ref: "goal-1".into(),
            media_type: "text/markdown".into(),
            bytes: 4,
            digest: "0".repeat(64),
        },
        issues,
        constraints: vec![],
        verification_argv: vec![],
    }
}

fn v2_request(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    request_id: &str,
    role: InvocationRole,
    base: &str,
    task: TaskBody,
) -> WorkerRequestV2 {
    let mut request = WorkerRequestV2 {
        format_version: WORKER_REQUEST_V2_FORMAT.into(),
        request_id: request_id.into(),
        invocation_id: format!("inv-{request_id}"),
        request_digest: String::new(),
        store_id: store_id(store),
        work_id: work_id.0.clone(),
        execution_id: execution_id.0.clone(),
        role: role.clone(),
        repository: wire_repo(),
        base: RequestBase {
            repository: wire_repo(),
            object: base.into(),
        },
        spec: SpecRef {
            revision: 1,
            digest: spec_digest(store, work_id),
        },
        workspace: WorkspaceGrant {
            workspace_id: format!("ws-{}", work_id.0),
            mount: "/workspace".into(),
            read_only: false,
        },
        budget: budget(),
        required_capabilities: capabilities(role),
        continuation: vec![],
        task,
    };
    request.request_digest = request.digest();
    request
}

fn insert_body(store: &SqliteStore, request: &WorkerRequestV2) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertWorkerRequestBody {
                body: WorkerRequestBody {
                    request_id: request.request_id.clone(),
                    format_version: WORKER_REQUEST_V2_FORMAT.into(),
                    canonical_body: request.to_canonical_bytes(),
                    body_digest: request.digest(),
                },
            })
        })
        .unwrap();
}

fn insert_request_row(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    request_id: &str,
    role: InvocationRole,
) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: request_id.into(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    role,
                    spec_revision: 1,
                    payload: serde_json::json!({}),
                    adapter_binding: None,
                },
            })
        })
        .unwrap();
}

/// The consume-time Invocation row a writer request carries; `paths`
/// is the declared required-artifact set the reuse gate tolerates.
fn declared_invocation(store: &SqliteStore, e: &ExecutionId, rid: &str, paths: &[&str]) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(format!("inv-{rid}")),
                    request_id: rid.into(),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Prepared,
                    process_identity: None,
                    required_artifacts: paths
                        .iter()
                        .map(|p| RequiredArtifact {
                            kind: (*p).to_string(),
                            path: (*p).to_string(),
                            expected_digest: None,
                        })
                        .collect(),
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
}

fn layout(root: &Path) -> HandoffLayout {
    let bare = root.join("handoff.git");
    git(root, &["init", "--bare", bare.to_str().unwrap()]);
    HandoffLayout {
        repository: bare,
        workspace_root: root.join("ws"),
        out_root: root.join("out"),
        state_root: None,
        verification: None,
    }
}

fn supervisor(root: &Path) -> (Supervisor<PosixProcessControl>, HandoffLayout) {
    let layout = layout(root);
    std::fs::create_dir_all(&layout.workspace_root).unwrap();
    (
        Supervisor::new(
            PosixProcessControl,
            layout.workspace_root.clone(),
            vec!["/bin/true".into()],
            config(),
        )
        .with_handoff(layout.clone()),
        layout,
    )
}

fn source(root: &Path, dir: &Path) -> RepositorySource {
    let _ = root;
    RepositorySource {
        repository: repo(),
        path: dir.to_path_buf(),
    }
}

fn coding_fixture(
    store: &SqliteStore,
    key: &str,
    base: &str,
    task: TaskBody,
) -> (WorkId, ExecutionId, String, WorkerRequestV2) {
    let (w, e, rid) = admit(store, key);
    let request = v2_request(store, &w, &e, &rid, InvocationRole::Coding, base, task);
    insert_body(store, &request);
    (w, e, rid, request)
}

fn ws_path(root: &Path, work_id: &WorkId) -> PathBuf {
    root.join("ws").join(format!("ws-{}", work_id.0))
}

fn assert_clean_pinned(ws: &Path, branch: &str, base: &str, head: &str) {
    assert_eq!(git(ws, &["symbolic-ref", "--short", "HEAD"]), branch);
    assert_eq!(git(ws, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(ws, &["rev-parse", "refs/heads/taisan-base"]), base);
    assert!(git(ws, &["status", "--porcelain"]).is_empty());
    assert!(ws.join(".git").is_dir());
    assert!(!ws.join(".git/objects/info/alternates").exists());
    assert!(git(ws, &["remote"]).is_empty());
}

#[test]
fn coding_workspace_pins_exact_base_not_the_moved_source_head() {
    let root = tmp_root("coding-pin");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, newer) = git_source(&root);
    assert_ne!(base, newer);
    let (w, _e, rid, request) = coding_fixture(&store, "p-pin", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);

    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);
    assert_eq!(prepared.path, ws);
    assert_eq!(prepared.branch, format!("work/{}", w.0));
    assert_eq!(prepared.base, base);
    assert_eq!(prepared.head, base, "pinned to base, not the moved head");
    assert_eq!(prepared.request_digest, request.request_digest);
    assert_clean_pinned(&ws, &format!("work/{}", w.0), &base, &base);
}

#[test]
fn grouped_issue_order_is_replayed_verbatim() {
    let root = tmp_root("order");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let issue = |order: u64, id: &str| GroupedIssue {
        order,
        issue: WireIssue {
            host: "fixture".into(),
            repository: "repo".into(),
            issue: id.into(),
        },
        source_revision: "1".into(),
        snapshot_digest: "a".repeat(64),
        body: IssueBodyRef {
            body_ref: format!("body-{id}"),
            bytes: 2,
            digest: "b".repeat(64),
        },
    };
    let issues = vec![issue(7, "11"), issue(3, "22")];
    let (_w, _e, rid, _request) =
        coding_fixture(&store, "p-order", &base, coding_task(issues.clone()));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();

    let body = store
        .transact(|tx| tx.worker_request_body(&rid))
        .unwrap()
        .unwrap();
    let replayed = WorkerRequestV2::parse(&body.canonical_body).unwrap();
    let TaskBody::Coding { issues: kept, .. } = replayed.task else {
        panic!("expected coding task");
    };
    assert_eq!(kept, issues, "stored order survives preparation verbatim");
}

#[test]
fn a_missing_base_is_a_typed_refusal_without_a_workspace() {
    let root = tmp_root("missing-base");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _n) = git_source(&root);
    let absent = "1".repeat(40);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-miss", &absent, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);

    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::MissingBase { .. }),
        "{err:?}"
    );
    assert!(!ws_path(&root, &w).exists());
}

#[test]
fn a_wrong_source_identity_is_a_repository_mismatch() {
    let root = tmp_root("wrong-repo");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (_w, _e, rid, _r) = coding_fixture(&store, "p-repo", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let foreign = RepositorySource {
        repository: RepositoryRef {
            host: host(),
            repository: RepositoryId::trusted("other-repo"),
        },
        path: src,
    };
    let err = sup
        .prepare_workspace(&store, &rid, &foreign, NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::RepositoryMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn a_missing_source_path_is_typed() {
    let root = tmp_root("missing-src");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (_w, _e, rid, _r) = coding_fixture(&store, "p-src", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let gone = RepositorySource {
        repository: repo(),
        path: src.join("does-not-exist"),
    };
    let err = sup.prepare_workspace(&store, &rid, &gone, NOW).unwrap_err();
    assert!(
        matches!(err, PreparationError::MissingBase { .. }),
        "{err:?}"
    );
}

#[allow(clippy::too_many_arguments)]
fn acknowledge_predecessor(
    store: &SqliteStore,
    layout: &HandoffLayout,
    source_dir: &Path,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    artifact_id: &str,
    produced: &str,
    base: &str,
    manifest: &str,
) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-1".into()),
                    request_id: format!("{}:inv:0", execution_id.0),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
    git(
        &layout.repository,
        &[
            "fetch",
            source_dir.to_str().unwrap(),
            "+refs/heads/*:refs/taisan/imported/*",
        ],
    );
    git(
        &layout.repository,
        &["update-ref", &artifact_ref(artifact_id), produced],
    );
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: ArtifactHandoff {
                    artifact_id: artifact_id.into(),
                    invocation_id: InvocationId("inv-1".into()),
                    request_id: format!("{}:inv:0", execution_id.0),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    base: base.into(),
                    produced: Some(produced.into()),
                    manifest_digest: Some(manifest.into()),
                    state: HandoffState::Acknowledged,
                    invalid_reason: None,
                    attempt: 0,
                    recorded_seq: seq,
                },
                expected_seq: 0,
            })
        })
        .unwrap();
}

fn fix_task(artifact_id: &str, manifest: &str, produced: &str) -> TaskBody {
    TaskBody::Fix {
        goal: GoalRef {
            body_ref: "goal-1".into(),
            media_type: "text/markdown".into(),
            bytes: 4,
            digest: "0".repeat(64),
        },
        issues: vec![],
        predecessor: ArtifactRef {
            artifact_id: artifact_id.into(),
            manifest_digest: manifest.into(),
            produced: produced.into(),
        },
        origin: Box::new(FixOrigin::Prepublication {
            failed_evidence: vec![],
        }),
        constraints: vec![],
        verification_argv: vec![],
    }
}

fn fix_fixture(
    store: &SqliteStore,
    root: &Path,
    key: &str,
    manifest: &str,
) -> (WorkId, ExecutionId, String, PathBuf, String, String, String) {
    fix_fixture_aid(store, root, key, manifest, "art-1")
}

fn fix_fixture_aid(
    store: &SqliteStore,
    root: &Path,
    key: &str,
    manifest: &str,
    aid: &str,
) -> (WorkId, ExecutionId, String, PathBuf, String, String, String) {
    let (w, e, _rid0) = admit(store, key);
    let (src, base, _n) = git_source(root);
    std::fs::write(src.join("fix.txt"), "fix").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "produced"]);
    let produced = git(&src, &["rev-parse", "HEAD"]);
    let rid = format!("{}:inv:1", e.0);
    insert_request_row(store, &w, &e, &rid, InvocationRole::Fix);
    let request = v2_request(
        store,
        &w,
        &e,
        &rid,
        InvocationRole::Fix,
        &base,
        fix_task(aid, manifest, &produced),
    );
    insert_body(store, &request);
    (w, e, rid, src, base, produced, aid.into())
}

#[test]
fn fix_workspace_starts_at_the_exact_acknowledged_predecessor() {
    let root = tmp_root("fix-ok");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e, rid, src, base, produced, aid) = fix_fixture(&store, &root, "p-fix", "m-1");
    let (sup, layout) = supervisor(&root);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");

    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, produced);
    assert_clean_pinned(&prepared.path, &format!("work/{}", w.0), &base, &produced);
}

#[test]
fn fix_predecessor_failures_are_all_typed_refusals() {
    let root = tmp_root("fix-bad");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e, rid, src, base, produced, aid) = fix_fixture(&store, &root, "p-fbad", "m-1");
    let (sup, layout) = supervisor(&root);
    let src_of = || RepositorySource {
        repository: repo(),
        path: src.clone(),
    };

    let err = sup
        .prepare_workspace(&store, &rid, &src_of(), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );

    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");

    git(
        &layout.repository,
        &["update-ref", "-d", &artifact_ref(&aid)],
    );
    let err = sup
        .prepare_workspace(&store, &rid, &src_of(), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );
    git(
        &layout.repository,
        &["update-ref", &artifact_ref(&aid), &produced],
    );

    store
        .transact(|tx| {
            let mut h = tx.artifact_handoff(&aid)?.unwrap();
            let expected = h.recorded_seq;
            h.state = HandoffState::Imported;
            h.recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: h,
                expected_seq: expected,
            })
        })
        .unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &src_of(), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );
}

#[test]
fn fix_handoff_base_mismatch_refuses() {
    let root = tmp_root("fix-base");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e, rid, src, base, produced, aid) = fix_fixture(&store, &root, "p-fbase", "m-1");
    let (sup, layout) = supervisor(&root);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");

    store
        .transact(|tx| {
            let mut h = tx.artifact_handoff(&aid)?.unwrap();
            let expected = h.recorded_seq;
            h.base = produced.clone();
            h.recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: h,
                expected_seq: expected,
            })
        })
        .unwrap();
    let err = sup
        .prepare_workspace(
            &store,
            &rid,
            &RepositorySource {
                repository: repo(),
                path: src.clone(),
            },
            NOW,
        )
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );
}

#[test]
fn fix_manifest_mismatch_and_non_descendant_refuse() {
    let root = tmp_root("fix-mismatch");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e, rid, src, base, produced, aid) = fix_fixture(&store, &root, "p-fmis", "m-1");
    let (sup, layout) = supervisor(&root);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");
    let src_of = || RepositorySource {
        repository: repo(),
        path: src.clone(),
    };

    let rid2 = format!("{}:inv:2", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let bad = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task(&aid, "other-manifest", &produced),
    );
    insert_body(&store, &bad);
    let err = sup
        .prepare_workspace(&store, &rid2, &src_of(), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );

    git(&src, &["checkout", "--orphan", "foreign"]);
    std::fs::write(src.join("foreign.txt"), "x").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "foreign produced"]);
    let foreign = git(&src, &["rev-parse", "HEAD"]);
    git(&src, &["checkout", "main"]);
    git(
        &layout.repository,
        &[
            "fetch",
            src.to_str().unwrap(),
            "+refs/heads/foreign:refs/taisan/imported/foreign",
        ],
    );
    let aid2 = "art-2";
    git(
        &layout.repository,
        &["update-ref", &artifact_ref(aid2), &foreign],
    );
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-2".into()),
                    request_id: format!("{}:inv:8", e.0),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: ArtifactHandoff {
                    artifact_id: aid2.into(),
                    invocation_id: InvocationId("inv-2".into()),
                    request_id: rid.clone(),
                    work_id: w.clone(),
                    execution_id: e.clone(),
                    base: base.clone(),
                    produced: Some(foreign.clone()),
                    manifest_digest: Some("m-2".into()),
                    state: HandoffState::Acknowledged,
                    invalid_reason: None,
                    attempt: 0,
                    recorded_seq: seq,
                },
                expected_seq: 0,
            })
        })
        .unwrap();
    let rid3 = format!("{}:inv:3", e.0);
    insert_request_row(&store, &w, &e, &rid3, InvocationRole::Fix);
    let bad2 = v2_request(
        &store,
        &w,
        &e,
        &rid3,
        InvocationRole::Fix,
        &base,
        fix_task(aid2, "m-2", &foreign),
    );
    insert_body(&store, &bad2);
    let err = sup
        .prepare_workspace(&store, &rid3, &src_of(), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidPredecessor { .. }),
        "{err:?}"
    );
    let _ = (rid, produced);
}

#[test]
fn a_clean_restart_reuses_the_same_pinned_workspace() {
    let root = tmp_root("restart");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-restart", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);

    let first = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let second = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(first, second);
    assert_clean_pinned(&ws_path(&root, &w), &format!("work/{}", w.0), &base, &base);
}

#[test]
fn receipt_without_final_reconstructs_the_same_pin() {
    let root = tmp_root("reconstruct");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-recon", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    std::fs::remove_dir_all(ws_path(&root, &w)).unwrap();

    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, base);
    assert_clean_pinned(&prepared.path, &format!("work/{}", w.0), &base, &base);
}

#[test]
fn dirty_or_advanced_or_foreign_workspaces_are_preserved_not_reset() {
    let root = tmp_root("preserve");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-preserve", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    std::fs::write(ws.join("seed.txt"), "wip").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(ws.join("seed.txt")).unwrap(), "wip");
    git(&ws, &["checkout", "--", "."]);

    std::fs::write(ws.join("wip.txt"), "wip").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    std::fs::remove_file(ws.join("wip.txt")).unwrap();

    git(&ws, &["config", "user.email", "t@t"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("ahead.txt"), "ahead").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-m", "wip"]);
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(ws.join("ahead.txt").exists(), "the WIP commit is kept");

    git(&ws, &["checkout", "-b", "foreign", &base]);
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(git(&ws, &["symbolic-ref", "--short", "HEAD"]), "foreign");
}

#[test]
fn declared_untracked_artifacts_are_output_not_dirt() {
    let root = tmp_root("declared-art");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-art", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    // With no Invocation row the declared set is empty: even the
    // manifest name still counts as dirt.
    std::fs::write(ws.join("artifact.json"), "{}").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );

    // Once the Invocation declares it, the untracked manifest is the
    // round's expected output and the same request reuses the tree.
    declared_invocation(&store, &e, &rid, &["artifact.json"]);
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws);

    // An undeclared untracked file next to it still refuses.
    std::fs::write(ws.join("stray.txt"), "wip").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("stray.txt")).unwrap(),
        "wip"
    );
}

/// Issue #449: a worker runtime leaves interpreter caches in the tree
/// it ran in (`python3 -m unittest` writes `__pycache__/` under every
/// tested package plus loose `*.pyc`/`*.pyo` files). Residue is
/// normalized into `.git/info/exclude` — preserved, never deleted — so
/// reuse proceeds, while genuinely untracked WIP still refuses.
#[test]
fn worker_runtime_residue_never_refuses_reuse_but_wip_still_does() {
    let root = tmp_root("residue");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-residue", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    // A fresh workspace is born carrying the residue excludes.
    let exclude = std::fs::read_to_string(ws.join(".git/info/exclude")).unwrap();
    for pattern in ["__pycache__/", "*.pyc", "*.pyo"] {
        assert!(
            exclude.lines().any(|line| line == pattern),
            "{pattern} missing from .git/info/exclude: {exclude}"
        );
    }

    // Residue at two package depths plus a loose compiled file: the
    // same request reuses the tree and nothing is deleted.
    std::fs::create_dir_all(ws.join("__pycache__")).unwrap();
    std::fs::write(ws.join("__pycache__/mod.cpython-311.pyc"), "x").unwrap();
    std::fs::create_dir_all(ws.join("tests/__pycache__")).unwrap();
    std::fs::write(ws.join("tests/__pycache__/test_mod.cpython-311.pyc"), "x").unwrap();
    std::fs::write(ws.join("loose.pyc"), "x").unwrap();
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws);
    assert!(
        ws.join("__pycache__/mod.cpython-311.pyc").exists(),
        "residue is preserved, never deleted"
    );
    assert!(
        ws.join("tests/__pycache__/test_mod.cpython-311.pyc")
            .exists()
    );
    assert!(ws.join("loose.pyc").exists());

    // A workspace predating the seeded excludes normalizes on the
    // reuse pass — the upgrade path never needs a rebuild.
    std::fs::write(ws.join(".git/info/exclude"), "").unwrap();
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws);
    let exclude = std::fs::read_to_string(ws.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|line| line == "__pycache__/"));

    // Untracked WIP beside residue still refuses, untouched.
    std::fs::write(ws.join("wip.txt"), "wip").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(ws.join("wip.txt")).unwrap(), "wip");
}

/// An `.git/info/exclude` a tool wrote with non-UTF-8 bytes is not a
/// reason to refuse reuse: the seed reads bytes, preserves them, and
/// appends the residue patterns after a newline.
#[test]
fn a_non_utf8_exclude_still_gains_the_residue_patterns() {
    let root = tmp_root("residue-nonutf8");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-nonutf8", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    let foreign: &[u8] = b"\xFF\xFE-not-utf8\n";
    std::fs::write(ws.join(".git/info/exclude"), foreign).unwrap();
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws, "reuse proceeds over the raw bytes");

    let exclude = std::fs::read(ws.join(".git/info/exclude")).unwrap();
    assert!(
        exclude.starts_with(foreign),
        "existing content is byte-preserved: {exclude:?}"
    );
    for pattern in [b"__pycache__/".as_slice(), b"*.pyc", b"*.pyo"] {
        assert!(
            exclude
                .split(|byte| *byte == b'\n')
                .any(|line| line == pattern),
            "{pattern:?} missing from .git/info/exclude: {exclude:?}"
        );
    }
}

/// A `.git/info/exclude` planted as a symlink is never followed: the
/// seed opens O_NOFOLLOW, so reuse refuses instead of writing through
/// to the link target.
#[test]
fn a_symlinked_exclude_refuses_and_never_writes_through() {
    let root = tmp_root("residue-symlink");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-symlink", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    let victim = root.join("victim.txt");
    std::fs::write(&victim, "untouched").unwrap();
    let exclude = ws.join(".git/info/exclude");
    std::fs::remove_file(&exclude).unwrap();
    std::os::unix::fs::symlink(&victim, &exclude).unwrap();

    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(
            err,
            PreparationError::WorkspaceConflict { .. } | PreparationError::Io(_)
        ),
        "a symlinked exclude refuses, never writes through: {err:?}"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
}

#[test]
fn a_retried_request_reuses_a_workspace_holding_declared_output() {
    let root = tmp_root("retry-art");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-retry", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);
    std::fs::write(ws.join("artifact.json"), "{}").unwrap();

    // A retry records a fresh request under a new digest; it takes the
    // workspace over once the tree verifies, manifest and all.
    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Coding);
    let request2 = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Coding,
        &base,
        coding_task(vec![]),
    );
    insert_body(&store, &request2);
    declared_invocation(&store, &e, &rid2, &["artifact.json"]);

    let prepared = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.request_digest, request2.request_digest);
    assert!(ws.join("artifact.json").exists());
}

#[test]
fn a_declared_path_on_a_modified_tracked_file_still_refuses() {
    let root = tmp_root("tracked-art");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base0, _n) = git_source(&root);
    // `artifact.json` tracked in the base: a modification is a real
    // change, never the untracked manifest slot the tolerance covers.
    std::fs::write(src.join("artifact.json"), "tracked").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "track artifact"]);
    let base = git(&src, &["rev-parse", "HEAD"]);
    let (w, e, rid, _r) = coding_fixture(&store, "p-track", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);
    declared_invocation(&store, &e, &rid, &["artifact.json"]);

    std::fs::write(ws.join("artifact.json"), "edited").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("artifact.json")).unwrap(),
        "edited"
    );
}

#[test]
fn a_fix_reuses_the_workspace_its_predecessor_left() {
    let root = tmp_root("fix-reuse");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-fxreuse", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    // The Coding writer commits its output on the managed branch and
    // leaves the declared manifest untracked at the workspace root.
    git(&ws, &["config", "user.email", "t@t"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("out.txt"), "produced").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-m", "produced"]);
    let produced = git(&ws, &["rev-parse", "HEAD"]);
    std::fs::write(ws.join("artifact.json"), "{}").unwrap();
    acknowledge_predecessor(
        &store, &layout, &ws, &w, &e, "art-1", &produced, &base, "m-1",
    );

    // The follow-up Fix binds the acknowledged predecessor; the
    // workspace already sits on `produced` with the manifest left
    // over — reuse must succeed, not deadlock on WorkspaceConflict.
    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let request2 = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task("art-1", "m-1", &produced),
    );
    insert_body(&store, &request2);
    declared_invocation(&store, &e, &rid2, &["artifact.json"]);

    let prepared = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws);
    assert_eq!(prepared.head, produced);
    assert_eq!(prepared.request_digest, request2.request_digest);
    assert!(ws.join("artifact.json").exists());
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), produced);
}

/// A runtime's `git add -A` can still capture a manifest a previous
/// round left behind — the produced commit then tracks it. The tree
/// is clean and the next round must reuse it: refusing would deadlock
/// a state the writer's own round created. (The shim separately
/// guarantees the file is never a tracked *modification*: tracked
/// repository content is left exactly as it is.)
#[test]
fn a_manifest_captured_by_the_produced_commit_still_verifies_clean() {
    let root = tmp_root("tracked-mf");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-trkmf", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);

    // Round N's produced commit captured the manifest alongside the
    // output: the file is tracked and unmodified.
    git(&ws, &["config", "user.email", "t@t"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("out.txt"), "produced").unwrap();
    std::fs::write(ws.join("artifact.json"), "{}").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-m", "produced"]);
    let produced = git(&ws, &["rev-parse", "HEAD"]);
    acknowledge_predecessor(
        &store, &layout, &ws, &w, &e, "art-1", &produced, &base, "m-1",
    );

    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let request2 = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task("art-1", "m-1", &produced),
    );
    insert_body(&store, &request2);
    declared_invocation(&store, &e, &rid2, &["artifact.json"]);

    let prepared = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.path, ws);
    assert_eq!(prepared.head, produced);
    assert_eq!(prepared.request_digest, request2.request_digest);
}

#[test]
fn unknown_partial_and_symlinked_workspaces_are_refused_untouched() {
    let root = tmp_root("partial");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-partial", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let ws = ws_path(&root, &w);

    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("leftover"), "x").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(ws.join("leftover").exists());
    std::fs::remove_dir_all(&ws).unwrap();

    let target = root.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &ws).unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(
        std::fs::symlink_metadata(&ws)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink is never followed or replaced"
    );
}

#[test]
fn reservation_and_writer_gates_refuse_before_any_mutation() {
    let root = tmp_root("gates");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-gates", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);

    let ws_res = WriteResource::Workspace {
        workspace_id: format!("ws-{}", w.0),
    };
    let br_res = WriteResource::Branch {
        repository: repo(),
        branch: format!("work/{}", w.0),
    };
    store
        .transact(|tx| {
            tx.apply(&Effect::DeleteReservations {
                resource_keys: vec![ws_res.key(), br_res.key()],
                holder: ReservationHolder::Execution {
                    execution_id: e.clone(),
                },
            })?;
            tx.apply(&Effect::InsertReservation {
                reservation: ResourceReservation {
                    resource_key: ws_res.key(),
                    resource: ws_res.clone(),
                    holder: ReservationHolder::External {
                        actor_id: "other".into(),
                    },
                    expires_at_unix_seconds: Some(NOW + 3600),
                },
            })
        })
        .unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::ReservationConflict { .. }),
        "{err:?}"
    );

    store
        .transact(|tx| {
            tx.apply(&Effect::DeleteReservations {
                resource_keys: vec![ws_res.key()],
                holder: ReservationHolder::External {
                    actor_id: "other".into(),
                },
            })?;
            tx.apply(&Effect::InsertReservation {
                reservation: ResourceReservation {
                    resource_key: ws_res.key(),
                    resource: ws_res.clone(),
                    holder: ReservationHolder::Execution {
                        execution_id: e.clone(),
                    },
                    expires_at_unix_seconds: None,
                },
            })
        })
        .unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::ReservationConflict { .. }),
        "{err:?}"
    );

    store
        .transact(|tx| {
            tx.apply(&Effect::InsertReservation {
                reservation: ResourceReservation {
                    resource_key: br_res.key(),
                    resource: br_res.clone(),
                    holder: ReservationHolder::Execution {
                        execution_id: e.clone(),
                    },
                    expires_at_unix_seconds: None,
                },
            })?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-live".into()),
                    request_id: format!("{}:inv:9", e.0),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        })
        .unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::ActiveWriter { .. }),
        "{err:?}"
    );
    assert!(
        !ws_path(&root, &w).exists(),
        "no filesystem mutation happened"
    );
}

#[test]
fn a_cancelled_execution_fails_closed() {
    let root = tmp_root("cancelled");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-cancel", &base, coding_task(vec![]));
    store
        .transact(|tx| {
            let mut execution = tx.execution(&e)?.unwrap();
            let expected = execution.state.clone();
            execution.state = ExecutionState::Cancelled;
            tx.apply(&Effect::UpdateExecution {
                execution,
                expected_state: expected,
            })
        })
        .unwrap();
    let (sup, _layout) = supervisor(&root);
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidRequest { .. }),
        "{err:?}"
    );
    assert!(!ws_path(&root, &w).exists());
}

#[test]
fn a_second_prepare_cannot_overwrite_the_first() {
    let root = tmp_root("concurrent");
    let store = Arc::new(SqliteStore::in_memory(2).unwrap());
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-conc", &base, coding_task(vec![]));
    let layout = layout(&root);
    std::fs::create_dir_all(&layout.workspace_root).unwrap();
    let layout_a = layout.clone();
    let layout_b = layout.clone();
    let store_a = Arc::clone(&store);
    let src_a = src.clone();
    let rid_a = rid.clone();
    let a = std::thread::spawn(move || {
        mk_supervisor(&layout_a).prepare_workspace(&*store_a, &rid_a, &source_for(&src_a), NOW)
    });
    let store_b = Arc::clone(&store);
    let src_b = src.clone();
    let rid_b = rid.clone();
    let b = std::thread::spawn(move || {
        mk_supervisor(&layout_b).prepare_workspace(&*store_b, &rid_b, &source_for(&src_b), NOW)
    });
    let ra = a.join().unwrap();
    let rb = b.join().unwrap();
    match (ra, rb) {
        (Ok(pa), Ok(pb)) => assert_eq!(pa, pb),
        (Ok(p), Err(e)) | (Err(e), Ok(p)) => {
            assert!(
                matches!(e, PreparationError::WorkspaceConflict { .. }),
                "{e:?}"
            );
            assert_eq!(p.head, base);
        }
        (Err(ea), Err(eb)) => panic!("both failed: {ea:?} {eb:?}"),
    }
    let ws = ws_path(&root, &w);
    assert_clean_pinned(&ws, &format!("work/{}", w.0), &base, &base);
}

fn mk_supervisor(layout: &HandoffLayout) -> Supervisor<PosixProcessControl> {
    Supervisor::new(
        PosixProcessControl,
        layout.workspace_root.clone(),
        vec!["/bin/true".into()],
        config(),
    )
    .with_handoff(layout.clone())
}

fn source_for(dir: &Path) -> RepositorySource {
    RepositorySource {
        repository: repo(),
        path: dir.to_path_buf(),
    }
}

#[test]
fn source_configuration_and_credentials_never_reach_the_workspace() {
    let root = tmp_root("secret");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    git(
        &src,
        &["config", "credential.helper", "store --file=/tmp/secret"],
    );
    git(
        &src,
        &[
            "remote",
            "add",
            "origin",
            "https://token:secret@example.invalid/repo.git",
        ],
    );
    std::fs::write(src.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
    let (w, _e, rid, _r) = coding_fixture(&store, "p-secret", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws_config = std::fs::read_to_string(prepared.path.join(".git/config")).unwrap();
    assert!(!ws_config.contains("example.invalid"), "{ws_config}");
    assert!(!ws_config.contains("/tmp/secret"), "{ws_config}");
    assert!(!prepared.path.join(".git/hooks/pre-commit").exists());
    assert_clean_pinned(&ws_path(&root, &w), &format!("work/{}", w.0), &base, &base);
}

#[test]
fn a_detached_base_not_on_any_branch_is_imported_exactly() {
    let root = tmp_root("detached-base");
    let store = SqliteStore::in_memory(1).unwrap();
    let src = root.join("source");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-b", "main"]);
    git(&src, &["config", "user.email", "t@t"]);
    git(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("seed.txt"), "unrelated").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "unrelated"]);
    git(&src, &["checkout", "--orphan", "gone"]);
    git(&src, &["rm", "-rf", "."]);
    std::fs::write(src.join("detached.txt"), "base").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "base"]);
    let base = git(&src, &["rev-parse", "HEAD"]);
    git(&src, &["checkout", "main"]);
    git(&src, &["branch", "-D", "gone"]);
    std::fs::write(src.join("seed.txt"), "newer").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "newer"]);
    let newer = git(&src, &["rev-parse", "HEAD"]);
    assert_ne!(base, newer);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-det", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);

    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, base);
    let ws = ws_path(&root, &w);
    assert_clean_pinned(&ws, &format!("work/{}", w.0), &base, &base);
    let refs = git(&ws, &["for-each-ref", "--format=%(refname)"]);
    let mut names: Vec<&str> = refs.lines().collect();
    names.sort();
    let mut expected = vec![
        format!("refs/heads/{}", prepared.branch),
        "refs/heads/taisan-base".into(),
    ];
    expected.sort();
    assert_eq!(names, expected, "no unrelated refs were imported: {refs}");
}

#[test]
fn a_fix_with_a_new_digest_reuses_the_clean_predecessor_workspace() {
    let root = tmp_root("fix-redigest");
    let store = SqliteStore::in_memory(1).unwrap();
    let (w, e, rid, src, base, produced, aid) = fix_fixture(&store, &root, "p-fdig", "m-1");
    let (sup, layout) = supervisor(&root);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");
    let first = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(first.head, produced);

    let rid2 = format!("{}:inv:2", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let second_request = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task(&aid, "m-1", &produced),
    );
    insert_body(&store, &second_request);
    assert_ne!(first.request_digest, second_request.request_digest);
    let second = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(second.head, produced);
    assert_eq!(second.request_digest, second_request.request_digest);
}

#[test]
fn a_missing_final_with_a_fix_receipt_rebuilds_at_produced_not_base() {
    let root = tmp_root("fix-recon");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-fre", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    std::fs::remove_dir_all(ws_path(&root, &w)).unwrap();

    std::fs::write(src.join("fix.txt"), "fix").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "produced"]);
    let produced = git(&src, &["rev-parse", "HEAD"]);
    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let aid = "art-1";
    let request = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task(aid, "m-1", &produced),
    );
    insert_body(&store, &request);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, aid, &produced, &base, "m-1");
    let prepared = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(
        prepared.head, produced,
        "rebuilt at produced, not the Coding base"
    );
    assert_clean_pinned(&prepared.path, &format!("work/{}", w.0), &base, &produced);
}

#[test]
fn a_successor_request_rebinds_the_receipt_only_while_the_tree_matches() {
    let root = tmp_root("successor-rebind");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-succ", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let first = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(first.head, base);

    // A retried Execution's fresh Coding request: same Work, same pin,
    // different durable identity. The tree still matches the pins, so
    // preparation re-binds the receipt to the new digest.
    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Coding);
    let second_request = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Coding,
        &base,
        coding_task(vec![]),
    );
    insert_body(&store, &second_request);
    assert_ne!(first.request_digest, second_request.request_digest);
    let second = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(second.request_digest, second_request.request_digest);
    assert_clean_pinned(&ws_path(&root, &w), &format!("work/{}", w.0), &base, &base);

    // Once the tree moves, another different-digest request refuses:
    // the successor cannot claim WIP it did not produce, and the
    // committed work is preserved untouched.
    let ws = ws_path(&root, &w);
    git(&ws, &["config", "user.email", "t@t"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("wip.txt"), "wip").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-m", "wip"]);
    let wip_head = git(&ws, &["rev-parse", "HEAD"]);
    let rid3 = format!("{}:inv:2", e.0);
    insert_request_row(&store, &w, &e, &rid3, InvocationRole::Coding);
    let third_request = v2_request(
        &store,
        &w,
        &e,
        &rid3,
        InvocationRole::Coding,
        &base,
        coding_task(vec![]),
    );
    insert_body(&store, &third_request);
    assert_ne!(second_request.request_digest, third_request.request_digest);
    let err = sup
        .prepare_workspace(&store, &rid3, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(ws.join("wip.txt").exists(), "the WIP commit is kept");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), wip_head);
}

#[test]
fn a_successor_request_rebuilds_a_missing_workspace_at_the_new_pins() {
    let root = tmp_root("successor-rebuild");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-sreb", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    std::fs::remove_dir_all(ws_path(&root, &w)).unwrap();

    let rid2 = format!("{}:inv:1", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Coding);
    let request = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Coding,
        &base,
        coding_task(vec![]),
    );
    insert_body(&store, &request);
    let prepared = sup
        .prepare_workspace(&store, &rid2, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, base);
    assert_eq!(prepared.request_digest, request.request_digest);
    assert_clean_pinned(&prepared.path, &format!("work/{}", w.0), &base, &base);
}

#[test]
fn a_tampered_receipt_head_under_the_same_digest_is_refused() {
    let root = tmp_root("tamper");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-tam", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let receipt_path = layout
        .workspace_root
        .join(".prepare")
        .join(format!("{}.receipt.json", w.0));
    let raw = std::fs::read_to_string(&receipt_path).unwrap();
    let tampered = raw.replace(&base, &"0".repeat(40));
    assert_ne!(raw, tampered);
    std::fs::write(&receipt_path, tampered).unwrap();

    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );

    std::fs::remove_dir_all(ws_path(&root, &w)).unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
}

#[test]
fn a_hostile_git_dir_never_gets_a_git_invocation() {
    let root = tmp_root("hostile-git");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-hos", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let ws = ws_path(&root, &w);
    let marker = root.join("filter-ran");
    let script = root.join("evil.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }
    std::fs::write(
        ws.join(".git/config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n[filter \"x\"]\n\tclean = {}\n",
            script.display()
        ),
    )
    .unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(!marker.exists(), "the hostile filter never ran");

    let outside = root.join("outside-config");
    std::fs::write(&outside, "[user]\n\tname = nobody\n").unwrap();
    std::fs::remove_file(ws.join(".git/config")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join(".git/config")).unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "[user]\n\tname = nobody\n"
    );
}

#[test]
fn a_cross_process_lock_holder_gets_a_prompt_typed_refusal() {
    let root = tmp_root("xproc-lock");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-xl", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let lock = layout
        .workspace_root
        .join(".prepare")
        .join(format!("{}.lock", w.0));
    assert!(lock.exists());
    let mut holder = std::process::Command::new("python3")
        .arg("-c")
        .arg("import fcntl,sys,time\nf=open(sys.argv[1],'r+')\nfcntl.flock(f,fcntl.LOCK_EX)\nsys.stdout.write('R')\nsys.stdout.flush()\ntime.sleep(30)\n")
        .arg(&lock)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Read;
        holder
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut [0u8; 1])
            .unwrap();
    }
    let started = std::time::Instant::now();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the second preparer refused promptly instead of holding the store transaction"
    );
}

#[test]
fn normative_slashed_artifact_ids_are_accepted() {
    let root = tmp_root("aid-slash");
    let store = SqliteStore::in_memory(1).unwrap();
    let aid = "inv-39/artifact.json";
    let (w, e, rid, src, base, produced, aid) = fix_fixture_aid(&store, &root, "p-aid", "m-1", aid);
    let (sup, layout) = supervisor(&root);
    acknowledge_predecessor(&store, &layout, &src, &w, &e, &aid, &produced, &base, "m-1");
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, produced);
}

#[test]
fn an_expired_request_deadline_refuses_before_any_mutation() {
    let root = tmp_root("deadline");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid) = admit(&store, "p-dl");
    let mut request = v2_request(
        &store,
        &w,
        &e,
        &rid,
        InvocationRole::Coding,
        &base,
        coding_task(vec![]),
    );
    request.budget.deadline_unix_seconds = NOW - 1;
    request.request_digest = request.digest();
    insert_body(&store, &request);
    let (sup, _layout) = supervisor(&root);
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::InvalidRequest { .. }),
        "{err:?}"
    );
    assert!(!ws_path(&root, &w).exists());
}

#[test]
fn a_coding_workspace_transitions_to_the_acknowledged_fix_head() {
    let root = tmp_root("coding-to-fix");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-c2f", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    let prepared = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    assert_eq!(prepared.head, base);
    let ws = ws_path(&root, &w);
    git(&ws, &["config", "user.email", "t@t"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("fix.txt"), "fix").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-m", "produced"]);
    let produced = git(&ws, &["rev-parse", "HEAD"]);

    let aid = "art-1";
    acknowledge_predecessor(&store, &layout, &ws, &w, &e, aid, &produced, &base, "m-1");
    let rid2 = format!("{}:inv:2", e.0);
    insert_request_row(&store, &w, &e, &rid2, InvocationRole::Fix);
    let request = v2_request(
        &store,
        &w,
        &e,
        &rid2,
        InvocationRole::Fix,
        &base,
        fix_task(aid, "m-1", &produced),
    );
    insert_body(&store, &request);

    let second = sup
        .prepare_workspace(&store, &rid2, &source(&root, &layout.repository), NOW)
        .unwrap();
    assert_eq!(second.head, produced);
    assert_eq!(second.request_digest, request.request_digest);
    let third = sup
        .prepare_workspace(&store, &rid2, &source(&root, &layout.repository), NOW)
        .unwrap();
    assert_eq!(third, second);
    assert_clean_pinned(&ws, &format!("work/{}", w.0), &base, &produced);

    std::fs::write(ws.join("fix.txt"), "dirty").unwrap();
    let err = sup
        .prepare_workspace(&store, &rid2, &source(&root, &layout.repository), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("fix.txt")).unwrap(),
        "dirty",
        "worker WIP is preserved"
    );
}

#[test]
fn root_control_and_lock_symlinks_are_refused_and_targets_untouched() {
    let root = tmp_root("root-symlink");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (_w, _e, rid, _r) = coding_fixture(&store, "p-rs", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    let real = root.join("real-ws");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::remove_dir_all(&layout.workspace_root).unwrap();
    std::os::unix::fs::symlink(&real, &layout.workspace_root).unwrap();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(
        !real.join(".prepare").exists(),
        "the symlink target was not mutated"
    );
    std::fs::remove_file(&layout.workspace_root).unwrap();

    let root2 = tmp_root("control-symlink");
    let store2 = SqliteStore::in_memory(1).unwrap();
    let (w2, _e2, rid2, _r2) = coding_fixture(&store2, "p-cs", &base, coding_task(vec![]));
    let (sup2, layout2) = supervisor(&root2);
    let target2 = root2.join("elsewhere");
    std::fs::create_dir_all(&target2).unwrap();
    std::os::unix::fs::symlink(&target2, layout2.workspace_root.join(".prepare")).unwrap();
    let err = sup2
        .prepare_workspace(&store2, &rid2, &source(&root2, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(target2.read_dir().unwrap().next().is_none());
    assert!(!ws_path(&root2, &w2).exists());

    let root3 = tmp_root("lock-symlink");
    let store3 = SqliteStore::in_memory(1).unwrap();
    let (w3, _e3, rid3, _r3) = coding_fixture(&store3, "p-ls", &base, coding_task(vec![]));
    let (sup3, layout3) = supervisor(&root3);
    sup3.prepare_workspace(&store3, &rid3, &source(&root3, &src), NOW)
        .unwrap();
    let lock = layout3
        .workspace_root
        .join(".prepare")
        .join(format!("{}.lock", w3.0));
    let victim = root3.join("victim");
    std::fs::write(&victim, "keep").unwrap();
    std::fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(&victim, &lock).unwrap();
    let err = sup3
        .prepare_workspace(&store3, &rid3, &source(&root3, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
}

struct FifoGate {
    opened: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    fifo: PathBuf,
    _writer: std::thread::JoinHandle<()>,
}

impl Drop for FifoGate {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.fifo);
    }
}

fn fifo_config_gate(source_dir: &Path) -> FifoGate {
    let config = source_dir.join(".git/config");
    let original = std::fs::read(&config).unwrap();
    std::fs::remove_file(&config).unwrap();
    let st = std::process::Command::new("mkfifo")
        .arg(&config)
        .status()
        .unwrap();
    assert!(st.success());
    let (opened_tx, opened) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let fifo = config.clone();
    let _writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut fd = match std::fs::OpenOptions::new().write(true).open(&fifo) {
            Ok(fd) => fd,
            Err(_) => return,
        };
        let _ = opened_tx.send(());
        let _ = release_rx.recv();
        let _ = fd.write_all(&original);
        drop(fd);
        loop {
            let mut fd = match std::fs::OpenOptions::new().write(true).open(&fifo) {
                Ok(fd) => fd,
                Err(_) => return,
            };
            let _ = fd.write_all(&original);
        }
    });
    FifoGate {
        opened,
        release: release_tx,
        fifo: config,
        _writer,
    }
}

#[test]
fn a_slow_source_read_does_not_hold_the_store_writer_lock() {
    let root = tmp_root("slow-git");
    let db = root.join("store.sqlite");
    let store = Arc::new(SqliteStore::open(&db, 2).unwrap());
    let peer = SqliteStore::open(&db, 1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-slow", &base, coding_task(vec![]));
    let (sup, _layout) = supervisor(&root);
    let gate = fifo_config_gate(&src);
    let store_p = Arc::clone(&store);
    let src_p = src.clone();
    let rid_p = rid.clone();
    let prep = std::thread::spawn(move || {
        sup.prepare_workspace(&*store_p, &rid_p, &source_for(&src_p), NOW)
    });
    gate.opened
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("git opened the fifo config");

    let (probe_tx, probe_rx) = std::sync::mpsc::channel();
    let store_probe = Arc::clone(&store);
    let probe = std::thread::spawn(move || {
        let result = store_probe.transact(|tx| tx.store_id());
        let _ = probe_tx.send(result.is_ok());
    });
    let same_store = probe_rx.recv_timeout(std::time::Duration::from_secs(15));
    let peer_ok = peer.transact(|tx| tx.store_id()).is_ok();

    let _ = gate.release.send(());
    let prepared = prep.join().unwrap();
    let _ = probe.join();

    assert_eq!(
        same_store,
        Ok(true),
        "an unrelated same-store transaction completes while git is blocked"
    );
    assert!(peer_ok, "an independent connection still writes");
    let prepared = prepared.unwrap();
    assert_eq!(prepared.head, base);
    assert_clean_pinned(&ws_path(&root, &w), &format!("work/{}", w.0), &base, &base);
}

#[test]
fn a_reservation_lost_mid_preparation_refuses_at_finalization() {
    let root = tmp_root("slow-res");
    let db = root.join("store.sqlite");
    let store = Arc::new(SqliteStore::open(&db, 2).unwrap());
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-sres", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    let gate = fifo_config_gate(&src);
    let store_p = Arc::clone(&store);
    let src_p = src.clone();
    let rid_p = rid.clone();
    let prep = std::thread::spawn(move || {
        sup.prepare_workspace(&*store_p, &rid_p, &source_for(&src_p), NOW)
    });
    gate.opened
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("git opened the fifo config");

    let ws_res = WriteResource::Workspace {
        workspace_id: format!("ws-{}", w.0),
    };
    let (mtx, mrx) = std::sync::mpsc::channel();
    let store_m = Arc::clone(&store);
    let mutator = std::thread::spawn(move || {
        let result = store_m.transact(|tx| {
            tx.apply(&Effect::DeleteReservations {
                resource_keys: vec![ws_res.key()],
                holder: ReservationHolder::Execution {
                    execution_id: e.clone(),
                },
            })
        });
        let _ = mtx.send(result.is_ok());
    });
    let applied = mrx.recv_timeout(std::time::Duration::from_secs(15));

    let _ = gate.release.send(());
    let err = prep.join().unwrap().unwrap_err();
    let _ = mutator.join();

    assert_eq!(
        applied,
        Ok(true),
        "the mutation landed while git was blocked"
    );
    assert!(
        matches!(err, PreparationError::ReservationConflict { .. }),
        "{err:?}"
    );
    assert!(!ws_path(&root, &w).exists(), "no final workspace landed");
    let receipt = layout
        .workspace_root
        .join(".prepare")
        .join(format!("{}.receipt.json", w.0));
    assert!(!receipt.exists(), "no success receipt was written");
    let staging_left = std::fs::read_dir(layout.workspace_root.join(".prepare"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().starts_with("staging-"));
    assert!(!staging_left, "failed preparation removed its staging tree");
}

#[test]
fn a_new_writer_mid_preparation_refuses_at_finalization() {
    let root = tmp_root("slow-writer");
    let db = root.join("store.sqlite");
    let store = Arc::new(SqliteStore::open(&db, 2).unwrap());
    let (src, base, _n) = git_source(&root);
    let (w, e, rid, _r) = coding_fixture(&store, "p-sw", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    let gate = fifo_config_gate(&src);
    let store_p = Arc::clone(&store);
    let src_p = src.clone();
    let rid_p = rid.clone();
    let prep = std::thread::spawn(move || {
        sup.prepare_workspace(&*store_p, &rid_p, &source_for(&src_p), NOW)
    });
    gate.opened
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("git opened the fifo config");

    let (mtx, mrx) = std::sync::mpsc::channel();
    let store_m = Arc::clone(&store);
    let mutator = std::thread::spawn(move || {
        let result = store_m.transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-mid".into()),
                    request_id: format!("{}:inv:9", e.0),
                    execution_id: e.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: None,
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })
        });
        let _ = mtx.send(result.is_ok());
    });
    let applied = mrx.recv_timeout(std::time::Duration::from_secs(15));
    let receipt_path = layout
        .workspace_root
        .join(".prepare")
        .join(format!("{}.receipt.json", w.0));

    assert!(!ws_path(&root, &w).exists());
    assert!(!receipt_path.exists());

    let _ = gate.release.send(());
    let err = prep.join().unwrap().unwrap_err();
    let _ = mutator.join();

    assert_eq!(applied, Ok(true), "the writer landed while git was blocked");
    assert!(
        matches!(err, PreparationError::ActiveWriter { .. }),
        "{err:?}"
    );
    assert!(!ws_path(&root, &w).exists());
    assert!(!receipt_path.exists());
    let staging_left = std::fs::read_dir(layout.workspace_root.join(".prepare"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().starts_with("staging-"));
    assert!(!staging_left, "failed preparation removed its staging tree");
}

#[test]
fn a_nonregular_receipt_is_a_prompt_typed_refusal() {
    let root = tmp_root("fifo-receipt");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, _n) = git_source(&root);
    let (w, _e, rid, _r) = coding_fixture(&store, "p-fifo", &base, coding_task(vec![]));
    let (sup, layout) = supervisor(&root);
    sup.prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap();
    let receipt = layout
        .workspace_root
        .join(".prepare")
        .join(format!("{}.receipt.json", w.0));
    std::fs::remove_file(&receipt).unwrap();
    let st = std::process::Command::new("mkfifo")
        .arg(&receipt)
        .status()
        .unwrap();
    assert!(st.success());
    let started = std::time::Instant::now();
    let err = sup
        .prepare_workspace(&store, &rid, &source(&root, &src), NOW)
        .unwrap_err();
    assert!(
        matches!(err, PreparationError::WorkspaceConflict { .. }),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the fifo was never opened for reading"
    );
    assert!(ws_path(&root, &w).exists());
}
