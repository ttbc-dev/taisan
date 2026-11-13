//! U4.6c — swap-window recovery: recollection and safe stop. Crash,
//! restart and recovery at the adapter-selection boundary
//! (`docs/worker-protocol.md`): a supervisor restarted after a
//! `worker_argv` change reconciles in-flight Invocations against their
//! recorded adapter ID, version and capability digest — no migration,
//! no second writer — interrupted export/import recovers per the
//! [crash-recovery table](../../docs/worker-protocol.md#crash-recovery)
//! under the changed configuration with no duplicate artifact IDs, and
//! real-runtime safe stop plus artifact recollection retain evidence for
//! Invocations launched under the prior selection. Each named scenario
//! retains one U4.6 record to `target/u4.6c-<scenario>.json`, fail-closed
//! on the revision stamp: no pristine checkout, no record.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use workloop_core::command::{CommandBody, CommandDecision, CommandEffect, CommandRequest};
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::{CapabilityEntry, CapabilityState, Observation, ObservationFact};
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, SignalOutcome, SpawnError,
};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation,
};
use workloop_exec::adapter::{self, AdapterSelection};
use workloop_exec::handoff::{HandoffLayout, artifact_ref, quarantine_ref};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn contract_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-contract-worker")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-u46c-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn host() -> HostInstanceId {
    HostInstanceId::trusted("swap-recovery-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("swap-recovery-repo"))
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
        budget: ExecutionBudget {
            artifact_recovery_attempts: 3,
            artifact_recovery_seconds: 60,
            ..ExecutionBudget::default()
        },
        policy: WorkPolicy::default(),
    }
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::Human,
        external_id: "op".into(),
    }
}

/// Grant the host capabilities so a finished Execution's publish stage
/// records its pending intent — the canonical `Waiting·HostWrite` shape
/// the recollection scenarios assert.
fn authorize(store: &SqliteStore) {
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: None,
            fact: ObservationFact::CapabilityReport {
                host: host(),
                capabilities: vec![
                    CapabilityEntry {
                        capability: HostCapability::PushRef,
                        support: CapabilityState::Supported,
                        authorization: CapabilityState::Authorized,
                    },
                    CapabilityEntry {
                        capability: HostCapability::ChangeRequestCreate,
                        support: CapabilityState::Supported,
                        authorization: CapabilityState::Authorized,
                    },
                ],
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
}

fn submit_and_admit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {
    submit_and_admit_with(store, key, vec![])
}

fn submit_and_admit_with(
    store: &SqliteStore,
    key: &str,
    verification_argv: Vec<Vec<String>>,
) -> (WorkId, ExecutionId) {
    let request = CommandRequest {
        actor: actor(),
        origin: Origin("swap-recovery".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "swap-window recovery".into(),
            sources: vec![],
            constraints: vec![],
            verification_argv,
        },
    };
    let decision = handle_command(store, &config(), &request, NOW).unwrap();
    let (work_id, execution_id) = match decision {
        CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("submit rejected: {other:?}"),
    };
    authorize(store);
    let admitted = store
        .transact(|tx| admit_queued(tx, &config(), &execution_id, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, execution_id)
}

fn request(store: &SqliteStore, execution: &ExecutionId) -> InvocationRequest {
    store
        .transact(|tx| tx.request(&format!("{}:inv:0", execution.0)))
        .unwrap()
        .expect("request")
}

fn invocations(store: &SqliteStore, execution: &ExecutionId) -> Vec<Invocation> {
    store
        .transact(|tx| tx.invocations_for_execution(execution))
        .unwrap()
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution")
}

fn cancel(store: &SqliteStore, work_id: &WorkId, key: &str) {
    let version = store
        .transact(|tx| tx.work(work_id))
        .unwrap()
        .map(|work| work.version)
        .expect("work exists");
    let decision = handle_command(
        store,
        &config(),
        &CommandRequest {
            actor: actor(),
            origin: Origin("swap-recovery".into()),
            idempotency_key: key.into(),
            body: CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        },
        NOW,
    )
    .unwrap();
    assert!(matches!(decision, CommandDecision::Accepted { .. }));
}

/// Drive supervisor passes until `pred` holds or the bound expires.
fn drive_until<P: ProcessControlPort>(
    sup: &Supervisor<P>,
    store: &SqliteStore,
    what: &str,
    mut pred: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        sup.run_once(store, NOW).unwrap();
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// Spawn-counting decorator: the "no second writer" assertions bind the
/// port boundary, never supervisor internals.
struct CountingPosix {
    calls: Rc<Cell<usize>>,
}

impl ProcessControlPort for CountingPosix {
    fn spawn(&self, argv: &[String], cwd: &Path) -> Result<ProcessIdentity, SpawnError> {
        self.calls.set(self.calls.get() + 1);
        PosixProcessControl.spawn(argv, cwd)
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
    fn reap(&self, pid: i32) {
        PosixProcessControl.reap(pid)
    }
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        PosixProcessControl.run_probe(argv, cwd, timeout)
    }
}

// ---- git fixtures ----------------------------------------------------

/// Plain git helper for workspace/handoff setup (the implementation's
/// own wrapper applies the hardened export profile; tests need identity
/// config too).
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

/// Workspace repo: base commit on the default branch, then a produced
/// commit on `work/<work_id>`; returns `(base, produced)`.
fn git_workspace(ws: &Path, work_id: &WorkId) -> (String, String) {
    std::fs::create_dir_all(ws).unwrap();
    git(ws, &["init", "-b", "main"]);
    git(ws, &["config", "user.email", "t@t"]);
    git(ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("seed.txt"), "base").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-m", "base"]);
    let base = git(ws, &["rev-parse", "HEAD"]);
    let branch = format!("work/{}", work_id.0);
    git(ws, &["checkout", "-b", &branch]);
    std::fs::write(ws.join("artifact.json"), "{\"ok\":true}").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-m", "produced"]);
    let produced = git(ws, &["rev-parse", "HEAD"]);
    (base, produced)
}

fn bare_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("handoff.git");
    git(dir, &["init", "--bare", repo.to_str().unwrap()]);
    repo
}

/// Every durable artifact ref the handoff repository holds, sorted by
/// the ref query order — the authoritative "no duplicate artifact IDs"
/// set.
fn artifact_refs(repo_dir: &Path) -> Vec<String> {
    git(
        repo_dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/taisan/artifacts/",
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

/// Rewind one handoff row to a crash-window residue — the test-level
/// CAS write the crash-recovery walks use (fresh expected seq, so a
/// later drive's compare-and-set still lands).
fn rewind(store: &SqliteStore, artifact_id: &str, mutate: impl FnOnce(&mut ArtifactHandoff)) {
    store
        .transact(|tx| {
            let mut handoff = tx
                .artifact_handoff(artifact_id)?
                .expect("handoff row to rewind");
            let expected_seq = handoff.recorded_seq;
            mutate(&mut handoff);
            handoff.recorded_seq = expected_seq;
            tx.apply(&Effect::UpsertHandoff {
                handoff,
                expected_seq,
            })?;
            Ok(())
        })
        .unwrap();
}

// ---- U4.6 record retention (fail-closed stamping) --------------------

/// One git query whose trimmed stdout. `None` on any failure — the
/// revision stamping never substitutes a placeholder for it.
fn git_stdout(argv: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(argv)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A required revision field must be a full commit id: `unknown`, a
/// truncated id, or any non-hex output fails the recorded run instead
/// of being stamped into a record.
fn assert_commit_id(sha: &str, what: &str) {
    assert_eq!(sha.len(), 40, "{what} must be a full commit id: {sha}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "{what} must be a hex commit id: {sha}"
    );
}

/// Whether a cited commit could describe this checkout: no file —
/// tracked or added-but-uncommitted — differs from `HEAD`. This slice
/// adds files, so unlike the shared-suite helper this check counts
/// untracked (non-ignored) files too: a commit that does not contain
/// the exercised slice never stamps a record. A git failure answers
/// `false` rather than panicking, so the question itself never fails a
/// run: the answer only decides whether the run may stamp records.
fn tracked_tree_is_pristine() -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// The base fork point of a normal checkout: the first merge-base
/// against a base branch this clone keeps. A checkout that retains no
/// base history at all cites the deepest commit it does retain; a
/// checkout with no commit history yields `None` and retains no records.
fn base_revision() -> Option<String> {
    for revs in [
        ["merge-base", "HEAD", "origin/main"],
        ["merge-base", "HEAD", "main"],
        ["merge-base", "HEAD", "origin/HEAD"],
    ] {
        if let Some(sha) = git_stdout(&revs) {
            return Some(sha);
        }
    }
    git_stdout(&["rev-list", "--max-parents=0", "HEAD"])
        .and_then(|roots| roots.lines().next().map(str::to_string))
}

/// The revision pair a CI run stamps, read from the workflow event
/// payload — a pull_request run cites `head.sha`/`base.sha`, a push run
/// `GITHUB_SHA`/`before`. `None` outside Actions or without the pair.
fn ci_event_revisions() -> Option<(String, String)> {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return None;
    }
    let event: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::env::var("GITHUB_EVENT_PATH").ok()?).ok()?,
    )
    .ok()?;
    if let Some(pr) = event.get("pull_request") {
        return Some((
            pr.get("head")?.get("sha")?.as_str()?.to_string(),
            pr.get("base")?.get("sha")?.as_str()?.to_string(),
        ));
    }
    Some((
        std::env::var("GITHUB_SHA").ok()?,
        event.get("before")?.as_str()?.to_string(),
    ))
}

/// The revision pair a recorded run stamps, validated fail-closed —
/// `None` whenever no trustworthy stamp exists (a dirty tree, a
/// checkout git cannot answer). A run without a stamp still executes
/// every scenario and every record-independent assertion, but builds
/// and retains no records: a retained record must cite a commit that
/// contains the exercised slice, so an unstamped run degrades to *no
/// evidence* rather than to a placeholder.
fn record_revisions() -> Option<(String, String)> {
    if !tracked_tree_is_pristine() {
        return None;
    }
    let (tested, base) = match ci_event_revisions() {
        Some(pair) => pair,
        None => (git_stdout(&["rev-parse", "HEAD"])?, base_revision()?),
    };
    assert_commit_id(&tested, "tested_revision");
    assert_commit_id(&base, "base_revision");
    Some((tested, base))
}

fn installation() -> serde_json::Value {
    serde_json::json!({
        "host_instance": "swap-recovery-host",
        "repository": "swap-recovery-repo",
        "adapter": "fixture",
        "contract_version": 1,
    })
}

/// One `adapter_selection` entry of the U4.6 record: the binding the
/// request recorded, labelled with the phase of the config change that
/// the scenario exercises.
fn selection_entry(
    phase: &str,
    request_id: &str,
    invocation_id: &str,
    binding: &WorkerAdapterBinding,
) -> serde_json::Value {
    serde_json::json!({
        "phase": phase,
        "adapter_id": binding.adapter.as_str(),
        "adapter_version": binding.adapter_version,
        "runtime_version": binding.runtime_version,
        "capability_digest": binding.capability_digest,
        "argv": binding.argv,
        "request_id": request_id,
        "invocation_id": invocation_id,
    })
}

/// A retained record in the U4.6 evidence format — the closed field set
/// of `docs/worker-protocol.md#u46-evidence-record`.
fn build_record(
    tested: &str,
    base: &str,
    scenario: &str,
    configuration_identity: serde_json::Value,
    evidence_refs: Vec<String>,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "tested_revision": tested,
        "base_revision": base,
        "claim_class": "fixture_supported",
        "configuration_identity": configuration_identity,
        "command_scenario": scenario,
        "result": "pass",
        "evidence_refs": evidence_refs,
    })
}

/// Retain the scenario's record under `target/` — only when the checkout
/// yields a faithful revision stamp; otherwise the scenario ran and no
/// record is written (no placeholder ever reaches a file).
fn retain_record(
    file: &str,
    scenario: &str,
    configuration_identity: serde_json::Value,
    evidence_refs: Vec<String>,
) {
    let Some((tested, base)) = record_revisions() else {
        eprintln!(
            "u4.6c: no faithful revision stamp (dirty or non-git checkout) — \
             {scenario} ran; no record retained"
        );
        return;
    };
    let record = build_record(
        &tested,
        &base,
        scenario,
        configuration_identity,
        evidence_refs,
    );
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(file);
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
    )
    .unwrap_or_else(|e| panic!("retain the U4.6 record at {}: {e}", path.display()));
}

// ---- WS-20/WS-03: restart reconciliation under a config change -------

/// The named swap-window restart scenario: two Invocations are in
/// flight under the prior selection when `worker_argv` changes and the
/// supervisor restarts over the reopened durable store. Each in-flight
/// Invocation reconciles against its recorded adapter ID, version and
/// capability digest — inspected, never re-launched, never migrated —
/// while only a request created after the change binds the new adapter.
/// The safe stop that follows covers every recorded identity launched
/// under either selection with a durable stop intent.
#[test]
fn u46c_restart_after_worker_argv_change_reconciles_in_flight_on_recorded_bindings() {
    let root = tmp_root("restart");
    let db = root.join("store.db");
    let ws_root = root.join("ws");
    let spawns = Rc::new(Cell::new(0usize));
    let prior_argv = vec![fixture_bin(), "hang".into()];
    let changed_argv = vec![contract_bin(), "hang".into()];

    // Phase 1 — the prior selection: two requests bind and launch real
    // fixture writers; both Executions run mid-flight.
    let in_flight = {
        let store = SqliteStore::open(&db, 4).unwrap();
        let sup = Supervisor::new(
            CountingPosix {
                calls: spawns.clone(),
            },
            ws_root.clone(),
            prior_argv.clone(),
            config(),
        );
        let (work_a, exec_a) = submit_and_admit(&store, "u46c-restart-a1");
        let (work_b, exec_b) = submit_and_admit(&store, "u46c-restart-a2");
        sup.consume_requests(&store, NOW).unwrap();
        assert_eq!(
            spawns.get(),
            2,
            "one writer per request under the prior selection"
        );
        let mut snap = Vec::new();
        for (work, exec) in [(&work_a, &exec_a), (&work_b, &exec_b)] {
            let invocation = invocations(&store, exec)
                .into_iter()
                .next()
                .expect("invocation");
            assert_eq!(invocation.state, InvocationState::Running);
            let binding = request(&store, exec)
                .adapter_binding
                .expect("request bound at first consume");
            assert_eq!(binding.adapter.as_str(), "workloop-fixture-worker");
            snap.push((
                work.clone(),
                exec.clone(),
                binding,
                invocation
                    .process_identity
                    .expect("recorded writer identity"),
            ));
        }
        snap
    };

    // The config change and the restart: a fresh supervisor over the
    // reopened durable store, bound to the new worker_argv.
    let store = SqliteStore::open(&db, 4).unwrap();
    let sup = Supervisor::new(
        CountingPosix {
            calls: spawns.clone(),
        },
        ws_root.clone(),
        changed_argv.clone(),
        config(),
    );

    // Restart pass: each in-flight Invocation reconciles against its
    // recorded adapter ID, version and capability digest — no second
    // writer, no migration.
    sup.run_once(&store, NOW + 1).unwrap();
    assert_eq!(
        spawns.get(),
        2,
        "restart reconciliation must not start a second writer"
    );
    for (_, exec, binding, identity) in &in_flight {
        assert_eq!(
            request(&store, exec).adapter_binding.as_ref(),
            Some(binding),
            "the recorded binding is unchanged across the restart"
        );
        let invocation = invocations(&store, exec).into_iter().next().unwrap();
        assert_eq!(
            invocation.process_identity.as_ref(),
            Some(identity),
            "the recorded writer identity is unchanged across the restart"
        );
        assert_eq!(invocation.state, InvocationState::Running);
        // The recorded adapter ID, version and capability digest still
        // hold under the recorded argv — the binding re-verifies Ready.
        let workspace = sup.workspace_of(&request(&store, exec).work_id);
        let recorded =
            adapter::observe_worker_declaration(&PosixProcessControl, &binding.argv, &workspace)
                .expect("the recorded argv still declares");
        assert_eq!(
            adapter::select_adapter(&recorded, binding),
            AdapterSelection::Ready(binding.clone()),
            "the recorded binding re-verifies field-for-field under its own argv"
        );
        // The changed configuration would be a repair for these
        // requests, not a retarget: it declares a different adapter.
        let changed =
            adapter::observe_worker_declaration(&PosixProcessControl, &changed_argv, &workspace)
                .expect("the changed argv still declares");
        match adapter::select_adapter(&changed, binding) {
            AdapterSelection::ConfigurationRepair { reason } => {
                assert!(reason.contains("adapter.identity_mismatch"), "{reason}");
                assert!(reason.contains("capability.digest_mismatch"), "{reason}");
            }
            other => panic!("the changed config must not match a stored binding: {other:?}"),
        }
    }

    // Only a request created after the change uses the new adapter.
    let (work_n, exec_n) = submit_and_admit(&store, "u46c-restart-new");
    sup.consume_requests(&store, NOW + 2).unwrap();
    assert_eq!(spawns.get(), 3, "exactly one writer for the new request");
    let new_binding = request(&store, &exec_n)
        .adapter_binding
        .expect("the new request binds an adapter");
    assert_eq!(new_binding.adapter.as_str(), "workloop-contract-worker");
    assert_eq!(new_binding.argv, changed_argv);
    assert_ne!(
        new_binding.capability_digest, in_flight[0].2.capability_digest,
        "a different adapter declares a different capability surface"
    );
    for (_, exec, binding, _) in &in_flight {
        assert_eq!(
            request(&store, exec).adapter_binding.as_ref(),
            Some(binding),
            "the new binding never touches the in-flight requests"
        );
    }

    // Safe stop: a durable stop intent covers every recorded identity —
    // the prior-selection writers and the new one — and each is
    // inspected stopped before the terminal cancel lands. The stop path
    // never starts a writer.
    let mut works = Vec::new();
    let mut execs = Vec::new();
    for (work, exec, _, _) in &in_flight {
        works.push(work.clone());
        execs.push(exec.clone());
    }
    works.push(work_n.clone());
    execs.push(exec_n.clone());
    for (i, work) in works.iter().enumerate() {
        cancel(&store, work, &format!("u46c-restart-cancel-{i}"));
    }
    drive_until(&sup, &store, "safe stop under either selection", || {
        execs
            .iter()
            .all(|e| execution(&store, e).state == ExecutionState::Cancelled)
    });
    assert_eq!(
        spawns.get(),
        3,
        "the stop path never starts a writer under either selection"
    );
    let mut stopped_identities = Vec::new();
    for (work, exec, binding, identity) in &in_flight {
        let invocation = invocations(&store, exec).into_iter().next().unwrap();
        assert_eq!(
            invocation.state,
            InvocationState::Done,
            "stop resolved the prior-selection writer"
        );
        assert_eq!(
            invocation.process_identity.as_ref(),
            Some(identity),
            "the recorded identity survives the stop"
        );
        let intent = store
            .transact(|tx| tx.stop_intent(&invocation.id))
            .unwrap()
            .expect("durable stop intent");
        assert!(
            intent.identities.iter().any(|i| i.pid == identity.pid),
            "the intent covers the recorded writer"
        );
        for id in &intent.identities {
            assert!(
                matches!(
                    PosixProcessControl.inspect(id),
                    ProcessEvidence::Stopped { .. }
                ),
                "every identity the intent recorded is inspected stopped"
            );
        }
        let settled = execution(&store, exec);
        assert_eq!(settled.state, ExecutionState::Cancelled);
        assert_eq!(settled.terminal_code.as_deref(), Some("cancelled"));
        assert_eq!(
            request(&store, exec).adapter_binding.as_ref(),
            Some(binding),
            "the stop never rewrites the recorded binding"
        );
        stopped_identities.push(format!(
            "{} cancelled with stop intent over {} recorded identities",
            work.0,
            intent.identities.len()
        ));
    }
    let invocation_n = invocations(&store, &exec_n).into_iter().next().unwrap();
    assert_eq!(invocation_n.state, InvocationState::Done);
    let intent_n = store
        .transact(|tx| tx.stop_intent(&invocation_n.id))
        .unwrap()
        .expect("durable stop intent for the new request");
    for id in &intent_n.identities {
        assert!(matches!(
            PosixProcessControl.inspect(id),
            ProcessEvidence::Stopped { .. }
        ));
    }

    let identity_json = serde_json::json!({
        "installation": installation(),
        "worker_argv_change": {"before": prior_argv, "after": changed_argv},
        "adapter_selection": in_flight
            .iter()
            .map(|(_, exec, binding, _)| {
                selection_entry(
                    "prior_selection",
                    &request(&store, exec).request_id,
                    &invocations(&store, exec)[0].id.0,
                    binding,
                )
            })
            .chain(std::iter::once(selection_entry(
                "post_restart_new_request",
                &request(&store, &exec_n).request_id,
                &invocations(&store, &exec_n)[0].id.0,
                &new_binding,
            )))
            .collect::<Vec<_>>(),
    });
    let mut evidence_refs = vec![
        "restart pass: spawn_attempts stayed 2 — both in-flight Invocations reconciled, none re-launched, none migrated".to_string(),
        "both recorded bindings unchanged across the restart (adapter ID, adapter version, runtime version, capability digest, argv) and re-verify Ready under their own argv".to_string(),
        "the changed worker_argv selects ConfigurationRepair against each stored binding (adapter.identity_mismatch, capability.digest_mismatch) — a repair for them, never a retarget".to_string(),
        "only the post-restart request bound workloop-contract-worker; spawn_attempts=3".to_string(),
        format!("safe stop: {}", stopped_identities.join("; ")),
        "spawn_attempts stayed 3 through the whole stop path".to_string(),
    ];
    evidence_refs.extend(in_flight.iter().map(|(_, exec, binding, _)| {
        format!(
            "prior selection {} capability_digest={}",
            request(&store, exec).request_id,
            binding.capability_digest
        )
    }));
    evidence_refs.push(format!(
        "post-restart selection {} capability_digest={}",
        request(&store, &exec_n).request_id,
        new_binding.capability_digest
    ));
    retain_record(
        "u4.6c-restart.json",
        "swap_recovery::u46c_restart_after_worker_argv_change_reconciles_in_flight_on_recorded_bindings (WS-20, WS-03)",
        identity_json,
        evidence_refs,
    );
}

// ---- WS-11: the crash-recovery table under the changed config --------

/// One Phase-1 writer that finished under the prior selection: the
/// identities the restart and recovery then act on.
struct FinishedWriter {
    work: WorkId,
    exec: ExecutionId,
    inv: InvocationId,
    produced: String,
}

/// Whether `reference` resolves inside `repo_dir`.
fn ref_exists(repo_dir: &Path, reference: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--verify", reference])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The named swap-window crash-recovery scenario: two real fixture
/// writers finish under the prior selection, the `worker_argv` changes,
/// and the restarted supervisor walks the crash-recovery table for one
/// artifact — no export row, truncated bundle, interrupted import,
/// imported row without ref, ref present with the row still imported —
/// each recovering to `acknowledged` with exactly one artifact ID, while
/// a second artifact whose workspace was lost before acknowledgement
/// converges on `recovery_exhausted` inside its bound. Recovery under
/// the changed configuration never launches a worker.
#[test]
fn u46c_interrupted_export_import_recovers_under_changed_config_without_duplicate_artifact_ids() {
    let root = tmp_root("crash-table");
    let db = root.join("store.db");
    let ws_root = root.join("ws");
    let out_root = root.join("out");
    let repo_dir = bare_repo(&root);
    let layout = HandoffLayout {
        repository: repo_dir.clone(),
        workspace_root: ws_root.clone(),
        out_root: out_root.clone(),
    };
    let spawns = Rc::new(Cell::new(0usize));
    let prior_argv = vec![fixture_bin(), "clean-exit".into()];
    let changed_argv = vec![contract_bin(), "clean-exit".into()];

    // Phase 1 — the prior selection: two real fixture writers finish in
    // git workspaces carrying their produced branches. No handoff is
    // configured here: the rows must not exist before the restart.
    let phase_one = {
        let store = SqliteStore::open(&db, 4).unwrap();
        let sup = Supervisor::new(
            CountingPosix {
                calls: spawns.clone(),
            },
            ws_root.clone(),
            prior_argv.clone(),
            config(),
        );
        let (work_1, exec_1) = submit_and_admit_with(
            &store,
            "u46c-crash-a",
            vec![vec!["git".into(), "rev-parse".into(), "HEAD".into()]],
        );
        let (work_2, exec_2) = submit_and_admit(&store, "u46c-crash-b");
        let (_base_1, produced_1) =
            git_workspace(&ws_root.join(format!("ws-{}", work_1.0)), &work_1);
        let (_base_2, _produced_2) =
            git_workspace(&ws_root.join(format!("ws-{}", work_2.0)), &work_2);
        sup.consume_requests(&store, NOW).unwrap();
        drive_until(&sup, &store, "both fixture writers finish", || {
            [&exec_1, &exec_2]
                .iter()
                .all(|e| invocations(&store, e)[0].state == InvocationState::Done)
        });
        assert_eq!(
            execution(&store, &exec_1).waiting_reason,
            Some(WaitingReason::HostWrite)
        );
        [
            FinishedWriter {
                inv: invocations(&store, &exec_1)[0].id.clone(),
                work: work_1,
                exec: exec_1,
                produced: produced_1,
            },
            FinishedWriter {
                inv: invocations(&store, &exec_2)[0].id.clone(),
                work: work_2,
                exec: exec_2,
                produced: String::new(),
            },
        ]
    };
    let [writer_1, writer_2] = phase_one;
    assert_eq!(
        spawns.get(),
        2,
        "one real writer per request under the prior selection"
    );

    // Config change + restart: a fresh supervisor over the reopened
    // store, bound to the new worker_argv, carrying the handoff layout.
    let store = SqliteStore::open(&db, 4).unwrap();
    let sup = Supervisor::new(
        CountingPosix {
            calls: spawns.clone(),
        },
        ws_root.clone(),
        changed_argv.clone(),
        config(),
    )
    .with_handoff(layout.clone());
    assert_eq!(spawns.get(), 2, "the restart itself spawns nothing");

    let aid_1 = format!("{}/artifact.json", writer_1.inv.0);
    let aid_2 = format!("{}/artifact.json", writer_2.inv.0);
    let ref_1 = artifact_ref(&aid_1);
    let ref_2 = artifact_ref(&aid_2);
    let bundle_1 = out_root
        .join(&writer_1.exec.0)
        .join(&writer_1.inv.0)
        .join("export.bundle");

    // Before any window: no export row, no artifact ref — the residue
    // state every recoverable window starts from.
    assert!(
        store
            .transact(|tx| tx.artifact_handoff(&aid_1))
            .unwrap()
            .is_none(),
        "no export row exists before the recovery sweep"
    );
    assert!(artifact_refs(&repo_dir).is_empty());

    // Window: before `export_started` — exit confirmed, no export row.
    // The sweep runs export (the first artifact-recovery attempt) and
    // drives the row through import and ref to acknowledged.
    let scoped_w1 = |store: &SqliteStore, sup: &Supervisor<CountingPosix>| {
        sup.drive_handoffs_scoped(store, Some(&writer_1.work), &mut || 0, None)
            .unwrap();
    };
    scoped_w1(&store, &sup);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid_1))
        .unwrap()
        .expect("the sweep opened the export row");
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(row.produced.as_deref(), Some(writer_1.produced.as_str()));
    assert_eq!(
        artifact_refs(&repo_dir),
        vec![ref_1.clone()],
        "exactly one artifact ID acknowledged"
    );
    let results = store
        .transact(|tx| tx.verification_results(&aid_1))
        .unwrap();
    assert_eq!(results.len(), 1, "the declared verification ran once");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    assert_eq!(results[0].produced, writer_1.produced);
    // Re-driving an acknowledged row changes nothing: no second row, no
    // second ref, no second verification result.
    scoped_w1(&store, &sup);
    assert_eq!(artifact_refs(&repo_dir), vec![ref_1.clone()]);
    assert_eq!(
        store
            .transact(|tx| tx.verification_results(&aid_1))
            .unwrap()
            .len(),
        1
    );

    // Window: after `export_started`, a truncated bundle. The partial
    // is deleted (it is not evidence) and the export re-runs.
    rewind(&store, &aid_1, |h| {
        h.state = HandoffState::ExportStarted;
        h.produced = None;
        h.manifest_digest = None;
    });
    std::fs::write(&bundle_1, b"truncated-bundle-not-evidence").unwrap();
    scoped_w1(&store, &sup);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid_1))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(row.produced.as_deref(), Some(writer_1.produced.as_str()));
    git(&repo_dir, &["bundle", "verify", bundle_1.to_str().unwrap()]);
    assert_eq!(artifact_refs(&repo_dir), vec![ref_1.clone()]);

    // Window: bundle verified, import interrupted — leftover quarantine
    // objects, no `imported` row, no artifact ref yet. The sweep
    // discards the quarantine, re-imports the same bundle, and
    // recreates the ref under the expected-absent guard.
    rewind(&store, &aid_1, |h| {
        h.state = HandoffState::BundleWritten;
        h.manifest_digest = None;
    });
    git(
        &repo_dir,
        &[
            "fetch",
            bundle_1.to_str().unwrap(),
            &format!(
                "+refs/heads/work/{}:{}",
                writer_1.work.0,
                quarantine_ref(&aid_1)
            ),
        ],
    );
    git(&repo_dir, &["update-ref", "-d", &ref_1]);
    assert!(artifact_refs(&repo_dir).is_empty());
    scoped_w1(&store, &sup);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid_1))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(
        git(&repo_dir, &["rev-parse", &ref_1]),
        writer_1.produced,
        "the recreated ref resolves the produced commit"
    );
    assert_eq!(artifact_refs(&repo_dir), vec![ref_1.clone()]);

    // Window: `imported` row, ref absent — recreate the ref after
    // re-verifying the stored manifest digest.
    rewind(&store, &aid_1, |h| {
        h.state = HandoffState::Imported;
    });
    git(&repo_dir, &["update-ref", "-d", &ref_1]);
    scoped_w1(&store, &sup);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid_1))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(git(&repo_dir, &["rev-parse", &ref_1]), writer_1.produced);
    assert_eq!(artifact_refs(&repo_dir), vec![ref_1.clone()]);

    // Window: ref present, row still `imported` — read back the ref and
    // digest, then commit `acknowledged`.
    rewind(&store, &aid_1, |h| {
        h.state = HandoffState::Imported;
    });
    scoped_w1(&store, &sup);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid_1))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(artifact_refs(&repo_dir), vec![ref_1.clone()]);

    // Window: workspace deleted or lost before acknowledgement — the
    // export row the crash left has no source to re-export from; each
    // sweep pass charges an attempt and the row settles
    // `recovery_exhausted` at the bound, never acknowledged, never
    // minting an artifact ID.
    let request_2 = request(&store, &writer_2.exec).request_id;
    store
        .transact(|tx| {
            let seq = tx.alloc_seq()?;
            tx.apply(&Effect::UpsertHandoff {
                handoff: ArtifactHandoff {
                    artifact_id: aid_2.clone(),
                    invocation_id: writer_2.inv.clone(),
                    request_id: request_2,
                    work_id: writer_2.work.clone(),
                    execution_id: writer_2.exec.clone(),
                    base: String::new(),
                    produced: None,
                    manifest_digest: None,
                    state: HandoffState::ExportStarted,
                    invalid_reason: None,
                    attempt: 0,
                    recorded_seq: seq,
                },
                expected_seq: 0,
            })?;
            Ok(())
        })
        .unwrap();
    std::fs::remove_dir_all(ws_root.join(format!("ws-{}", writer_2.work.0))).unwrap();
    let scoped_w2 = |store: &SqliteStore, sup: &Supervisor<CountingPosix>| {
        sup.drive_handoffs_scoped(store, Some(&writer_2.work), &mut || 0, None)
            .unwrap();
    };
    for _ in 0..6 {
        let h = store
            .transact(|tx| tx.artifact_handoff(&aid_2))
            .unwrap()
            .unwrap();
        if matches!(h.state, HandoffState::Acknowledged | HandoffState::Invalid) {
            break;
        }
        scoped_w2(&store, &sup);
    }
    let lost = store
        .transact(|tx| tx.artifact_handoff(&aid_2))
        .unwrap()
        .unwrap();
    assert_eq!(lost.state, HandoffState::Invalid);
    assert_eq!(lost.invalid_reason.as_deref(), Some("recovery_exhausted"));
    assert_eq!(
        lost.attempt, 3,
        "exactly the configured artifact-recovery bound (3) was charged"
    );
    assert!(
        !ref_exists(&repo_dir, &ref_2),
        "the lost workspace never mints an artifact ref"
    );

    // Final: one row per invocation under its deterministic ID, one ref
    // total, no incomplete rows, no worker launched by recovery.
    assert_eq!(
        store
            .transact(|tx| tx.handoff_for_invocation(&writer_1.inv))
            .unwrap()
            .map(|h| h.artifact_id),
        Some(aid_1.clone()),
        "the acknowledged artifact keeps its deterministic ID"
    );
    assert_eq!(
        store
            .transact(|tx| tx.handoff_for_invocation(&writer_2.inv))
            .unwrap()
            .map(|h| h.artifact_id),
        Some(aid_2.clone())
    );
    assert_eq!(
        artifact_refs(&repo_dir),
        vec![ref_1.clone()],
        "the artifact ref set holds exactly the acknowledged ID"
    );
    assert!(
        store
            .transact(|tx| tx.incomplete_handoffs())
            .unwrap()
            .is_empty(),
        "every row reached a terminal state"
    );
    scoped_w1(&store, &sup);
    scoped_w2(&store, &sup);
    assert_eq!(
        artifact_refs(&repo_dir),
        vec![ref_1.clone()],
        "a final full pass mints no duplicate artifact IDs"
    );
    assert_eq!(
        spawns.get(),
        2,
        "recovery under the changed configuration never launched a worker"
    );
    assert_eq!(
        store
            .transact(|tx| tx.verification_results(&aid_1))
            .unwrap()
            .len(),
        1,
        "re-drives never duplicate verification evidence"
    );

    let selections = [&writer_1, &writer_2]
        .iter()
        .map(|writer| {
            let req = request(&store, &writer.exec);
            let binding = req.adapter_binding.as_ref().expect("bound at consume");
            selection_entry("prior_selection", &req.request_id, &writer.inv.0, binding)
        })
        .collect::<Vec<_>>();
    let identity_json = serde_json::json!({
        "installation": installation(),
        "worker_argv_change": {"before": prior_argv, "after": changed_argv},
        "adapter_selection": selections,
    });
    retain_record(
        "u4.6c-crash-recovery.json",
        "swap_recovery::u46c_interrupted_export_import_recovers_under_changed_config_without_duplicate_artifact_ids (WS-11)",
        identity_json,
        vec![
            "two real fixture writers under the prior selection finished (spawn_attempts=2); recovery under workloop-contract-worker launched nothing (spawn_attempts stayed 2)".to_string(),
            "window before export_started: no row → export → acknowledged; verification_argv[0] Pass bound to produced".to_string(),
            "window export_started + truncated bundle: partial deleted, re-exported, acknowledged; bundle verify passes".to_string(),
            "window bundle_written + leftover quarantine: quarantine discarded, bundle re-imported, ref recreated".to_string(),
            "window imported row without ref: ref recreated after manifest re-verification".to_string(),
            "window imported row with ref: acknowledged committed on read-back".to_string(),
            "window workspace lost: invalid recovery_exhausted at attempt 3 of bound 3, no ref minted".to_string(),
            format!("artifact ref set after every window and a final full pass: {ref_1} — no duplicate artifact IDs"),
            "one row per invocation under its deterministic ID; re-drives leave verification results at exactly 1".to_string(),
        ],
    );
}

// ---- WS-11/WS-01: recollection under the changed config ---------------

/// The named swap-window recollection scenario: a real fixture writer
/// launched under the prior selection finishes in a git workspace and
/// the Execution waits — mid-Execution — when `worker_argv` changes and
/// the supervisor restarts. The restarted supervisor recollects the
/// artifact to `acknowledged`, runs the declared verification on the
/// handed-off commit, and leaves the workspace and git objects intact
/// through the verified handoff; the request binding never moves.
#[test]
fn u46c_recollection_acknowledges_and_retains_git_objects_under_changed_config() {
    let root = tmp_root("recollection");
    let db = root.join("store.db");
    let ws_root = root.join("ws");
    let out_root = root.join("out");
    let repo_dir = bare_repo(&root);
    let layout = HandoffLayout {
        repository: repo_dir.clone(),
        workspace_root: ws_root.clone(),
        out_root: out_root.clone(),
    };
    let spawns = Rc::new(Cell::new(0usize));
    let prior_argv = vec![fixture_bin(), "clean-exit".into()];
    let changed_argv = vec![contract_bin(), "hang".into()];

    // Phase 1 — the prior selection: a real fixture writer finishes in
    // a git workspace; the Execution waits. No handoff configured: the
    // recollection must happen after the config change.
    let phase_one = {
        let store = SqliteStore::open(&db, 4).unwrap();
        let sup = Supervisor::new(
            CountingPosix {
                calls: spawns.clone(),
            },
            ws_root.clone(),
            prior_argv.clone(),
            config(),
        );
        let (work, exec) = submit_and_admit_with(
            &store,
            "u46c-recall",
            vec![vec!["git".into(), "rev-parse".into(), "HEAD".into()]],
        );
        let (_base, produced) = git_workspace(&ws_root.join(format!("ws-{}", work.0)), &work);
        sup.consume_requests(&store, NOW).unwrap();
        drive_until(&sup, &store, "the fixture writer finishes", || {
            invocations(&store, &exec)[0].state == InvocationState::Done
        });
        let settled = execution(&store, &exec);
        assert_eq!(settled.state, ExecutionState::Waiting);
        assert_eq!(settled.waiting_reason, Some(WaitingReason::HostWrite));
        let invocation = invocations(&store, &exec)[0].clone();
        let binding = request(&store, &exec)
            .adapter_binding
            .expect("bound under the prior selection");
        assert_eq!(binding.adapter.as_str(), "workloop-fixture-worker");
        (work, exec, invocation, produced, binding)
    };
    let (work, exec, invocation, produced, binding) = phase_one;
    assert_eq!(spawns.get(), 1);

    // Config changed mid-Execution: the Execution is still waiting when
    // the restarted supervisor runs under the new worker_argv.
    let store = SqliteStore::open(&db, 4).unwrap();
    let sup = Supervisor::new(
        CountingPosix {
            calls: spawns.clone(),
        },
        ws_root.clone(),
        changed_argv.clone(),
        config(),
    )
    .with_handoff(layout.clone());
    sup.run_once(&store, NOW + 1).unwrap();
    assert_eq!(
        spawns.get(),
        1,
        "recollection under the changed configuration launches nothing"
    );

    // Recollection reached acknowledgement: row, ref, and the declared
    // verification on the exact handed-off commit.
    let aid = format!("{}/artifact.json", invocation.id.0);
    let row = store
        .transact(|tx| tx.artifact_handoff(&aid))
        .unwrap()
        .expect("the sweep opened the export row");
    assert_eq!(row.state, HandoffState::Acknowledged);
    assert_eq!(row.produced.as_deref(), Some(produced.as_str()));
    assert_eq!(
        git(&repo_dir, &["rev-parse", &artifact_ref(&aid)]),
        produced,
        "the durable acknowledgement: the ref resolves the produced commit"
    );
    let results = store.transact(|tx| tx.verification_results(&aid)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].requirement, "verification_argv[0]");
    assert_eq!(results[0].result, VerificationOutcome::Pass);
    assert_eq!(results[0].produced, produced);
    assert_eq!(
        results[0].argv,
        vec![
            "git".to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string()
        ]
    );

    // Workspace and git objects survive the verified handoff.
    let ws_dir = ws_root.join(format!("ws-{}", work.0));
    assert!(
        ws_dir.join(".git").exists(),
        "the workspace is retained through the acknowledged handoff"
    );
    assert_eq!(
        git(&ws_dir, &["rev-parse", &format!("work/{}", work.0)]),
        produced,
        "the managed work branch still resolves the produced commit"
    );
    git(
        &ws_dir,
        &["cat-file", "-e", &format!("{produced}^{{commit}}")],
    );
    git(
        &repo_dir,
        &["cat-file", "-e", &format!("{produced}^{{commit}}")],
    );
    git(&repo_dir, &["fsck", "--connectivity-only"]);

    // The Invocation stays on its recorded selection; the changed
    // configuration never rewrote the binding.
    assert_eq!(
        request(&store, &exec).adapter_binding.as_ref(),
        Some(&binding),
        "the recorded binding survives the restart and the recollection"
    );
    assert_eq!(
        execution(&store, &exec).state,
        ExecutionState::Waiting,
        "recollection needs no terminal transition of the Execution"
    );

    let identity_json = serde_json::json!({
        "installation": installation(),
        "worker_argv_change": {"before": prior_argv, "after": changed_argv},
        "adapter_selection": [selection_entry(
            "prior_selection",
            &request(&store, &exec).request_id,
            &invocation.id.0,
            &binding,
        )],
    });
    retain_record(
        "u4.6c-recollection.json",
        "swap_recovery::u46c_recollection_acknowledges_and_retains_git_objects_under_changed_config (WS-11, WS-01)",
        identity_json,
        vec![
            "prior-selection fixture writer finished (spawn_attempts=1); the Execution waited HostWrite when worker_argv changed".to_string(),
            "restart pass under workloop-contract-worker: spawn_attempts stayed 1 — recollection launches nothing".to_string(),
            format!("handoff row acknowledged with produced {produced}; ref refs/taisan/artifacts/{aid} resolves it"),
            "verification_argv[0] Pass recorded on the exact handed-off commit".to_string(),
            "workspace .git and the work branch survive; the handoff repository holds the produced closure (cat-file + fsck connectivity clean)".to_string(),
            "the request binding is unchanged across the restart and the recollection".to_string(),
        ],
    );
}

// ---- evidence machinery checks ---------------------------------------

/// The stamping helpers are fail-closed: either both revisions are full
/// commit ids stamped over a pristine tree, or there is no stamp at all
/// — a dirty tree, a checkout git cannot answer, or an unresolvable
/// base. A placeholder such as `unknown` is never produced.
#[test]
fn u46c_revision_stamp_is_fail_closed() {
    match record_revisions() {
        Some((tested, base)) => {
            assert_commit_id(&tested, "tested_revision");
            assert_commit_id(&base, "base_revision");
            assert!(
                tracked_tree_is_pristine(),
                "a stamp may only describe a pristine checkout"
            );
        }
        None => assert!(
            !tracked_tree_is_pristine()
                || git_stdout(&["rev-parse", "HEAD"]).is_none()
                || base_revision().is_none(),
            "no stamp means a dirty tree or an unanswerable git — never a placeholder"
        ),
    }
}

/// The retained record carries exactly the closed field set of the U4.6
/// evidence record, the closed claim class, and every binding field on
/// each adapter-selection entry.
#[test]
fn u46c_retained_record_carries_the_closed_field_set() {
    let binding = WorkerAdapterBinding {
        adapter: AdapterId::trusted("workloop-fixture-worker"),
        adapter_version: "1.0.0".into(),
        runtime_version: "fixture-1.0.0".into(),
        capability_digest: "903619b2baf3427f681f7119da1644b2491a81a232948767360bcb8f88158f75"
            .into(),
        argv: vec!["workloop-fixture-worker".into(), "hang".into()],
    };
    let identity = serde_json::json!({
        "installation": installation(),
        "worker_argv_change": {"before": ["a"], "after": ["b"]},
        "adapter_selection": [selection_entry("prior_selection", "e-1:inv:0", "inv-1", &binding)],
    });
    let record = build_record(
        &"0".repeat(40),
        &"1".repeat(40),
        "swap_recovery::example (WS-11)",
        identity,
        vec!["example evidence".into()],
    );
    let object = record.as_object().expect("record is an object");
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "schema_version",
            "tested_revision",
            "base_revision",
            "claim_class",
            "configuration_identity",
            "command_scenario",
            "result",
            "evidence_refs",
        ]),
        "the closed U4.6 field set"
    );
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["claim_class"], "fixture_supported");
    assert_eq!(record["result"], "pass");
    let identity_keys: BTreeSet<&str> = record["configuration_identity"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        identity_keys,
        BTreeSet::from(["installation", "worker_argv_change", "adapter_selection",])
    );
    let entry = &record["configuration_identity"]["adapter_selection"][0];
    let entry_keys: BTreeSet<&str> = entry
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        entry_keys,
        BTreeSet::from([
            "phase",
            "adapter_id",
            "adapter_version",
            "runtime_version",
            "capability_digest",
            "argv",
            "request_id",
            "invocation_id",
        ]),
        "each selection entry carries the recorded binding and identity"
    );
}
