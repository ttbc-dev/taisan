//! Executable wire-boundary Worker conformance (v0.3 / #268): the same
//! scenario suite runs unchanged against every supplied conforming
//! Worker implementation — the reference fixture binary and the
//! independent contract Worker — through the real dispatch path:
//! durable request materialization, declaration negotiation, a real
//! process boundary (`run --request --out`), bounded report
//! collection, and durable evidence. No scenario branches on which
//! implementation is bound; the only difference between the two runs
//! is the worker binary selected inside the marked identity boundary
//! at the bottom of this file.
//!
//! Evidence class: fixture/contract evidence only. These passes prove
//! the wire contract is implementable and enforced end-to-end inside
//! the supervisor — they do not claim live-provider integration, and
//! no row here is substituted for one. Rows a real process cannot
//! express (lost spawn identity, uninspectable liveness) stay in the
//! provider-model suite (`worker_contract.rs`); every row below is
//! exercised by a real spawned process.
//!
//! When the checkout yields a faithful revision stamp the run retains
//! one results record per implementation per scenario to
//! `target/worker-wire-contract.json` — tested/base revisions, the
//! sanitized configuration, per-row outcomes, and explicit skip
//! reasons. A dirty or non-git checkout runs the identical suite and
//! every record-independent assertion but retains nothing, so the
//! prescribed `cargo test --workspace` stays runnable mid-edit.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::{
    BranchState, HostActionStatus, NormalizedEffectFact, Observation, ObservationFact,
};
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, SignalOutcome, SpawnError, WorkerEnv,
};
use workloop_core::store::{Effect, HostAction, HostActionIntent, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
use workloop_core::worker_v2::*;
use workloop_exec::adapter::observe_worker_declaration_v2;
use workloop_exec::bodies::BodyStore;
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
        "workloop-wire-{}-{}-{}",
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
/// by these wire-contract tests.
fn fixture_sup<P: ProcessControlPort>(
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

/// Submit a Work, settle its source gate, and admit the first
/// Execution — the same durable shape a production pass produces.
fn admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId, String) {
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
                sources: vec![],
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
    let admitted = store
        .transact(|tx| admit_queued(tx, &cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
}

/// `admit` with caller-declared `verification_argv` — the spec payload
/// the handoff's independent checks read.
fn admit_ex(
    store: &SqliteStore,
    key: &str,
    verification_argv: Vec<Vec<String>>,
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
                sources: vec![],
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
    let admitted = store
        .transact(|tx| admit_queued(tx, &cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    let request_id = format!("{}:inv:0", eid.0);
    (work_id, eid, request_id)
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
        state_root: None,
        verification: None,
    }
}

/// Observational decorator: counts every spawn call — refused and
/// unknown attempts included — and records the identities the real
/// port produced, so a scenario can assert both no second attempt and
/// no second writer. It delegates every call to `PosixProcessControl`.
struct CountingPosix {
    spawned: Rc<RefCell<Vec<ProcessIdentity>>>,
    calls: Rc<Cell<usize>>,
}

impl ProcessControlPort for CountingPosix {
    fn spawn(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
    ) -> Result<ProcessIdentity, SpawnError> {
        self.calls.set(self.calls.get() + 1);
        let out = PosixProcessControl.spawn(argv, cwd, env);
        if let Ok(identity) = &out {
            self.spawned.borrow_mut().push(identity.clone());
        }
        out
    }
    fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {
        PosixProcessControl.snapshot_descendants(leader_pid)
    }
    fn group_members(&self, pgid: i32) -> Result<Vec<ProcessIdentity>, String> {
        PosixProcessControl.group_members(pgid)
    }
    fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {
        PosixProcessControl.inspect(identity)
    }
    fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome {
        PosixProcessControl.signal_verified(identity, signal)
    }
    fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome {
        PosixProcessControl.signal_group(pgid, signal)
    }
    fn reap(&self, pid: i32) -> Option<i32> {
        PosixProcessControl.reap(pid)
    }
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        env: &WorkerEnv,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        PosixProcessControl.run_probe(argv, cwd, env, timeout)
    }
}

/// A v2 supervisor over caller-chosen worker argv plus the spawn-call
/// counter the port boundary exposes. Every scenario launches through
/// this one construction — complete request, process boundary, report
/// collection — never by invoking a worker directly.
fn wire_sup(
    root: &Path,
    state_root: &Path,
    src: &Path,
    argv: Vec<String>,
) -> (Supervisor<CountingPosix>, Rc<Cell<usize>>) {
    let handoff = layout(root);
    std::fs::create_dir_all(&handoff.workspace_root).unwrap();
    let spawned = Rc::new(RefCell::new(Vec::new()));
    let calls = Rc::new(Cell::new(0));
    let sup = fixture_sup(
        CountingPosix {
            spawned,
            calls: calls.clone(),
        },
        handoff.workspace_root.clone(),
        argv,
        config(),
    )
    .with_handoff(handoff)
    .with_v2_dispatch(dispatch_profile(state_root, src));
    (sup, calls)
}

fn execution_state(store: &SqliteStore, id: &ExecutionId) -> ExecutionState {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .unwrap()
        .state
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

/// The protocol code a launch refusal reason leads with — the detail
/// tail can carry adapter- and version-specific text, the code never
/// does, so signatures compare codes only.
fn refusal_code(reason: &str) -> String {
    reason.split(':').next().unwrap_or(reason).to_string()
}

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

fn class_name(c: &ReportClassification) -> String {
    match c {
        ReportClassification::Success => "success".into(),
        ReportClassification::Negative => "negative".into(),
        ReportClassification::Unavailable => "unavailable".into(),
        ReportClassification::Failure { code } => format!("failure:{code}"),
        ReportClassification::Invalid { reasons } => {
            let mut reasons = reasons.clone();
            reasons.sort();
            format!("invalid:{}", reasons.join(","))
        }
        ReportClassification::Absent => "absent".into(),
    }
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

/// Drive passes until the Invocation resolves `Done`.
fn drive_to_done<P: ProcessControlPort>(sup: &Supervisor<P>, store: &SqliteStore, rid: &str) {
    for _ in 0..100 {
        sup.run_once(store, NOW).unwrap();
        if invocation_state(store, rid) == Some(InvocationState::Done) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("invocation {rid} did not resolve Done");
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

/// Open the Work's change request and record the head evidence that
/// makes it the current accepted diff target.
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

/// The managed workspace directory for one Work — where the worker's
/// commits live before handoff exports them.
fn managed_workspace(root: &Path, work_id: &WorkId) -> PathBuf {
    root.join("ws").join(format!("ws-{}", work_id.0))
}

/// The durable handoff row for `<invocation>/artifact.json`.
fn handoff_row(store: &SqliteStore, invocation_id: &InvocationId) -> Option<ArtifactHandoff> {
    store
        .transact(|tx| tx.artifact_handoff(&format!("{}/artifact.json", invocation_id.0)))
        .unwrap()
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

fn cancel_work(store: &SqliteStore, w: &WorkId) {
    let version = store.transact(|tx| tx.work(w)).unwrap().unwrap().version;
    let d = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: format!("wire-cancel-{}", w.0),
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
}

// ---- the adapter-neutral outcome signature ----------------------------

/// What one suite pass leaves behind, reduced to facts two different
/// Worker implementations must produce identically: durable states,
/// refusal codes, report classifications, handoff states, and the
/// port-boundary spawn count. Worker-authored payloads (report bodies,
/// artifact bytes, runtime versions) never enter the signature.
#[derive(Debug, PartialEq)]
struct WireSignature {
    invocation_states: Vec<String>,
    writers: usize,
    report_classes: Vec<String>,
    execution_state: String,
    launch_refusal_codes: Vec<String>,
    dispatch_refusal_codes: Vec<String>,
    handoff_states: Vec<String>,
    claim_agreements: Vec<bool>,
    spawn_calls: usize,
    pending: usize,
}

fn collect_signature(
    store: &SqliteStore,
    work_id: &WorkId,
    execution_id: &ExecutionId,
    spawn_calls: usize,
) -> WireSignature {
    let invocations = store
        .transact(|tx| tx.invocations_for_execution(execution_id))
        .unwrap();
    let invocation_states = invocations
        .iter()
        .map(|i| format!("{:?}", i.state))
        .collect();
    let writers = invocations
        .iter()
        .filter(|i| i.process_identity.is_some())
        .count();
    let report_classes = report_facts(store, execution_id)
        .iter()
        .filter_map(|f| match f {
            ObservationFact::WorkerReport { classification, .. } => {
                Some(class_name(classification))
            }
            _ => None,
        })
        .collect();
    let execution_state = format!("{:?}", execution_state(store, execution_id));
    let launch_refusal_codes = launch_refusals(store, work_id)
        .iter()
        .map(|r| refusal_code(r))
        .collect();
    let dispatch_refusal_codes = dispatch_refusals(store, work_id)
        .iter()
        .map(|(c, _)| c.clone())
        .collect();
    let handoff_states = invocations
        .iter()
        .map(|i| {
            handoff_row(store, &i.id)
                .map(|h| format!("{:?}", h.state))
                .unwrap_or_else(|| "none".into())
        })
        .collect();
    let claim_agreements = claim_facts(store, execution_id)
        .iter()
        .filter_map(|f| match f {
            ObservationFact::ClaimedHeadCheck { agrees, .. } => Some(*agrees),
            _ => None,
        })
        .collect();
    WireSignature {
        invocation_states,
        writers,
        report_classes,
        execution_state,
        launch_refusal_codes,
        dispatch_refusal_codes,
        handoff_states,
        claim_agreements,
        spawn_calls,
        pending: pending_ids(store).len(),
    }
}

/// One suite row's outcome: the compared signature plus the adapter
/// identity the durable binding recorded — bookkeeping for the
/// non-vacuity proof only, never part of the compared signature.
struct WireRun {
    signature: WireSignature,
    bound_adapter: Option<String>,
}

/// The adapter id a request's durable binding recorded — read through
/// the boundary so no case branches on it.
fn bound_adapter_id(store: &SqliteStore, request_id: &str) -> Option<String> {
    store
        .transact(|tx| tx.request(request_id))
        .unwrap()
        .and_then(|r| r.adapter_binding)
        .map(|b| b.adapter.as_str().to_string())
}

// ---- scenarios --------------------------------------------------------

/// WW-REQ-01 / role `coding`: the complete immutable request crosses
/// the process boundary and the validated report resolves the
/// Invocation — the normal driver path end to end.
fn coding_request_runs_the_wire(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-coding"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-coding"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);

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
    let io_dir = invocation_io_root(&root.join("ws")).join(&invocation.id.0);
    // request.json is the exact durable bytes, read-only; input/ holds
    // the resolved goal body read-only; out/ is the writable dir.
    assert_eq!(
        std::fs::read(io_dir.join("request.json")).unwrap(),
        body.canonical_body,
        "request.json is the stored canonical body verbatim"
    );
    assert!(io_dir.join("input").join("goal.md").is_file());
    assert!(io_dir.join("out").is_dir(), "out dir exists before spawn");

    drive_to_done(&sup, &store, &rid);

    // The collected report is classified evidence bound to this
    // request — never worker prose promoted to fact.
    let facts = report_facts(&store, &e);
    assert_eq!(facts.len(), 1, "exactly one report fact: {facts:?}");
    let ObservationFact::WorkerReport { classification, .. } = &facts[0] else {
        unreachable!()
    };
    assert_eq!(*classification, ReportClassification::Success);

    let resolved = invocation_for(&store, &rid).expect("resolved invocation");
    assert!(
        resolved.process_identity.is_some(),
        "the v2 launch recorded its process identity"
    );
    assert!(
        !io_dir.exists(),
        "the resolved invocation's io tree is reclaimed"
    );
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-ROLE-FIX: a Fix request binds the acknowledged predecessor and
/// failed evidence, and its worker report resolves Done on the wire.
fn fix_role_runs_the_wire(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-fix"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, &format!("wire-{tag}-fix"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
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

    drive_to_done(&sup, &store, &rid1);
    let facts = report_facts(&store, &e);
    assert!(
        facts.iter().any(|f| matches!(
            f,
            ObservationFact::WorkerReport {
                classification: ReportClassification::Success,
                ..
            }
        )),
        "the fix report classifies Success: {facts:?}"
    );
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid1),
    }
}

/// WW-ROLE-IC: an InvariantCheck request binds the approved set under
/// a read-only grant; the worker writes nothing and reports Success.
fn invariant_check_role_runs_the_wire(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-ic"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, &format!("wire-{tag}-ic"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
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
        verification_evidence,
    } = &wire.task
    else {
        panic!("expected invariant_check task: {:?}", wire.task)
    };
    assert_eq!(artifact.artifact_id, artifact_id);
    assert_eq!(invariant_set.id, "invariants-1");
    assert!(
        verification_evidence.is_empty(),
        "no recorded results means no evidence member"
    );

    drive_to_done(&sup, &store, &rid1);
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid1),
    }
}

/// WW-ROLE-REVIEW: a Review request binds the current diff target and
/// review source under a read-only grant; the report resolves Done.
fn review_role_runs_the_wire(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-review"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, base, produced) = git_source(&root);
    let (w, e, rid0) = admit(&store, &format!("wire-{tag}-review"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
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
    assert_eq!(target.target_revision, Some("0".repeat(40)));
    assert_eq!(review_source.source, "supervisor");

    drive_to_done(&sup, &store, &rid1);
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid1),
    }
}

/// WW-DECL-01: a worker whose `describe` never answers is
/// `adapter.declaration_unavailable` — an explicit configuration
/// repair recorded before any launch, never a degraded start.
fn unanswered_declaration_is_a_configuration_repair(_bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-decl-silent"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-silent"));
    let (sup, calls) = wire_sup(&root, &state_root, &src, vec!["/bin/false".into()]);

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
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: None,
    }
}

/// WW-DECL-02: a bound request whose recorded runtime no longer
/// matches a fresh `describe` is a configuration repair — no worker
/// start is attempted and the recorded binding never moves.
fn bound_runtime_drift_is_a_configuration_repair(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-decl-drift"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-drift"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    // The binding the request recorded when it bound: the real
    // declaration's capability surface, but a runtime version the
    // worker no longer reports — the upgrade happened after binding.
    let (_declared, surface) =
        observe_worker_declaration_v2(&PosixProcessControl, &argv, &root, &WorkerEnv::default())
            .expect("the worker declares its v2 surface");
    let mut binding = surface.binding(&argv);
    binding.runtime_version = "9.9.9-drift".into();
    store
        .transact(|tx| {
            tx.apply(&Effect::BindRequestAdapter {
                request_id: rid.clone(),
                binding: binding.clone(),
            })
        })
        .unwrap();
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);

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
        request
            .adapter_binding
            .map(|b| b.runtime_version)
            .as_deref(),
        Some("9.9.9-drift")
    );
    assert!(!pending_ids(&store).contains(&rid));
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// A bound request whose `describe` answer cannot negotiate its role
/// or a required capability refuses the launch before any worker
/// process exists — the `--decl-file` surface lets the real binary
/// answer with a caller-chosen drifted declaration.
fn declined_declaration_launch(
    bin: &str,
    tag: &str,
    roles: &[&str],
    capabilities: &[&str],
    want: &str,
) -> WireRun {
    let root = tmp_root(tag);
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}"));
    let decl = root.join("decl.json");
    std::fs::write(&decl, drifted_decl(roles, capabilities)).unwrap();
    let argv = vec![
        bin.to_string(),
        "clean-exit".into(),
        "--decl-file".into(),
        decl.to_string_lossy().into_owned(),
    ];
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains(want),
        "the refusal names the drift ({want}): {}",
        refusals[0]
    );
    assert!(!pending_ids(&store).contains(&rid));
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-DECL-03: a declaration whose roles exclude the request's role
/// cannot negotiate — the attempt is abandoned pre-spawn.
fn declaration_role_drift_is_refused(bin: &str, tag: &str) -> WireRun {
    declined_declaration_launch(
        bin,
        &format!("{tag}-role"),
        &["review"],
        &["workspace_write", "artifact_json", "process_control"],
        "role_mismatch",
    )
}

/// WW-DECL-04: a declaration missing a required capability cannot
/// negotiate — the attempt is abandoned pre-spawn.
fn declaration_capability_drift_is_refused(bin: &str, tag: &str) -> WireRun {
    declined_declaration_launch(
        bin,
        &format!("{tag}-cap"),
        &["coding", "fix", "invariant_check", "review"],
        &["workspace_write", "process_control"],
        "capability.missing",
    )
}

/// WW-DECL-05: a worker that answers `describe` with the legacy
/// declaration cannot negotiate the v2 request — the attempt is
/// abandoned before any process exists.
fn legacy_declaration_cannot_negotiate(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-decl-legacy"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-legacy"));
    // No `--wire v2`: `describe` prints the legacy declaration, which
    // is not a `taisan.worker.declaration/1` envelope.
    let argv = vec![bin.to_string(), "clean-exit".into()];
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();

    let invocation = invocation_for(&store, &rid).expect("invocation recorded");
    assert_eq!(invocation.state, InvocationState::Abandoned);
    assert!(invocation.process_identity.is_none());
    let refusals = launch_refusals(&store, &w);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].contains("declaration"),
        "the refusal names the undeclarable surface: {}",
        refusals[0]
    );
    assert!(!pending_ids(&store).contains(&rid));
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-DUP-01: the same durable request delivered twice reconciles by
/// request identity — one Invocation, one spawn, no second writer.
fn duplicate_delivery_starts_no_second_writer(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-dup"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-dup"));
    let argv = wire_argv(bin, "clean-exit", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);

    sup.consume_requests(&store, NOW).unwrap();
    // The delivered-twice window: a second consume pass sees the same
    // durable request already consumed — it must not start a writer.
    sup.consume_requests(&store, NOW).unwrap();
    assert_eq!(
        calls.get(),
        1,
        "one spawn across both deliveries — the port boundary counted every attempt"
    );
    let invocations = store
        .transact(|tx| tx.invocations_for_execution(&e))
        .unwrap();
    assert_eq!(invocations.len(), 1, "one Invocation for the request");

    drive_to_done(&sup, &store, &rid);
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// A worker report outside the contract — malformed bytes, foreign
/// identity, absent, or oversize — classifies as bounded invalid or
/// absent evidence: retained, never adopted, never an empty success.
fn report_class_leg(
    bin: &str,
    tag: &str,
    mode: &str,
    want_class_prefix: &str,
    want_reason: &str,
) -> WireRun {
    let root = tmp_root(tag);
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}"));
    let argv = wire_argv(bin, "clean-exit", &["--report-mode", mode]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);

    let facts = report_facts(&store, &e);
    assert_eq!(facts.len(), 1, "{tag}: exactly one report fact: {facts:?}");
    let ObservationFact::WorkerReport { classification, .. } = &facts[0] else {
        unreachable!()
    };
    let name = class_name(classification);
    assert!(
        name.starts_with(want_class_prefix),
        "{tag}: expected {want_class_prefix}*, got {name}"
    );
    if !want_reason.is_empty() {
        assert!(
            name.contains(want_reason),
            "{tag}: reason {want_reason} recorded: {name}"
        );
    }
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-REP-01: report bytes that are not JSON classify as bounded
/// invalid evidence.
fn malformed_report_is_bounded_evidence(bin: &str, tag: &str) -> WireRun {
    report_class_leg(
        bin,
        &format!("{tag}-malformed"),
        "malformed",
        "invalid:",
        "report.malformed",
    )
}

/// WW-REP-02: a well-formed report bound to a different invocation
/// identity is rejected as wrong-identity evidence.
fn stale_report_is_bounded_evidence(bin: &str, tag: &str) -> WireRun {
    report_class_leg(
        bin,
        &format!("{tag}-wrongid"),
        "wrong_identity",
        "invalid:",
        "report.wrong_invocation",
    )
}

/// WW-REP-03: no report at all classifies as absent — the Invocation
/// resolves without a validated report ever existing.
fn partial_report_is_bounded_evidence(bin: &str, tag: &str) -> WireRun {
    report_class_leg(bin, &format!("{tag}-skip"), "skip", "absent", "")
}

/// WW-REP-04: a report over the configured bound is rejected before
/// classification completes — bounded invalid evidence.
fn oversize_report_is_bounded_evidence(bin: &str, tag: &str) -> WireRun {
    report_class_leg(
        bin,
        &format!("{tag}-oversize"),
        "oversize",
        "invalid:",
        "report.oversize",
    )
}

/// WW-STOP-01: a cancel request is not terminal — the Execution sits
/// in `stopping` while the recorded writer is provably alive and
/// reaches `cancelled` only after the stop proof.
fn cancel_waits_for_stop_proof(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-stop"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-stop"));
    let argv = wire_argv(bin, "hang", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
    sup.consume_requests(&store, NOW).unwrap();
    sup.run_once(&store, NOW).unwrap();
    wait_for(
        || invocation_for(&store, &rid).is_some_and(|i| i.process_identity.is_some()),
        "writer launched",
    );

    cancel_work(&store, &w);
    for _ in 0..60 {
        sup.drive_stops(&store, NOW).unwrap();
        if execution_state(&store, &e) == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        execution_state(&store, &e),
        ExecutionState::Cancelled,
        "the Execution reaches cancelled only after the stop proof"
    );
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-STOP-02: a durable stop intent survives a supervisor restart —
/// the restarted supervisor resumes signaling and inspection from the
/// recorded intent and the already-written report is collected once.
fn stop_intent_resumes_after_restart(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-stop-restart"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w, e, rid) = admit(&store, &format!("wire-{tag}-stoprst"));
    let argv = wire_argv(bin, "hang", &[]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv.clone());
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
    // the descendant snapshot — is durable but the restarted
    // supervisor holds no memory of it.
    cancel_work(&store, &w);
    let record = sup
        .record_stop_intent(&store, &invocation, &leader)
        .unwrap();
    assert!(
        matches!(record, StopRecord::Recorded(_)),
        "the intent persisted before the crash: {record:?}"
    );
    drop(sup);

    let (restarted, _calls2) = wire_sup(&root, &state_root, &src, argv);
    for _ in 0..60 {
        restarted.run_once(&store, NOW).unwrap();
        if execution_state(&store, &e) == ExecutionState::Cancelled {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        execution_state(&store, &e),
        ExecutionState::Cancelled,
        "the durable intent carried the stop across the restart"
    );
    // The report the worker wrote before hanging is collected exactly
    // once at the proven stop — never duplicated, never lost.
    let facts = report_facts(&store, &e);
    assert_eq!(facts.len(), 1, "one collected report: {facts:?}");
    WireRun {
        signature: collect_signature(&store, &w, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-ART-01: a produced commit hands off through the durable
/// export → acknowledge path, the declared verification runs on the
/// exact produced commit, and an agreeing `claimed_head` is recorded
/// as evidence — never authority.
fn produced_commit_is_verified_and_handed_off(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-artifact"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let store = SqliteStore::in_memory(1).unwrap();
    let (src, _base, newer) = git_source(&root);
    let (_w, e, rid) = admit_ex(
        &store,
        &format!("wire-{tag}-artifact"),
        vec![vec!["sh".into(), "-c".into(), "true".into()]],
    );
    let argv = wire_argv(bin, "clean-exit", &["--git-commit", "--claim-head", "HEAD"]);
    let (sup, calls) = wire_sup(&root, &state_root, &src, argv);
    sup.consume_requests(&store, NOW).unwrap();
    drive_to_done(&sup, &store, &rid);
    sup.drive_handoffs(&store).unwrap();

    let inv = invocation_for(&store, &rid).expect("invocation recorded");
    let row = handoff_row(&store, &inv.id).expect("handoff row recorded");
    assert_eq!(row.state, HandoffState::Acknowledged, "{row:?}");
    let produced = row.produced.clone().expect("produced recorded");
    assert!(row.manifest_digest.is_some(), "manifest bound");

    // The produced commit is exactly the managed branch's head — the
    // commit the worker made — and it descends from the pinned base.
    let ws = managed_workspace(&root, &row.work_id);
    let head = git(&ws, &["rev-parse", &format!("work/{}", row.work_id.0)]);
    assert_eq!(produced, head);
    assert_eq!(row.base, newer, "the base is the request's pin");
    git(&ws, &["merge-base", "--is-ancestor", &newer, &produced]);

    // The declared verification ran on the exact produced commit.
    let results = store
        .transact(|tx| tx.verification_results(&row.artifact_id))
        .unwrap();
    assert_eq!(results.len(), 1, "one declared check: {results:?}");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    assert_eq!(results[0].produced, produced);

    // The agreeing claim is recorded as a verdict, nothing more.
    let facts = claim_facts(&store, &e);
    assert_eq!(facts.len(), 1, "one verdict fact: {facts:?}");
    let ObservationFact::ClaimedHeadCheck { agrees, .. } = &facts[0] else {
        unreachable!()
    };
    assert!(agrees, "the claim named the promoted commit");

    WireRun {
        signature: collect_signature(&store, &row.work_id, &e, calls.get()),
        bound_adapter: bound_adapter_id(&store, &rid),
    }
}

/// WW-SWAP-01: a `worker_argv` change binds only requests created
/// after the swap — the in-flight Invocation keeps its recorded
/// adapter argv and runtime, reconciles without a second spawn, and
/// the new request binds the new declaration. No running Invocation
/// migrates.
fn config_only_swap_binds_only_new_requests(bin: &str, tag: &str) -> WireRun {
    let root = tmp_root(&format!("{tag}-swap"));
    let state_root = root.join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    // Two Works must hold capacity slots at once: the first writer is
    // still in flight when the second request binds.
    let store = SqliteStore::in_memory(2).unwrap();
    let (src, _base, _newer) = git_source(&root);
    let (w1, e1, rid1) = admit(&store, &format!("wire-{tag}-swap-a"));
    let argv_a = wire_argv(bin, "hang", &["--runtime-version", "rt-a"]);
    let (sup_a, _calls_a) = wire_sup(&root, &state_root, &src, argv_a);
    sup_a.consume_requests(&store, NOW).unwrap();
    wait_for(
        || invocation_for(&store, &rid1).is_some_and(|i| i.process_identity.is_some()),
        "first writer launched",
    );
    let identity_before = invocation_for(&store, &rid1)
        .unwrap()
        .process_identity
        .clone();

    // The configuration changes; the second Work's request is created
    // only now, so it is the first request eligible for the new argv.
    let (w2, e2, rid2) = admit(&store, &format!("wire-{tag}-swap-b"));
    let argv_b = wire_argv(bin, "hang", &["--runtime-version", "rt-b"]);
    let (sup_b, calls_b) = wire_sup(&root, &state_root, &src, argv_b);
    sup_b.consume_requests(&store, NOW).unwrap();

    // The in-flight Invocation reconciles on its recorded binding:
    // same process identity, no second spawn for its request.
    assert_eq!(
        invocation_for(&store, &rid1).unwrap().process_identity,
        identity_before,
        "the recorded writer identity never moved"
    );
    assert_eq!(
        calls_b.get(),
        1,
        "exactly one spawn under the new argv — the new request's writer"
    );
    assert!(pending_ids(&store).is_empty());
    let inv2 = invocation_for(&store, &rid2).expect("second invocation");
    assert!(inv2.process_identity.is_some());

    // Both writers are stopped through the durable stop path; the
    // signature records the post-swap durable state for both Works —
    // collected per-Work below.
    cancel_work(&store, &w1);
    cancel_work(&store, &w2);
    for _ in 0..80 {
        sup_b.drive_stops(&store, NOW).unwrap();
        if execution_state(&store, &e1) == ExecutionState::Cancelled
            && execution_state(&store, &e2) == ExecutionState::Cancelled
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(execution_state(&store, &e1), ExecutionState::Cancelled);
    assert_eq!(execution_state(&store, &e2), ExecutionState::Cancelled);
    // The swap is configuration-only: each request kept the binding
    // recorded at its own consume — provable through the durable
    // rows, never through the worker's identity.
    assert_ne!(rid1, rid2);
    // The compared signature is the second Work's row; the first
    // Work's assertions above are the swap-specific half.
    WireRun {
        signature: collect_signature(&store, &w2, &e2, calls_b.get()),
        bound_adapter: bound_adapter_id(&store, &rid2),
    }
}

// ---- the scenario table -------------------------------------------------

struct WireScenario {
    id: &'static str,
    /// The acceptance-criteria row this scenario proves.
    criterion: &'static str,
    run: fn(&str, &str) -> WireRun,
}

fn wire_scenarios() -> Vec<WireScenario> {
    vec![
        WireScenario {
            id: "WW-REQ-01",
            criterion: "complete request → process boundary → validated report",
            run: coding_request_runs_the_wire,
        },
        WireScenario {
            id: "WW-ROLE-FIX",
            criterion: "supported role: fix",
            run: fix_role_runs_the_wire,
        },
        WireScenario {
            id: "WW-ROLE-IC",
            criterion: "supported role: invariant_check (read-only)",
            run: invariant_check_role_runs_the_wire,
        },
        WireScenario {
            id: "WW-ROLE-REVIEW",
            criterion: "supported role: review (read-only)",
            run: review_role_runs_the_wire,
        },
        WireScenario {
            id: "WW-DECL-01",
            criterion: "declaration: unanswered describe is a repair",
            run: unanswered_declaration_is_a_configuration_repair,
        },
        WireScenario {
            id: "WW-DECL-02",
            criterion: "declaration/version drift: runtime mismatch",
            run: bound_runtime_drift_is_a_configuration_repair,
        },
        WireScenario {
            id: "WW-DECL-03",
            criterion: "declaration drift: role mismatch refuses pre-spawn",
            run: declaration_role_drift_is_refused,
        },
        WireScenario {
            id: "WW-DECL-04",
            criterion: "declaration drift: missing capability refuses pre-spawn",
            run: declaration_capability_drift_is_refused,
        },
        WireScenario {
            id: "WW-DECL-05",
            criterion: "declaration drift: legacy envelope cannot negotiate",
            run: legacy_declaration_cannot_negotiate,
        },
        WireScenario {
            id: "WW-DUP-01",
            criterion: "duplicates: redelivery starts no second writer",
            run: duplicate_delivery_starts_no_second_writer,
        },
        WireScenario {
            id: "WW-REP-01",
            criterion: "malformed report classifies as bounded invalid evidence",
            run: malformed_report_is_bounded_evidence,
        },
        WireScenario {
            id: "WW-REP-02",
            criterion: "stale report classifies as bounded invalid evidence",
            run: stale_report_is_bounded_evidence,
        },
        WireScenario {
            id: "WW-REP-03",
            criterion: "absent report classifies as bounded absent evidence",
            run: partial_report_is_bounded_evidence,
        },
        WireScenario {
            id: "WW-REP-04",
            criterion: "oversize report classifies as bounded invalid evidence",
            run: oversize_report_is_bounded_evidence,
        },
        WireScenario {
            id: "WW-STOP-01",
            criterion: "stop: cancel waits for stop proof",
            run: cancel_waits_for_stop_proof,
        },
        WireScenario {
            id: "WW-STOP-02",
            criterion: "stop/restart: durable intent resumes the stop",
            run: stop_intent_resumes_after_restart,
        },
        WireScenario {
            id: "WW-ART-01",
            criterion: "artifact verification: handoff + independent checks",
            run: produced_commit_is_verified_and_handed_off,
        },
        WireScenario {
            id: "WW-SWAP-01",
            criterion: "config-only swap binds only new requests",
            run: config_only_swap_binds_only_new_requests,
        },
    ]
}

// ---- retained results ----------------------------------------------------

fn git_stdout(argv: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(argv).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

/// The fail-closed revision stamp: two full commit ids over a pristine
/// tracked tree, or no stamp at all — never a placeholder.
fn revision_stamp() -> Option<(String, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !status.status.success() || !String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return None;
    }
    let tested = git_stdout(&["-C", root.to_str()?, "rev-parse", "HEAD"])?;
    // The PR base is the fork point, never `HEAD^` — on a multi-commit
    // branch that is the previous commit, and in a depth-1 checkout it
    // does not resolve at all.
    let base = git_stdout(&["-C", root.to_str()?, "merge-base", "HEAD", "origin/main"])
        .or_else(|| git_stdout(&["-C", root.to_str()?, "rev-parse", "HEAD^"]))?;
    Some((tested, base))
}

struct WireRow {
    id: &'static str,
    criterion: &'static str,
    signature: WireSignature,
    bound_adapter: Option<String>,
}

/// Run every scenario unchanged against one bound implementation.
fn run_wire_suite(tag: &str, bin: &str) -> Vec<WireRow> {
    wire_scenarios()
        .into_iter()
        .map(|s| {
            let run = (s.run)(bin, &format!("{tag}-{}", s.id.to_lowercase()));
            WireRow {
                id: s.id,
                criterion: s.criterion,
                signature: run.signature,
                bound_adapter: run.bound_adapter,
            }
        })
        .collect()
}

fn row_json(row: &WireRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "criterion": row.criterion,
        "result": "pass",
        "signature": format!("{:?}", row.signature),
        "skipped": null,
        "inconclusive": null,
    })
}

// ---- adapter identity boundary ------------------------------------------
// The only place worker adapter identities live: the binaries the
// suite binds, the name list the static check scans for, and the one
// comparison of bound identities. Adapter names stay plain data even
// here; outside this boundary no line may branch on an identity field.

/// Adapter identities the suite may be bound to — the set the static
/// check scans the whole file for.
const SUITE_WIRE_ADAPTERS: &[&str] = &["workloop-fixture-worker", "workloop-contract-worker"];

/// The reference worker binary — the suite's baseline implementation.
fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

/// The independent deterministic contract Worker binary.
fn contract_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-contract-worker")
        .to_string_lossy()
        .into_owned()
}

/// The argv one bound implementation receives for a scenario leg:
/// `<bin> <mode> --wire v2 <extra...>`. `bin` is data supplied by the
/// boundary callers — no case sees or branches on which it is.
fn wire_argv(bin: &str, mode: &str, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        mode.to_string(),
        "--wire".into(),
        "v2".into(),
    ];
    for flag in extra {
        argv.push((*flag).to_string());
    }
    argv
}

/// A `taisan.worker.declaration/1` envelope with caller-chosen
/// roles/capabilities for the `--decl-file` drift surface — the
/// declared adapter id is deliberately neutral: the file models a
/// drifted declaration, not either supplied implementation.
fn drifted_decl(roles: &[&str], capabilities: &[&str]) -> String {
    serde_json::json!({
        "format_version": "taisan.worker.declaration/1",
        "adapter": "wire-suite-declaration",
        "adapter_version": "1.0.0",
        "runtime": {"product": "suite-runtime", "version": "suite-1.0.0"},
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

/// The one designated comparison of bound adapter identities — the
/// non-vacuity proof the named equality run delegates to this
/// boundary, so no case outside it branches on which worker is bound.
/// Returns whether both rows bound a real declaration (and asserts
/// the bound identities differ and stay inside the declared set).
fn bound_pair_exercised(x: &WireRow, y: &WireRow) -> bool {
    let (Some(xa), Some(ya)) = (&x.bound_adapter, &y.bound_adapter) else {
        return false;
    };
    assert_ne!(
        xa, ya,
        "{}: the bound adapter is the only variable that changed",
        x.id
    );
    assert!(
        SUITE_WIRE_ADAPTERS.contains(&xa.as_str()),
        "{}: reference adapter in the declared set",
        x.id
    );
    assert!(
        SUITE_WIRE_ADAPTERS.contains(&ya.as_str()),
        "{}: bound adapter in the declared set",
        y.id
    );
    true
}

// ---- end adapter identity boundary --------------------------------------

/// The identical suite executed once per supplied conforming
/// implementation: every row's durable signature — invocation states,
/// writer identities, report classifications, refusal codes, handoff
/// states, claim verdicts, spawn counts — must be equal across the
/// two runs, and at least one row must have bound a real declaration
/// in both so the adapter difference is exercised, not vacuous.
#[test]
fn the_identical_wire_suite_passes_under_each_implementation() {
    let reference = run_wire_suite("u268-ref", &fixture_bin());
    let rerun = run_wire_suite("u268-rerun", &contract_bin());
    assert_eq!(reference.len(), rerun.len(), "the identical case table");
    assert_eq!(reference.len(), wire_scenarios().len());
    let mut bound_pairs = 0;
    for (a, b) in reference.iter().zip(&rerun) {
        assert_eq!(a.id, b.id, "the identical suite table");
        assert_eq!(
            a.signature, b.signature,
            "{}: identical wire outcome under either implementation",
            a.id
        );
        if bound_pair_exercised(a, b) {
            bound_pairs += 1;
        }
    }
    assert!(
        bound_pairs > 0,
        "at least one case bound a real declaration in both runs — \
         the adapter difference is exercised, not vacuous"
    );

    // Retain the criterion→result matrix with the exact implementation
    // revision and sanitized configuration — only when the checkout
    // yields a faithful stamp.
    if let Some((tested, base)) = revision_stamp() {
        let retained = serde_json::json!({
            "suite": "worker-wire-contract",
            "issue": 268,
            "tested_revision": tested,
            "base_revision": base,
            "claim_class": "fixture_supported",
            "evidence_class": "contract_fixture_not_live",
            "skipped": [],
            "inconclusive": [],
            "sanitized_config": {
                "wire": ["taisan.worker.request/2", "taisan.worker.report/2"],
                "transport": "run --request <path> --out <dir>",
                "declaration": "taisan.worker.declaration/1 via describe",
                "isolation_profile": workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID,
                "process_control": "posix (spawn-counting decorator)",
                "store": "sqlite in-memory",
                "workspace": "managed per-Work git worktree",
            },
            "implementations": [
                {"role": "reference", "rows": reference.iter().map(row_json).collect::<Vec<_>>()},
                {"role": "contract", "rows": rerun.iter().map(row_json).collect::<Vec<_>>()},
            ],
        });
        let records = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/worker-wire-contract.json");
        std::fs::write(
            &records,
            format!("{}\n", serde_json::to_string_pretty(&retained).unwrap()),
        )
        .unwrap_or_else(|e| {
            panic!(
                "retain the wire-contract record at {}: {e}",
                records.display()
            )
        });
    } else {
        eprintln!(
            "worker-wire-contract: no faithful revision stamp (dirty or \
             non-git checkout) — the suite ran; no records were retained"
        );
    }
}

/// Static check: no line branches on which worker is bound. Adapter
/// name literals may appear only inside the marked boundary below and
/// never on a conditional, comparison, or assertion line; the
/// identity-revealing identifiers appear outside the boundary only on
/// branch-free data lines.
#[test]
fn wire_suite_source_has_no_adapter_name_branch() {
    const BEGIN: &str = "// ---- adapter identity boundary";
    const END: &str = "// ---- end adapter identity boundary";
    const BRANCH_TOKENS: &[&str] = &[
        "if ",
        "if(",
        "match ",
        "matches!",
        "==",
        "!=",
        "=>",
        "assert",
        ".contains(",
        "starts_with(",
        "ends_with(",
    ];
    /// Beyond the adapter-name literals (scanned via
    /// `SUITE_WIRE_ADAPTERS`), these identifiers expose which worker is
    /// bound — a branch on any of them decides adapter identity even
    /// without naming an adapter.
    const IDENTITY_TOKENS: &[&str] = &[
        "fixture_bin",
        "contract_bin",
        "SUITE_WIRE_ADAPTERS",
        "bound_adapter",
        "adapter_id",
        "adapter_version",
        "runtime_version",
        "capability_digest",
        "worker_argv",
    ];
    let lines: Vec<&str> = include_str!("worker_wire_contract.rs").lines().collect();
    let markers = |marker: &str| {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_start().starts_with(marker))
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    let begins = markers(BEGIN);
    let ends = markers(END);
    assert_eq!(begins.len(), 1, "exactly one boundary-begin marker");
    assert_eq!(ends.len(), 1, "exactly one boundary-end marker");
    let (begin, end) = (begins[0], ends[0]);
    assert!(begin < end, "the boundary opens before it closes");
    for (n, line) in lines.iter().enumerate() {
        let inside_boundary = n > begin && n < end;
        let branches = BRANCH_TOKENS.iter().any(|t| line.contains(t));
        for name in SUITE_WIRE_ADAPTERS {
            if line.contains(name) {
                assert!(
                    inside_boundary,
                    "adapter name outside the identity boundary (line {}): {line}",
                    n + 1
                );
                assert!(
                    !branches,
                    "adapter-name conditional (line {}): {line}",
                    n + 1
                );
            }
        }
        if !inside_boundary {
            for token in IDENTITY_TOKENS {
                if line.contains(token) {
                    assert!(
                        !branches,
                        "adapter-identity branch outside the identity boundary (line {}): {line}",
                        n + 1
                    );
                }
            }
        }
    }
}
