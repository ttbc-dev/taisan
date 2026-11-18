//! Coordinator contract tests for intake (#203) and the bounded
//! exchange integration (#201): real adapter scripts driven through
//! `run_intake` against a real store — preflight sizing, exclusion
//! scope, ordered admission through `submit`, replay, capacity
//! deferral, failure classification, and the lease/active-run guards.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::{
    Actor, ActorKind, AllowedEffects, ExecutionBudget, ExecutionId, ExecutionState, Origin, WorkId,
    WorkPolicy,
};
use workloop_core::identity::{
    ActorRole, AdapterId, CredentialRef, HostActorId, HostActorRef, HostCapability, HostInstanceId,
    IssueId, IssueRef, RepositoryId, RepositoryRef, RoleBinding, SourceSnapshot,
};
use workloop_core::observation::Outcome;
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

/// A manual submit returning the created `(WorkId, ExecutionId)` —
/// the identities the finished-source and owner-state scenarios
/// settle or inspect.
fn submit_ids(
    store: &SqliteStore,
    cfg: &WorkloopConfig,
    key: &str,
    sources: Vec<IssueRef>,
) -> (WorkId, ExecutionId) {
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
    match d {
        CommandDecision::Accepted {
            work_id,
            effect:
                CommandEffect::WorkCreated { execution_id }
                | CommandEffect::ExecutionCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("manual submit {key}: {other:?}"),
    }
}

/// Force an Execution into `state` — the same durable shape the
/// supervisor's own transitions write, including the terminal
/// code/outcome a terminal state requires.
fn force_execution(store: &SqliteStore, id: &ExecutionId, to: ExecutionState) {
    store
        .transact(|tx| {
            let from = tx.execution(id)?.expect("execution exists").state.clone();
            let mut e = tx.execution(id)?.expect("execution exists");
            e.state = to.clone();
            if to.is_terminal() {
                e.terminal_code = Some("setup".into());
                e.outcome = Some(Outcome {
                    state: to.clone(),
                    code: Some("setup".into()),
                    detail: None,
                    evidence_refs: vec![],
                    recorded_seq: 0,
                });
            }
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: from,
            })
        })
        .unwrap();
}

/// Release a Work's issue claims — the durable half of safe terminal
/// settlement that lets a finished source be submitted again.
fn release_claims(store: &SqliteStore, work_id: &WorkId) {
    store
        .transact(|tx| {
            tx.apply(&Effect::ReleaseIssueClaims {
                work_id: work_id.clone(),
            })
        })
        .unwrap();
}

fn works_count(store: &SqliteStore) -> usize {
    store.transact_deferred(|tx| tx.works()).unwrap().len()
}

/// An adapter script that captures the request, announces launch,
/// then blocks until `release` exists before emitting `template` —
/// a deterministic mid-exchange interleave point for race scenarios.
fn gated_adapter_script(dir: &Path, template: &str, launched: &Path, release: &Path) -> String {
    let template_path = dir.join("gated.template");
    std::fs::write(&template_path, template).unwrap();
    let capture = dir.join("gated-request.json");
    let script = format!(
        "req=$(cat)\nprintf '%s' \"$req\" > '{}'\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nrid=$(printf '%s' \"$req\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\nsed \"s/__RID__/$rid/g\" '{}'\n",
        capture.display(),
        launched.display(),
        release.display(),
        template_path.display()
    );
    raw_script(dir, "gated.sh", &script)
}

/// Wait (bounded) for `path` to appear — the launch marker a gated
/// adapter writes once it holds the exchange open.
fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(20));
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
fn run_intake_skips_without_launching_when_repositories_break_the_wire_bound() {
    let dir = tmp_root("repos-bound");
    let marker = dir.join("launched.marker");
    let script = raw_script(&dir, "sel.sh", &format!("touch '{}'", marker.display()));
    let mut f = fixture(dir.clone(), script);
    // 65 repositories exceed the protocol's 1-64 bound — the request
    // can never be built, so the attempt records `skipped` rather
    // than surfacing a config error on every invocation.
    f.repositories = (0..65)
        .map(|i| RepositoryRef::new(host(), RepositoryId::trusted(format!("repo-{i}"))))
        .collect();
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Skipped);
    assert_eq!(report.detail_code.as_deref(), Some("repositories_bound"));
    assert!(!report.launched);
    assert!(report.candidates.is_empty());
    assert!(
        !marker.exists(),
        "adapter must not launch for an unbuildable request"
    );

    // The empty list breaks the same bound the same way.
    f.repositories = vec![];
    let report = run(&f, Duration::from_secs(10));
    assert_eq!(report.state, IntakeRunState::Skipped);
    assert_eq!(report.detail_code.as_deref(), Some("repositories_bound"));
    assert!(!marker.exists());
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
    // A transport-level failure never reaches response validation —
    // but the adapter process did launch and exit nonzero.
    assert!(report.launched);
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
    // The child environment is the bounded allowlist plus the few
    // variables `/bin/sh` itself exports (PWD, SHLVL, _) — every name
    // the adapter sees must come from that closed set, so nothing the
    // test process (or an operator's shell) carries can leak through.
    // Asserting on names avoids mutating the shared process env,
    // which is unsound under parallel tests.
    let capture = dir.join("env.txt");
    let script = raw_script(&dir, "sel.sh", &format!("env > '{}'", capture.display()));
    let f = fixture(dir.clone(), script);
    let _ = run(&f, Duration::from_secs(10));
    let env = std::fs::read_to_string(&capture).expect("env captured");
    let allowed = [
        "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "PWD", "SHLVL", "_",
    ];
    for line in env.lines() {
        let name = line.split('=').next().unwrap_or("");
        assert!(
            allowed.contains(&name),
            "non-allowlisted variable reached the selector: {name}"
        );
    }
    assert!(
        env.lines().any(|l| l.starts_with("PATH=")),
        "allowlisted PATH missing:\n{env}"
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
    // File-backed store: the mid-batch capacity loss is injected by a
    // trigger inside the first selector decision's own transaction —
    // deterministic, no polling thread.
    let db = dir.join("workloop.db");
    let store = SqliteStore::open(&db, 4).unwrap();
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

    // The first selector-origin command decision shrinks the pending
    // limit to 2 inside its own commit: the first two candidates
    // admit, the third onward sees a full queue and defers.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE fault_counter (n INTEGER NOT NULL DEFAULT 0);
         INSERT INTO fault_counter (n) VALUES (0);
         CREATE TRIGGER shrink_pending_limit AFTER INSERT ON commands
         WHEN NEW.origin = 'selector'
         BEGIN
           UPDATE fault_counter SET n = n + 1;
           UPDATE intake_settings SET pending_queue_limit = 2
           WHERE (SELECT n FROM fault_counter) = 1;
         END;",
    )
    .unwrap();
    drop(conn);

    let report = run(&f, Duration::from_secs(10));

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
    // The fault trigger has done its work — remove it before the
    // capacity-restore submit so it cannot fire a second time.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER shrink_pending_limit; DROP TABLE fault_counter;")
        .unwrap();
    drop(conn);
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
/// admission halts before the remainder, the live owner closes the
/// run `interrupted` rather than leaving it `admitting` forever, and
/// the recovery sweep reconciles the unresolved candidate against the
/// ledger — the earlier accepted Work is untouched and the aborted
/// submit leaves no partial rows.
#[test]
fn a_submit_store_fault_halts_and_reconciles_the_candidate() {
    let dir = tmp_root("interrupted-candidate");
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
    // The unresolved candidate is reconciled against the ledger
    // immediately: no decision was ever saved under its replay key,
    // so it is `interrupted` — never silently acknowledged.
    assert_eq!(
        report.candidates[1].status,
        IntakeCandidateStatus::Interrupted,
        "{:?}",
        report.candidates[1]
    );
    assert_eq!(
        report.candidates[1].status_reason.as_deref(),
        Some("no_saved_decision")
    );
    // A live owner never leaves the run `admitting`: the halted run
    // closes `interrupted` so the next invocation is not wedged.
    assert_eq!(report.state, IntakeRunState::Interrupted);
    assert_eq!(report.detail_code.as_deref(), Some("unresolved_candidates"));

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

    // The wedge this guards against: a second invocation must not be
    // refused as `ActiveRun` — the interrupted run freed the active
    // slot, so a fresh run records and reaches a terminal state (the
    // fixed script's replayed proposals now collide with exclusions,
    // so `failed` is the expected honest outcome here).
    let second = run(&f, Duration::from_secs(10));
    assert_ne!(second.run_id, report.run_id);
    assert!(
        matches!(
            second.state,
            IntakeRunState::Completed
                | IntakeRunState::Failed
                | IntakeRunState::Interrupted
                | IntakeRunState::Skipped
        ),
        "second invocation must reach a terminal state, got {:?}",
        second.state
    );
}

/// A store fault that propagates out of the batch insert must not
/// leave the run non-terminal under this live process: the invocation
/// aborts it `interrupted` and the next invocation is not wedged
/// behind `ActiveRun`.
#[test]
fn a_batch_store_fault_aborts_the_run_instead_of_wedging() {
    let dir = tmp_root("abort-wedge");
    let db = dir.join("workloop.db");
    let proposals = vec![
        proposal("issue-1", "first work"),
        proposal("issue-2", "second work"),
    ];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = SqliteStore::open(&db, 4).unwrap();
    // Abort the validated batch insert: the run record exists (`open`)
    // but no candidate ever lands — the propagating store error path.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fault_candidate_batch BEFORE INSERT ON intake_candidates
         BEGIN SELECT RAISE(ABORT, 'injected batch fault'); END;",
    )
    .unwrap();
    drop(conn);

    let err = run_result(&f, Duration::from_secs(10)).unwrap_err();
    assert!(
        matches!(err, IntakeError::Store(_)),
        "expected a store error, got {err:?}"
    );

    // The run is closed `interrupted`, not left `open` under a live
    // owner — and no active run remains to block the next call.
    let runs = f.store.transact_deferred(|tx| tx.intake_runs()).unwrap();
    assert_eq!(runs.len(), 1);
    let aborted = &runs[0];
    assert_eq!(aborted.state, IntakeRunState::Interrupted);
    assert_eq!(aborted.detail_code.as_deref(), Some("store_error"));
    assert!(
        f.store
            .transact_deferred(|tx| tx.active_intake_run())
            .unwrap()
            .is_none(),
        "aborted run must not stay active"
    );
    assert!(
        f.store
            .transact_deferred(|tx| tx.intake_candidates(&aborted.run_id))
            .unwrap()
            .is_empty(),
        "aborted batch must leave no candidate rows"
    );

    // And the wedge is gone: once the fault clears, the same
    // invocation completes normally end to end.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER fault_candidate_batch;")
        .unwrap();
    drop(conn);
    let second = run(&f, Duration::from_secs(10));
    assert_eq!(second.state, IntakeRunState::Completed);
    assert_eq!(second.candidates.len(), 2);
}

/// The same guard on the `admitting` side: a propagating store error
/// after every candidate reached a decided status still closes the
/// run `interrupted` — the accepted Work and ledger decisions are
/// untouched, and the next invocation is not wedged.
#[test]
fn an_admission_store_fault_aborts_and_reconciles_candidates() {
    let dir = tmp_root("abort-admitting");
    let db = dir.join("workloop.db");
    let proposals = vec![
        proposal("issue-1", "first work"),
        proposal("issue-2", "second work"),
    ];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = SqliteStore::open(&db, 4).unwrap();
    // Admit the whole batch, then fault only the run's terminal
    // transition: candidates reach decided statuses, but the
    // `admitting` → `completed` write fails and propagates.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fault_run_completion BEFORE UPDATE ON intake_runs
         WHEN NEW.state = 'completed'
         BEGIN SELECT RAISE(ABORT, 'injected completion fault'); END;",
    )
    .unwrap();
    drop(conn);

    let err = run_result(&f, Duration::from_secs(10)).unwrap_err();
    assert!(
        matches!(err, IntakeError::Store(_)),
        "expected a store error, got {err:?}"
    );

    let runs = f.store.transact_deferred(|tx| tx.intake_runs()).unwrap();
    assert_eq!(runs.len(), 1);
    let aborted = &runs[0];
    assert_eq!(aborted.state, IntakeRunState::Interrupted);
    assert_eq!(aborted.detail_code.as_deref(), Some("store_error"));
    assert!(
        f.store
            .transact_deferred(|tx| tx.active_intake_run())
            .unwrap()
            .is_none(),
        "aborted run must not stay active"
    );

    // Every candidate had already reached a decided status before the
    // fault, so the sweep finds nothing unresolved — the accepted
    // Work and its ledger decision are untouched.
    let candidates = f
        .store
        .transact_deferred(|tx| tx.intake_candidates(&aborted.run_id))
        .unwrap();
    assert!(
        candidates
            .iter()
            .all(|c| c.status == IntakeCandidateStatus::Accepted),
        "{candidates:?}"
    );

    // The next invocation is not wedged: it fails later on the
    // now-excluded sources — a normal failed run, never ActiveRun.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER fault_run_completion;")
        .unwrap();
    drop(conn);
    let second = run(&f, Duration::from_secs(10));
    assert_eq!(second.state, IntakeRunState::Failed);
}

// --- #218 cross-component acceptance ---------------------------------------
//
// The scenarios below exercise the races the unit suites cannot reach:
// a manual submit interleaved inside a live exchange, finished sources
// re-presented under new and replayed identities, non-terminal owners
// beyond `queued`, and store availability while the selector blocks.

/// A manual submit that lands while the selector exchange is still
/// open takes the last pending position: preflight sized the ask for
/// one free slot, but admission re-decides against the live queue —
/// the candidate defers as `capacity_deferred` and no Work is created
/// for it. The manual Work stands; intake never overshoots the limit.
#[test]
fn a_manual_submit_taking_the_last_position_defers_the_intake_candidate() {
    let dir = tmp_root("lastpos");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    // Occupy 9 of the default 10 pending positions: preflight sees
    // exactly one free slot and asks for at most one candidate.
    for i in 0..9 {
        manual_submit(&store, &cfg, &format!("fill-{i}"), vec![]);
    }
    let proposals = vec![proposal("issue-last", "last slot work")];
    let launched = dir.join("launched.marker");
    let release = dir.join("release.marker");
    let script = gated_adapter_script(&dir, &success_template(&proposals), &launched, &release);
    let mut f = fixture(dir.clone(), script);
    f.store = store;

    let report = std::thread::scope(|s| {
        let intake = s.spawn(|| run(&f, Duration::from_secs(30)));
        // The selector holds the exchange open; the manual submit
        // takes the position the candidate was sized for.
        wait_for(&launched, "selector launch");
        manual_submit(&f.store, &f.config, "race-winner", vec![]);
        std::fs::write(&release, "").unwrap();
        intake.join().unwrap()
    });

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates.len(), 1);
    let c = &report.candidates[0];
    assert_eq!(c.status, IntakeCandidateStatus::CapacityDeferred);
    assert_eq!(c.status_reason.as_deref(), Some("capacity_exhausted"));
    assert!(c.command_id.is_none() && c.work_id.is_none());
    // The ask was sized to the one free position preflight saw.
    let request = captured_request(&dir.join("gated-request.json"));
    assert_eq!(request["max_candidates"], 1, "{request}");
    // Nine fillers plus the racing manual submit — the deferred
    // candidate created no Work of its own.
    assert_eq!(works_count(&f.store), 10);
}

/// Once a Work's Execution settles terminally and its issue claims
/// are released, the source is ordinary again: intake re-proposing it
/// under a *new* proposal identity (different task, new key) submits
/// fresh and admits a new Work — there is no finished-source gate and
/// no finished-Issue blacklist.
#[test]
fn a_finished_source_is_readmitted_under_a_new_proposal_key() {
    let dir = tmp_root("finished-src");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    let (w1, e1) = submit_ids(&store, &cfg, "src-1", vec![issue("issue-1")]);
    force_execution(&store, &e1, ExecutionState::Succeeded);
    release_claims(&store, &w1);

    let proposals = vec![proposal("issue-1", "reworked issue-1")];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates.len(), 1);
    let c = &report.candidates[0];
    assert_eq!(c.status, IntakeCandidateStatus::Accepted, "{c:?}");
    let w2 = c.work_id.clone().expect("accepted candidate links a Work");
    assert_ne!(w2, w1, "readmission creates a new Work, not a rewrite");
    assert_eq!(works_count(&f.store), 2);
}

/// The same proposal identity against a finished source replays the
/// recorded decision instead of admitting again: the candidate lands
/// `replayed` with the original Work link and the Work count does not
/// grow — no duplicate Work, no lost committed decision.
#[test]
fn the_same_proposal_key_replays_the_recorded_decision_for_a_finished_source() {
    let dir = tmp_root("finished-replay");
    let proposals = vec![proposal("issue-1", "first work")];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let f = fixture(dir.clone(), script);

    let first = run(&f, Duration::from_secs(10));
    assert_eq!(first.state, IntakeRunState::Completed);
    assert_eq!(first.candidates[0].status, IntakeCandidateStatus::Accepted);
    let w1 = first.candidates[0].work_id.clone().unwrap();

    // Settle the admitted Work terminally and release its claims —
    // the durable state safe terminal settlement leaves behind.
    let executions = f
        .store
        .transact_deferred(|tx| tx.executions_for_work(&w1))
        .unwrap();
    assert_eq!(executions.len(), 1);
    force_execution(&f.store, &executions[0].id, ExecutionState::Succeeded);
    release_claims(&f.store, &w1);

    // A second invocation re-proposing the identical proposal finds
    // no live owner to exclude, submits under the same replay key,
    // and lands on the recorded decision.
    let second = run(&f, Duration::from_secs(10));
    assert_eq!(second.state, IntakeRunState::Completed);
    assert_eq!(second.candidates.len(), 1);
    let c = &second.candidates[0];
    assert_eq!(c.status, IntakeCandidateStatus::Replayed, "{c:?}");
    assert_eq!(c.work_id.as_ref(), Some(&w1));
    assert_eq!(works_count(&f.store), 1, "replay never duplicates Work");
}

/// `queued` is not the only owning state: Works forced into `running`
/// and `paused` still hold their issues, so the request's
/// `excluded_sources` carries both — the wire-level half of the
/// overlap rule the admission path re-checks.
#[test]
fn running_and_paused_owners_still_exclude_their_sources() {
    let dir = tmp_root("owner-states");
    let store = SqliteStore::in_memory(4).unwrap();
    let cfg = config(true);
    let (_w_run, e_run) = submit_ids(&store, &cfg, "own-run", vec![issue("issue-run")]);
    force_execution(&store, &e_run, ExecutionState::Running);
    let (_w_pause, e_pause) = submit_ids(&store, &cfg, "own-pause", vec![issue("issue-pause")]);
    force_execution(&store, &e_pause, ExecutionState::Paused);

    let proposals = vec![proposal("issue-free", "unowned work")];
    let capture = dir.join("request.json");
    let script = adapter_script(&dir, "sel.sh", &success_template(&proposals), &capture);
    let mut f = fixture(dir.clone(), script);
    f.store = store;
    let report = run(&f, Duration::from_secs(10));

    assert_eq!(report.state, IntakeRunState::Completed);
    let request = captured_request(&capture);
    let excluded: Vec<&str> = request["excluded_sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["issue"].as_str().unwrap())
        .collect();
    assert!(excluded.contains(&"issue-run"), "{excluded:?}");
    assert!(excluded.contains(&"issue-pause"), "{excluded:?}");
    assert_eq!(excluded.len(), 2, "{excluded:?}");
    assert_eq!(report.candidates[0].status, IntakeCandidateStatus::Accepted);
}

/// A selector that blocks inside the exchange holds only the intake
/// lease — the store's ordinary submission path stays fully
/// available, so manual Work keeps flowing while the adapter is
/// still deciding.
#[test]
fn a_blocked_exchange_leaves_manual_submit_free() {
    let dir = tmp_root("blocked-exchange");
    let proposals = vec![proposal("issue-1", "slow work")];
    let launched = dir.join("launched.marker");
    let release = dir.join("release.marker");
    let script = gated_adapter_script(&dir, &success_template(&proposals), &launched, &release);
    let f = fixture(dir.clone(), script);

    let report = std::thread::scope(|s| {
        let intake = s.spawn(|| run(&f, Duration::from_secs(30)));
        wait_for(&launched, "selector launch");
        // The exchange is open and the selector is still deciding;
        // a manual submit through the ordinary command path is
        // unaffected.
        manual_submit(&f.store, &f.config, "during-exchange", vec![]);
        std::fs::write(&release, "").unwrap();
        intake.join().unwrap()
    });

    assert_eq!(report.state, IntakeRunState::Completed);
    assert_eq!(report.candidates[0].status, IntakeCandidateStatus::Accepted);
    assert_eq!(works_count(&f.store), 2, "manual + intake Work both exist");
}
