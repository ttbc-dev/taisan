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
    HostActionStatus, IssueState, NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::store::{
    BodyCatalog, Effect, HostAction, HostActionIntent, Store, StoreError, TxView,
};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, classify_failure, handle_command,
    handle_observation, material_progress_basis,
};
use workloop_core::worker_v2::*;
use workloop_exec::bodies::{BodyStore, CAS_CATEGORY};
use workloop_exec::handoff::{HandoffLayout, artifact_ref};
use workloop_exec::request_v2::{
    InvariantSetConfig, V2BudgetBounds, V2Dispatch, invocation_io_root,
};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
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
    let cfg = config();
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
                verification_argv: vec![],
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
    let bodies = BodyStore::new(state_root.join("bodies"));
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
        bodies: BodyStore::new(state_root.join("bodies")),
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
    BodyStore::new(state_root.join("bodies"))
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
    let file = state_root.join("bodies").join(snapshot.retained_path());
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
        target_revision: None,
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
    let resolved = BodyStore::new(state_root.join("bodies"))
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
/// reports the unknown writer again; it cannot start a second writer
/// for the consumed request.
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

    // Reconciliation and further passes replay the recorded
    // Invocation — the unknown writer is reported, never replaced.
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
    assert_eq!(invocations[0].state, InvocationState::Prepared);
    assert!(invocations[0].process_identity.is_none());
    assert!(!pending_ids(&store).contains(&rid));
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
    let bodies = BodyStore::new(state_root.join("bodies"));
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
    let bodies = BodyStore::new(state_root.join("bodies"));
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
    let bodies = BodyStore::new(state_root.join("bodies"));
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
