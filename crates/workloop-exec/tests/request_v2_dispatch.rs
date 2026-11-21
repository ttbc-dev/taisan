//! Per-Invocation v2 request materialization (`docs/v0.3.md` §4): the
//! consume path builds the immutable `taisan.worker.request/2` body
//! from durable Work/spec/evidence, retains every required input body
//! in the CAS catalogue, lays out `request.json`/`input/`/`out/`
//! before launch, and refuses — typed, before any process exists —
//! when a required body is absent, stale, corrupt, oversized, or
//! inconsistent with the approved spec.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use workloop_core::command::{
    CommandBody, CommandDecision, CommandEffect, CommandRequest, FindingChoice, FindingDecision,
};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::{
    AssessmentVerdict, BranchState, Completeness, HostActionStatus, IssueState,
    NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::store::{
    BodyCatalog, Effect, HostAction, HostActionIntent, Store, StoreError, TxView,
};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, admit_queued_role, classify_failure,
    handle_command, handle_observation, material_progress_basis, sweep,
};
use workloop_core::worker_v2::*;
use workloop_exec::bodies::{BodyStore, CAS_CATEGORY};
use workloop_exec::handoff::{HandoffLayout, artifact_ref};
use workloop_exec::request_v2::{
    InvariantSetConfig, V2BudgetBounds, V2Dispatch, invocation_io_root,
};
use workloop_exec::supervisor::{PosixProcessControl, StopRecord, Supervisor};
use workloop_exec::workspace::RepositorySource;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "workloop-v2dispatch-{}-{}-{}",
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

/// Contract launches are fixture-profile evidence: the reference
/// profile's bwrap boundary is proven by the containment suite, not
/// by these dispatch tests.
fn fixture_sup<P: workloop_core::ports::ProcessControlPort>(
    port: P,
    root: PathBuf,
    argv: Vec<String>,
    cfg: WorkloopConfig,
) -> Supervisor<P> {
    Supervisor::new(port, root, argv, cfg).with_isolation_profile(
        workloop_core::worker::isolation_profile(
            workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
        )
        .unwrap(),
    )
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
            HostCapability::IssueRead,
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

fn fixture_bin() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
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

/// A source repository with two commits: `base` (the spec-bound start)
/// and `newer` (HEAD — the Coding request's base object).
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

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repository: repo(),
        issue: IssueId::trusted(format!("issue-{number}")),
        display_number: Some(number),
    }
}

fn source_snapshot(number: u64, revision: &str, digest: &str) -> SourceSnapshot {
    SourceSnapshot {
        issue: issue(number),
        source_revision: revision.into(),
        content_digest: digest.into(),
    }
}

/// Submit a Work and drive its source gate to `Matches` with one
/// authoritative Issue read per declared source, then admit the first
/// Execution — the same durable shape a production pass produces.
fn admit(
    store: &SqliteStore,
    key: &str,
    sources: Vec<SourceSnapshot>,
) -> (WorkId, ExecutionId, String) {
    admit_ex(store, key, sources, vec![])
}

/// `admit` with caller-declared `verification_argv` — the spec payload
/// the handoff's independent checks read.
fn admit_ex(
    store: &SqliteStore,
    key: &str,
    sources: Vec<SourceSnapshot>,
    verification_argv: Vec<Vec<String>>,
) -> (WorkId, ExecutionId, String) {
    admit_ex_cfg(store, key, sources, verification_argv, &config())
}

/// `admit_ex` under a caller-shaped config — lane policy and host
/// bindings are spec inputs a test must be able to vary.
fn admit_ex_cfg(
    store: &SqliteStore,
    key: &str,
    sources: Vec<SourceSnapshot>,
    verification_argv: Vec<Vec<String>>,
    cfg: &WorkloopConfig,
) -> (WorkId, ExecutionId, String) {
    let cfg = cfg.clone();
    let d = handle_command(
        store,
        &cfg,
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("intent-{key}"),
                sources,
                constraints: vec![],
                verification_argv,
            },
        },
        NOW,
    )
    .unwrap();
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
    // One matching authoritative read per spec source settles the
    // source gate; an unsourced Work is `NotApplicable` already.
    let spec_sources: Vec<SourceSnapshot> = store
        .transact(|tx| tx.spec(&work_id, Some(1)))
        .unwrap()
        .map(|s| s.payload.sources)
        .unwrap_or_default();
    for source in &spec_sources {
        handle_observation(
            store,
            &cfg,
            &Observation {
                delivery: None,
                subject_work_id: Some(work_id.clone()),
                subject_execution_id: None,
                fact: ObservationFact::Issue {
                    issue: source.issue.clone(),
                    source_revision: source.source_revision.clone(),
                    content_digest: source.content_digest.clone(),
                    state: IssueState::Open,
                },
                observed_unix_seconds: NOW,
            },
            NOW,
        )
        .unwrap();
    }
    let admitted = store
        .transact(|tx| admit_queued(tx, &cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
}

/// Build the snapshot envelope a source refresh would retain for one
/// spec-bound read — the content digest is the real
/// `issue_content_digest(title, body)` the spec's `SourceSnapshot`
/// records — retain it in the CAS catalogue at its deterministic
/// lookup path (the same write `retain_issue_snapshots` performs in
/// phase 1), and return both the spec source and the snapshot body.
fn retained_issue_source(
    store: &SqliteStore,
    state_root: &Path,
    number: u64,
    revision: &str,
    title: &str,
    body: &str,
) -> (SourceSnapshot, IssueSnapshotBody) {
    let digest = issue_content_digest(title, body);
    let source = source_snapshot(number, revision, &digest);
    let snapshot = IssueSnapshotBody::new(
        WireIssue {
            host: source.issue.repository.host.as_str().to_string(),
            repository: source.issue.repository.repository.as_str().to_string(),
            issue: source.issue.issue.as_str().to_string(),
        },
        source.source_revision.clone(),
        title,
        body,
        digest,
    );
    let bodies = BodyStore::new(state_root.to_path_buf());
    bodies
        .retain_at(
            store,
            CAS_CATEGORY,
            &snapshot.retained_path(),
            &snapshot.canonical_bytes(),
            NOW,
            None,
        )
        .unwrap();
    (source, snapshot)
}

fn dispatch_profile(state_root: &Path, src: &Path) -> V2Dispatch {
    V2Dispatch {
        bodies: BodyStore::new(state_root.to_path_buf()),
        sources: vec![RepositorySource {
            repository: repo(),
            path: src.to_path_buf(),
        }],
        invariant_set: Some(InvariantSetConfig {
            id: "invariants-1".into(),
            revision: 1,
            items: vec!["no-todo".into(), "tests-pass".into()],
        }),
        budget: V2BudgetBounds {
            max_turns: 8,
            max_report_bytes: 1 << 20,
            max_log_bytes: 1 << 25,
            max_verification_commands: 4,
            max_continuation_refs: 4,
        },
        capabilities: vec!["artifact_json".into(), "process_control".into()],
        verification_egress_hosts: vec![],
    }
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

/// A v2 supervisor whose worker argv selects the fixture's v2 wire:
/// `describe` prints the `taisan.worker.declaration/1` envelope and
/// `run --request/--out` executes the transport. `extra` appends
/// fixture flags (e.g. an empty vec keeps the defaults).
fn supervisor_v2(
    root: &Path,
    state_root: &Path,
    src: &Path,
    mode: &str,
    wire_v2: bool,
) -> Supervisor<PosixProcessControl> {
    let handoff = layout(root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let mut argv = vec![fixture_bin(), mode.to_string()];
    if wire_v2 {
        argv.push("--wire".into());
        argv.push("v2".into());
    }
    fixture_sup(
        PosixProcessControl,
        handoff.workspace_root.clone(),
        argv,
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(state_root, src))
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store.transact(|tx| tx.execution(id)).unwrap().unwrap()
}

fn pending_ids(store: &SqliteStore) -> Vec<String> {
    store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .map(|r| r.request_id)
        .collect()
}

fn request_body(store: &SqliteStore, request_id: &str) -> WorkerRequestBody {
    store
        .transact(|tx| tx.worker_request_body(request_id))
        .unwrap()
        .unwrap_or_else(|| panic!("no durable v2 body for {request_id}"))
}

fn invocation_for(store: &SqliteStore, request_id: &str) -> Option<Invocation> {
    store
        .transact(|tx| tx.invocation_for_request(request_id))
        .unwrap()
}

fn invocation_state(store: &SqliteStore, request_id: &str) -> Option<InvocationState> {
    invocation_for(store, request_id).map(|i| i.state)
}

fn execution_invocation(
    store: &SqliteStore,
    execution_id: &ExecutionId,
    id: &InvocationId,
) -> Invocation {
    store
        .transact(|tx| tx.invocations_for_execution(execution_id))
        .unwrap()
        .into_iter()
        .find(|i| i.id == *id)
        .expect("invocation recorded")
}

fn dispatch_refusals(store: &SqliteStore, work_id: &WorkId) -> Vec<(String, String)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::DispatchRefused { code, detail, .. } => {
                Some((code.clone(), detail.clone()))
            }
            _ => None,
        })
        .collect()
}

fn launch_refusals(store: &SqliteStore, work_id: &WorkId) -> Vec<String> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .iter()
        .filter_map(|o| match &o.fact {
            ObservationFact::LaunchRefused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn insert_request(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    request_id: &str,
    role: InvocationRole,
    payload: serde_json::Value,
) {
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertRequest {
                request: InvocationRequest {
                    request_id: request_id.into(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    role: role.clone(),
                    spec_revision: 1,
                    payload,
                    adapter_binding: None,
                },
            })?;
            // Keep the durable attempt counter ahead of the synthetic
            // request exactly as `admit_queued_role` would — the next
            // real admission must never collide with this id.
            let exec = tx.execution(execution_id)?.expect("execution exists");
            let mut next = exec.clone();
            next.invocations_used += 1;
            if role == InvocationRole::Review {
                next.review_rounds_used += 1;
            }
            tx.apply(&Effect::UpdateExecution {
                execution: next,
                expected_state: exec.state.clone(),
            })?;
            Ok(())
        })
        .unwrap();
}

/// Record the first Execution's Coding request as already consumed by
/// a finished writer and acknowledge its artifact handoff — the
/// durable predecessor every follow-on role binds. Returns the
/// handoff's `artifact_id` (`<invocation>/<kind>`), the shape
/// `latest_acknowledged_handoff` derives.
fn done_writer_with_handoff(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    request_id: &str,
    base: &str,
    produced: &str,
) -> String {
    let invocation_id = "inv-prev".to_string();
    let artifact_id = format!("{invocation_id}/artifact.json");
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(invocation_id.clone()),
                    request_id: request_id.into(),
                    execution_id: execution_id.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Done,
                    process_identity: None,
                    required_artifacts: vec![RequiredArtifact {
                        kind: "artifact.json".into(),
                        path: "artifact.json".into(),
                        expected_digest: None,
                    }],
                    evidence_refs: vec![],
                    descendants: vec![],
                },
            })?;
            tx.apply(&Effect::MarkRequestConsumed {
                request_id: request_id.into(),
            })?;
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: ArtifactHandoff {
                    artifact_id: artifact_id.clone(),
                    invocation_id: InvocationId(invocation_id.clone()),
                    request_id: request_id.into(),
                    work_id: work_id.clone(),
                    execution_id: execution_id.clone(),
                    base: base.into(),
                    produced: Some(produced.into()),
                    manifest_digest: Some("ab".repeat(32)),
                    state: HandoffState::Acknowledged,
                    invalid_reason: None,
                    attempt: 0,
                    recorded_seq: seq,
                },
                expected_seq: 0,
            })?;
            Ok(())
        })
        .unwrap();
    artifact_id
}

/// Make `produced` fetchable as the artifact's exact commit: the
/// handoff repository learns the source objects and the artifact ref
/// pins `produced`, the same shape `drive_handoffs` leaves behind.
fn publish_predecessor(layout: &HandoffLayout, src: &Path, artifact_id: &str, produced: &str) {
    git(
        &layout.repository,
        &[
            "fetch",
            src.to_str().unwrap(),
            "+refs/heads/*:refs/taisan/imported/*",
        ],
    );
    git(
        &layout.repository,
        &["update-ref", &artifact_ref(artifact_id), produced],
    );
}

/// Record one failed verification result against the acknowledged
/// artifact — the pre-publication evidence a Fix binds.
fn failed_verification(store: &SqliteStore, artifact_id: &str, produced: &str) {
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertVerificationResult {
                result: VerificationResult {
                    id: "vr-1".into(),
                    artifact_id: artifact_id.into(),
                    requirement: "verification_argv[0]".into(),
                    argv: vec!["true".into()],
                    produced: produced.into(),
                    environment_digest: "env-1".into(),
                    result: VerificationOutcome::Fail,
                    output: "checks failed".into(),
                    recorded_seq: seq,
                },
            })
        })
        .unwrap();
}

/// Seal a review round on the Work's current diff target and record
/// the exact-set judgment a review-origin Fix binds. `occurrences`
/// are `(source_key, content_digest)` pairs — `None` mirrors a review
/// source that recorded no content identity; `fix_indices` selects
/// which occurrences the judgment marks for repair.
fn judged_round(
    store: &SqliteStore,
    work_id: &WorkId,
    target: &workloop_core::identity::DiffTarget,
    occurrences: &[(&str, Option<&str>)],
    fix_indices: &[usize],
) {
    store
        .transact(|tx| {
            let round_seq = tx.alloc_seq()?;
            let judgment_seq = tx.alloc_seq()?;
            let round = ReviewRound {
                id: "round-1".into(),
                work_id: work_id.clone(),
                target: target.clone(),
                source: "reviewer".into(),
                source_revision: None,
                completeness: "complete".into(),
                set_digest: "set-1".into(),
                generation: 0,
                seq: round_seq,
            };
            let occurrences: Vec<FindingOccurrence> = occurrences
                .iter()
                .enumerate()
                .map(|(i, (key, digest))| FindingOccurrence {
                    id: format!("occ-{}", i + 1),
                    round_id: round.id.clone(),
                    source_key: (*key).to_string(),
                    content_digest: digest.map(|d| d.to_string()),
                    severity: None,
                    position: None,
                    thread_id: None,
                    summary: Some(format!("summary for {key}")),
                })
                .collect();
            let judgment = RoundJudgment {
                id: "jdg-1".into(),
                work_id: work_id.clone(),
                round_id: round.id.clone(),
                set_digest: round.set_digest.clone(),
                decisions: fix_indices
                    .iter()
                    .map(|i| FindingDecision {
                        occurrence_id: format!("occ-{}", i + 1),
                        choice: FindingChoice::Fix,
                        rationale: None,
                    })
                    .collect(),
                actor_id: "reviewer".into(),
                seq: judgment_seq,
            };
            tx.apply(&Effect::InsertReviewRound { round, occurrences })?;
            tx.apply(&Effect::InsertRoundJudgment { judgment })?;
            Ok(())
        })
        .unwrap();
}

/// Open the Work's change request and record the head evidence that
/// makes it the current accepted diff target: an applied
/// `open_change_request` intent, its matching receipt, and the latest
/// `ChangeHead` observation.
fn open_change_target(
    store: &SqliteStore,
    work_id: &WorkId,
    change: &ChangeRequestRef,
    head: &CommitRevision,
    target_branch: &str,
) {
    let cfg = config();
    let action_id = store
        .transact(|tx| {
            let action_id = tx.alloc_id("act")?;
            let created_seq = tx.alloc_seq()?;
            tx.apply(&Effect::InsertHostActionIntent {
                intent: HostActionIntent {
                    action_id: action_id.clone(),
                    work_id: work_id.clone(),
                    host: cfg.host.repository.host.clone(),
                    repository: cfg.host.repository.clone(),
                    role: ActorRole::Publication,
                    actor: cfg.host.actors[0].actor.clone(),
                    action: HostAction::OpenChangeRequest {
                        source_branch: "work/w".into(),
                        target_branch: target_branch.into(),
                        title: "t".into(),
                        body: String::new(),
                    },
                    payload_digest: "digest".into(),
                    status: HostActionStatus::Applied,
                    authenticated_actor: None,
                    effects: vec![],
                    created_seq,
                },
            })?;
            Ok(action_id)
        })
        .unwrap();
    let obs = |fact| Observation {
        delivery: None,
        subject_work_id: Some(work_id.clone()),
        subject_execution_id: None,
        fact,
        observed_unix_seconds: NOW,
    };
    handle_observation(
        store,
        &cfg,
        &obs(ObservationFact::HostActionResult {
            action_id,
            authenticated_actor: None,
            status: HostActionStatus::Applied,
            effects: vec![NormalizedEffectFact::ChangeOpened {
                change: change.clone(),
                head: head.clone(),
                source_branch: "work/w".into(),
                target_branch: target_branch.into(),
            }],
        }),
        NOW,
    )
    .unwrap();
    handle_observation(
        store,
        &cfg,
        &obs(ObservationFact::ChangeHead {
            change: change.clone(),
            head: head.clone(),
        }),
        NOW,
    )
    .unwrap();
    // The target-branch state a poller records before any review can
    // dispatch: an absent revision is ignorance, never a bound target.
    handle_observation(
        store,
        &cfg,
        &obs(ObservationFact::BranchState {
            repository: cfg.host.repository.clone(),
            branch: target_branch.into(),
            state: BranchState::At {
                revision: fixture_target_revision(),
            },
        }),
        NOW,
    )
    .unwrap();
}

/// The fixture's observed target-branch revision. Dispatch refuses
/// while the branch state is unobserved — an opened change is only a
/// dispatchable target once the poller has pinned where `main` sits.
fn fixture_target_revision() -> CommitRevision {
    CommitRevision::new(repo(), "0".repeat(40)).unwrap()
}

fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn mode(n: u64) -> String {
    format!("issue-{n}")
}

#[test]
fn coding_dispatch_materializes_the_request_and_per_invocation_io() {
    let root = tmp_root("coding");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-coding", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    // The durable body is the launch record: it verifies, parses, and
    // binds this exact request/work/execution/spec.
    let body = request_body(&store, &rid);
    assert!(body.verifies(), "stored body verifies");
    let wire = WorkerRequestV2::parse(&body.canonical_body).unwrap();
    assert_eq!(wire.request_id, rid);
    assert_eq!(wire.role, InvocationRole::Coding);
    assert_eq!(wire.work_id, w.0);
    assert_eq!(wire.execution_id, e.0);
    assert_eq!(wire.spec.revision, 1);
    assert!(matches!(wire.task, TaskBody::Coding { .. }));
    assert!(!wire.workspace.read_only);
    assert!(
        !wire
            .required_capabilities
            .capabilities
            .iter()
            .any(|capability| capability == "workspace_read_only"),
        "the writer grant never requires the read-only capability"
    );

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let io_root = invocation_io_root(&root.join("ws"));
    let io_dir = io_root.join(&invocation.id.0);
    // request.json is the exact durable bytes, read-only; input/ holds
    // the resolved goal body read-only; out/ is the writable dir.
    let request_file = io_dir.join("request.json");
    assert_eq!(
        std::fs::read(&request_file).unwrap(),
        body.canonical_body,
        "request.json is the stored canonical body verbatim"
    );
    assert_eq!(
        std::fs::metadata(&request_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    let goal = io_dir.join("input").join("goal.md");
    assert!(goal.is_file(), "goal body materialized: {goal:?}");
    assert_eq!(
        std::fs::metadata(&goal).unwrap().permissions().mode() & 0o777,
        0o444,
        "input bodies are read-only to the worker"
    );
    assert!(io_dir.join("out").is_dir(), "out dir exists before spawn");

    // The fixture speaks the real v2 transport: it echoes the request
    // binding into a classified report and writes runtime.log into the
    // per-Invocation out dir.
    let report_path = io_dir.join("out").join("report.json");
    wait_for(|| report_path.is_file(), "worker report.json");
    let report = std::fs::read(&report_path).unwrap();
    let classified = classify_report(
        &report,
        &ReportBinding {
            request_id: &rid,
            invocation_id: &invocation.id.0,
            request_digest: &wire.request_digest,
            role: InvocationRole::Coding,
        },
        1 << 20,
    );
    assert_eq!(
        classified.classification,
        ReportClassification::Success,
        "the fixture report verifies against the durable request"
    );
    assert!(io_dir.join("out").join("runtime.log").is_file());

    // Drive full passes until the exited worker is collected: the
    // Invocation resolves and the per-Invocation tree is reclaimed by
    // the pass-end sweep — nothing of the request survives outside
    // the durable record and the delivered artifact.
    for _ in 0..100 {
        sup.run_once(&store, NOW).unwrap();
        if invocation_state(&store, &rid) == Some(InvocationState::Done) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        invocation_state(&store, &rid),
        Some(InvocationState::Done),
        "the clean-exit worker resolves Done"
    );
    // Done is only reachable through the recorded process identity:
    // the launch committed `process_identity`/`descendants` with the
    // spawn, and every exit observation inspected that identity — a
    // v2 launch that lost its identity record could never resolve.
    let resolved = invocation_for(&store, &rid).expect("resolved invocation");
    assert!(
        resolved.process_identity.is_some(),
        "the v2 launch recorded its process identity"
    );
    assert!(
        !io_dir.exists(),
        "the resolved invocation's io tree is reclaimed"
    );
}

#[test]
fn grouped_issues_keep_the_spec_order_and_materialize_in_order() {
    let root = tmp_root("issues");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    // Spec order is the submit order — deliberately not sorted by
    // issue number.
    let (s9, snap9) =
        retained_issue_source(&store, &state_root, 9, "rev-9", "title nine", "body nine");
    let (s3, snap3) =
        retained_issue_source(&store, &state_root, 3, "rev-3", "title three", "body three");
    let (_w, _e, rid) = admit(&store, "v2-issues", vec![s9, s3]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    let wire = WorkerRequestV2::parse(&request_body(&store, &rid).canonical_body).unwrap();
    let TaskBody::Coding { issues, .. } = &wire.task else {
        panic!("expected coding task: {:?}", wire.task)
    };
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].order, 0);
    assert_eq!(issues[0].issue.issue, mode(9));
    assert_eq!(issues[0].source_revision, "rev-9");
    assert_eq!(issues[1].order, 1);
    assert_eq!(issues[1].issue.issue, mode(3));
    assert_eq!(issues[1].source_revision, "rev-3");
    // Each body ref binds the retained snapshot's own wire digest.
    assert_eq!(issues[0].body.digest, snap9.wire_digest());
    assert_eq!(issues[1].body.digest, snap3.wire_digest());

    let invocation = invocation_for(&store, &rid).unwrap();
    let input = invocation_io_root(&root.join("ws"))
        .join(&invocation.id.0)
        .join("input");
    assert_eq!(
        std::fs::read(input.join("issue-0.json")).unwrap(),
        snap9.canonical_bytes()
    );
    assert_eq!(
        std::fs::read(input.join("issue-1.json")).unwrap(),
        snap3.canonical_bytes()
    );
}

#[test]
fn a_missing_issue_snapshot_refuses_dispatch_before_any_process() {
    let root = tmp_root("missing");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    // The spec binds the source but phase 1 never retained its body.
    let (w, e, rid) = admit(
        &store,
        "v2-missing",
        vec![source_snapshot(7, "rev-7", "digest-7")],
    );
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "one typed refusal: {refusals:?}");
    assert_eq!(refusals[0].0, "request.snapshot_missing");
    assert!(
        refusals[0].1.contains("issue-7"),
        "the refusal names the missing issue: {}",
        refusals[0].1
    );
    // No Invocation was ever recorded and no worker ran.
    assert!(invocation_for(&store, &rid).is_none());
    assert!(!pending_ids(&store).contains(&rid));
    let exec = execution(&store, &e);
    assert_eq!(exec.state, ExecutionState::Paused);
    assert!(
        exec.pause_reason
            .as_deref()
            .is_some_and(|r| r.contains("dispatch refused request.snapshot_missing")),
        "pause reason carries the typed code: {:?}",
        exec.pause_reason
    );
    // The request is terminal for its id — a second pass does not
    // rebuild or relaunch it.
    sup.consume_requests(&store, NOW + 1).unwrap();
    assert_eq!(dispatch_refusals(&store, &w).len(), 1);
    assert!(invocation_for(&store, &rid).is_none());
}

#[test]
fn a_snapshot_disagreeing_with_the_spec_is_stale_never_dispatched() {
    let root = tmp_root("stale");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let source = source_snapshot(5, "rev-5", "digest-5");
    // Retain a snapshot whose recorded content digest does not
    // recompute from its title/body — the spec-bound identity resolves
    // the path, but the body fails the freshness check.
    let lying = IssueSnapshotBody::new(
        WireIssue {
            host: "fixture".into(),
            repository: "repo".into(),
            issue: "issue-5".into(),
        },
        "rev-5",
        "title",
        "body",
        "digest-5",
    );
    BodyStore::new(state_root.to_path_buf())
        .retain_at(
            &store,
            CAS_CATEGORY,
            &lying.retained_path(),
            &lying.canonical_bytes(),
            NOW,
            None,
        )
        .unwrap();
    let (w, _e, rid) = admit(&store, "v2-stale", vec![source]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.snapshot_stale");
    assert!(invocation_for(&store, &rid).is_none());
    assert!(!pending_ids(&store).contains(&rid));
}

#[test]
fn a_corrupt_snapshot_file_is_body_corrupt_never_partial_input() {
    let root = tmp_root("corrupt");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (source, snapshot) = retained_issue_source(&store, &state_root, 6, "rev-6", "t", "b");
    // Tamper with the retained bytes behind the catalogue's back: the
    // row still says `present`, the file no longer recomputes.
    let file = state_root.join(snapshot.retained_path());
    std::fs::write(&file, b"tampered").unwrap();
    let (w, _e, rid) = admit(&store, "v2-corrupt", vec![source]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.body_corrupt");
    assert!(invocation_for(&store, &rid).is_none());
    assert!(!pending_ids(&store).contains(&rid));
}

#[test]
fn an_oversized_snapshot_body_refuses_dispatch() {
    let root = tmp_root("oversize");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    // One input body past the 8 MiB per-body bound.
    let huge = "x".repeat(8 * 1024 * 1024 + 1);
    let (source, _snap) = retained_issue_source(&store, &state_root, 8, "rev-8", "big", &huge);
    let (w, _e, rid) = admit(&store, "v2-oversize", vec![source]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.body_oversize");
    assert!(invocation_for(&store, &rid).is_none());
    assert!(!pending_ids(&store).contains(&rid));
}

#[test]
fn fix_dispatch_binds_the_acknowledged_predecessor_and_failed_evidence() {
    let root = tmp_root("fix");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-fix", vec![]);
    // The producing writer finished and its artifact was acknowledged;
    // one failed verification result is the fix's durable evidence.
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let lo = layout(&root);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    failed_verification(&store, &artifact_id, &produced);
    publish_predecessor(&lo, &src, &artifact_id, &produced);
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Fix,
        serde_json::json!({}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let wire = WorkerRequestV2::parse(&request_body(&store, &rid1).canonical_body).unwrap();
    assert_eq!(wire.role, InvocationRole::Fix);
    let TaskBody::Fix {
        predecessor,
        origin,
        ..
    } = &wire.task
    else {
        panic!("expected fix task: {:?}", wire.task)
    };
    assert_eq!(predecessor.artifact_id, artifact_id);
    assert_eq!(predecessor.produced, produced);
    let FixOrigin::Prepublication { failed_evidence } = origin.as_ref() else {
        panic!("expected prepublication origin: {origin:?}")
    };
    assert_eq!(failed_evidence.len(), 1);
    assert_eq!(failed_evidence[0].kind, "verification");
    assert_eq!(wire.base.object, base, "the fix pins the recorded base");

    let invocation = invocation_for(&store, &rid1).expect("fix invocation");
    let input = invocation_io_root(&root.join("ws"))
        .join(&invocation.id.0)
        .join("input");
    assert!(
        input.join("evidence-0.json").is_file(),
        "the failed-evidence body is materialized read-only"
    );
    assert_eq!(
        std::fs::metadata(input.join("evidence-0.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );

    for _ in 0..100 {
        sup.run_once(&store, NOW).unwrap();
        if invocation_state(&store, &rid1) == Some(InvocationState::Done) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        invocation_state(&store, &rid1),
        Some(InvocationState::Done),
        "the fix worker resolves Done"
    );
}

#[test]
fn a_fix_without_durable_evidence_refuses_dispatch() {
    let root = tmp_root("fix-noev");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-fixnoev", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // No failed verification and no judged round: the fix has no
    // durable basis to repair against.
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Fix,
        serde_json::json!({}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.evidence_missing");
    assert!(invocation_for(&store, &rid1).is_none());
    assert!(!pending_ids(&store).contains(&rid1));
}

/// The accepted diff target every review-bound role shares: the
/// opened change plus its latest head, exactly as `current_target`
/// reconstructs it from the recorded observations.
fn current_diff_target(
    change: &ChangeRequestRef,
    head: &CommitRevision,
) -> workloop_core::identity::DiffTarget {
    workloop_core::identity::DiffTarget {
        change: change.clone(),
        head: head.clone(),
        base: None,
        target_revision: Some(fixture_target_revision()),
    }
}

#[test]
fn a_review_origin_fix_binds_the_judged_findings() {
    let root = tmp_root("fix-review");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-fixreview", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), produced.clone()).unwrap();
    open_change_target(&store, &w, &change, &head, "main");
    let digest = "ab".repeat(32);
    judged_round(
        &store,
        &w,
        &current_diff_target(&change, &head),
        &[("find-1", Some(digest.as_str())), ("find-2", None)],
        &[0],
    );
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Fix,
        serde_json::json!({}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let wire = WorkerRequestV2::parse(&request_body(&store, &rid1).canonical_body).unwrap();
    let TaskBody::Fix { origin, .. } = &wire.task else {
        panic!("expected fix task: {:?}", wire.task)
    };
    let FixOrigin::Review {
        target,
        round,
        judgment,
        findings,
    } = origin.as_ref()
    else {
        panic!("expected review origin: {origin:?}")
    };
    assert_eq!(round, "round-1");
    assert_eq!(judgment, "jdg-1");
    assert_eq!(target.change, "cr-1");
    assert_eq!(target.head, produced);
    assert_eq!(findings.len(), 1, "only the Fix-judged occurrence binds");
    assert_eq!(findings[0].occurrence, "occ-1");
    assert_eq!(findings[0].source, "find-1");
    assert_eq!(findings[0].digest, digest);
    assert_eq!(findings[0].order, 0);
    assert_eq!(
        findings[0].summary.as_deref(),
        Some("summary for find-1"),
        "the sealed claim's finding text reaches the Fix worker"
    );
}

#[test]
fn a_fix_whose_selected_finding_lacks_a_content_digest_refuses_dispatch() {
    let root = tmp_root("fix-nodigest");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-fixnodigest", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), produced.clone()).unwrap();
    open_change_target(&store, &w, &change, &head, "main");
    // The review source recorded the finding without a content
    // digest: there is no immutable content identity for the request
    // to bind, so dispatch refuses rather than fabricate one.
    judged_round(
        &store,
        &w,
        &current_diff_target(&change, &head),
        &[("find-1", None)],
        &[0],
    );
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Fix,
        serde_json::json!({}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.finding_missing");
    assert!(invocation_for(&store, &rid1).is_none());
    assert!(!pending_ids(&store).contains(&rid1));
}

#[test]
fn invariant_check_binds_the_approved_set_read_only() {
    let root = tmp_root("ic");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-ic", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // The check runs against a snapshot of the producing workspace —
    // the durable predecessor implies one exists.
    std::fs::create_dir_all(sup.workspace_of(&w)).unwrap();
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::InvariantCheck,
        serde_json::json!({"required_artifacts": []}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let wire = WorkerRequestV2::parse(&request_body(&store, &rid1).canonical_body).unwrap();
    assert_eq!(wire.role, InvocationRole::InvariantCheck);
    assert!(wire.workspace.read_only, "the check grant is read-only");
    assert!(
        wire.required_capabilities
            .capabilities
            .iter()
            .any(|capability| capability == "workspace_read_only"),
        "the check requires the read-only capability so an adapter that lacks it fails negotiation"
    );
    let TaskBody::InvariantCheck {
        artifact,
        invariant_set,
    } = &wire.task
    else {
        panic!("expected invariant_check task: {:?}", wire.task)
    };
    assert_eq!(artifact.artifact_id, artifact_id);
    assert_eq!(invariant_set.id, "invariants-1");
    assert_eq!(invariant_set.revision, 1);

    let invocation = invocation_for(&store, &rid1).expect("check invocation");
    let input = invocation_io_root(&root.join("ws"))
        .join(&invocation.id.0)
        .join("input");
    assert!(input.join("invariant-set.json").is_file());
    for _ in 0..100 {
        sup.run_once(&store, NOW).unwrap();
        if invocation_state(&store, &rid1) == Some(InvocationState::Done) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        invocation_state(&store, &rid1),
        Some(InvocationState::Done),
        "the check worker resolves Done"
    );
}

#[test]
fn an_invariant_check_without_an_approved_set_refuses_dispatch() {
    let root = tmp_root("ic-noset");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-icnoset", vec![]);
    let mut sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // No approved invariant set configured for this deployment.
    sup.v2.as_mut().unwrap().invariant_set = None;
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::InvariantCheck,
        serde_json::json!({"required_artifacts": []}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.invariant_set_missing");
    assert!(invocation_for(&store, &rid1).is_none());
    assert!(!pending_ids(&store).contains(&rid1));
}

#[test]
fn review_dispatch_binds_the_current_target_and_review_source() {
    let root = tmp_root("review");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-review", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // The accepted diff target: opened change + latest head.
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), produced.clone()).unwrap();
    open_change_target(&store, &w, &change, &head, "main");
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let wire = WorkerRequestV2::parse(&request_body(&store, &rid1).canonical_body).unwrap();
    assert_eq!(wire.role, InvocationRole::Review);
    assert!(wire.workspace.read_only, "the review grant is read-only");
    assert!(
        wire.required_capabilities
            .capabilities
            .iter()
            .any(|capability| capability == "workspace_read_only"),
        "the review requires the read-only capability so an adapter that lacks it fails negotiation"
    );
    let TaskBody::CodeReview {
        target,
        review_source,
    } = &wire.task
    else {
        panic!("expected code_review task: {:?}", wire.task)
    };
    assert_eq!(target.change, "cr-1");
    assert_eq!(target.base, base);
    assert_eq!(target.head, produced);
    assert_eq!(review_source.source, "supervisor");
    assert!(review_source.bytes > 0);

    let invocation = invocation_for(&store, &rid1).expect("review invocation");
    let input = invocation_io_root(&root.join("ws"))
        .join(&invocation.id.0)
        .join("input");
    assert!(input.join("review-source.json").is_file());
    for _ in 0..100 {
        sup.run_once(&store, NOW).unwrap();
        if invocation_state(&store, &rid1) == Some(InvocationState::Done) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        invocation_state(&store, &rid1),
        Some(InvocationState::Done),
        "the review worker resolves Done"
    );
}

#[test]
fn a_review_without_a_current_target_is_head_stale_never_dispatched() {
    let root = tmp_root("review-stale");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-reviewstale", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // No opened change, no head: there is no accepted diff target.
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.head_stale");
    assert!(invocation_for(&store, &rid1).is_none());
    assert!(!pending_ids(&store).contains(&rid1));
}

#[test]
fn a_review_whose_target_head_moved_past_the_produced_commit_is_head_stale() {
    let root = tmp_root("review-moved");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-reviewmoved", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    // The accepted target head is the base commit — the produced
    // commit the workspace pins no longer matches the current head.
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), base.clone()).unwrap();
    open_change_target(&store, &w, &change, &head, "main");
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );

    sup.consume_requests(&store, NOW).unwrap();

    let refusals = dispatch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].0, "request.head_stale");
    assert!(invocation_for(&store, &rid1).is_none());
    assert!(!pending_ids(&store).contains(&rid1));
}

#[test]
fn a_restart_replays_the_recorded_invocation_and_stored_body() {
    let root = tmp_root("restart");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-restart", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "hang", true);

    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &e).state == ExecutionState::Running,
        "execution running",
    );
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let body_before = request_body(&store, &rid);
    let io_dir = invocation_io_root(&root.join("ws")).join(&invocation.id.0);
    assert!(io_dir.join("request.json").is_file());
    assert!(io_dir.join("out").is_dir());

    // The restart: a fresh supervisor over the same store and
    // workspace root must reconcile the recorded Invocation — never
    // rebuild the body, never spawn a second worker.
    drop(sup);
    let restarted = supervisor_v2(&root, &state_root, &src, "hang", true);
    restarted.run_once(&store, NOW + 1).unwrap();
    restarted.run_once(&store, NOW + 2).unwrap();

    let invocations: Vec<Invocation> = store
        .transact(|tx| tx.invocations_for_execution(&e))
        .unwrap();
    assert_eq!(invocations.len(), 1, "exactly one Invocation ever");
    assert_eq!(invocations[0].id, invocation.id);
    let body_after = request_body(&store, &rid);
    assert_eq!(
        body_after.canonical_body, body_before.canonical_body,
        "the persisted bytes are replayed verbatim"
    );
    assert_eq!(
        std::fs::read_dir(invocation_io_root(&root.join("ws")))
            .unwrap()
            .count(),
        1,
        "one per-Invocation tree"
    );

    // Stop the fixture so the test does not leak a writer.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-restart-cancel".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    for _ in 0..60 {
        restarted.drive_stops(&store, NOW).unwrap();
        if !execution_invocation(&store, &e, &invocation.id)
            .state
            .is_unresolved_writer()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !execution_invocation(&store, &e, &invocation.id)
            .state
            .is_unresolved_writer(),
        "the writer is stopped"
    );
}

#[test]
fn a_declaration_that_cannot_negotiate_abandons_before_any_worker() {
    let root = tmp_root("negotiation");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-neg", vec![]);
    // No `--wire v2`: `describe` prints the legacy declaration, which
    // is not a `taisan.worker.declaration/1` envelope — the probe
    // answer cannot negotiate the request.
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", false);

    sup.consume_requests(&store, NOW).unwrap();

    // The body and Invocation are still the durable record — the
    // request was consumed — but the attempt is abandoned and the
    // launch refused; no worker process ever existed.
    let body = request_body(&store, &rid);
    assert!(body.verifies());
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("declaration"),
        "the refusal names the declaration: {}",
        refusals[0]
    );
    assert!(
        !pending_ids(&store).contains(&rid),
        "the consumed request is terminal for its binding"
    );
    // The per-Invocation tree a refused launch may have partially
    // written is reclaimed.
    assert!(
        !invocation_io_root(&root.join("ws"))
            .join(&invocation.id.0)
            .exists(),
        "refused launch releases the invocation io tree"
    );
}

#[test]
fn the_invocation_io_sweep_reclaims_orphans_and_keeps_live_trees() {
    let root = tmp_root("sweep");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, _rid) = admit(&store, "v2-sweep", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "hang", true);
    sup.consume_requests(&store, NOW).unwrap();
    wait_for(
        || execution(&store, &e).state == ExecutionState::Running,
        "execution running",
    );
    let live = invocation_for(&store, &_rid).unwrap();

    let io_root = invocation_io_root(&root.join("ws"));
    // An orphan no unresolved Invocation names — the crash-between-
    // materialization-and-commit leftover.
    let orphan = io_root.join("inv-orphan");
    std::fs::create_dir_all(orphan.join("input")).unwrap();
    std::fs::write(orphan.join("request.json"), b"{}").unwrap();
    // A resolved Invocation's tree is equally reclaimable.
    let resolved = io_root.join("inv-resolved");
    std::fs::create_dir_all(&resolved).unwrap();
    store
        .transact(|tx| {
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId("inv-resolved".into()),
                    request_id: format!("{}:inv:9", e.0),
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
    let keep = io_root.join(&live.id.0);
    assert!(keep.is_dir());

    sup.sweep_invocation_io(&store).unwrap();

    assert!(!orphan.exists(), "the orphan tree is reclaimed");
    assert!(
        !resolved.exists(),
        "a resolved invocation's tree is reclaimed"
    );
    assert!(keep.is_dir(), "the live invocation keeps its tree");

    // Stop the fixture so the test does not leak a writer.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-sweep-cancel".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    for _ in 0..60 {
        sup.drive_stops(&store, NOW).unwrap();
        if !execution_invocation(&store, &e, &live.id)
            .state
            .is_unresolved_writer()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_leftover_io_tree_from_a_crashed_attempt_is_rebuilt_never_trusted() {
    let root = tmp_root("crash-io");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, _e, rid) = admit(&store, "v2-crashio", vec![]);
    // The crash-between-materialization-and-launch leftover: an orphan
    // tree under the deterministic per-Invocation name carrying bytes
    // no durable record vouches for. The first allocation in a fresh
    // store is `inv-1`.
    let orphan = invocation_io_root(&root.join("ws")).join("inv-1");
    std::fs::create_dir_all(orphan.join("input")).unwrap();
    std::fs::create_dir_all(orphan.join("out")).unwrap();
    std::fs::write(orphan.join("request.json"), b"{\"forged\":true}").unwrap();
    std::fs::write(orphan.join("input").join("goal.md"), b"forged").unwrap();
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    sup.consume_requests(&store, NOW).unwrap();

    // Materialization rebuilt the tree from durable state: the request
    // file is the stored canonical body verbatim, never the leftover.
    let body = request_body(&store, &rid);
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.id.0, "inv-1");
    let io_dir = invocation_io_root(&root.join("ws")).join("inv-1");
    assert_eq!(
        std::fs::read(io_dir.join("request.json")).unwrap(),
        body.canonical_body,
        "the rebuilt request.json is the durable body, not the leftover"
    );
    // The rebuilt goal.md is the catalogue-resolved body the wire ref
    // names — never the leftover bytes.
    let wire = WorkerRequestV2::parse(&body.canonical_body).unwrap();
    let TaskBody::Coding { goal, .. } = &wire.task else {
        panic!("expected coding task")
    };
    let resolved = BodyStore::new(state_root.to_path_buf())
        .resolve_at(store.body_catalog(), CAS_CATEGORY, &goal.body_ref)
        .unwrap()
        .expect("goal body resolves");
    assert_eq!(
        std::fs::read(io_dir.join("input").join("goal.md")).unwrap(),
        resolved.bytes,
        "the rebuilt goal.md is the catalogue body the wire ref names"
    );
}

#[test]
fn without_the_v2_profile_the_v1_path_leaves_no_request_body() {
    let root = tmp_root("v1compat");
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v1-compat", vec![]);
    let handoff = layout(&root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let sup = fixture_sup(
        PosixProcessControl,
        handoff.workspace_root.clone(),
        vec![fixture_bin(), "clean-exit".into()],
        config(),
    )
    .with_handoff(handoff);
    let _ = src;

    sup.consume_requests(&store, NOW).unwrap();

    // The legacy path consumes through its own export record — no v2
    // body row is ever fabricated for it.
    assert!(
        store
            .transact(|tx| tx.worker_request_body(&rid))
            .unwrap()
            .is_none(),
        "the v1 path writes no v2 request body"
    );
    let _ = w;
}

/// A store wrapper that commits a `Paused` transition in the first
/// transaction after the consume commit's launch-authorization read —
/// the racing stop/cancel the spawn-boundary recheck exists to catch
/// (`docs/v0.3.md` §14.8). The flip lands after the post-consume
/// `Ready` read returns but before the read-role launch path reaches
/// `port.spawn`, so only the recheck immediately before spawn can
/// still observe it.
struct PauseAfterConsumeRead<'a> {
    inner: &'a SqliteStore,
    request_id: String,
    execution_id: ExecutionId,
    invocation_seen: std::cell::Cell<bool>,
    flipped: std::cell::Cell<bool>,
}

impl Store for PauseAfterConsumeRead<'_> {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let out = self.inner.transact(f)?;
        if self.flipped.get() {
            return Ok(out);
        }
        if self.invocation_seen.get() {
            // The consume commit already landed; the transact that
            // just returned is the post-consume authorization read.
            // Commit the racing pause now — every later read,
            // including the spawn-boundary recheck, sees it.
            self.flipped.set(true);
            self.inner.transact(|tx| {
                let exec = tx.execution(&self.execution_id)?.expect("execution");
                let mut next = exec.clone();
                next.state = ExecutionState::Paused;
                next.pause_reason = Some("racing stop".into());
                next.paused_since_unix_seconds = Some(NOW);
                tx.apply(&Effect::UpdateExecution {
                    execution: next,
                    expected_state: ExecutionState::Ready,
                })?;
                Ok(())
            })?;
        } else if self
            .inner
            .transact(|tx| tx.invocation_for_request(&self.request_id))?
            .is_some()
        {
            self.invocation_seen.set(true);
        }
        Ok(out)
    }

    fn body_catalog(&self) -> &dyn BodyCatalog {
        self.inner.body_catalog()
    }
}

/// A stop committed in the window between the post-consume
/// authorization read and the spawn itself — while the read-role
/// launch path is still preparing — must block the launch: the
/// recorded Invocation abandons and no worker process ever exists.
/// The InvariantCheck role is the sharp case: its launch path runs
/// no durable `workspace::prepare` gates, so the spawn-boundary
/// recheck is the only authorization read left before `port.spawn`.
#[test]
fn a_stop_racing_launch_preparation_abandons_before_spawn() {
    let root = tmp_root("racing-stop");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-race", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    std::fs::create_dir_all(sup.workspace_of(&w)).unwrap();
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::InvariantCheck,
        serde_json::json!({"required_artifacts": []}),
    );

    let racing = PauseAfterConsumeRead {
        inner: &store,
        request_id: rid1.clone(),
        execution_id: e.clone(),
        invocation_seen: std::cell::Cell::new(false),
        flipped: std::cell::Cell::new(false),
    };
    sup.consume_requests(&racing, NOW).unwrap();

    assert!(
        racing.flipped.get(),
        "the racing pause committed inside the launch window"
    );
    let invocation = invocation_for(&store, &rid1).expect("check invocation");
    assert_eq!(
        invocation.state,
        InvocationState::Abandoned,
        "the spawn-boundary recheck abandoned the prepared invocation"
    );
    assert!(invocation.process_identity.is_none(), "no process started");
    assert_eq!(
        execution(&store, &e).state,
        ExecutionState::Paused,
        "the racing stop stands — the launch never revived it"
    );
    assert!(
        !pending_ids(&store).contains(&rid1),
        "the consumed request is terminal for its binding"
    );
}

/// The spawn-boundary authorization re-reads the deadline against the
/// injected clock, not the pass-start timestamp — a request whose
/// budget elapsed during launch preparation abandons rather than
/// spawning.
#[test]
fn a_deadline_elapsed_during_launch_preparation_abandons_before_spawn() {
    let root = tmp_root("race-deadline");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-dline", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    std::fs::create_dir_all(sup.workspace_of(&w)).unwrap();
    // The deadline lands inside the launch window: valid against the
    // pass timestamp `build_request_body` checks, elapsed by the time
    // the spawn boundary re-reads the clock.
    store
        .transact(|tx| {
            let mut exec = tx.execution(&e)?.expect("execution");
            exec.budget.runtime_limit_seconds = 60;
            exec.started_unix_seconds = Some(NOW);
            tx.apply(&Effect::UpdateExecution {
                execution: exec,
                expected_state: ExecutionState::Ready,
            })?;
            Ok(())
        })
        .unwrap();
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::InvariantCheck,
        serde_json::json!({"required_artifacts": []}),
    );

    let mut clock = || NOW + 120;
    sup.consume_requests_scoped(&store, NOW, None, &mut clock, None)
        .unwrap();

    let invocation = invocation_for(&store, &rid1).expect("check invocation");
    assert_eq!(
        invocation.state,
        InvocationState::Abandoned,
        "the spawn-boundary deadline recheck abandoned the prepared invocation"
    );
    assert!(invocation.process_identity.is_none(), "no process started");
}

/// The review leg launches inside the shared writer checkout — a
/// refused or abandoned review launch must never release `ws-<work>`,
/// the artifact and recovery source `run_recovery_scoped` and
/// `drive_handoffs` still read. The racing stop lands the same
/// abandonment the refuse paths take; the checkout outlives it.
#[test]
fn an_abandoned_review_launch_never_releases_the_shared_checkout() {
    let root = tmp_root("review-ws-kept");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, "v2-review-race", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), produced.clone()).unwrap();
    open_change_target(&store, &w, &change, &head, "main");
    let rid1 = format!("{}:inv:1", e.0);
    insert_request(
        &store,
        &w,
        &e,
        &rid1,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );
    // The checkout the review would have run in — a surviving marker
    // proves the abandonment released nothing.
    let ws = sup.workspace_of(&w);
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("artifact.json"), b"{}").unwrap();

    // The pause lands after the post-consume `Ready` read: the
    // review's workspace resolves, then the spawn-boundary
    // authorization read sees the stopped Execution.
    let racing = PauseAfterConsumeRead {
        inner: &store,
        request_id: rid1.clone(),
        execution_id: e.clone(),
        invocation_seen: std::cell::Cell::new(false),
        flipped: std::cell::Cell::new(false),
    };
    sup.consume_requests(&racing, NOW).unwrap();

    assert!(
        racing.flipped.get(),
        "the racing pause committed inside the launch window"
    );
    let invocation = invocation_for(&store, &rid1).expect("review invocation");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none(), "no process started");
    assert!(
        ws.join("artifact.json").is_file(),
        "the abandoned review launch never releases the shared checkout"
    );
}

/// A request deadline that elapses while the launch is still being
/// prepared — declaration, materialization, workspace — is caught at
/// the spawn boundary: the recorded Invocation abandons and no
/// worker process starts, and the prepared writer workspace goes
/// with it. The tick deadline is unset so the pass's only clock read
/// is the spawn-boundary recheck itself; `now` built the request
/// body while the deadline still held.
#[test]
fn a_deadline_elapsed_during_preparation_releases_the_writer_workspace() {
    let root = tmp_root("racing-deadline");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, _e, rid) = admit(&store, "v2-deadline", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);

    let mut clock = || i64::MAX;
    sup.consume_requests_scoped(&store, NOW, None, &mut clock, None)
        .unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none(), "no process started");
    assert!(
        !pending_ids(&store).contains(&rid),
        "the consumed request is terminal for its binding"
    );
    assert!(
        !sup.workspace_of(&_w).exists(),
        "the prepared writer workspace is released — its receipt is bound \
         to the abandoned request and would wedge a Coding retry"
    );
    let receipt = sup
        .workspace_of(&_w)
        .parent()
        .expect("the workspace has a control-area parent")
        .join(".prepare")
        .join(format!(
            "{}.receipt.json",
            workloop_exec::isolation::sanitize_key(&_w.0)
        ));
    assert!(
        !receipt.exists(),
        "the receipt goes with the workspace — a surviving receipt still \
         refuses the retry on 'receipt belongs to another request'"
    );
}

/// A v2 supervisor over a caller-chosen worker argv — the
/// declaration-drift and transport-failure cases need argv shapes
/// `supervisor_v2`'s fixed `--wire v2` tail does not produce.
fn supervisor_v2_argv(
    root: &Path,
    state_root: &Path,
    src: &Path,
    argv: Vec<String>,
) -> Supervisor<PosixProcessControl> {
    let handoff = layout(root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    fixture_sup(
        PosixProcessControl,
        handoff.workspace_root.clone(),
        argv,
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(state_root, src))
}

/// A `taisan.worker.declaration/1` envelope with caller-chosen
/// roles/capabilities — the `--decl-file` surface lets a real fixture
/// process declare exactly the drifted surface a test needs.
fn v2_decl(roles: &[&str], capabilities: &[&str]) -> String {
    serde_json::json!({
        "format_version": "taisan.worker.declaration/1",
        "adapter": "workloop-fixture-worker",
        "adapter_version": "1.0.0",
        "runtime": {"product": "fixture-runtime", "version": "fixture-1.0.0"},
        "protocol_versions": [
            "taisan.worker.request/2",
            "taisan.worker.report/2",
        ],
        "roles": roles,
        "capabilities": capabilities,
        "resume": "unsupported",
        "checkpoints": "workspace_commits",
        "runtime_artifact_store": null,
        "local_retries": "runtime_internal",
    })
    .to_string()
}

/// A worker whose `describe` never answers — here a real `/bin/false`
/// child exiting non-zero — is `adapter.declaration_unavailable`: an
/// explicit configuration repair recorded before any launch, never a
/// degraded start.
#[test]
fn an_unanswered_declaration_is_a_configuration_repair() {
    let root = tmp_root("decl-silent");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-silent", vec![]);
    let sup = supervisor_v2_argv(&root, &state_root, &src, vec!["/bin/false".into()]);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("declaration_unavailable"),
        "the refusal names the unanswered declaration: {}",
        refusals[0]
    );
    assert!(!pending_ids(&store).contains(&rid));
}

/// A declaration that answers but does not offer the request's role
/// is `adapter.role_mismatch` — the launch is refused before any
/// worker process exists.
#[test]
fn a_declaration_lacking_the_request_role_refuses_launch() {
    let root = tmp_root("decl-role");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-role", vec![]);
    let decl = root.join("decl.json");
    std::fs::write(
        &decl,
        v2_decl(
            &["review"],
            &["workspace_write", "artifact_json", "process_control"],
        ),
    )
    .unwrap();
    let argv = vec![
        fixture_bin(),
        "clean-exit".into(),
        "--decl-file".into(),
        decl.to_string_lossy().into_owned(),
    ];
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("role_mismatch"),
        "the refusal names the role mismatch: {}",
        refusals[0]
    );
    assert!(!pending_ids(&store).contains(&rid));
}

/// A declaration missing a required capability is
/// `capability.missing` — the exact §5.2 negotiation, never a
/// best-effort launch.
#[test]
fn a_declaration_missing_a_required_capability_refuses_launch() {
    let root = tmp_root("decl-cap");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-cap", vec![]);
    let decl = root.join("decl.json");
    std::fs::write(
        &decl,
        v2_decl(
            &["coding", "fix", "invariant_check", "review"],
            &["workspace_write", "process_control"],
        ),
    )
    .unwrap();
    let argv = vec![
        fixture_bin(),
        "clean-exit".into(),
        "--decl-file".into(),
        decl.to_string_lossy().into_owned(),
    ];
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("capability.missing"),
        "the refusal names the missing capability: {}",
        refusals[0]
    );
    assert!(!pending_ids(&store).contains(&rid));
}

/// A request whose recorded binding no longer matches the freshly
/// declared runtime is a configuration repair — the binding is
/// immutable, the drift is named, and no worker starts under it.
#[test]
fn a_bound_request_whose_runtime_drifts_is_a_configuration_repair() {
    let root = tmp_root("decl-drift");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-drift", vec![]);
    let argv = vec![
        fixture_bin(),
        "clean-exit".into(),
        "--wire".into(),
        "v2".into(),
    ];
    // The binding the request recorded when it bound: the real
    // declaration's capability surface, but a runtime version the
    // worker no longer reports — the upgrade happened after binding.
    let (_declared, surface) = workloop_exec::adapter::observe_worker_declaration_v2(
        &PosixProcessControl,
        &argv,
        &root,
        &workloop_core::ports::WorkerEnv::default(),
    )
    .expect("the fixture declares its v2 surface");
    let mut binding = surface.binding(&argv);
    binding.runtime_version = "fixture-9.9.9".into();
    store
        .transact(|tx| {
            tx.apply(&Effect::BindRequestAdapter {
                request_id: rid.clone(),
                binding: binding.clone(),
            })
        })
        .unwrap();
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("runtime.version_mismatch"),
        "the refusal names the runtime drift: {}",
        refusals[0]
    );
    // The refused request keeps its recorded binding — a later
    // configuration can never retarget it.
    let request = store
        .transact(|tx| tx.request(&rid))
        .unwrap()
        .expect("request row");
    assert_eq!(
        request.adapter_binding.map(|b| b.runtime_version),
        Some("fixture-9.9.9".to_string())
    );
    assert!(!pending_ids(&store).contains(&rid));
}

/// A `taisan.worker.declaration/1` envelope for the scripted port —
/// the same surface the fixture prints under `--wire v2`.
fn scripted_v2_declaration() -> String {
    v2_decl(
        &["coding", "fix", "invariant_check", "review"],
        &["workspace_write", "artifact_json", "process_control"],
    )
}

/// A spawn whose outcome is lost — `SpawnError::Unknown` — leaves the
/// recorded Invocation `Prepared` with no process identity: unknown,
/// never retried. A later pass reconciles the recorded Invocation and
/// re-binds the orphaned writer its launch signature still names; it
/// cannot start a second writer for the consumed request.
#[test]
fn an_unknown_spawn_outcome_stays_unknown_and_never_starts_a_second_writer() {
    use workloop_core::ports::ProbeOutcome;
    use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};

    let root = tmp_root("spawn-unknown");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-unknown", vec![]);
    let _ = w;
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: scripted_v2_declaration().into_bytes(),
        success: true,
    }));
    port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let handoff = layout(&root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let sup = fixture_sup(
        port.clone(),
        handoff.workspace_root.clone(),
        vec!["scripted-worker".into()],
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(&state_root, &src));

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(
        invocation.state,
        InvocationState::Prepared,
        "an unknown spawn outcome is never guessed"
    );
    assert!(invocation.process_identity.is_none());
    assert_eq!(
        port.spawn_calls(),
        1,
        "the unknown outcome consumed exactly one start attempt"
    );

    // Reconciliation finds the orphaned writer by the v2 transport
    // signature and re-binds it through the same durable commit a
    // normal launch uses — the unknown writer is recovered, never
    // replaced.
    sup.consume_requests(&store, NOW + 1).unwrap();
    sup.run_once(&store, NOW + 2).unwrap();
    assert_eq!(
        port.spawn_calls(),
        1,
        "no second writer ever starts for the consumed request"
    );
    let invocations: Vec<Invocation> = store
        .transact(|tx| tx.invocations_for_execution(&e))
        .unwrap();
    assert_eq!(invocations.len(), 1, "exactly one Invocation ever");
    assert_eq!(
        invocations[0].state,
        InvocationState::Running,
        "the orphaned writer is re-bound, not left unidentified"
    );
    assert!(invocations[0].process_identity.is_some());
    assert!(!pending_ids(&store).contains(&rid));
}

/// A lost v2 launch is re-bound by the command line the spawn
/// actually exec'd — `argv run --request <path> --out <path>`, never
/// the bare adapter argv. Searching only the bare argv leaves the
/// orphaned writer unmatchable: never re-bound, and eventually
/// declared absent on directory occupancy alone while still running.
#[test]
fn a_lost_v2_launch_is_rebound_by_its_transport_signature() {
    use workloop_core::ports::ProbeOutcome;
    use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};

    let root = tmp_root("v2-orphan-rebind");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, _e, rid) = admit(&store, "v2-orphan", vec![]);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: scripted_v2_declaration().into_bytes(),
        success: true,
    }));
    port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let handoff = layout(&root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let sup = fixture_sup(
        port.clone(),
        handoff.workspace_root.clone(),
        vec!["scripted-worker".into()],
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(&state_root, &src));

    sup.consume_requests(&store, NOW).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Prepared);
    assert!(invocation.process_identity.is_none());

    // The orphaned writer is still live carrying the v2 transport
    // command line — reconciliation re-binds it through the same
    // durable commit a normal launch uses.
    sup.poll_invocations(&store, NOW + 1).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(
        invocation.state,
        InvocationState::Running,
        "the orphan is re-bound, never replaced"
    );
    assert!(invocation.process_identity.is_some());
    assert_eq!(
        port.spawn_calls(),
        1,
        "re-binding never starts a second writer"
    );
}

/// A lost v2 launch never re-binds a process running the bare
/// adapter argv: the spawn exec'd only the transport-shaped line, so
/// the bare argv is not a signature — a foreign process carrying just
/// those words inside the launch directory stays an unexplained
/// occupant, never adopted, inspected, or signaled as this launch's
/// writer.
#[test]
fn a_lost_v2_launch_never_binds_a_bare_argv_lookalike() {
    use workloop_core::ports::ProbeOutcome;
    use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};

    let root = tmp_root("v2-orphan-lookalike");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, _e, rid) = admit(&store, "v2-lookalike", vec![]);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: scripted_v2_declaration().into_bytes(),
        success: true,
    }));
    port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let handoff = layout(&root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let sup = fixture_sup(
        port.clone(),
        handoff.workspace_root.clone(),
        vec!["scripted-worker".into()],
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(&state_root, &src));

    sup.consume_requests(&store, NOW).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Prepared);
    // The real writer is gone; a foreign process running just the
    // bare adapter argv inside the launch directory is the only live
    // lookalike.
    port.exit_all(0);
    let lost = port.spawned()[0].pid;
    port.adopt_child_with(lost, "scripted-worker");

    sup.poll_invocations(&store, NOW + 1).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert!(
        invocation.process_identity.is_none(),
        "a bare-argv lookalike is never this launch's writer"
    );
    assert_ne!(
        invocation.state,
        InvocationState::Running,
        "the lookalike was never re-bound"
    );
}

// === Worker report evidence collection (`taisan.worker.report/2`,
// docs/v0.3.md §6) ==============================================

/// The `WorkerReport`/`WorkerReportConflict` facts recorded against
/// one Execution, in observation order.
fn report_facts(store: &SqliteStore, execution_id: &ExecutionId) -> Vec<ObservationFact> {
    store
        .transact(|tx| tx.observations_for_execution(execution_id))
        .unwrap()
        .into_iter()
        .map(|o| o.fact)
        .filter(|f| {
            matches!(
                f,
                ObservationFact::WorkerReport { .. } | ObservationFact::WorkerReportConflict { .. }
            )
        })
        .collect()
}

/// Drive passes until the Invocation resolves `Done` — the same loop
/// the materialization test uses.
fn drive_to_done(sup: &Supervisor<PosixProcessControl>, store: &SqliteStore, rid: &str) {
    for _ in 0..100 {
        sup.run_once(store, NOW).unwrap();
        if invocation_state(store, rid) == Some(InvocationState::Done) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("invocation {rid} did not resolve Done");
}

/// Fixture argv for the v2 transport plus report-shaping flags:
/// `[fixture, <mode>, --wire v2, <extra>...]`.
fn v2_argv(mode: &str, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![
        fixture_bin(),
        mode.to_string(),
        "--wire".into(),
        "v2".into(),
    ];
    for flag in extra {
        argv.push((*flag).to_string());
    }
    argv
}

/// Run one v2 dispatch whose fixture worker shapes its report via
/// `--report-mode`, drive it to `Done`, and return the recorded
/// report facts plus the handles a caller needs to resolve evidence.
fn run_report_mode(
    tag: &str,
    mode: &str,
    extra: &[&str],
) -> (
    SqliteStore,
    PathBuf,
    WorkId,
    ExecutionId,
    String,
    Vec<ObservationFact>,
) {
    let root = tmp_root(tag);
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("v2-report-{tag}"), vec![]);
    let sup = supervisor_v2_argv(&root, &state_root, &src, v2_argv(mode, extra));
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    let facts = report_facts(&store, &e);
    (store, state_root, w, e, rid, facts)
}

/// The single accepted `WorkerReport` fact, panicking on anything
/// else — every class test asserts exactly one report observation.
fn the_report_fact(facts: &[ObservationFact]) -> &ObservationFact {
    assert_eq!(facts.len(), 1, "exactly one report fact: {facts:?}");
    match &facts[0] {
        ObservationFact::WorkerReport { .. } => &facts[0],
        other => panic!("expected WorkerReport, got {other:?}"),
    }
}

#[test]
fn a_finished_v2_invocation_records_classified_report_evidence() {
    let (store, state_root, _w, e, rid, facts) = run_report_mode("ok", "clean-exit", &[]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        invocation_id,
        request_id,
        classification,
        report,
        log,
    } = fact
    else {
        unreachable!()
    };
    assert_eq!(request_id, &rid);
    assert_eq!(*classification, ReportClassification::Success);
    assert!(invocation_id.0.starts_with("inv-"));

    // The retained report bytes resolve through the CAS and parse as
    // the exact envelope the worker wrote — echoing this request.
    let report = report.as_ref().expect("report bytes retained");
    assert!(!report.truncated);
    assert!(report.bytes > 0);
    let bodies = BodyStore::new(state_root.to_path_buf());
    let resolved = bodies
        .resolve(store.body_catalog(), &report.body_id)
        .unwrap()
        .expect("retained report body resolves");
    let reparsed: WorkerReportV2 = serde_json::from_slice(&resolved.bytes).unwrap();
    assert_eq!(reparsed.status, WorkerReportStatus::Success);
    assert_eq!(reparsed.request_id, rid);
    assert_eq!(reparsed.invocation_id, invocation_id.0);

    // The measured runtime.log retention is recorded alongside.
    let log = log.as_ref().expect("runtime.log retained");
    assert!(!log.truncated);
    assert!(log.bytes > 0);

    // Report evidence carries no lifecycle authority: it is neither
    // material progress nor a failure class — exit-zero and worker
    // claims cannot move the Execution by themselves.
    assert_eq!(material_progress_basis(fact), None);
    assert_eq!(classify_failure(fact), None);
    let _ = e;
}

#[test]
fn a_negative_report_is_recorded_distinct_from_success() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("neg", "clean-exit", &["--report-mode", "negative"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        ..
    } = fact
    else {
        unreachable!()
    };
    assert_eq!(*classification, ReportClassification::Negative);
    assert!(report.is_some(), "the negative envelope is retained");
}

#[test]
fn an_unavailable_report_is_recorded_distinct_from_negative() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("unavail", "clean-exit", &["--report-mode", "unavailable"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { classification, .. } = fact else {
        unreachable!()
    };
    assert_eq!(*classification, ReportClassification::Unavailable);
}

#[test]
fn a_provider_refusal_is_a_typed_failure_not_a_retry() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("refused", "clean-exit", &["--report-mode", "refused"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { classification, .. } = fact else {
        unreachable!()
    };
    assert_eq!(
        *classification,
        ReportClassification::Failure {
            code: "provider_refused".into()
        }
    );
}

#[test]
fn a_missing_report_is_absent_never_empty_success() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("skip", "clean-exit", &["--report-mode", "skip"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        log,
        ..
    } = fact
    else {
        unreachable!()
    };
    assert_eq!(*classification, ReportClassification::Absent);
    assert!(report.is_none(), "absent carries no report body");
    assert!(log.is_some(), "the runtime.log evidence is still kept");
}

#[test]
fn a_malformed_report_is_invalid_with_its_raw_bytes_retained() {
    let (store, state_root, _w, _e, _rid, facts) =
        run_report_mode("malformed", "clean-exit", &["--report-mode", "malformed"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        ..
    } = fact
    else {
        unreachable!()
    };
    let ReportClassification::Invalid { reasons } = classification else {
        panic!("expected invalid, got {classification:?}")
    };
    assert!(
        reasons.iter().any(|r| r.starts_with("report.malformed")),
        "malformed reason recorded: {reasons:?}"
    );
    let report = report.as_ref().expect("invalid raw bytes retained");
    let bodies = BodyStore::new(state_root.to_path_buf());
    let resolved = bodies
        .resolve(store.body_catalog(), &report.body_id)
        .unwrap()
        .expect("retained invalid body resolves");
    assert_eq!(resolved.bytes, b"{not json");
}

#[test]
fn a_report_for_another_invocation_is_invalid_and_never_adopted() {
    let (_store, _state_root, _w, _e, _rid, facts) = run_report_mode(
        "wrongid",
        "clean-exit",
        &["--report-mode", "wrong_identity"],
    );
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { classification, .. } = fact else {
        unreachable!()
    };
    let ReportClassification::Invalid { reasons } = classification else {
        panic!("expected invalid, got {classification:?}")
    };
    assert!(
        reasons.iter().any(|r| r == "report.wrong_invocation"),
        "wrong_invocation reason recorded: {reasons:?}"
    );
    assert!(
        !reasons.iter().any(|r| r == "report.wrong_request"),
        "the request binding itself matched: {reasons:?}"
    );
}

#[test]
fn an_oversize_report_is_invalid_before_parsing() {
    let (store, state_root, _w, _e, _rid, facts) =
        run_report_mode("oversize", "clean-exit", &["--report-mode", "oversize"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        ..
    } = fact
    else {
        unreachable!()
    };
    let ReportClassification::Invalid { reasons } = classification else {
        panic!("expected invalid, got {classification:?}")
    };
    assert!(
        reasons.iter().any(|r| r == "report.oversize"),
        "oversize reason recorded: {reasons:?}"
    );
    // Only the bounded prefix is retained, marked truncated — never
    // the unbounded tail.
    let report = report.as_ref().expect("bounded prefix retained");
    assert!(report.truncated);
    assert_eq!(report.bytes, 1 << 20, "the dispatch max_report_bytes");
    let bodies = BodyStore::new(state_root.to_path_buf());
    let resolved = bodies
        .resolve(store.body_catalog(), &report.body_id)
        .unwrap()
        .expect("prefix body resolves");
    assert_eq!(resolved.bytes.len() as u64, 1 << 20);
}

#[test]
fn a_symlinked_report_is_unsafe_never_followed() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("symlink", "clean-exit", &["--report-mode", "symlink"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        log,
        ..
    } = fact
    else {
        unreachable!()
    };
    let ReportClassification::Invalid { reasons } = classification else {
        panic!("expected invalid, got {classification:?}")
    };
    assert!(
        reasons.iter().any(|r| r == "report.unsafe_path"),
        "unsafe_path reason recorded: {reasons:?}"
    );
    assert!(report.is_none(), "a symlink is never read as evidence");
    assert!(log.is_some(), "the regular runtime.log is still kept");
}

#[test]
fn an_unreadable_report_is_invalid_and_the_invocation_still_finishes() {
    let (_store, _state_root, _w, _e, _rid, facts) =
        run_report_mode("unreadable", "clean-exit", &["--report-mode", "unreadable"]);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport {
        classification,
        report,
        log,
        ..
    } = fact
    else {
        unreachable!()
    };
    let ReportClassification::Invalid { reasons } = classification else {
        panic!("expected invalid, got {classification:?}")
    };
    assert!(
        reasons.iter().any(|r| r == "report.unreadable"),
        "unreadable reason recorded: {reasons:?}"
    );
    assert!(
        report.is_none(),
        "no bytes retained from an unreadable file"
    );
    assert!(log.is_some(), "the regular runtime.log is still kept");
}

#[test]
fn an_overbound_log_is_retained_truncated_and_marked() {
    let root = tmp_root("logcap");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, e, rid) = admit(&store, "v2-logcap", vec![]);
    let mut dispatch = dispatch_profile(&state_root, &src);
    dispatch.budget.max_log_bytes = 256;
    let handoff = layout(&root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let sup = fixture_sup(
        PosixProcessControl,
        handoff.workspace_root.clone(),
        v2_argv("clean-exit", &["--log-bytes", "4096"]),
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch);
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);

    let facts = report_facts(&store, &e);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { log, .. } = fact else {
        unreachable!()
    };
    let log = log.as_ref().expect("log retained");
    assert!(log.truncated, "the over-bound log is marked truncated");
    assert_eq!(log.bytes, 256, "only the bounded prefix is kept");
}

#[test]
fn a_report_is_not_ingested_before_the_process_tree_proves_stopped() {
    let root = tmp_root("report-gate");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-report-gate", vec![]);
    // `hang` writes report.json/runtime.log and then loops forever —
    // the out dir is complete on disk while the writer still lives.
    let sup = supervisor_v2(&root, &state_root, &src, "hang", true);
    sup.consume_requests(&store, NOW).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let io_dir = invocation_io_root(&root.join("ws"))
        .join(workloop_exec::isolation::sanitize_key(&invocation.id.0));
    wait_for(
        || io_dir.join("out").join("report.json").is_file(),
        "report.json on disk",
    );

    // Poll passes observe the live writer but must not ingest the
    // report — collection begins only after the tree proves stopped.
    for _ in 0..5 {
        sup.poll_invocations(&store, NOW).unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        report_facts(&store, &e).is_empty(),
        "no report evidence while the writer lives"
    );

    // Cancel drives the stop path; the proven exit collects the
    // already-written report exactly once.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-report-gate-cancel".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    for _ in 0..60 {
        sup.drive_stops(&store, NOW).unwrap();
        if !execution_invocation(&store, &e, &invocation.id)
            .state
            .is_unresolved_writer()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let facts = report_facts(&store, &e);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { classification, .. } = fact else {
        unreachable!()
    };
    assert_eq!(
        *classification,
        ReportClassification::Success,
        "the completed report is collected at the proven stop"
    );
}

#[test]
fn duplicate_collection_is_idempotent_and_changed_bytes_are_a_retained_conflict() {
    let root = tmp_root("report-conflict");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-report-conflict", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    sup.consume_requests(&store, NOW).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let request = store
        .transact(|tx| tx.request(&rid))
        .unwrap()
        .expect("request recorded");
    let io_dir = invocation_io_root(&root.join("ws"))
        .join(workloop_exec::isolation::sanitize_key(&invocation.id.0));
    let report_path = io_dir.join("out").join("report.json");
    wait_for(|| report_path.is_file(), "report.json on disk");
    let bodies = BodyStore::new(state_root.to_path_buf());
    let ws_root = root.join("ws");
    let collect = |store: &SqliteStore| {
        workloop_exec::report_v2::collect(
            store,
            Some(&bodies),
            &ws_root,
            &invocation,
            &request,
            NOW,
        )
    };

    // First collection yields the accepted WorkerReport fact; deliver
    // it through the same observation path the supervisor uses.
    let first = collect(&store)
        .unwrap()
        .expect("first collection produces the report fact");
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(e.clone()),
            fact: first,
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();

    // Identical bytes re-collect to nothing — idempotent replay.
    assert!(
        collect(&store).unwrap().is_none(),
        "identical evidence is never re-recorded"
    );

    // Different bytes under the same identity — here the same
    // semantic report with a trailing byte — are a retained conflict;
    // the accepted evidence is never overwritten.
    let mut changed = std::fs::read(&report_path).unwrap();
    changed.push(b' ');
    std::fs::write(&report_path, &changed).unwrap();
    let conflict = collect(&store)
        .unwrap()
        .expect("changed bytes produce a conflict fact");
    let ObservationFact::WorkerReportConflict {
        accepted_report,
        observed_report,
        ..
    } = &conflict
    else {
        panic!("expected WorkerReportConflict, got {conflict:?}")
    };
    assert!(accepted_report.is_some(), "accepted evidence named");
    assert!(observed_report.is_some(), "conflicting evidence named");
    assert_ne!(
        accepted_report, observed_report,
        "the conflict names two different byte identities"
    );
    // The conflicting bytes themselves are retained in the CAS.
    let observed = observed_report.as_ref().unwrap();
    let resolved = bodies
        .resolve(store.body_catalog(), &observed.body_id)
        .unwrap()
        .expect("conflicting body resolves");
    assert_eq!(resolved.bytes, changed);

    // Deliver the conflict through the observation path — it is
    // retained as evidence, never a replacement for the accepted
    // report.
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(w.clone()),
            subject_execution_id: Some(e.clone()),
            fact: conflict.clone(),
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let facts = report_facts(&store, &e);
    let reports = facts
        .iter()
        .filter(|f| matches!(f, ObservationFact::WorkerReport { .. }))
        .count();
    assert_eq!(reports, 1, "one accepted report fact survives");
    assert!(
        facts
            .iter()
            .any(|f| matches!(f, ObservationFact::WorkerReportConflict { .. })),
        "the conflict is retained as evidence"
    );

    // Let the pass finish the Invocation; the automatic collection at
    // the proven stop sees the same conflict and dedupes against it.
    drive_to_done(&sup, &store, &rid);
}

/// An `absent` first report followed by an unreadable report path is
/// not an idempotent replay: both observations carry no report
/// evidence, but the classification changed, and the fact record must
/// retain the conflict rather than silently dropping the second read.
#[test]
fn an_absent_then_unreadable_report_is_a_conflict_not_a_replay() {
    let root = tmp_root("report-class-conflict");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, _e, rid) = admit(&store, "v2-report-class-conflict", vec![]);
    let sup = supervisor_v2(&root, &state_root, &src, "clean-exit", true);
    sup.consume_requests(&store, NOW).unwrap();
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let request = store
        .transact(|tx| tx.request(&rid))
        .unwrap()
        .expect("request recorded");
    let io_dir = invocation_io_root(&root.join("ws"))
        .join(workloop_exec::isolation::sanitize_key(&invocation.id.0));
    let out_dir = io_dir.join("out");
    let report_path = out_dir.join("report.json");
    wait_for(|| report_path.is_file(), "report.json on disk");
    let bodies = BodyStore::new(state_root.join("bodies"));
    let ws_root = root.join("ws");
    let collect = |store: &SqliteStore| {
        workloop_exec::report_v2::collect(
            store,
            Some(&bodies),
            &ws_root,
            &invocation,
            &request,
            NOW,
        )
    };

    // Remove the report before the first collection: the accepted
    // fact is `absent` with no retained evidence.
    std::fs::remove_file(&report_path).unwrap();
    let first = collect(&store)
        .unwrap()
        .expect("first collection produces the absent report fact");
    let ObservationFact::WorkerReport {
        classification,
        report,
        ..
    } = &first
    else {
        panic!("expected WorkerReport, got {first:?}")
    };
    assert_eq!(
        *classification,
        ReportClassification::Absent,
        "a missing report file classifies absent"
    );
    assert!(report.is_none(), "absent carries no report evidence");
    handle_observation(
        &store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: Some(_w.clone()),
            subject_execution_id: Some(_e.clone()),
            fact: first,
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();

    // Replace the out directory with a link: the second read is
    // `report.unsafe_path` — also evidence-free, but a different
    // class. Equal evidence under a different class is a conflict.
    std::fs::remove_dir_all(&out_dir).unwrap();
    let elsewhere = root.join("elsewhere-out");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &out_dir).unwrap();
    let conflict = collect(&store)
        .unwrap()
        .expect("a class change with equal evidence is a conflict fact");
    let ObservationFact::WorkerReportConflict {
        accepted_report,
        observed_report,
        ..
    } = &conflict
    else {
        panic!("expected WorkerReportConflict, got {conflict:?}")
    };
    assert!(accepted_report.is_none(), "the accepted class was absent");
    assert!(
        observed_report.is_none(),
        "an unsafe path retains no report evidence"
    );
}

// === Produced-commit handoff and independent verification
// (docs/v0.3.md §6.3) ==============================================

/// The managed workspace directory for one Work — where the worker's
/// commits live before handoff exports them.
fn managed_workspace(root: &Path, work_id: &WorkId) -> PathBuf {
    root.join("ws").join(format!("ws-{}", work_id.0))
}

/// The durable handoff row for `<invocation>/artifact.json`.
fn handoff_row(store: &SqliteStore, invocation_id: &InvocationId) -> ArtifactHandoff {
    store
        .transact(|tx| tx.artifact_handoff(&format!("{}/artifact.json", invocation_id.0)))
        .unwrap()
        .expect("handoff row recorded")
}

/// The `ClaimedHeadCheck` facts recorded against one Execution.
fn claim_facts(store: &SqliteStore, execution_id: &ExecutionId) -> Vec<ObservationFact> {
    store
        .transact(|tx| tx.observations_for_execution(execution_id))
        .unwrap()
        .into_iter()
        .map(|o| o.fact)
        .filter(|f| matches!(f, ObservationFact::ClaimedHeadCheck { .. }))
        .collect()
}

/// Drive only the poll phase until the Invocation resolves `Done` —
/// no handoff pass runs, so a test can mutate the workspace in the
/// gap between stop-proof and export exactly like a crashed or
/// adversarial interleaving would.
fn poll_to_done(sup: &Supervisor<PosixProcessControl>, store: &SqliteStore, rid: &str) {
    for _ in 0..100 {
        sup.poll_invocations(store, NOW).unwrap();
        if invocation_state(store, rid) == Some(InvocationState::Done) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("invocation {rid} did not resolve Done");
}

/// A v2 Coding dispatch whose fixture commits its artifact on the
/// managed branch hands the produced commit through the durable
/// export → quarantine-import → acknowledge path: the acknowledged
/// ref resolves exactly that commit, the recorded base is the
/// request's pinned fork point (never a workspace ref), and the
/// manifest binds the exact `base..produced` object set.
#[test]
fn a_v2_coding_invocation_hands_off_the_produced_commit() {
    let root = tmp_root("handoff-ok");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-handoff-ok", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let produced = row.produced.clone().expect("produced recorded");
    assert!(row.manifest_digest.is_some(), "manifest bound");

    // The produced commit is exactly the managed branch's head — the
    // commit the worker made — and it descends from the pinned base.
    let ws = managed_workspace(&root, &w);
    let head = git(&ws, &["rev-parse", &format!("work/{}", w.0)]);
    assert_eq!(produced, head);
    assert_eq!(row.base, newer, "the base is the request's pin");
    git(&ws, &["merge-base", "--is-ancestor", &newer, &produced]);

    // Promote once: the acknowledged ref in the installation handoff
    // repository resolves the produced commit, the exact change
    // objects are present, and the row is terminal — a repeated
    // drive is a no-op.
    let lay = layout(&root);
    assert_eq!(
        git(
            &lay.repository,
            &["rev-parse", "--verify", &artifact_ref(&row.artifact_id)]
        ),
        produced
    );
    let changes = git(
        &lay.repository,
        &["rev-list", "--objects", &format!("{newer}..{produced}")],
    );
    assert!(
        changes.lines().any(|l| l.starts_with(&produced)),
        "the produced commit is in the promoted object set: {changes}"
    );
    assert!(workloop_exec::handoff::reclaimable(&lay, &row));
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();
    let again = handoff_row(&store, &inv.id);
    assert_eq!(again.produced, Some(produced));
    assert_eq!(again.state, HandoffState::Acknowledged);
}

/// A `claimed_head` naming the commit the handoff promoted is
/// recorded as an agreeing verdict — evidence that survives beside
/// the artifact, changing nothing about it.
#[test]
fn a_claimed_head_matching_the_produced_commit_records_agreement() {
    let root = tmp_root("claim-agree");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, e, rid) = admit(&store, "v2-claim-agree", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit", "--claim-head", "HEAD"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let facts = claim_facts(&store, &e);
    assert_eq!(facts.len(), 1, "one verdict fact: {facts:?}");
    let ObservationFact::ClaimedHeadCheck {
        claimed,
        produced,
        agrees,
        ..
    } = &facts[0]
    else {
        unreachable!()
    };
    assert!(agrees, "the claim named the promoted commit");
    assert_eq!(claimed, produced);
    // The verdict carries no authority either way: material progress
    // and failure classification stay empty for it.
    assert_eq!(material_progress_basis(&facts[0]), None);
    assert_eq!(classify_failure(&facts[0]), None);
}

/// A `claimed_head` naming a different commit — here the produced
/// commit's own parent — is retained as a disagreeing verdict and
/// never adopted: the handoff still promotes the real produced
/// commit and the ref resolves it, not the claim.
#[test]
fn a_claimed_head_disagreeing_with_produced_is_retained_never_adopted() {
    let root = tmp_root("claim-differ");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-claim-differ", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit", "--claim-head", "HEAD^"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let produced = row.produced.clone().unwrap();

    let facts = claim_facts(&store, &e);
    assert_eq!(facts.len(), 1, "one verdict fact: {facts:?}");
    let ObservationFact::ClaimedHeadCheck {
        claimed,
        produced: checked,
        agrees,
        artifact_id,
        ..
    } = &facts[0]
    else {
        unreachable!()
    };
    assert!(
        !agrees,
        "the claim named the parent, not the produced commit"
    );
    assert_eq!(claimed, &newer, "HEAD^ resolved to the base commit");
    assert_eq!(checked, &produced);
    assert_eq!(artifact_id, &row.artifact_id);

    // The disagreement moved nothing: the acknowledged ref still
    // resolves the real produced commit.
    let lay = layout(&root);
    assert_eq!(
        git(
            &lay.repository,
            &["rev-parse", "--verify", &artifact_ref(&row.artifact_id)]
        ),
        produced
    );
    let _ = w;
}

/// A report carrying no `claimed_head` records no verdict — but the
/// acknowledgement itself is still observed as `HandoffAcknowledged`,
/// so the waiting Execution's publish chain is re-driven instead of
/// sitting until the stage budget expires.
#[test]
fn a_report_without_claimed_head_still_records_the_acknowledgement() {
    let root = tmp_root("claim-none");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (_w, e, rid) = admit(&store, "v2-claim-none", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs(&store).unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");

    let facts = claim_facts(&store, &e);
    assert_eq!(facts.len(), 0, "no claim means no verdict: {facts:?}");
    let acks: Vec<_> = store
        .transact(|tx| tx.observations_for_execution(&e))
        .unwrap()
        .into_iter()
        .map(|o| o.fact)
        .filter(|f| matches!(f, ObservationFact::HandoffAcknowledged { .. }))
        .collect();
    assert_eq!(acks.len(), 1, "one acknowledgement fact: {acks:?}");
    let ObservationFact::HandoffAcknowledged {
        artifact_id,
        produced,
    } = &acks[0]
    else {
        unreachable!()
    };
    assert_eq!(artifact_id, &row.artifact_id);
    assert_eq!(produced, &row.produced.clone().unwrap());
}

/// A produced commit on foreign root history — with a worker-planted
/// `aaa-decoy` branch that a *derived* base would prefer over
/// `taisan-base` — is `stale_base` invalid: the pinned request base
/// bounds ancestry, never a ref the Worker controls.
#[test]
fn a_produced_commit_not_descending_from_the_pinned_base_is_invalid() {
    let root = tmp_root("handoff-stale");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-handoff-stale", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    poll_to_done(&sup, &store, &rid);

    // After the proven stop but before any handoff pass, rewrite the
    // managed branch onto foreign root history and plant a decoy ref
    // sorting before `taisan-base` — exactly what `derive_base`
    // would have bounded against.
    let ws = managed_workspace(&root, &w);
    let tree = git(&ws, &["rev-parse", "HEAD^{tree}"]);
    let foreign_root = git(
        &ws,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit-tree",
            &tree,
            "-m",
            "foreign root",
        ],
    );
    let foreign_work = git(
        &ws,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit-tree",
            &tree,
            "-p",
            &foreign_root,
            "-m",
            "foreign work",
        ],
    );
    git(&ws, &["branch", "aaa-decoy", &foreign_root]);
    git(
        &ws,
        &[
            "update-ref",
            &format!("refs/heads/work/{}", w.0),
            &foreign_work,
        ],
    );

    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();
    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Invalid, "{row:?}");
    assert_eq!(row.invalid_reason.as_deref(), Some("stale_base"));
    assert_eq!(row.base, newer, "the recorded base stayed the pin");
    assert!(row.produced.is_none(), "nothing was promoted");
}

/// A produced revision that cannot be resolved at export time — the
/// managed branch is gone by the time the handoff pass runs — is a
/// transport loss charged to the recovery bound, never a promoted
/// artifact: the row settles `recovery_exhausted` with no produced
/// commit and no ref ever written into the installation handoff
/// repository.
#[test]
fn an_unresolvable_produced_commit_fails_closed_never_promotes() {
    let root = tmp_root("handoff-missing");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-handoff-missing", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    poll_to_done(&sup, &store, &rid);

    // The writer is proven stopped; before any handoff pass the
    // produced revision disappears — `rev-parse --verify` can no
    // longer resolve it.
    let ws = managed_workspace(&root, &w);
    git(
        &ws,
        &["update-ref", "-d", &format!("refs/heads/work/{}", w.0)],
    );

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    for _ in 0..8 {
        sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
            .unwrap();
        if handoff_row(&store, &inv.id).state == HandoffState::Invalid {
            break;
        }
    }
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Invalid, "{row:?}");
    assert_eq!(row.invalid_reason.as_deref(), Some("recovery_exhausted"));
    assert!(row.produced.is_none(), "nothing was promoted");

    // No artifact ref was ever written into the installation
    // repository.
    let lay = layout(&root);
    let refs = git(&lay.repository, &["for-each-ref", "refs/taisan/artifacts"]);
    assert!(refs.is_empty(), "no artifact ref: {refs}");
}

/// A handoff row interrupted at `export_started` — the durable state
/// a crash between row creation and bundle write leaves — resumes on
/// the next pass from its committed stage and identity: one row, one
/// produced commit, acknowledged once.
#[test]
fn an_interrupted_v2_handoff_resumes_from_its_durable_stage() {
    let root = tmp_root("handoff-resume");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-handoff-resume", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    poll_to_done(&sup, &store, &rid);

    // Crash residue: the row exists at its first durable stage with
    // the pinned base already recorded — no bundle, no produced.
    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let aid = format!("{}/artifact.json", inv.id.0);
    let mut residue = ArtifactHandoff {
        artifact_id: aid.clone(),
        invocation_id: inv.id.clone(),
        request_id: inv.request_id.clone(),
        work_id: w.clone(),
        execution_id: e.clone(),
        base: newer.clone(),
        produced: None,
        manifest_digest: None,
        state: HandoffState::ExportStarted,
        invalid_reason: None,
        attempt: 0,
        recorded_seq: 0,
    };
    store
        .transact(|tx| {
            residue.recorded_seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: residue.clone(),
                expected_seq: 0,
            })
        })
        .unwrap();

    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let ws = managed_workspace(&root, &w);
    assert_eq!(
        row.produced.as_deref(),
        Some(git(&ws, &["rev-parse", &format!("work/{}", w.0)]).as_str())
    );
}

/// Mutating the managed branch after acknowledgement cannot move the
/// artifact: the ref still resolves the produced commit, the row is
/// unchanged, and the workspace stays reclaimable — post-run writes
/// are WIP evidence at most, never artifact motion.
#[test]
fn a_post_acknowledgement_workspace_mutation_cannot_move_the_artifact() {
    let root = tmp_root("handoff-postmut");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, _e, rid) = admit(&store, "v2-handoff-postmut", vec![]);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let produced = row.produced.clone().unwrap();

    // A later commit lands on the managed branch — the artifact does
    // not follow it.
    let ws = managed_workspace(&root, &w);
    git(
        &ws,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "post-ack mutation",
        ],
    );
    let moved = git(&ws, &["rev-parse", &format!("work/{}", w.0)]);
    assert_ne!(moved, produced, "the branch really moved");

    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();
    let again = handoff_row(&store, &inv.id);
    assert_eq!(again.state, HandoffState::Acknowledged);
    assert_eq!(again.produced.as_deref(), Some(produced.as_str()));
    let lay = layout(&root);
    assert_eq!(
        git(
            &lay.repository,
            &["rev-parse", "--verify", &artifact_ref(&row.artifact_id)]
        ),
        produced
    );
    assert!(workloop_exec::handoff::reclaimable(&lay, &again));
}

/// Declared verification argv runs on a clean detached worktree of
/// the exact produced commit in the installation handoff repository:
/// exit-0 with empty output is a legitimate Pass, non-zero is Fail,
/// a >64KiB writer proves the v2 8 MiB output bound (the recorded v1
/// 64 KiB cap would have truncated it), and a command outliving its
/// bound is Inconclusive — never a pass.
#[test]
fn v2_verification_runs_on_the_produced_commit_with_bounded_output() {
    let root = tmp_root("verify-v2");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let argv: Vec<Vec<String>> = [
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "git rev-parse HEAD".to_string(),
        ],
        vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
        vec!["sh".to_string(), "-c".to_string(), "false".to_string()],
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "head -c 200000 /dev/zero | tr '\\0' 'x'; echo tail-marker".to_string(),
        ],
    ]
    .to_vec();
    let (_w, _e, rid) = admit_ex(&store, "v2-verify", vec![], argv.clone());
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let produced = row.produced.clone().unwrap();
    let results = store
        .transact(|tx| tx.verification_results(&row.artifact_id))
        .unwrap();
    assert_eq!(
        results.len(),
        4,
        "every declared argv recorded: {results:?}"
    );
    let by_req: std::collections::HashMap<_, _> =
        results.iter().map(|r| (r.requirement.clone(), r)).collect();

    // The check ran inside a detached worktree at the exact produced
    // commit — `git rev-parse HEAD` there names it.
    let head_check = by_req["verification_argv[0]"];
    assert_eq!(head_check.result, VerificationOutcome::Pass);
    assert_eq!(head_check.produced, produced);
    assert_eq!(head_check.output.trim(), produced);
    assert_eq!(head_check.argv, argv[0]);

    // Empty legitimate output is a Pass, recorded verbatim.
    let empty = by_req["verification_argv[1]"];
    assert_eq!(empty.result, VerificationOutcome::Pass);
    assert!(empty.output.is_empty(), "empty output stays empty");

    // A failing check is Fail — evidence, not a retry.
    let failed = by_req["verification_argv[2]"];
    assert_eq!(failed.result, VerificationOutcome::Fail);

    // ~200 KiB of output exceeds the recorded v1 64 KiB cap: the v2
    // 8 MiB bound retained the whole tail.
    let loud = by_req["verification_argv[3]"];
    assert_eq!(loud.result, VerificationOutcome::Pass);
    assert!(
        loud.output.len() > 64 * 1024,
        "v2 bound retained past the v1 cap: {}",
        loud.output.len()
    );
    assert!(loud.output.len() <= 8 * 1024 * 1024);
    assert!(loud.output.contains("tail-marker"));

    // A command outliving its bound is Inconclusive — the injected
    // short timeout stands in for the profile's; timeout can never
    // pass.
    let lay = layout(&root);
    let scratch = root.join("verify-scratch");
    let timed_out = workloop_exec::handoff::verify_with_timeout(
        &store,
        &lay,
        &row.artifact_id,
        "verification_argv[9]",
        &["sh".into(), "-c".into(), "sleep 30".into()],
        &scratch,
        Duration::from_millis(300),
        64 * 1024,
        &[],
    )
    .unwrap();
    assert_eq!(timed_out.result, VerificationOutcome::Inconclusive);
    assert!(timed_out.output.contains("exceeded"));
}

// -------------------------------------------------------------------
// Issue #264 — v2 assessment binding: IC/Review reports → gate facts
// -------------------------------------------------------------------

/// Shared rig for assessment-role tests: an admitted Work whose first
/// request is already consumed by a synthetic acknowledged handoff
/// (`base`→`produced`), the published artifact ref, and — when asked —
/// the accepted diff target (`open_change_target`) a bound fact needs.
struct AssessRig {
    store: SqliteStore,
    sup: Supervisor<PosixProcessControl>,
    work_id: WorkId,
    execution_id: ExecutionId,
    base: String,
    produced: String,
    change: ChangeRequestRef,
    head: CommitRevision,
}

fn assess_rig(tag: &str, argv: Vec<String>, with_target: bool) -> AssessRig {
    assess_rig_cfg(tag, argv, with_target, &config())
}

fn assess_rig_cfg(
    tag: &str,
    argv: Vec<String>,
    with_target: bool,
    cfg: &WorkloopConfig,
) -> AssessRig {
    let root = tmp_root(tag);
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit_ex_cfg(&store, &format!("v2-assess-{tag}"), vec![], vec![], cfg);
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv);
    let artifact_id = done_writer_with_handoff(&store, &w, &e, &rid0, &base, &produced);
    publish_predecessor(&layout(&root), &src, &artifact_id, &produced);
    let change = ChangeRequestRef {
        repository: repo(),
        change_request: ChangeRequestId::trusted("cr-1"),
        display_number: Some(1),
    };
    let head = CommitRevision::new(repo(), produced.clone()).unwrap();
    if with_target {
        open_change_target(&store, &w, &change, &head, "main");
    }
    AssessRig {
        store,
        sup,
        work_id: w,
        execution_id: e,
        base,
        produced,
        change,
        head,
    }
}

/// Dispatch one assessment-role request through the v2 transport and
/// drive its worker to `Done`. InvariantCheck snapshots the producing
/// workspace, so the durable predecessor's tree must exist first —
/// exactly what a real handoff leaves behind.
fn run_assessment_role(rig: &AssessRig, seq: u64, role: InvocationRole) -> String {
    if role == InvocationRole::InvariantCheck {
        std::fs::create_dir_all(rig.sup.workspace_of(&rig.work_id)).unwrap();
    }
    let rid = format!("{}:inv:{seq}", rig.execution_id.0);
    insert_request(
        &rig.store,
        &rig.work_id,
        &rig.execution_id,
        &rid,
        role,
        serde_json::json!({"required_artifacts": []}),
    );
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    drive_to_done(&rig.sup, &rig.store, &rid);
    rid
}

/// `(complete, [(item id, verdict)])` for every `Assessment` fact the
/// Work carries, in observation order.
fn assessment_facts(
    store: &SqliteStore,
    work_id: &WorkId,
) -> Vec<(bool, Vec<(String, AssessmentVerdict)>)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::Assessment {
                complete, items, ..
            } => Some((
                complete,
                items
                    .into_iter()
                    .map(|i| (i.id, i.verdict))
                    .collect::<Vec<_>>(),
            )),
            _ => None,
        })
        .collect()
}

/// `(completeness, occurrence count)` for every `Review` fact.
fn review_facts(store: &SqliteStore, work_id: &WorkId) -> Vec<(Completeness, usize)> {
    store
        .transact(|tx| tx.observations_for_work(work_id))
        .unwrap()
        .into_iter()
        .filter_map(|o| match o.fact {
            ObservationFact::Review {
                completeness,
                occurrences,
                ..
            } => Some((completeness, occurrences.len())),
            _ => None,
        })
        .collect()
}

fn sealed_rounds(store: &SqliteStore, work_id: &WorkId) -> Vec<ReviewRound> {
    store.transact(|tx| tx.rounds_for_work(work_id)).unwrap()
}

/// The v2 argv for an InvariantCheck whose report claims `items` as
/// its `invariant_items` members.
fn ic_argv(items_json: &str) -> Vec<String> {
    v2_argv("clean-exit", &["--invariant-items", items_json])
}

/// Both approved set ids (`no-todo`, `tests-pass`) conforming.
const IC_CONFORMS: &str =
    r#"[{"id":"no-todo","verdict":"conforms"},{"id":"tests-pass","verdict":"conforms"}]"#;

#[test]
fn an_invariant_check_binds_assessment_only_once_its_artifact_is_the_current_head() {
    // No accepted target yet: the check ran pre-publication, so its
    // evidence cannot bind — and must not be lost either.
    let rig = assess_rig("ic-bind", ic_argv(IC_CONFORMS), false);
    run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
    assert!(
        assessment_facts(&rig.store, &rig.work_id).is_empty(),
        "no target, no binding"
    );

    // Publication makes the produced commit the accepted head; the
    // next pass binds the retained report to it — deferred, never
    // dropped, never duplicated.
    open_change_target(&rig.store, &rig.work_id, &rig.change, &rig.head, "main");
    rig.sup.run_once(&rig.store, NOW).unwrap();
    let facts = assessment_facts(&rig.store, &rig.work_id);
    assert_eq!(facts.len(), 1, "{facts:?}");
    let (complete, items) = &facts[0];
    assert!(*complete, "exact coverage is complete: {items:?}");
    assert_eq!(
        items.as_slice(),
        [
            ("no-todo".to_string(), AssessmentVerdict::Positive),
            ("tests-pass".to_string(), AssessmentVerdict::Positive),
        ],
        "items bind in the approved set's declared order"
    );
    // The fact binds the exact accepted target and the approved set's
    // wire digest.
    let bound = rig
        .store
        .transact(|tx| tx.observations_for_work(&rig.work_id))
        .unwrap()
        .into_iter()
        .find_map(|o| match o.fact {
            ObservationFact::Assessment {
                target, set_digest, ..
            } => Some((target, set_digest)),
            _ => None,
        })
        .unwrap();
    assert_eq!(bound.0.head.object, rig.produced);
    let set = InvariantSetBody::new(
        "invariants-1",
        1,
        vec!["no-todo".into(), "tests-pass".into()],
    );
    assert_eq!(bound.1, set.wire_digest());

    // Replays dedupe: another pass records no second assessment.
    rig.sup.run_once(&rig.store, NOW).unwrap();
    assert_eq!(assessment_facts(&rig.store, &rig.work_id).len(), 1);
}

#[test]
fn an_invariant_check_with_incomplete_coverage_records_complete_false() {
    // Missing member: `tests-pass` is never claimed.
    let rig = assess_rig(
        "ic-missing",
        ic_argv(r#"[{"id":"no-todo","verdict":"conforms"}]"#),
        true,
    );
    run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
    let facts = assessment_facts(&rig.store, &rig.work_id);
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert!(!facts[0].0, "missing coverage is never complete");
    assert_eq!(
        facts[0].1.as_slice(),
        [("no-todo".to_string(), AssessmentVerdict::Positive)]
    );
}

#[test]
fn an_invariant_check_with_unknown_or_duplicate_members_records_incomplete() {
    for (tag, items) in [
        (
            "ic-unknown",
            r#"[{"id":"no-todo","verdict":"conforms"},{"id":"tests-pass","verdict":"conforms"},{"id":"bogus","verdict":"conforms"}]"#,
        ),
        (
            "ic-dup",
            r#"[{"id":"no-todo","verdict":"conforms"},{"id":"no-todo","verdict":"violates"}]"#,
        ),
        // A member outside the closed grammar is malformed evidence.
        (
            "ic-malformed",
            r#"[{"id":"no-todo","verdict":"maybe"},{"id":"tests-pass","verdict":"conforms"}]"#,
        ),
    ] {
        let rig = assess_rig(tag, ic_argv(items), true);
        run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
        let facts = assessment_facts(&rig.store, &rig.work_id);
        assert_eq!(facts.len(), 1, "{tag}: {facts:?}");
        assert!(
            !facts[0].0,
            "{tag}: unverifiable coverage is never complete"
        );
    }
}

#[test]
fn invariant_item_verdicts_map_to_assessment_verdicts() {
    let rig = assess_rig(
        "ic-verdicts",
        ic_argv(
            r#"[{"id":"no-todo","verdict":"violates"},{"id":"tests-pass","verdict":"inconclusive"}]"#,
        ),
        true,
    );
    run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
    let facts = assessment_facts(&rig.store, &rig.work_id);
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert!(facts[0].0, "coverage is exact — the verdicts are the claim");
    assert_eq!(
        facts[0].1.as_slice(),
        [
            ("no-todo".to_string(), AssessmentVerdict::Negative),
            ("tests-pass".to_string(), AssessmentVerdict::Inconclusive),
        ]
    );
}

#[test]
fn an_invariant_check_on_a_moved_head_never_binds() {
    let rig = assess_rig("ic-stale", ic_argv(IC_CONFORMS), true);
    // Split dispatch from completion: the accepted head moves to a
    // different commit while the check worker runs — the check covered
    // `produced`, not this head, so its evidence binds nothing.
    std::fs::create_dir_all(rig.sup.workspace_of(&rig.work_id)).unwrap();
    let rid = format!("{}:inv:1", rig.execution_id.0);
    insert_request(
        &rig.store,
        &rig.work_id,
        &rig.execution_id,
        &rid,
        InvocationRole::InvariantCheck,
        serde_json::json!({"required_artifacts": []}),
    );
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    let moved = CommitRevision::new(repo(), rig.base.clone()).unwrap();
    open_change_target(&rig.store, &rig.work_id, &rig.change, &moved, "main");
    drive_to_done(&rig.sup, &rig.store, &rid);
    for _ in 0..3 {
        rig.sup.run_once(&rig.store, NOW).unwrap();
    }
    assert!(
        assessment_facts(&rig.store, &rig.work_id).is_empty(),
        "a stale artifact's assessment never binds the moved head"
    );
}

#[test]
fn invalid_and_absent_reports_emit_no_assessment_facts() {
    for (tag, mode) in [("ic-invalid", "malformed"), ("ic-absent", "skip")] {
        let argv = v2_argv(
            "clean-exit",
            &["--report-mode", mode, "--invariant-items", IC_CONFORMS],
        );
        let rig = assess_rig(tag, argv, true);
        run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
        for _ in 0..3 {
            rig.sup.run_once(&rig.store, NOW).unwrap();
        }
        assert!(
            assessment_facts(&rig.store, &rig.work_id).is_empty(),
            "{tag}: {mode} classification emits no gate fact"
        );
    }
}

#[test]
fn a_code_review_with_findings_seals_occurrences_for_judgment() {
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "findings",
            "--findings",
            r#"[{"key":"f-1","summary":"off by one in the bound check","severity":"high","position":"handoff.rs:12"}]"#,
        ],
    );
    let rig = assess_rig("review-findings", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);

    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0].0, Completeness::Complete);
    assert_eq!(reviews[0].1, 1);

    // The fact sealed an immutable round whose occurrence carries the
    // supervisor-derived content digest a judgment/Fix origin binds.
    let rounds = sealed_rounds(&rig.store, &rig.work_id);
    assert_eq!(rounds.len(), 1, "{rounds:?}");
    let round = &rounds[0];
    assert_eq!(round.completeness, "complete");
    assert_eq!(round.target.head.object, rig.produced);
    let occurrences = rig.store.transact(|tx| tx.occurrences(&round.id)).unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].source_key, "f-1");
    assert_eq!(occurrences[0].severity.as_deref(), Some("high"));
    assert_eq!(
        occurrences[0].summary.as_deref(),
        Some("off by one in the bound check")
    );
    let digest = occurrences[0]
        .content_digest
        .clone()
        .expect("worker findings carry a content digest");
    assert_eq!(digest.len(), 64);
}

#[test]
fn a_clean_code_review_seals_a_zero_finding_round() {
    let rig = assess_rig("review-clean", v2_argv("clean-exit", &[]), true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0], (Completeness::Complete, 0));
    let rounds = sealed_rounds(&rig.store, &rig.work_id);
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].completeness, "complete");
    assert!(
        rig.store
            .transact(|tx| tx.occurrences(&rounds[0].id))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_review_with_malformed_findings_records_partial_and_never_seals() {
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "findings",
            // `summary` missing — outside the closed member grammar.
            "--findings",
            r#"[{"key":"f-1"},{"key":"f-2","summary":"ok"}]"#,
        ],
    );
    let rig = assess_rig("review-partial", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert!(
        matches!(reviews[0].0, Completeness::Partial { .. }),
        "malformed members degrade to partial: {:?}",
        reviews[0].0
    );
    assert!(
        sealed_rounds(&rig.store, &rig.work_id).is_empty(),
        "a partial review never seals a round"
    );
}

#[test]
fn a_review_reported_unavailable_records_unavailable_never_seals() {
    let rig = assess_rig(
        "review-unavailable",
        v2_argv("clean-exit", &["--report-mode", "unavailable"]),
        true,
    );
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert!(
        matches!(reviews[0].0, Completeness::Unavailable { .. }),
        "{:?}",
        reviews[0].0
    );
    assert!(sealed_rounds(&rig.store, &rig.work_id).is_empty());
}

/// A Negative review report that names no findings — `checks_failed`
/// or `invariant_violated` — records partial evidence under its own
/// reason code and never seals a clean round, so a report that
/// explicitly failed can never authorize completion. A negative
/// report carrying findings still seals them as occurrences: the
/// lane then stands on the judgment path, never the report's claim.
#[test]
fn a_negative_review_report_is_partial_and_never_seals_a_round() {
    let rig = assess_rig(
        "review-negative",
        v2_argv("clean-exit", &["--report-mode", "negative"]),
        true,
    );
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert!(
        matches!(&reviews[0].0, Completeness::Partial { reason } if reason == "checks_failed"),
        "{:?}",
        reviews[0].0
    );
    assert_eq!(reviews[0].1, 0);
    assert!(sealed_rounds(&rig.store, &rig.work_id).is_empty());
}

#[test]
fn a_review_for_a_moved_head_emits_no_gate_fact() {
    let rig = assess_rig("review-stale", v2_argv("clean-exit", &[]), true);
    // Dispatch binds the target current *now*; the head moves before
    // the report binds — the review covers a superseded target.
    let rid = format!("{}:inv:1", rig.execution_id.0);
    insert_request(
        &rig.store,
        &rig.work_id,
        &rig.execution_id,
        &rid,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    let moved = CommitRevision::new(repo(), rig.base.clone()).unwrap();
    open_change_target(&rig.store, &rig.work_id, &rig.change, &moved, "main");
    drive_to_done(&rig.sup, &rig.store, &rid);
    for _ in 0..3 {
        rig.sup.run_once(&rig.store, NOW).unwrap();
    }
    assert!(
        review_facts(&rig.store, &rig.work_id).is_empty(),
        "a stale-head review binds no gate fact"
    );
    assert!(sealed_rounds(&rig.store, &rig.work_id).is_empty());
}

/// A Negative report is the check's own failure claim — all-conform
/// member coverage can never turn it into a completed assessment.
#[test]
fn a_negative_invariant_report_is_never_a_complete_assessment() {
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "violated",
            "--invariant-items",
            IC_CONFORMS,
        ],
    );
    let rig = assess_rig("ic-negative", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
    let facts = assessment_facts(&rig.store, &rig.work_id);
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert!(!facts[0].0, "a negative report is never complete");
    assert_eq!(
        facts[0].1.as_slice(),
        [
            ("no-todo".to_string(), AssessmentVerdict::Positive),
            ("tests-pass".to_string(), AssessmentVerdict::Positive),
        ],
        "member claims stay recorded evidence"
    );
}

/// A Negative review report whose reason is not `findings_reported`
/// but which still carries findings seals those members for the
/// judgment path — the reason code describes the report's claim, not
/// the member set's disposition. Silently dropping the members would
/// lose real findings a judgment must answer.
#[test]
fn a_negative_review_report_with_findings_seals_them_as_occurrences() {
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "negative",
            "--findings",
            r#"[{"key":"f-1","summary":"dropped bound check","severity":"high"}]"#,
        ],
    );
    let rig = assess_rig("review-negative-findings", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0].0, Completeness::Complete, "{:?}", reviews[0].0);
    assert_eq!(reviews[0].1, 1, "the member seals as an occurrence");
    let rounds = sealed_rounds(&rig.store, &rig.work_id);
    assert_eq!(rounds.len(), 1, "{rounds:?}");
    let occurrences = rig
        .store
        .transact(|tx| tx.occurrences(&rounds[0].id))
        .unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].source_key, "f-1");
}

/// Over the member cap the whole `findings[]` set is malformed
/// evidence: the fact records `partial` and parses no member into an
/// occurrence — the cap bounds the retained fan-out, matching the
/// invariant-items treatment.
#[test]
fn a_review_over_the_member_cap_records_partial_and_no_occurrences() {
    let flood = serde_json::Value::Array(
        (0..=MAX_REPORT_MEMBERS)
            .map(|i| serde_json::json!({"key": format!("f-{i}"), "summary": "s"}))
            .collect(),
    )
    .to_string();
    // A success report carrying more members than the cap: the
    // completeness check reads the flood as malformed evidence.
    let argv = v2_argv("clean-exit", &["--findings", &flood]);
    let rig = assess_rig("review-flood", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let reviews = review_facts(&rig.store, &rig.work_id);
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert!(
        matches!(&reviews[0].0, Completeness::Partial { reason } if reason == "too_many_findings"),
        "{:?}",
        reviews[0].0
    );
    assert_eq!(reviews[0].1, 0, "no member parses past the cap");
    assert!(sealed_rounds(&rig.store, &rig.work_id).is_empty());
}

/// Record a target-branch revision observation — the axis
/// `DiffTarget.target_revision` tracks so a target-branch-only move
/// changes the diff target.
fn observe_branch_state(store: &SqliteStore, work_id: &WorkId, revision: &CommitRevision) {
    let cfg = config();
    handle_observation(
        store,
        &cfg,
        &Observation {
            delivery: None,
            subject_work_id: Some(work_id.clone()),
            subject_execution_id: None,
            fact: ObservationFact::BranchState {
                repository: cfg.host.repository.clone(),
                branch: "main".into(),
                state: BranchState::At {
                    revision: revision.clone(),
                },
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

/// The review binds the exact dispatch-time target — including the
/// target-branch revision: a target-branch-only move supersedes it
/// just as a head move does, and the retained report seals no round
/// for a target it never examined.
#[test]
fn a_review_binds_only_to_the_dispatch_time_target_revision() {
    let rev_a = CommitRevision::new(repo(), "a".repeat(40)).unwrap();
    let rev_b = CommitRevision::new(repo(), "b".repeat(40)).unwrap();

    // Control: the dispatch-time revision still holds at bind time.
    let rig = assess_rig("review-rev-stable", v2_argv("clean-exit", &[]), true);
    observe_branch_state(&rig.store, &rig.work_id, &rev_a);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    assert_eq!(
        review_facts(&rig.store, &rig.work_id),
        vec![(Completeness::Complete, 0)]
    );
    assert_eq!(sealed_rounds(&rig.store, &rig.work_id).len(), 1);

    // Same change and head, a new target-branch revision: the report
    // binds nothing.
    let rig = assess_rig("review-rev-moved", v2_argv("clean-exit", &[]), true);
    observe_branch_state(&rig.store, &rig.work_id, &rev_a);
    let rid = format!("{}:inv:1", rig.execution_id.0);
    insert_request(
        &rig.store,
        &rig.work_id,
        &rig.execution_id,
        &rid,
        InvocationRole::Review,
        serde_json::json!({"required_artifacts": []}),
    );
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    observe_branch_state(&rig.store, &rig.work_id, &rev_b);
    drive_to_done(&rig.sup, &rig.store, &rid);
    for _ in 0..3 {
        rig.sup.run_once(&rig.store, NOW).unwrap();
    }
    assert!(
        review_facts(&rig.store, &rig.work_id).is_empty(),
        "a target-branch-only move supersedes the review's target"
    );
    assert!(sealed_rounds(&rig.store, &rig.work_id).is_empty());
}

/// Drive the real human-judgment path over the newest sealed round:
/// `JudgeFindings` with one decision per occurrence (in sealed
/// order), then `ReleaseJudgment` — leaving the Execution `queued`
/// for the next admission exactly as an operator release does.
fn judge_and_release(rig: &AssessRig, choices: &[FindingChoice]) {
    // Undecided findings pause gracefully: `Stopping` drains to
    // `Paused` on the stop sweep, and only `Paused` releases.
    for _ in 0..100 {
        let state = rig
            .store
            .transact(|tx| tx.execution(&rig.execution_id))
            .unwrap()
            .unwrap()
            .state;
        if state == ExecutionState::Paused {
            break;
        }
        rig.sup.run_once(&rig.store, NOW).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let state = rig
        .store
        .transact(|tx| tx.execution(&rig.execution_id))
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state, ExecutionState::Paused, "findings pause for judgment");

    let (version, head, occurrences) = rig
        .store
        .transact(|tx| {
            let work = tx.work(&rig.work_id)?.unwrap();
            let round = tx.latest_round(&rig.work_id)?.unwrap();
            let occurrences = tx.occurrences(&round.id)?;
            Ok::<_, StoreError>((work.version, round.target.head, occurrences))
        })
        .unwrap();
    assert_eq!(
        occurrences.len(),
        choices.len(),
        "one decision per occurrence"
    );
    let d = handle_command(
        &rig.store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "judge-1".into(),
            body: CommandBody::JudgeFindings {
                work_id: rig.work_id.clone(),
                expected_work_version: version,
                expected_head: head,
                decisions: occurrences
                    .iter()
                    .zip(choices.iter())
                    .map(|(occ, choice)| FindingDecision {
                        occurrence_id: occ.id.clone(),
                        // `dismiss`/`accept` dispositions must record
                        // their rationale verbatim.
                        rationale: matches!(choice, FindingChoice::Accept | FindingChoice::Dismiss)
                            .then(|| "operator reviewed the claim".to_string()),
                        choice: choice.clone(),
                    })
                    .collect(),
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");

    let version = rig
        .store
        .transact(|tx| tx.work(&rig.work_id).map(|w| w.unwrap().version))
        .unwrap();
    let head = rig
        .store
        .transact(|tx| {
            tx.latest_round(&rig.work_id)
                .map(|r| r.unwrap().target.head)
        })
        .unwrap();
    let d = handle_command(
        &rig.store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "release-1".into(),
            body: CommandBody::ReleaseJudgment {
                work_id: rig.work_id.clone(),
                expected_work_version: version,
                expected_head: head,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
}

/// The request id `admit_queued*` just created for the Execution.
fn admitted_request_id(rig: &AssessRig) -> String {
    rig.store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .find(|r| r.execution_id == rig.execution_id)
        .expect("admission left a pending request")
        .request_id
}

#[test]
fn same_head_finding_growth_seals_a_new_round() {
    // `--findings-file` is read at report time, so two sequential
    // review Invocations under one argv can carry different claims.
    let root = tmp_root("review-growth");
    let findings_path = root.join("findings.json");
    std::fs::write(&findings_path, r#"[{"key":"f-1","summary":"first"}]"#).unwrap();
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "findings",
            "--findings-file",
            findings_path.to_str().unwrap(),
        ],
    );
    let rig = assess_rig("review-growth", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let first = sealed_rounds(&rig.store, &rig.work_id);
    assert_eq!(first.len(), 1);
    // Undecided findings on the current target stop the Execution
    // for human judgment — the gate the second review must pass
    // through, not around.
    let state = rig
        .store
        .transact(|tx| tx.execution(&rig.execution_id))
        .unwrap()
        .unwrap()
        .state;
    assert!(
        matches!(state, ExecutionState::Stopping | ExecutionState::Paused),
        "findings stop the Execution for judgment, got {state:?}"
    );

    // Same head, more findings: the operator accepts round 1's
    // finding, releases the judgment gate, and Core admits another
    // review — the occurrence set differs, so the seal records a NEW
    // round, never a silent overwrite of the judged one.
    judge_and_release(&rig, &[FindingChoice::Accept]);
    rig.store
        .transact(|tx| {
            admit_queued_role(
                tx,
                &config(),
                &rig.execution_id,
                InvocationRole::Review,
                NOW,
            )
        })
        .unwrap();
    std::fs::write(
        &findings_path,
        r#"[{"key":"f-1","summary":"first"},{"key":"f-2","summary":"second"}]"#,
    )
    .unwrap();
    let rid2 = admitted_request_id(&rig);
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    drive_to_done(&rig.sup, &rig.store, &rid2);

    let rounds = sealed_rounds(&rig.store, &rig.work_id);
    assert_eq!(rounds.len(), 2, "{rounds:?}");
    let latest = rounds
        .iter()
        .max_by_key(|r| r.generation)
        .expect("a latest round");
    assert_eq!(latest.generation, 1);
    assert_ne!(
        latest.set_digest, first[0].set_digest,
        "different occurrence sets seal different rounds"
    );
    assert_eq!(
        rig.store
            .transact(|tx| tx.occurrences(&latest.id))
            .unwrap()
            .len(),
        2
    );
    // Both review facts bind the same accepted target.
    assert_eq!(review_facts(&rig.store, &rig.work_id).len(), 2);
}

#[test]
fn sealed_findings_feed_the_bounded_fix_origin() {
    let argv = v2_argv(
        "clean-exit",
        &[
            "--report-mode",
            "findings",
            "--findings",
            r#"[{"key":"f-1","summary":"off by one","severity":"high"}]"#,
        ],
    );
    let rig = assess_rig("review-fix", argv, true);
    run_assessment_role(&rig, 1, InvocationRole::Review);
    let round = sealed_rounds(&rig.store, &rig.work_id)
        .into_iter()
        .next()
        .expect("findings sealed a round");
    let occurrences = rig.store.transact(|tx| tx.occurrences(&round.id)).unwrap();
    assert_eq!(occurrences.len(), 1);

    // The real human path: the operator judges the finding for repair,
    // releases the gate, and admission routes the bounded Fix request.
    judge_and_release(&rig, &[FindingChoice::Fix]);
    let admitted = rig
        .store
        .transact(|tx| admit_queued(tx, &config(), &rig.execution_id, NOW))
        .unwrap();
    assert!(admitted.is_some(), "fix disposition admits a Fix request");
    let rid2 = admitted_request_id(&rig);
    rig.sup.consume_requests(&rig.store, NOW).unwrap();
    let wire = WorkerRequestV2::parse(&request_body(&rig.store, &rid2).canonical_body).unwrap();
    let TaskBody::Fix { origin, .. } = &wire.task else {
        panic!("expected fix task: {:?}", wire.task)
    };
    let FixOrigin::Review {
        round: bound_round,
        judgment,
        findings,
        ..
    } = origin.as_ref()
    else {
        panic!("expected review origin: {origin:?}")
    };
    let recorded = rig
        .store
        .transact(|tx| tx.judgment_for_round(&round.id))
        .unwrap()
        .expect("the judgment is durable");
    assert_eq!(bound_round, &round.id);
    assert_eq!(judgment.as_str(), recorded.id.as_str());
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].occurrence, occurrences[0].id);
    assert_eq!(
        findings[0].digest,
        occurrences[0].content_digest.clone().unwrap(),
        "the Fix origin binds the worker-derived occurrence digest"
    );
}

#[test]
fn assessment_facts_bind_identically_with_lanes_disabled() {
    // Lane policy lives in the completion gate, not the binder: with
    // both lanes off the spec records it, and the same durable report
    // still binds the same evidence — the gate alone decides whether
    // the lane requires it.
    let mut cfg = config();
    cfg.lanes = LanePolicy {
        invariant_check: false,
        code_review: false,
    };
    let rig = assess_rig_cfg("lanes-off", ic_argv(IC_CONFORMS), true, &cfg);
    run_assessment_role(&rig, 1, InvocationRole::InvariantCheck);
    let facts = assessment_facts(&rig.store, &rig.work_id);
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert!(facts[0].0);
    let spec = rig
        .store
        .transact(|tx| {
            let work = tx.work(&rig.work_id)?.unwrap();
            tx.spec(&rig.work_id, Some(work.approved_spec))
        })
        .unwrap()
        .unwrap();
    assert!(!spec.payload.lanes.invariant_check);
    assert!(!spec.payload.lanes.code_review);
}

/// A supervisor that dies after spawn but before report collection
/// still collects the exited writer's report exactly once: the
/// restarted supervisor reconciles the recorded identity, proves the
/// tree stopped, and ingests the same durable report — never a
/// second writer, never a duplicate fact.
#[test]
fn a_restart_before_report_collection_ingests_the_same_report_once() {
    let root = tmp_root("report-restart");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-report-restart", vec![]);
    // `hang` writes report.json/runtime.log then loops forever: the
    // out dir is complete on disk while the writer still lives.
    let argv = v2_argv("hang", &[]);
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv.clone());
    sup.consume_requests(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    wait_for(
        || invocation_for(&store, &rid).is_some_and(|i| i.process_identity.is_some()),
        "writer launched",
    );
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");

    // The crash: the writer keeps running under the recorded identity
    // while a fresh supervisor takes over the same store and roots.
    drop(sup);
    let restarted = supervisor_v2_argv(&root, &state_root, &src, argv);
    assert!(
        report_facts(&store, &e).is_empty(),
        "no report evidence before the tree proves stopped"
    );

    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-report-restart-cancel".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    for _ in 0..60 {
        restarted.run_once(&store, NOW).unwrap();
        if !execution_invocation(&store, &e, &invocation.id)
            .state
            .is_unresolved_writer()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !execution_invocation(&store, &e, &invocation.id)
            .state
            .is_unresolved_writer(),
        "the restarted supervisor resolved the same writer"
    );
    let facts = report_facts(&store, &e);
    let fact = the_report_fact(&facts);
    let ObservationFact::WorkerReport { classification, .. } = fact else {
        unreachable!()
    };
    assert_eq!(
        *classification,
        ReportClassification::Success,
        "the completed report is collected exactly once after restart"
    );
}

/// The stop path persists its intent and descendant snapshot before
/// the first signal; a supervisor that dies inside that window leaves
/// the durable intent behind, and the restarted supervisor resumes
/// signaling and inspection from it — never re-deriving the tree,
/// never spawning a writer.
#[test]
fn a_durable_stop_intent_resumes_the_stop_after_a_supervisor_restart() {
    let root = tmp_root("stop-restart");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-stop-restart", vec![]);
    let argv = v2_argv("hang", &[]);
    let sup = supervisor_v2_argv(&root, &state_root, &src, argv.clone());
    sup.consume_requests(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    wait_for(
        || invocation_for(&store, &rid).is_some_and(|i| i.process_identity.is_some()),
        "writer launched",
    );
    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    let leader = invocation
        .process_identity
        .clone()
        .expect("the writer identity is recorded");

    // Cancel, then crash inside the stop window: the intent — with
    // the descendant snapshot — is durable but no signal was sent.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-stop-restart-cancel".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    let record = sup
        .record_stop_intent(&store, &invocation, &leader)
        .unwrap();
    assert!(
        matches!(record, StopRecord::Recorded(_)),
        "the intent persisted before the crash: {record:?}"
    );
    drop(sup);

    // The restarted supervisor holds no memory of the stop; the stop
    // pass must resume signaling and inspection from the durable
    // intent alone — never re-deriving the tree, never spawning.
    let restarted = supervisor_v2_argv(&root, &state_root, &src, argv);
    for _ in 0..60 {
        restarted.run_once(&store, NOW).unwrap();
        if execution(&store, &e).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        execution(&store, &e).state,
        ExecutionState::Cancelled,
        "the durable intent carried the stop across the restart"
    );
}

/// A terminally failed Execution keeps its cumulative Work-wide
/// accounting: charged usage, a retry into a successor Execution,
/// and a fresh real worker dispatch preserve every counter — nothing
/// is refunded by the succession — while the failed Execution keeps
/// surfacing actionable Attention and its collected evidence.
#[test]
fn a_retry_launches_a_successor_execution_without_refunding_cumulative_usage() {
    let root = tmp_root("retry-successor");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, "v2-retry-successor", vec![]);
    // `exit-no-artifact` exits cleanly without producing the required
    // artifact: bounded recovery exhausts and the Execution fails.
    let sup = supervisor_v2(&root, &state_root, &src, "exit-no-artifact", true);
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        sweep(&store, &config(), NOW).unwrap();
        if execution(&store, &e).state == ExecutionState::Failed {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        execution(&store, &e).state,
        ExecutionState::Failed,
        "bounded artifact recovery exhausted"
    );

    // The failure surfaces actionable Attention for the operator.
    let attention = store
        .transact(|tx| workloop_core::projection::attention(tx, Some(&w), NOW + 60))
        .unwrap();
    let workloop_core::projection::QueryResult::Found { value, .. } = attention else {
        panic!("attention query found nothing")
    };
    assert!(
        value.iter().any(|a| a.reason == "confirmed_failure"),
        "the failed Execution surfaces confirmed_failure: {value:?}"
    );

    // Charge the cumulative counters the way spent transport retries
    // and fix dispatches would have.
    store
        .transact(|tx| {
            let mut usage = tx.work_usage(&w)?.unwrap();
            usage.transport_retries_used = 2;
            usage.fix_invocations_used = 1;
            usage.equivalent_failure_streak = 1;
            tx.apply(&Effect::UpdateWorkUsage { usage })
        })
        .unwrap();

    // Retry: the successor Execution is a new row on the same Work.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-retry-successor-1".into(),
            body: CommandBody::Retry {
                work_id: w.clone(),
                expected_work_version: version,
                expected_execution_id: e.clone(),
            },
        },
        NOW,
    )
    .unwrap();
    let e2 = match d {
        CommandDecision::Accepted { effect, .. } => match effect {
            CommandEffect::ExecutionCreated { execution_id } => execution_id,
            other => panic!("unexpected retry effect {other:?}"),
        },
        other => panic!("retry rejected: {other:?}"),
    };
    assert_ne!(e2, e, "the successor is a distinct Execution");
    assert_eq!(
        execution(&store, &e2).ordinal,
        execution(&store, &e).ordinal + 1,
        "the successor advances the Execution ordinal"
    );

    // The successor admits and dispatches a fresh real worker; its
    // durable request identity is new, never a replay of the failed
    // one's.
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &e2, NOW))
        .unwrap();
    assert!(admitted.is_some(), "the successor admits");
    sup.consume_requests(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    for _ in 0..200 {
        if store
            .transact(|tx| tx.invocations_for_execution(&e2))
            .unwrap()
            .iter()
            .any(|i| i.process_identity.is_some())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let successor_invocations = store
        .transact(|tx| tx.invocations_for_execution(&e2))
        .unwrap();
    assert_eq!(successor_invocations.len(), 1);
    assert!(
        successor_invocations[0].process_identity.is_some(),
        "the successor's writer launched"
    );
    assert_ne!(
        successor_invocations[0].request_id, rid,
        "a fresh durable request identity"
    );

    // Cumulative Work-wide usage survived the succession untouched.
    let usage = store.transact(|tx| tx.work_usage(&w)).unwrap().unwrap();
    assert_eq!(usage.transport_retries_used, 2, "no refund");
    assert_eq!(usage.fix_invocations_used, 1, "no refund");
    assert_eq!(usage.equivalent_failure_streak, 1, "no refund");
    assert_eq!(
        usage.first_dispatch_unix_seconds,
        Some(NOW),
        "the budget anchor is never re-armed"
    );
    // The failed Execution's collected report evidence is retained.
    assert_eq!(report_facts(&store, &e).len(), 1);
}

/// A cancel committed while declared verification work is pending
/// does not skip the owned child: the bounded verification run still
/// executes and records its result while the Execution is stopping,
/// and the terminal release comes only afterwards — never while a
/// child could still be running.
#[test]
fn a_cancel_with_pending_verification_accounts_for_the_owned_child_before_release() {
    let root = tmp_root("cancel-verify");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit_ex(
        &store,
        "v2-cancel-verify",
        vec![],
        vec![vec!["git".into(), "rev-parse".into(), "HEAD".into()]],
    );
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    poll_to_done(&sup, &store, &rid);

    // Cancel while the acknowledged artifact's declared verification
    // has not run yet: the Execution enters stopping but is not
    // released — the writer proof and the owned child both precede
    // the terminal commit.
    let version = store.transact(|tx| tx.work(&w)).unwrap().unwrap().version;
    let d = handle_command(
        &store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: "v2-cancel-verify-1".into(),
            body: CommandBody::Cancel {
                work_id: w.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(d, CommandDecision::Accepted { .. }), "{d:?}");
    assert_ne!(
        execution(&store, &e).state,
        ExecutionState::Cancelled,
        "cancel is not an immediate release"
    );

    // The handoff sweep still drives the acknowledged artifact's
    // declared verification — the owned child runs inside its
    // bounded call and its result is recorded as evidence before any
    // release.
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();
    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let aid = format!("{}/artifact.json", inv.id.0);
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1, "the verification child was accounted");
    assert_eq!(
        results[0].result,
        VerificationOutcome::Pass,
        "{:?}",
        results[0]
    );

    // Only now can the Execution settle: the writer was already
    // proven stopped and every owned child is accounted.
    for _ in 0..40 {
        sup.run_once(&store, NOW).unwrap();
        sweep(&store, &config(), NOW).unwrap();
        if execution(&store, &e).state == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution(&store, &e).state, ExecutionState::Cancelled);
    // The recorded verification is final: a later sweep never
    // duplicates the child or its evidence.
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1, "recorded terminal results are final");
}

/// §14.5 bounds the 30 min verification profile by the Execution
/// deadline: the immutable wire `deadline_unix_seconds` clamps the
/// per-command timeout, so a command can never outlive the budget the
/// request granted. A spent deadline bounds the run to zero — the
/// command times out immediately and records `Inconclusive`, the same
/// evidence a real timeout reports, never a pass.
#[test]
fn v2_verification_timeout_is_bounded_by_the_execution_deadline() {
    let root = tmp_root("verify-v2-deadline");
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let argv: Vec<Vec<String>> = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    let (_w, _e, rid) = admit_ex(&store, "v2-verify-deadline", vec![], argv);
    let sup = supervisor_v2_argv(
        &root,
        &state_root,
        &src,
        v2_argv("clean-exit", &["--git-commit"]),
    );
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs_scoped(&store, None, &mut || NOW, None)
        .unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id);
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let wire = WorkerRequestV2::parse(&request_body(&store, &rid).canonical_body).unwrap();
    let deadline = wire.budget.deadline_unix_seconds;

    let bounds = |now| {
        workloop_exec::handoff::verification_bounds(&store, &row.artifact_id, now)
            .unwrap()
            .0
    };
    // The request's whole budget remains: the full 30 min profile.
    assert_eq!(bounds(NOW), Duration::from_secs(30 * 60));
    // One second remains: the deadline clamps below the profile.
    assert_eq!(bounds(deadline - 1), Duration::from_secs(1));
    // The deadline is spent: no command may run at all.
    assert_eq!(bounds(deadline), Duration::ZERO);
    assert_eq!(bounds(deadline + 600), Duration::ZERO);
}
