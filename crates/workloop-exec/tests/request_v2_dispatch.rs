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
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
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
