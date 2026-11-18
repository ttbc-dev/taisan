//! Coordinator contract tests for intake (#203) and the bounded
//! exchange integration (#201): real adapter scripts driven through
//! `run_intake` against a real store — preflight sizing, exclusion
//! scope, ordered admission through `submit`, replay, capacity
//! deferral, failure classification, and the lease/active-run guards.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use workloop_core::command::{CommandBody, CommandDecision, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, Origin, WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability, HostInstanceId,
    IssueId, IssueRef, RepositoryId, RepositoryRef, RoleBinding, SourceSnapshot,
};
use workloop_core::ports::ProcessControlPort;
use workloop_core::store::{
    Effect, IntakeCandidateStatus, IntakeRun, IntakeRunState, IntakeSettingsRecord, IntakeTrigger,
    Store,
};
use workloop_core::work::{HostTemplate, WorkloopConfig, handle_command, set_intake_settings};
use workloop_exec::supervisor::PosixProcessControl;
use workloop_intake::{IntakeError, IntakeParams, IntakeReport, SelectorBinding, run_intake};
use workloop_selector::admission;
use workloop_selector::protocol;
use workloop_selector::{Proposal, Provenance, SourceBinding, TaskInput, proposal_key};
use workloop_store::sqlite::{RunLeaseOwner, SqliteStore};

const NOW: i64 = 1_700_000_000;
const NOW_MS: i64 = NOW * 1000;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-intake-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo(name: &str) -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted(name))
}

fn issue(name: &str) -> IssueRef {
    IssueRef {
        repository: repo("repo-a"),
        issue: IssueId::trusted(name),
        display_number: None,
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

fn publisher() -> HostActorRef {
    HostActorRef {
        host: host(),
        actor: HostActorId::trusted("pub-agent"),
    }
}

/// `issue_read` mirrors what `required_capabilities` demands of a
/// source-bound submit: sourced Work needs `IssueRead` on top of the
/// publication baseline.
fn config(issue_read: bool) -> WorkloopConfig {
    let mut capabilities = vec![
        HostCapability::RepositoryRead,
        HostCapability::PushRef,
        HostCapability::ChangeRequestCreate,
        HostCapability::ChangeRequestRead,
        HostCapability::ChecksRead,
        HostCapability::ReviewFindingsRead,
    ];
    if issue_read {
        capabilities.push(HostCapability::IssueRead);
    }
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("fixture"),
            repository: repo("repo-a"),
            contract_version: 1,
            actors: vec![RoleBinding {
                role: ActorRole::Publication,
                actor: publisher(),
                credential_ref: CredentialRef::trusted("cred"),
            }],
        },
        declared_capabilities: capabilities.into_iter().collect(),
        lanes: Default::default(),
        required_checks: vec![],
        allowed_effects: AllowedEffects {
            workspace_write: true,
            host_write: true,
            network: false,
        },
        budget: ExecutionBudget::default(),
        policy: WorkPolicy::default(),
    }
}

/// A proposal exactly matching the request fields every fixture in
/// this file carries: constraints `["constraint-a"]`, argv `["true"]`,
/// permissions `["perm-a"]`.
fn proposal(issue_name: &str, intent: &str) -> Proposal {
    let source = SourceBinding {
        issue: issue(issue_name),
        revision: "rev-1".to_string(),
        content_digest: "digest-1".to_string(),
    };
    let task = TaskInput {
        intent: intent.to_string(),
        constraints: vec!["constraint-a".to_string()],
        verification_argv: vec!["true".to_string()],
    };
    let permissions = vec!["perm-a".to_string()];
    let sources = vec![source];
    Proposal {
        key: proposal_key("test-sel", 1, 7, &sources, &task, &permissions),
        selector: AdapterId::trusted("test-sel"),
        selector_version: 1,
        policy_revision: 7,
        sources,
        task,
        required_permissions: permissions,
        provenance: Provenance::default(),
    }
}

/// The success-response template the fixture script emits; the real
/// `request_id` is substituted for `__RID__` at run time.
fn success_template(proposals: &[Proposal]) -> String {
    String::from_utf8(protocol::success_response("__RID__", proposals)).unwrap()
}

/// Write a `#!/bin/sh` adapter that captures the request, substitutes
/// the real request id into `template`, and prints the response.
fn adapter_script(dir: &Path, name: &str, template: &str, capture: &Path) -> String {
    let template_path = dir.join(format!("{name}.template"));
    std::fs::write(&template_path, template).unwrap();
    let script = format!(
        "#!/bin/sh\nreq=$(cat)\nprintf '%s' \"$req\" > '{}'\nrid=$(printf '%s' \"$req\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\nsed \"s/__RID__/$rid/g\" '{}'\n",
        capture.display(),
        template_path.display()
    );
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn raw_script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

struct Fixture {
    store: SqliteStore,
    config: WorkloopConfig,
    actor: Actor,
    constraints: Vec<String>,
    argv: Vec<String>,
    permissions: Vec<String>,
    repositories: Vec<RepositoryRef>,
    cwd: PathBuf,
    binding: SelectorBinding,
}

fn fixture(dir: PathBuf, script: String) -> Fixture {
    Fixture {
        store: SqliteStore::in_memory(4).unwrap(),
        config: config(true),
        actor: actor(),
        constraints: vec!["constraint-a".to_string()],
        argv: vec!["true".to_string()],
        permissions: vec!["perm-a".to_string()],
        repositories: vec![repo("repo-a"), repo("repo-b")],
        cwd: dir,
        binding: SelectorBinding {
            adapter: "test-sel".into(),
            selector_version: 1,
            policy_revision: 7,
            argv: vec!["/bin/sh".into(), script],
        },
    }
}

fn params<'a>(f: &'a Fixture, binding: &'a SelectorBinding, timeout: Duration) -> IntakeParams<'a> {
    IntakeParams {
        trigger: IntakeTrigger::Manual,
        binding,
        actor: &f.actor,
        work_config: &f.config,
        repositories: &f.repositories,
        task_constraints: &f.constraints,
        verification_argv: &f.argv,
        required_permissions: &f.permissions,
        settings_revision: None,
        cwd: &f.cwd,
        timeout,
    }
}

fn run(f: &Fixture, timeout: Duration) -> IntakeReport {
    let p = params(f, &f.binding, timeout);
    run_intake(&f.store, &p, NOW, NOW_MS).expect("run_intake succeeds")
}

fn run_result(f: &Fixture, timeout: Duration) -> Result<IntakeReport, IntakeError> {
    let p = params(f, &f.binding, timeout);
    run_intake(&f.store, &p, NOW, NOW_MS)
}

fn captured_request(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("request captured");
    serde_json::from_str(&text).expect("request is JSON")
}

/// A plain manual submit through the ordinary command path.
fn manual_submit(store: &SqliteStore, cfg: &WorkloopConfig, key: &str, sources: Vec<IssueRef>) {
    let d = handle_command(
        store,
        cfg,
        &CommandRequest {
            actor: actor(),
            origin: Origin("cli".into()),
            idempotency_key: key.into(),
            body: CommandBody::Submit {
                intent: format!("manual-{key}"),
                sources: sources
                    .into_iter()
                    .map(|issue| SourceSnapshot {
                        issue,
                        source_revision: "rev-0".into(),
                        content_digest: "digest-0".into(),
                    })
                    .collect(),
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    assert!(
        matches!(d, CommandDecision::Accepted { .. }),
        "manual submit {key}: {d:?}"
    );
}

fn settings(revision: &str, limit: u32) -> IntakeSettingsRecord {
    IntakeSettingsRecord {
        settings_revision: revision.into(),
        pending_queue_limit: limit,
        recorded_seq: 1,
    }
}

// --- happy path and request shaping ----------------------------------------

#[test]
fn run_intake_admits_candidates_in_order_through_submit() {
    let dir = tmp_root("order");
    let proposals = vec![
        proposal("issue-1", "first work"),
        proposal("issue-2", "second work"),
        proposal("issue-3", "third work"),
    ];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.detail_code, None);
    assert!(report.launched);
    assert_eq!(report.candidates.len(), 3);
    for (i, c) in report.candidates.iter().enumerate() {
        assert_eq!(c.ordinal, i as u32);
        assert_eq!(c.status, IntakeCandidateStatus::Accepted, "{c:?}");
        assert!(c.work_id.is_some());
        assert!(c.command_id.is_some());
        assert_eq!(
            c.replay_key,
            admission::replay_key(&proposals[i]),
            "replay key derives from the proposal"
        );
        assert!(c.replay_key.starts_with("proposal/test-sel:"));
    }

    // The request carried the free-position bound (default limit 10,
    // empty queue) and both configured repositories.
    let request = captured_request(&capture);
    assert_eq!(request["max_candidates"], 10);
    assert_eq!(request["selector"], "test-sel");
    assert_eq!(request["protocol_version"], "1");
    assert_eq!(request["repositories"].as_array().unwrap().len(), 2);

    // Every candidate's submit committed a Work claiming its issue,
    // and the decision is durable under the replay identity.
    for (i, c) in report.candidates.iter().enumerate() {
        let work_id = c.work_id.clone().unwrap();
        let spec = f
            .store
            .transact_deferred(|tx| tx.spec(&work_id, None))
            .unwrap()
            .expect("spec exists");
        assert_eq!(spec.payload.intent, proposals[i].task.intent);
        let claim = f
            .store
            .transact_deferred(|tx| tx.issue_claim(&issue(&format!("issue-{}", i + 1))))
            .unwrap();
        assert!(claim.is_some(), "issue claim held for candidate {i}");
        let decision = f
            .store
            .transact_deferred(|tx| {
                tx.stored_decision(&Origin("selector".into()), &f.actor, &c.replay_key)
            })
            .unwrap();
        assert!(decision.is_some(), "ledger decision for {i}");
    }

    // The durable candidate rows carry the deciding command's commit
    // sequence.
    let durable = f
        .store
        .transact_deferred(|tx| tx.intake_candidates(&report.run_id))
        .unwrap();
    assert_eq!(durable.len(), 3);
    assert!(durable.iter().all(|c| c.committed_seq.is_some()));
}

#[test]
fn run_intake_with_empty_response_completes_without_candidates() {
    let dir = tmp_root("empty");
    let script = adapter_script(
        &dir,
        "sel.sh",
        &success_template(&[]),
        &dir.join("req.json"),
    );
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Completed);
    assert!(report.candidates.is_empty());
    assert!(report.launched);
}

#[test]
fn run_intake_sizes_max_candidates_to_free_positions() {
    let dir = tmp_root("sizing");
    // Occupy 8 of the default 10 pending positions, then run intake
    // against the same store: the ask shrinks to the 2 free slots.
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    for i in 0..8 {
        manual_submit(&store, &cfg, &format!("fill-{i}"), vec![]);
    }
    let proposals = vec![proposal("issue-1", "one"), proposal("issue-2", "two")];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    let request = captured_request(&capture);
    assert_eq!(request["max_candidates"], 2, "free positions size the ask");
    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates.len(), 2);
    assert!(
        report
            .candidates
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::Accepted)
    );
}

#[test]
fn run_intake_skips_without_launching_when_queue_is_full() {
    let dir = tmp_root("full");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    for i in 0..10 {
        manual_submit(&store, &cfg, &format!("fill-{i}"), vec![]);
    }
    let marker = dir.join("launched.marker");
    let script = raw_script(&dir, "sel.sh", &format!("touch '{}'", marker.display()));
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Skipped);
    assert_eq!(report.detail_code.as_deref(), Some("queue_full"));
    assert!(!report.launched);
    assert!(report.candidates.is_empty());
    assert!(!marker.exists(), "adapter must not launch at zero capacity");
}

#[test]
fn run_intake_excludes_owned_issues_and_scopes_repositories() {
    let dir = tmp_root("exclude");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    // A queued Work already owns issue-1 in repo-a.
    manual_submit(&store, &cfg, "owner-1", vec![issue("issue-1")]);
    let proposals = vec![proposal("issue-2", "free work")];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    let request = captured_request(&capture);
    let excluded = request["excluded_sources"].as_array().unwrap();
    assert_eq!(excluded.len(), 1, "one owned issue excluded: {excluded:?}");
    assert_eq!(excluded[0]["issue"], "issue-1");
    assert_eq!(excluded[0]["repository"]["repository"], "repo-a");
    // The proposal for the unowned issue admits normally.
    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates[0].status, IntakeCandidateStatus::Accepted);
}

// --- failure classification -------------------------------------------------

#[test]
fn selector_nonzero_exit_fails_the_run_without_candidates() {
    let dir = tmp_root("exit3");
    let script = raw_script(&dir, "sel.sh", "exit 3");
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(report.detail_code.as_deref(), Some("exchange_exit"));
    // A transport-level failure never reaches response validation.
    assert!(!report.launched);
    assert!(report.candidates.is_empty());
}

#[test]
fn selector_malformed_output_is_a_bounded_failure() {
    let dir = tmp_root("malformed");
    let script = raw_script(&dir, "sel.sh", "printf 'this is not json'");
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(report.detail_code.as_deref(), Some("malformed_response"));
    assert!(report.launched);
    assert!(report.candidates.is_empty());
}

#[test]
fn selector_contract_violation_fails_the_whole_response() {
    let dir = tmp_root("violation");
    let proposals = vec![proposal("issue-1", "work")];
    let mut template: serde_json::Value =
        serde_json::from_slice(&protocol::success_response("__RID__", &proposals)).unwrap();
    template["protocol_version"] = serde_json::json!("2");
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &template.to_string(), &capture);
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(
        report.detail_code.as_deref(),
        Some("contract_protocol_version")
    );
    assert!(report.candidates.is_empty());
}

#[test]
fn selector_declared_failure_maps_to_a_bounded_code() {
    let dir = tmp_root("adapterfail");
    let template = String::from_utf8(protocol::failure_response(
        Some("__RID__"),
        protocol::FailureCode::SourceUnavailable,
        "the source host is down",
    ))
    .unwrap();
    let script = adapter_script(&dir, "sel.sh", &template, &dir.join("req.json"));
    let f = fixture(dir.clone(), script);
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(
        report.detail_code.as_deref(),
        Some("adapter_source_unavailable")
    );
    assert!(report.candidates.is_empty());
}

#[test]
fn selector_timeout_is_bounded() {
    let dir = tmp_root("timeout");
    let script = raw_script(&dir, "sel.sh", "sleep 30");
    let f = fixture(dir.clone(), script);
    let start = Instant::now();
    let report = run(&f, Duration::from_millis(400));
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(report.detail_code.as_deref(), Some("exchange_deadline"));
}

#[test]
fn selector_environment_carries_no_inherited_secrets() {
    let dir = tmp_root("env");
    unsafe { std::env::set_var("WORKLOOP_TEST_LEAKED_SECRET", "s3cr3t-value") };
    let capture = dir.join("env.txt");
    let script = raw_script(&dir, "sel.sh", &format!("env > '{}'", capture.display()));
    let f = fixture(dir.clone(), script);
    let _ = run(&f, Duration::from_secs(10));
    unsafe { std::env::remove_var("WORKLOOP_TEST_LEAKED_SECRET") };
    let env = std::fs::read_to_string(&capture).expect("env captured");
    assert!(
        !env.contains("WORKLOOP_TEST_LEAKED_SECRET"),
        "inherited environment leaked into the selector:\n{env}"
    );
    assert!(
        !env.lines().any(|l| l.starts_with("CARGO")),
        "cargo vars leaked:\n{env}"
    );
}

#[test]
fn missing_adapter_binary_is_a_launch_failure() {
    let dir = tmp_root("nobin");
    let mut f = fixture(dir.clone(), String::new());
    f.binding.argv = vec!["/nonexistent/selector-binary".into()];
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Failed);
    assert_eq!(report.detail_code.as_deref(), Some("exchange_launch"));
    assert!(!report.launched);
    let _ = dir;
}

// --- admission semantics -----------------------------------------------------

#[test]
fn ordinary_rejection_is_recorded_and_later_candidates_continue() {
    let dir = tmp_root("reject");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    let actor = actor();

    // Seed an idempotency conflict for proposal A: same replay
    // identity, different payload — the ordinary path records the
    // conflict rejection.
    let a = proposal("issue-1", "conflicted work");
    let replay_a = admission::replay_key(&a);
    let seeded = handle_command(
        &store,
        &cfg,
        &CommandRequest {
            actor: actor.clone(),
            origin: Origin("selector".into()),
            idempotency_key: replay_a.clone(),
            body: CommandBody::Submit {
                intent: "a different payload entirely".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(seeded, CommandDecision::Accepted { .. }));

    let b = proposal("issue-2", "clean work");
    let proposals = vec![a, b];
    let script = adapter_script(
        &dir,
        "sel.sh",
        &success_template(&proposals),
        &dir.join("req.json"),
    );
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates.len(), 2);
    assert_eq!(report.candidates[0].status, IntakeCandidateStatus::Rejected);
    assert_eq!(
        report.candidates[0].status_reason.as_deref(),
        Some("idempotency_conflict")
    );
    assert_eq!(report.candidates[1].status, IntakeCandidateStatus::Accepted);
    assert!(report.candidates[1].work_id.is_some());
}

#[test]
fn rejected_decisions_replay_on_a_second_run() {
    let dir = tmp_root("replay");
    // Without IssueRead every sourced submit is refused — but the
    // refusal IS a saved decision, so a second identical run replays
    // rather than re-deciding.
    let store = SqliteStore::in_memory(4).unwrap();
    let proposals = vec![
        proposal("issue-1", "work one"),
        proposal("issue-2", "work two"),
    ];
    let script = adapter_script(
        &dir,
        "sel.sh",
        &success_template(&proposals),
        &dir.join("req.json"),
    );
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    f.config = config(false);

    let first = run(&f, Duration::from_secs(10));
    assert_eq!(first.state, IntakeRunState::Completed);
    assert!(
        first
            .candidates
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::Rejected)
    );
    assert!(
        first
            .candidates
            .iter()
            .all(|c| c.status_reason.as_deref() == Some("unsupported_capability"))
    );

    let second = run(&f, Duration::from_secs(10));
    assert_eq!(second.state, IntakeRunState::Completed);
    assert!(
        second
            .candidates
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::Replayed)
    );
    // The replayed rejection keeps its bounded reason.
    assert!(
        second
            .candidates
            .iter()
            .all(|c| c.status_reason.as_deref() == Some("unsupported_capability"))
    );
    // No Work was ever created.
    let works = f.store.transact_deferred(|tx| tx.works()).unwrap();
    assert!(works.is_empty(), "replayed refusals create no Work");
}

#[test]
fn a_busy_lease_refuses_the_invocation() {
    let dir = tmp_root("busy");
    let f = fixture(dir.clone(), String::new());
    // A live owner holds the installation intake lease.
    let identity = PosixProcessControl::identity_of(std::process::id() as i32).unwrap();
    let owner = RunLeaseOwner {
        pid: identity.pid,
        started_at: identity.started_at,
        nonce: "held-by-test".into(),
    };
    f.store
        .acquire_intake_lease(&owner, |r| PosixProcessControl.inspect(r), NOW)
        .unwrap();
    let err = run_result(&f, Duration::from_secs(10)).unwrap_err();
    match err {
        IntakeError::Busy { owner_pid } => assert_eq!(owner_pid, identity.pid),
        other => panic!("expected Busy, got {other:?}"),
    }
    let _ = dir;
}

#[test]
fn an_active_run_refuses_a_second_invocation() {
    let dir = tmp_root("activerun");
    let f = fixture(dir.clone(), String::new());
    // A durable open run exists (its owner is this live process, so it
    // is genuinely active).
    let identity = PosixProcessControl::identity_of(std::process::id() as i32).unwrap();
    f.store
        .transact(|tx| {
            let run = IntakeRun {
                run_id: "intake-stuck".into(),
                trigger: IntakeTrigger::ScheduledInterval,
                selector_id: AdapterId::trusted("test-sel"),
                selector_version: 1,
                policy_revision: 7,
                settings_revision: None,
                state: IntakeRunState::Open,
                detail_code: None,
                request_payload: b"{}".to_vec(),
                request_digest: "d".repeat(64),
                owner_pid: identity.pid,
                owner_started_at: identity.started_at.clone(),
                owner_nonce: "n".into(),
                created_unix_seconds: NOW,
                finished_unix_seconds: None,
                recorded_seq: tx.alloc_seq()?,
            };
            tx.apply(&Effect::InsertIntakeRun { run })
        })
        .unwrap();
    let err = run_result(&f, Duration::from_secs(10)).unwrap_err();
    match err {
        IntakeError::ActiveRun { run_id } => assert_eq!(run_id, "intake-stuck"),
        other => panic!("expected ActiveRun, got {other:?}"),
    }
    let _ = dir;
}

// --- capacity deferral --------------------------------------------------------

#[test]
fn mid_batch_capacity_loss_defers_the_unattempted_remainder() {
    let dir = tmp_root("capacity");
    let store = SqliteStore::in_memory(4).unwrap();
    // Start with a generous limit so preflight asks for a full batch.
    set_intake_settings(&store, settings("rev-a", 40), None).unwrap();

    let proposals: Vec<Proposal> = (0..12)
        .map(|i| proposal(&format!("issue-{i:02}"), &format!("work {i}")))
        .collect();
    let script = adapter_script(
        &dir,
        "sel.sh",
        &success_template(&proposals),
        &dir.join("req.json"),
    );
    let mut f = fixture(dir.clone(), script);
    f.store = store;

    // Drive the run on a thread; shrink the limit the moment the
    // validated batch lands — the deterministic mid-admission capacity
    // loss the atomic check exists for.
    let report = std::thread::scope(|s| {
        let runner = s.spawn(|| {
            let p = params(&f, &f.binding, Duration::from_secs(10));
            run_intake(&f.store, &p, NOW, NOW_MS).expect("run_intake succeeds")
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let active = f
                .store
                .transact_deferred(|tx| tx.active_intake_run())
                .unwrap();
            if active
                .as_ref()
                .is_some_and(|r| r.state == IntakeRunState::Admitting)
            {
                break;
            }
            assert!(Instant::now() < deadline, "run never reached admitting");
            std::thread::sleep(Duration::from_micros(200));
        }
        let current = f
            .store
            .transact_deferred(|tx| tx.intake_settings())
            .unwrap()
            .unwrap();
        set_intake_settings(
            &f.store,
            settings("rev-b", 2),
            Some(&current.settings_revision),
        )
        .unwrap();
        runner.join().unwrap()
    });

    assert_eq!(report.state, IntakeRunState::Completed);
    let deferred = report
        .candidates
        .iter()
        .filter(|c| c.status == IntakeCandidateStatus::CapacityDeferred)
        .count();
    assert!(
        deferred >= 1,
        "capacity loss must defer candidates: {report:?}"
    );
    // Once deferred, every later candidate defers too.
    let first_deferred = report
        .candidates
        .iter()
        .position(|c| c.status == IntakeCandidateStatus::CapacityDeferred)
        .unwrap();
    assert!(
        report.candidates[first_deferred..]
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::CapacityDeferred)
    );
    // Deferred candidates carry no decision links — a later run
    // decides them fresh.
    for c in &report.candidates[first_deferred..] {
        assert!(c.command_id.is_none());
        assert!(c.work_id.is_none());
        assert_eq!(c.status_reason.as_deref(), Some("capacity_exhausted"));
    }

    // Capacity deferral is transient: no decision was ever persisted
    // under a deferred replay key, so the ordinary path decides the
    // same proposal fresh once capacity returns.
    for c in &report.candidates[first_deferred..] {
        let decision = f
            .store
            .transact_deferred(|tx| {
                tx.stored_decision(&Origin("selector".into()), &f.actor, &c.replay_key)
            })
            .unwrap();
        assert!(
            decision.is_none(),
            "deferred candidate must hold no ledger decision"
        );
    }
    let current = f
        .store
        .transact_deferred(|tx| tx.intake_settings())
        .unwrap()
        .unwrap();
    set_intake_settings(
        &f.store,
        settings("rev-c", 40),
        Some(&current.settings_revision),
    )
    .unwrap();
    let deferred_proposal = &proposals[first_deferred];
    let command = admission::submit_command(deferred_proposal, f.actor.clone());
    let decision = handle_command(&f.store, &f.config, &command, NOW).unwrap();
    assert!(
        matches!(decision, CommandDecision::Accepted { .. }),
        "deferred proposal decides fresh: {decision:?}"
    );
}

/// A store fault inside one candidate's submit is never acknowledged:
/// the candidate records `unknown` with the bounded reason, admission
/// halts before the remainder, and the run stays `admitting` for
/// recovery — the earlier accepted Work is untouched and the aborted
/// submit leaves no partial rows.
#[test]
fn a_submit_store_fault_marks_the_candidate_unknown_and_halts() {
    let dir = tmp_root("unknown-candidate");
    let db = dir.join("workloop.db");
    let proposals = vec![
        proposal("issue-1", "first work"),
        proposal("issue-2", "second work"),
    ];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    // The fault needs a file-backed store: abort the second
    // selector-origin decision insert — the same mid-batch store
    // fault class the propose suite injects.
    f.store = SqliteStore::open(&db, 4).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE fault_counter (n INTEGER NOT NULL DEFAULT 0);
         INSERT INTO fault_counter (n) VALUES (0);
         CREATE TRIGGER mid_batch_fault BEFORE INSERT ON commands
         WHEN NEW.origin = 'selector'
         BEGIN
           UPDATE fault_counter SET n = n + 1;
           SELECT RAISE(ABORT, 'injected mid-batch fault')
             WHERE (SELECT n FROM fault_counter) >= 2;
         END;",
    )
    .unwrap();
    drop(conn);

    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.candidates.len(), 2);
    assert_eq!(
        report.candidates[0].status,
        IntakeCandidateStatus::Accepted,
        "{:?}",
        report.candidates[0]
    );
    assert_eq!(
        report.candidates[1].status,
        IntakeCandidateStatus::Unknown,
        "{:?}",
        report.candidates[1]
    );
    assert_eq!(
        report.candidates[1].status_reason.as_deref(),
        Some("submit_outcome_unknown")
    );
    // `unknown` is non-final: the halted run stays `admitting` for
    // the recovery pass rather than reporting completed.
    assert_eq!(report.state, IntakeRunState::Admitting);

    // Exactly one Work exists — the first candidate's. The aborted
    // submit committed nothing, and no ledger decision survives
    // under the second candidate's replay key.
    let works = f.store.transact_deferred(|tx| tx.works()).unwrap();
    assert_eq!(works.len(), 1, "{works:?}");
    let decision = f
        .store
        .transact_deferred(|tx| {
            tx.stored_decision(
                &Origin("selector".into()),
                &f.actor,
                &admission::replay_key(&proposals[1]),
            )
        })
        .unwrap();
    assert!(decision.is_none(), "aborted submit must leave no decision");
}
