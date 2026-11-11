//! Provider-parameterized Worker contract suite (U4.1): the same
//! scenarios run unchanged against every Worker provider whose declared
//! model surface covers the scenario's requirements. A scenario a
//! provider cannot model is reported skipped for that provider — never
//! substituted or weakened (`docs/adapter-boundaries.md` Worker
//! contract, `docs/worker-protocol.md`).
//!
//! Providers under test:
//! - `PosixProvider`: `Supervisor<CountingPosix>` — an observational
//!   decorator that delegates spawn/inspect/signal to
//!   `PosixProcessControl` and records every spawn call and spawned
//!   identity — spawning the real `workloop-fixture-worker` binary —
//!   real spawn, real signals.
//! - `ScriptedProvider`: `Supervisor<ScriptedProcessControl>`, an
//!   independently implemented in-memory port that models spawn
//!   outcomes no argv can produce (lost/unknown starts, uninspectable
//!   liveness).
//!
//! Scenario assertions bind the durable store plus facts observable at
//! the port boundary — spawn count, delivered signals, recorded writer
//! liveness — never provider internals beyond what the port itself
//! exposes. A completed scenario is a covered contract row.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::identity::*;
use workloop_core::observation::*;
use workloop_core::ports::{
    ProbeOutcome, ProcessControlPort, ProcessEvidence, SignalOutcome, SpawnError,
};
use workloop_core::store::{Effect, Store};
use workloop_core::work::{
    HostTemplate, WorkloopConfig, admit_queued, handle_command, handle_observation, sweep,
};
use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};
use workloop_exec::supervisor::{PosixProcessControl, Supervisor};
use workloop_github::fixture::FixtureGithub;
use workloop_store::sqlite::SqliteStore;

const NOW: i64 = 1_700_000_000;

fn host() -> HostInstanceId {
    HostInstanceId::trusted("test-host")
}

fn repo() -> RepositoryRef {
    RepositoryRef::new(host(), RepositoryId::trusted("local-repo"))
}

fn fixture_bin() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/workloop-fixture-worker")
        .to_string_lossy()
        .into_owned()
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("workloop-wc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_with(host_write: bool) -> WorkloopConfig {
    WorkloopConfig {
        host: HostTemplate {
            adapter: AdapterId::trusted("worker-contract"),
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
            host_write,
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

fn config() -> WorkloopConfig {
    config_with(true)
}

fn config_denied() -> WorkloopConfig {
    config_with(false)
}

/// Grant the fixture host the publication capabilities the `publish`
/// stage requires — same shape as the supervisor contract suite.
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

fn req(key: &str, body: CommandBody) -> CommandRequest {
    CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
        origin: Origin("worker-contract".into()),
        idempotency_key: key.into(),
        body,
    }
}

fn submit_and_admit(store: &SqliteStore, cfg: &WorkloopConfig, key: &str) -> (WorkId, ExecutionId) {
    let d = handle_command(
        store,
        cfg,
        &req(
            key,
            CommandBody::Submit {
                intent: "worker contract work".into(),
                sources: vec![],
                constraints: vec![],
                verification_argv: vec![],
            },
        ),
        NOW,
    )
    .unwrap();
    let (work_id, eid) = match d {
        CommandDecision::Accepted {
            work_id,
            effect: CommandEffect::WorkCreated { execution_id },
            ..
        } => (work_id.unwrap(), execution_id),
        other => panic!("submit rejected: {other:?}"),
    };
    authorize(store);
    let admitted = store
        .transact(|tx| admit_queued(tx, cfg, &eid, NOW))
        .unwrap();
    assert!(admitted.is_some(), "execution admitted");
    (work_id, eid)
}

fn execution(store: &SqliteStore, id: &ExecutionId) -> Execution {
    store
        .transact(|tx| tx.execution(id))
        .unwrap()
        .expect("execution")
}

fn invocations(store: &SqliteStore, eid: &ExecutionId) -> Vec<Invocation> {
    store
        .transact(|tx| tx.invocations_for_execution(eid))
        .unwrap()
}

fn cancel(store: &SqliteStore, work_id: &WorkId, key: &str) {
    let version = store
        .transact(|tx| tx.work(work_id))
        .unwrap()
        .map(|w| w.version)
        .expect("work exists");
    let d = handle_command(
        store,
        &config(),
        &req(
            key,
            CommandBody::Cancel {
                work_id: work_id.clone(),
                expected_work_version: version,
                execution_id: None,
            },
        ),
        NOW,
    )
    .unwrap();
    assert!(
        matches!(d, CommandDecision::Accepted { .. }),
        "cancel: {d:?}"
    );
}

// ---- provider abstraction ------------------------------------------

/// The worker shape a scenario needs the provider to model.
#[derive(Clone, Copy)]
enum WorkerShape {
    /// Writes the required artifact and exits 0.
    CleanExit,
    /// Exits 0 without the required artifact.
    NoArtifact,
    /// Stays resident until stopped.
    Resident,
}

/// A start-attempt failure class a scenario injects.
#[derive(Clone, Copy)]
enum SpawnFail {
    /// Definitely no process exists.
    Refused,
    /// The outcome could not be learned; the writer may exist.
    Unknown,
}

/// The provider handle a scenario steers. Every method is expressed in
/// contract terms — no provider-internal encoding leaks into a
/// scenario.
trait WorkerHandle {
    fn consume(&mut self, store: &SqliteStore);
    fn poll(&mut self, store: &SqliteStore);
    fn drive_stops(&mut self, store: &SqliteStore);
    fn recover(&mut self, store: &SqliteStore);
    fn pass(&mut self, store: &SqliteStore) {
        self.consume(store);
        self.poll(store);
        self.drive_stops(store);
        self.recover(store);
    }
    /// Every writer this provider spawned exits now.
    fn exit_writers(&mut self);
    /// Spawn a writer through the provider's own port, outside the
    /// supervisor — models the first leg of a delivered-twice request:
    /// a consumer that already recorded an identity for it.
    fn spawn_direct(&mut self, cwd: &std::path::Path) -> ProcessIdentity;
    /// Whether the most recently spawned writer currently inspects
    /// alive — a provider-neutral read on the recorded identity.
    fn writer_alive(&self) -> bool;
    /// Toggle whether the recorded writer's liveness is uninspectable.
    /// Providers that cannot model this do not declare
    /// `liveness_unknown` and are skipped for the row.
    fn set_liveness_unknown(&mut self, on: bool);
    /// Toggle whether the recorded writer survives a delivered signal —
    /// the realizable gap between signal delivery and process death.
    /// Providers that cannot model this do not declare
    /// `signal_survived` and are skipped for the row.
    fn set_survives_signal(&mut self, on: bool);
    /// The next start attempt reports the given failure class.
    fn fail_next_spawn(&mut self, f: SpawnFail);
    /// Spawn calls observed so far — the no-second-writer counter.
    /// Every `spawn` attempt counts, refused and unknown outcomes
    /// included, so a scenario can prove no second start attempt even
    /// when the first attempt produced no writer.
    fn spawns(&self) -> usize;
    /// Signals the port verifiably delivered — the signal audit used to
    /// prove a signal landed while settlement still waits. Providers
    /// without a signal log do not declare `signal_audit`.
    fn signals_sent(&self) -> usize;
    /// The managed workspace of a Work.
    fn workspace(&self, work: &WorkId) -> PathBuf;
    /// Block until the provider's writers have exited (a real process
    /// needs wall-clock settle; a scripted provider returns at once).
    fn wait_writers(&mut self);
    /// Write a file into the Work's workspace — models a writer's own
    /// file production for artifact cases.
    fn write_workspace_file(&mut self, work: &WorkId, name: &str, content: &str);
}

/// A provider that can be constructed for one scenario. `declares` is
/// the model surface used for per-row skip gating.
trait WorkerProvider {
    fn declares() -> &'static [&'static str];
    fn make(shape: WorkerShape, tag: &str, config: &WorkloopConfig) -> Self
    where
        Self: Sized;
    fn handle(&mut self) -> &mut dyn WorkerHandle;
}

// ---- Posix provider: real spawn of the fixture worker --------------

/// Observational decorator: counts every spawn call — refused and
/// unknown attempts included — and records the identities the real port
/// produced, so a scenario can assert both no second attempt and no
/// second writer. It delegates every call to `PosixProcessControl` —
/// spawn, inspect, and signal semantics are the real port's. The
/// `fail_next` toggle injects a refusal at the spawn call itself: the
/// describe probe still answers, so the scenario models a start attempt
/// that fails after admission, not an adapter that cannot declare.
struct CountingPosix {
    spawned: Rc<RefCell<Vec<ProcessIdentity>>>,
    calls: Rc<Cell<usize>>,
    fail_next: Rc<Cell<bool>>,
}

impl ProcessControlPort for CountingPosix {
    fn spawn(&self, argv: &[String], cwd: &std::path::Path) -> Result<ProcessIdentity, SpawnError> {
        // Count the attempt before delegating: the counter observes
        // start calls, not only the ones that produced a writer.
        self.calls.set(self.calls.get() + 1);
        if self.fail_next.replace(false) {
            return Err(SpawnError::Refused {
                reason: "adapter declined the request".into(),
            });
        }
        let out = PosixProcessControl.spawn(argv, cwd);
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
    fn reap(&self, pid: i32) {
        PosixProcessControl.reap(pid)
    }
    fn run_probe(
        &self,
        argv: &[String],
        cwd: &std::path::Path,
        timeout: Duration,
    ) -> Result<ProbeOutcome, SpawnError> {
        PosixProcessControl.run_probe(argv, cwd, timeout)
    }
}

struct PosixProvider {
    sup: Supervisor<CountingPosix>,
    spawned: Rc<RefCell<Vec<ProcessIdentity>>>,
    calls: Rc<Cell<usize>>,
    fail_next: Rc<Cell<bool>>,
}

impl WorkerProvider for PosixProvider {
    fn declares() -> &'static [&'static str] {
        // Real spawn semantics only: outcomes a process cannot fake
        // (unknown identity capture, uninspectable liveness) are not
        // declared, and those rows are skipped — never substituted.
        &[
            "clean_exit",
            "no_artifact",
            "resident_writer",
            "spawn_refused",
            "stop_signal",
            "spawn_count",
        ]
    }
    fn make(shape: WorkerShape, tag: &str, config: &WorkloopConfig) -> Self {
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let calls = Rc::new(Cell::new(0));
        let fail_next = Rc::new(Cell::new(false));
        let mode = match shape {
            WorkerShape::CleanExit => vec![fixture_bin(), "clean-exit".into()],
            WorkerShape::NoArtifact => vec![fixture_bin(), "exit-no-artifact".into()],
            WorkerShape::Resident => vec![fixture_bin(), "hang".into()],
        };
        PosixProvider {
            sup: Supervisor::new(
                CountingPosix {
                    spawned: spawned.clone(),
                    calls: calls.clone(),
                    fail_next: fail_next.clone(),
                },
                tmp_root(tag),
                mode,
                config.clone(),
            ),
            spawned,
            calls,
            fail_next,
        }
    }
    fn handle(&mut self) -> &mut dyn WorkerHandle {
        self
    }
}

impl WorkerHandle for PosixProvider {
    fn consume(&mut self, store: &SqliteStore) {
        self.sup.consume_requests(store, NOW).unwrap();
    }
    fn poll(&mut self, store: &SqliteStore) {
        self.sup.poll_invocations(store, NOW).unwrap();
    }
    fn drive_stops(&mut self, store: &SqliteStore) {
        self.sup.drive_stops(store, NOW).unwrap();
    }
    fn recover(&mut self, store: &SqliteStore) {
        self.sup.run_recovery(store, NOW).unwrap();
    }
    fn exit_writers(&mut self) {
        for id in self.spawned.borrow().iter() {
            unsafe { libc::kill(id.pid, libc::SIGKILL) };
        }
    }
    fn spawn_direct(&mut self, cwd: &std::path::Path) -> ProcessIdentity {
        self.sup
            .port
            .spawn(&self.sup.worker_argv.clone(), cwd)
            .expect("direct spawn")
    }
    fn writer_alive(&self) -> bool {
        self.spawned.borrow().last().is_some_and(|id| {
            matches!(
                PosixProcessControl.inspect(id),
                ProcessEvidence::Alive { .. }
            )
        })
    }
    fn set_liveness_unknown(&mut self, _on: bool) {
        unreachable!("PosixProvider does not declare liveness_unknown")
    }
    fn set_survives_signal(&mut self, _on: bool) {
        unreachable!("PosixProvider does not declare signal_survived")
    }
    fn fail_next_spawn(&mut self, f: SpawnFail) {
        match f {
            // The describe probe still answers; the start attempt after
            // admission is refused — the same refusal class Scripted
            // injects on its spawn plan.
            SpawnFail::Refused => {
                self.fail_next.set(true);
            }
            SpawnFail::Unknown => {
                unreachable!("PosixProvider does not declare spawn_unknown")
            }
        }
    }
    fn spawns(&self) -> usize {
        self.calls.get()
    }
    fn signals_sent(&self) -> usize {
        unreachable!("PosixProvider does not declare signal_audit")
    }
    fn workspace(&self, work: &WorkId) -> PathBuf {
        self.sup.workspace_of(work)
    }
    fn wait_writers(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let all_stopped = self.spawned.borrow().iter().all(|id| {
                matches!(
                    PosixProcessControl.inspect(id),
                    ProcessEvidence::Stopped { .. }
                )
            });
            if all_stopped || Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn write_workspace_file(&mut self, work: &WorkId, name: &str, content: &str) {
        let path = self.workspace(work).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

// ---- Scripted provider: in-memory process table --------------------

struct ScriptedProvider {
    port: ScriptedProcessControl,
    sup: Supervisor<ScriptedProcessControl>,
}

impl WorkerProvider for ScriptedProvider {
    fn declares() -> &'static [&'static str] {
        &[
            "clean_exit",
            "no_artifact",
            "resident_writer",
            "spawn_refused",
            "spawn_unknown",
            "liveness_unknown",
            "stop_signal",
            "spawn_count",
            "signal_audit",
            "signal_survived",
        ]
    }
    fn make(shape: WorkerShape, tag: &str, config: &WorkloopConfig) -> Self {
        let port = ScriptedProcessControl::new();
        port.set_default_plan(match shape {
            WorkerShape::CleanExit => SpawnPlan::ExitAfterWrite {
                artifacts: vec![(
                    "artifact.json".into(),
                    "{\"status\":\"done\",\"worker\":\"scripted\"}\n".into(),
                )],
                exit_code: 0,
            },
            WorkerShape::NoArtifact => SpawnPlan::ExitAfterWrite {
                artifacts: vec![],
                exit_code: 0,
            },
            WorkerShape::Resident => SpawnPlan::Resident,
        });
        ScriptedProvider {
            sup: Supervisor::new(
                port.clone(),
                tmp_root(tag),
                vec!["scripted".into()],
                config.clone(),
            ),
            port,
        }
    }
    fn handle(&mut self) -> &mut dyn WorkerHandle {
        self
    }
}

impl WorkerHandle for ScriptedProvider {
    fn consume(&mut self, store: &SqliteStore) {
        self.sup.consume_requests(store, NOW).unwrap();
    }
    fn poll(&mut self, store: &SqliteStore) {
        self.sup.poll_invocations(store, NOW).unwrap();
    }
    fn drive_stops(&mut self, store: &SqliteStore) {
        self.sup.drive_stops(store, NOW).unwrap();
    }
    fn recover(&mut self, store: &SqliteStore) {
        self.sup.run_recovery(store, NOW).unwrap();
    }
    fn exit_writers(&mut self) {
        self.port.exit_all(0);
    }
    fn spawn_direct(&mut self, cwd: &std::path::Path) -> ProcessIdentity {
        self.port
            .spawn(&["scripted".into()], cwd)
            .expect("direct spawn")
    }
    fn writer_alive(&self) -> bool {
        self.port
            .spawned()
            .last()
            .is_some_and(|id| matches!(self.port.inspect(id), ProcessEvidence::Alive { .. }))
    }
    fn set_liveness_unknown(&mut self, on: bool) {
        let spawned = self.port.spawned();
        let leader = spawned.last().expect("a spawned writer").pid;
        self.port
            .set_inspect_unknown(leader, on.then(|| "process table read unavailable".into()));
    }
    fn set_survives_signal(&mut self, on: bool) {
        let spawned = self.port.spawned();
        let leader = spawned.last().expect("a spawned writer").pid;
        self.port.set_survives_signal(leader, on);
    }
    fn fail_next_spawn(&mut self, f: SpawnFail) {
        self.port.push_plan(match f {
            SpawnFail::Refused => SpawnPlan::Refused {
                reason: "adapter declined the request".into(),
            },
            SpawnFail::Unknown => SpawnPlan::Unknown {
                reason: "identity capture lost after spawn".into(),
            },
        });
    }
    fn spawns(&self) -> usize {
        self.port.spawn_calls()
    }
    fn signals_sent(&self) -> usize {
        self.port.signals().len()
    }
    fn workspace(&self, work: &WorkId) -> PathBuf {
        self.sup.workspace_of(work)
    }
    fn wait_writers(&mut self) {}
    fn write_workspace_file(&mut self, work: &WorkId, name: &str, content: &str) {
        let path = self.workspace(work).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

// ---- scenarios ------------------------------------------------------

/// Drive provider passes until `pred` holds or the bound expires.
/// Scripted providers settle in one pass; real processes need a short
/// wall-clock settle.
fn drive_until(
    p: &mut dyn WorkerHandle,
    store: &SqliteStore,
    what: &str,
    mut pred: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        p.pass(store);
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// WC-START-01: the same durable request delivered a second time — to
/// a consumer that finds a recorded Invocation for it — reconciles by
/// request identity: it inspects the recorded writer, never spawns
/// another.
fn duplicate_request_delivery_starts_no_second_writer(
    p: &mut dyn WorkerHandle,
    store: &SqliteStore,
) {
    let (work_id, eid) = submit_and_admit(store, &config(), "wc-start-1");
    let request = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request");
    // The delivered-twice window: a first consumer already spawned and
    // recorded a writer identity for this request, and the durable
    // request is presented again while still unconsumed — the handoff
    // mark was lost between delivery and consumption.
    let ws = p.workspace(&work_id);
    std::fs::create_dir_all(&ws).unwrap();
    let identity = p.spawn_direct(&ws);
    store
        .transact(|tx| {
            let inv_id = tx.alloc_id("inv")?;
            tx.apply(&Effect::InsertInvocation {
                invocation: Invocation {
                    id: InvocationId(inv_id),
                    request_id: request.request_id.clone(),
                    execution_id: eid.clone(),
                    role: InvocationRole::Coding,
                    state: InvocationState::Running,
                    process_identity: Some(identity),
                    descendants: vec![],
                    required_artifacts: vec![],
                    evidence_refs: vec![],
                },
            })
        })
        .unwrap();
    let spawns_before = p.spawns();
    p.consume(store);
    p.consume(store);
    assert_eq!(
        p.spawns(),
        spawns_before,
        "a re-delivered request reconciles the recorded writer — no second spawn"
    );
    assert_eq!(
        invocations(store, &eid).len(),
        1,
        "one Invocation per request"
    );
    // The reconcile path really ran: it inspected the recorded writer
    // and re-asserted it running — it did not skip the request.
    assert_eq!(execution(store, &eid).state, ExecutionState::Running);
    p.exit_writers();
}

/// WC-START-02: a start whose outcome was lost leaves the writer
/// uncertain — never a silent not-started that permits a replacement.
/// Repeated loop passes reconcile the unidentifiable Invocation through
/// the poll path: uncertainty is re-reported, a second start never is.
fn unknown_start_outcome_blocks_a_second_writer(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (_w, eid) = submit_and_admit(store, &config(), "wc-start-2");
    p.fail_next_spawn(SpawnFail::Unknown);
    p.consume(store);
    // Re-drive the loop: the poll path re-reports the unidentifiable
    // Invocation as unknown — reconciliation, never a second start.
    p.pass(store);
    p.pass(store);
    assert_eq!(
        p.spawns(),
        1,
        "an unknown start outcome is never retried into a second writer"
    );
    let invocations = invocations(store, &eid);
    assert_eq!(invocations.len(), 1);
    assert!(
        invocations[0].state == InvocationState::Prepared
            && invocations[0].process_identity.is_none(),
        "an unrecorded start never claims a writer identity: {:?}",
        invocations[0].state
    );
    assert_eq!(
        execution(store, &eid).state,
        ExecutionState::Uncertain,
        "the Execution stays uncertain — never silently not-started"
    );
}

/// WC-START-03: a definitely-refused start pauses without a writer and
/// releases the slot — refusal is not uncertainty. The spawn counter
/// observes the refused attempt itself, so the scenario can prove the
/// refusal was attempted exactly once and never retried.
fn refused_start_pauses_without_a_writer(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (_w, eid) = submit_and_admit(store, &config(), "wc-start-3");
    let spawns_before = p.spawns();
    p.fail_next_spawn(SpawnFail::Refused);
    p.consume(store);
    assert_eq!(
        p.spawns(),
        spawns_before + 1,
        "the refused start attempt is observed by the counter"
    );
    let e = execution(store, &eid);
    assert_eq!(
        e.state,
        ExecutionState::Paused,
        "a refused launch pauses the Execution"
    );
    p.pass(store);
    assert_eq!(
        invocations(store, &eid)
            .iter()
            .filter(|i| i.state == InvocationState::Running)
            .count(),
        0,
        "refusal never produced a writer"
    );
    assert_eq!(
        p.spawns(),
        spawns_before + 1,
        "a refused start is not retried into a second attempt"
    );
}

/// WC-LIVE-01: uninspectable liveness is not death — the writer keeps
/// ownership, no replacement is attempted, and recovered liveness
/// re-asserts the same recorded writer running.
fn unknown_liveness_is_not_death(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (_w, eid) = submit_and_admit(store, &config(), "wc-live-1");
    p.consume(store);
    assert_eq!(p.spawns(), 1);
    let recorded = invocations(store, &eid)[0]
        .process_identity
        .clone()
        .expect("a recorded writer identity");
    p.set_liveness_unknown(true);
    p.poll(store);
    let invs = invocations(store, &eid);
    assert_eq!(invs.len(), 1, "no replacement Invocation");
    assert_ne!(
        invs[0].state,
        InvocationState::Done,
        "an uninspectable writer is never finished"
    );
    assert_eq!(p.spawns(), 1, "no replacement spawn while uncertain");
    // Liveness recovers: the same recorded writer is re-asserted running.
    p.set_liveness_unknown(false);
    p.poll(store);
    assert_eq!(p.spawns(), 1);
    assert_eq!(
        invocations(store, &eid)[0].process_identity.as_ref(),
        Some(&recorded),
        "recovered liveness re-asserts the same writer identity"
    );
    assert_eq!(
        execution(store, &eid).state,
        ExecutionState::Running,
        "the same writer is reported running again"
    );
    p.exit_writers();
}

/// WC-STOP-01: a cancel request is not terminal — while the recorded
/// writer is provably alive the Execution sits in `stopping`, and it
/// reaches `cancelled` only after the stop pass verifies the writer
/// stopped.
fn cancel_waits_for_stop_proof(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (work_id, eid) = submit_and_admit(store, &config(), "wc-stop-1");
    p.consume(store);
    assert_eq!(p.spawns(), 1);
    cancel(store, &work_id, "wc-stop-1-cancel");
    // Before any stop pass: the writer is provably still writing and the
    // Execution has entered `stopping` — not a settled state.
    assert!(
        p.writer_alive(),
        "the recorded writer is still writing before the stop pass"
    );
    assert_eq!(execution(store, &eid).state, ExecutionState::Stopping);
    drive_until(p, store, "cancelled", || {
        execution(store, &eid).state == ExecutionState::Cancelled
    });
    assert!(
        !p.writer_alive(),
        "settlement waited for the writer's verified stop"
    );
}

/// WC-STOP-02: a delivered signal is not stop proof — a writer that
/// survived its signal keeps the Execution in `stopping`, and a stop
/// that then becomes unverifiable still cannot settle, even though the
/// signal audit shows delivery.
fn a_delivered_signal_is_not_stop_proof(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (work_id, eid) = submit_and_admit(store, &config(), "wc-stop-2");
    p.consume(store);
    cancel(store, &work_id, "wc-stop-2-cancel");
    // The signal lands behind the durable intent, but the writer is
    // still alive: proof is withheld, not settled.
    p.set_survives_signal(true);
    p.drive_stops(store);
    assert!(
        p.signals_sent() > 0,
        "the stop signal was actually delivered"
    );
    assert!(p.writer_alive(), "the writer survived the delivered signal");
    assert_eq!(
        execution(store, &eid).state,
        ExecutionState::Stopping,
        "a delivered signal on a live writer is not proof"
    );
    // The liveness read then becomes unverifiable: still no settlement —
    // delivered signals plus an Unknown read are not proof either.
    p.set_liveness_unknown(true);
    p.drive_stops(store);
    assert_ne!(
        execution(store, &eid).state,
        ExecutionState::Cancelled,
        "an unverifiable stop never settles — delivered signals are not proof"
    );
    // Verification and the kill path recover: the writer inspects
    // stopped only after a signal it does not survive, and the
    // Execution settles.
    p.set_liveness_unknown(false);
    p.set_survives_signal(false);
    drive_until(p, store, "cancelled", || {
        execution(store, &eid).state == ExecutionState::Cancelled
    });
}

/// WC-ART-01: a confirmed exit without the required artifact runs
/// bounded recovery and then fails — never success.
fn missing_artifact_runs_bounded_recovery_then_fails(
    p: &mut dyn WorkerHandle,
    store: &SqliteStore,
) {
    let (_w, eid) = submit_and_admit(store, &config(), "wc-art-1");
    p.consume(store);
    p.wait_writers();
    drive_until(p, store, "artifact recovery", || {
        let e = execution(store, &eid);
        e.state == ExecutionState::Waiting
            && e.waiting_reason == Some(WaitingReason::ArtifactRecovery)
    });
    for _ in 0..4 {
        p.pass(store);
        sweep(store, &config(), NOW).unwrap();
    }
    let e = execution(store, &eid);
    assert_eq!(e.state, ExecutionState::Failed);
    assert_eq!(
        e.terminal_code.as_deref(),
        Some("artifact_recovery_exhausted")
    );
}

/// WC-ART-02: an artifact that cannot count — an empty file, or content
/// whose digest differs from the required one — never satisfies the
/// required set.
fn invalid_artifact_never_counts(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (work_id, eid) = submit_and_admit(store, &config(), "wc-art-2");
    p.consume(store);
    p.wait_writers();
    drive_until(p, store, "artifact recovery", || {
        let e = execution(store, &eid);
        e.state == ExecutionState::Waiting
            && e.waiting_reason == Some(WaitingReason::ArtifactRecovery)
    });
    let any_present = || {
        store
            .transact(|tx| tx.observations_for_execution(&eid))
            .unwrap()
            .iter()
            .any(|o| {
                matches!(
                    &o.fact,
                    ObservationFact::Artifact {
                        state: ArtifactFact::Present { .. },
                        ..
                    }
                )
            })
    };
    // An empty artifact carries no digest: it is absent, not Present.
    p.write_workspace_file(&work_id, "artifact.json", "");
    p.recover(store);
    assert!(
        !any_present(),
        "an empty artifact is never reported Present"
    );
    // Real content whose digest cannot match the required one: recovery
    // never records it Present, so it never satisfies.
    let required_digest = sha256_hex("artifact-v1", b"not-the-artifact");
    store
        .transact(|tx| {
            let mut inv = tx
                .invocations_for_execution(&eid)?
                .into_iter()
                .next()
                .expect("one invocation");
            for a in &mut inv.required_artifacts {
                a.expected_digest = Some(required_digest.clone());
            }
            tx.apply(&Effect::UpdateInvocation {
                invocation: inv.clone(),
                expected_state: inv.state.clone(),
            })
        })
        .unwrap();
    p.write_workspace_file(&work_id, "artifact.json", "real-but-wrong-bytes");
    p.recover(store);
    assert!(
        !any_present(),
        "digest-mismatched content is never reported Present"
    );
    assert_eq!(
        execution(store, &eid).waiting_reason,
        Some(WaitingReason::ArtifactRecovery),
        "a digest mismatch keeps the artifact requirement unsatisfied"
    );
    for _ in 0..4 {
        p.pass(store);
        sweep(store, &config(), NOW).unwrap();
    }
    assert_eq!(
        execution(store, &eid).state,
        ExecutionState::Failed,
        "invalid artifacts cannot satisfy; bounded recovery exhausts"
    );
}

/// WC-REV-01: evidence produced for a superseded Invocation — the stale
/// producer identity — cannot satisfy the current Invocation's
/// required set.
fn stale_revision_evidence_cannot_satisfy_current(_p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (_w, eid) = submit_and_admit(store, &config(), "wc-rev-1");
    let request_id = store
        .transact(|tx| tx.pending_requests())
        .unwrap()
        .into_iter()
        .next()
        .expect("one durable request")
        .request_id;
    let required = vec![RequiredArtifact {
        kind: "artifact.json".into(),
        path: "artifact.json".into(),
        expected_digest: None,
    }];
    // A stale producer (the superseded attempt) and the current one,
    // both finished — completion turns on whether the artifact fact is
    // bound to its producer identity.
    store
        .transact(|tx| {
            // `request_id` is unique per Invocation: the stale producer's
            // request is a superseded row, not a second claim on the
            // current request.
            for (id, req_id) in [
                ("inv-stale", format!("{request_id}:prev")),
                ("inv-current", request_id.clone()),
            ] {
                tx.apply(&Effect::InsertInvocation {
                    invocation: Invocation {
                        id: InvocationId(id.into()),
                        request_id: req_id,
                        execution_id: eid.clone(),
                        role: InvocationRole::Coding,
                        state: InvocationState::Done,
                        process_identity: None,
                        descendants: vec![],
                        required_artifacts: required.clone(),
                        evidence_refs: vec![],
                    },
                })?;
            }
            let mut e = tx.execution(&eid)?.unwrap();
            e.state = ExecutionState::Waiting;
            e.waiting_reason = Some(WaitingReason::Ci);
            tx.apply(&Effect::UpdateExecution {
                execution: e,
                expected_state: ExecutionState::Ready,
            })
        })
        .unwrap();
    // The artifact exists — but only under the stale producer's
    // identity. If evidence were not producer-bound, the current
    // Invocation would complete and a publish intent would be recorded.
    handle_observation(
        store,
        &config(),
        &Observation {
            delivery: None,
            subject_work_id: None,
            subject_execution_id: Some(eid.clone()),
            fact: ObservationFact::Artifact {
                invocation_id: InvocationId("inv-stale".into()),
                required_artifact: "artifact.json".into(),
                state: ArtifactFact::Present {
                    digest: "stale-digest".into(),
                },
            },
            observed_unix_seconds: NOW,
        },
        NOW,
    )
    .unwrap();
    let e = execution(store, &eid);
    assert_eq!(
        e.waiting_reason,
        Some(WaitingReason::Ci),
        "a stale producer's artifact cannot satisfy the current \
         Invocation — the wait reason never advances to HostWrite"
    );
    assert!(
        store.transact(|tx| tx.action_intents()).unwrap().is_empty(),
        "no effect is authorized by stale evidence — had the stale \
         Present fact satisfied the current set, a publish intent \
         would exist"
    );
}

/// WC-DENY-01: with `host_write` denied, a clean worker run produces
/// local evidence and a recorded intent only — the authorized
/// application boundary applies nothing and reports no fabricated
/// failure; the intent stays pending.
fn denied_write_produces_no_effect(p: &mut dyn WorkerHandle, store: &SqliteStore) {
    let (_w, eid) = submit_and_admit(store, &config_denied(), "wc-deny-1");
    p.consume(store);
    p.wait_writers();
    drive_until(p, store, "worker finish observed", || {
        execution(store, &eid).state == ExecutionState::Waiting
    });
    // Draining the outbox — under the denied installation config and
    // then under a permissive one — applies nothing: the execution-level
    // grant recorded at admission is what binds. Denial is neither an
    // applied effect nor a fabricated failure.
    let mut remote = FixtureGithub::new();
    remote.apply_outbox(store, &config_denied(), NOW).unwrap();
    remote.apply_outbox(store, &config(), NOW).unwrap();
    let intents = store.transact(|tx| tx.action_intents()).unwrap();
    assert!(
        !intents.is_empty(),
        "a finished worker records its publish intent"
    );
    assert!(
        intents
            .iter()
            .all(|i| i.status == HostActionStatus::Pending),
        "denied write leaves intents pending — no applied or fabricated failure: {intents:?}"
    );
    assert!(remote.applied().is_empty(), "no effect reached the remote");
    assert_ne!(
        execution(store, &eid).state,
        ExecutionState::Succeeded,
        "a denied write never completes the Work"
    );
}

// ---- the shared suite -----------------------------------------------

struct WcScenario {
    id: &'static str,
    requires: &'static [&'static str],
    shape: WorkerShape,
    config: fn() -> WorkloopConfig,
    run: fn(&mut dyn WorkerHandle, &SqliteStore),
}

fn scenarios() -> Vec<WcScenario> {
    vec![
        WcScenario {
            id: "WC-START-01 duplicate request delivery starts no second writer",
            requires: &["resident_writer", "spawn_count"],
            shape: WorkerShape::Resident,
            config,
            run: duplicate_request_delivery_starts_no_second_writer,
        },
        WcScenario {
            id: "WC-START-02 unknown start outcome blocks a second writer",
            requires: &["spawn_unknown", "spawn_count"],
            shape: WorkerShape::Resident,
            config,
            run: unknown_start_outcome_blocks_a_second_writer,
        },
        WcScenario {
            id: "WC-START-03 refused start pauses without a writer",
            requires: &["spawn_refused", "spawn_count"],
            shape: WorkerShape::Resident,
            config,
            run: refused_start_pauses_without_a_writer,
        },
        WcScenario {
            id: "WC-LIVE-01 unknown liveness is not death",
            requires: &["liveness_unknown"],
            shape: WorkerShape::Resident,
            config,
            run: unknown_liveness_is_not_death,
        },
        WcScenario {
            id: "WC-STOP-01 cancel waits for stop proof",
            requires: &["resident_writer", "stop_signal"],
            shape: WorkerShape::Resident,
            config,
            run: cancel_waits_for_stop_proof,
        },
        WcScenario {
            id: "WC-STOP-02 a delivered signal is not stop proof",
            requires: &[
                "resident_writer",
                "stop_signal",
                "signal_survived",
                "liveness_unknown",
                "signal_audit",
            ],
            shape: WorkerShape::Resident,
            config,
            run: a_delivered_signal_is_not_stop_proof,
        },
        WcScenario {
            id: "WC-ART-01 missing artifact runs bounded recovery then fails",
            requires: &["no_artifact"],
            shape: WorkerShape::NoArtifact,
            config,
            run: missing_artifact_runs_bounded_recovery_then_fails,
        },
        WcScenario {
            id: "WC-ART-02 invalid artifact never counts",
            requires: &["no_artifact"],
            shape: WorkerShape::NoArtifact,
            config,
            run: invalid_artifact_never_counts,
        },
        WcScenario {
            id: "WC-REV-01 stale producer evidence cannot satisfy current",
            requires: &[],
            shape: WorkerShape::NoArtifact,
            config,
            run: stale_revision_evidence_cannot_satisfy_current,
        },
        WcScenario {
            id: "WC-DENY-01 denied write produces no effect",
            requires: &["clean_exit"],
            shape: WorkerShape::CleanExit,
            config: config_denied,
            run: denied_write_produces_no_effect,
        },
    ]
}

/// Run every scenario the provider's declared surface covers and
/// return the (covered, skipped) row ids — mirroring the host
/// conformance runner's report shape.
fn run_suite<P: WorkerProvider>(tag: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let declared: BTreeSet<&'static str> = P::declares().iter().copied().collect();
    let mut covered_ids = Vec::new();
    let mut skipped_ids = Vec::new();
    for scenario in scenarios() {
        if scenario.requires.iter().all(|r| declared.contains(r)) {
            let store = SqliteStore::in_memory(2).unwrap();
            let cfg = (scenario.config)();
            let mut provider = P::make(scenario.shape, tag, &cfg);
            (scenario.run)(provider.handle(), &store);
            covered_ids.push(scenario.id);
        } else {
            skipped_ids.push(scenario.id);
        }
    }
    (covered_ids, skipped_ids)
}

#[test]
fn posix_provider_runs_the_suite_within_its_declared_surface() {
    let (covered, skipped) = run_suite::<PosixProvider>("posix");
    // Outcomes a real process cannot fake are reported skipped, never
    // substituted: unknown identity capture and uninspectable liveness.
    assert_eq!(
        skipped,
        vec![
            "WC-START-02 unknown start outcome blocks a second writer",
            "WC-LIVE-01 unknown liveness is not death",
            "WC-STOP-02 a delivered signal is not stop proof",
        ]
    );
    assert_eq!(
        covered.len(),
        scenarios().len() - skipped.len(),
        "{covered:?}"
    );
}

#[test]
fn scripted_provider_runs_the_same_suite() {
    let (covered, skipped) = run_suite::<ScriptedProvider>("scripted");
    assert!(skipped.is_empty());
    assert_eq!(covered.len(), scenarios().len(), "{covered:?}");
}
