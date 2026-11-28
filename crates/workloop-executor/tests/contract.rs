//! Contract fixtures for the remote executor endpoint (#377):
//! lifecycle, replay, stop, reconnect, restart, and refusal cases
//! proven against a scripted process table — no live remote host,
//! matching the task's fixture rule. The only real channel the suite
//! exercises is a local `UnixStream` pair between the coordinator-side
//! adapter and the endpoint's session loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use workloop_core::domain::sha256_hex;
use workloop_core::executor::{
    EXECUTOR_PROTOCOL_V1, EnvelopeError, ExecutorCommandV1, ExecutorIdentity, ExecutorReceiptV1,
    OperationKind, endpoint_identity_digest, executor_canonical_json, launch_config_digest,
};
use workloop_core::identity::AdapterId;
use workloop_core::ports::ProbeOutcome;
use workloop_exec::adapter::WorkerDeclaration;
use workloop_exec::scripted::{ScriptedProcessControl, SpawnPlan};
use workloop_executor::adapter::RemoteExecutor;
use workloop_executor::config::{CoordinatorEndpoint, EndpointConfig};
use workloop_executor::runner::item_digest;
use workloop_executor::service::ExecutorService;
use workloop_executor::state::EndpointState;

const INSTALLATION: &str = "inst-fixture";
const ENDPOINT: &str = "ep-fixture";
const ENDPOINT_CRED: &[u8] = b"endpoint-credential-fixture";
const COORDINATOR_CRED: &[u8] = b"coordinator-credential-fixture";
const PROFILE: &str = workloop_core::worker::FIXTURE_ISOLATION_PROFILE_ID;

/// The declaration the scripted `describe` probe answers — the same
/// shape the fixture worker emits over `--wire v2`.
const DECLARATION: &str = r#"{
    "format_version": "taisan.worker.declaration/1",
    "adapter": "workloop-fixture-worker",
    "adapter_version": "1.0.0",
    "runtime": {"product": "fixture-runtime", "version": "fixture-1.0.0"},
    "protocol_versions": ["taisan.worker.request/2", "taisan.worker.report/2"],
    "roles": ["coding", "fix", "invariant_check", "review"],
    "capabilities": ["workspace_write", "workspace_read_only", "artifact_json", "process_control"],
    "resume": "unsupported",
    "checkpoints": "workspace_commits",
    "runtime_artifact_store": null,
    "local_retries": "runtime_internal"
}"#;

fn declaration_surface() -> WorkerDeclaration {
    WorkerDeclaration {
        adapter: AdapterId::new("workloop-fixture-worker").expect("adapter id"),
        adapter_version: "1.0.0".into(),
        runtime_product: "fixture-runtime".into(),
        runtime_version: "fixture-1.0.0".into(),
        protocol_versions: vec![
            "taisan.worker.request/2".into(),
            "taisan.worker.report/2".into(),
        ],
        roles: vec![
            "coding".into(),
            "fix".into(),
            "invariant_check".into(),
            "review".into(),
        ],
        capabilities: vec![
            "workspace_write".into(),
            "workspace_read_only".into(),
            "artifact_json".into(),
            "process_control".into(),
        ],
        declared_surface: json!({
            "resume": "unsupported",
            "checkpoints": "workspace_commits",
            "runtime_artifact_store": null,
            "local_retries": "runtime_internal",
        }),
    }
}

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn test_dir(name: &str) -> PathBuf {
    let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "taisan-executor-test-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

fn write_credentials(dir: &Path) {
    std::fs::write(dir.join("endpoint.cred"), ENDPOINT_CRED).expect("endpoint cred");
    std::fs::write(dir.join("coordinator.cred"), COORDINATOR_CRED).expect("coordinator cred");
}

fn endpoint_config(dir: &Path) -> EndpointConfig {
    EndpointConfig {
        installation_id: INSTALLATION.into(),
        endpoint_id: ENDPOINT.into(),
        state_root: dir.join("state"),
        socket_path: dir.join("executor.sock"),
        endpoint_credential_file: dir.join("endpoint.cred"),
        coordinator_credential_file: dir.join("coordinator.cred"),
        worker_argv: vec!["fixture-worker".into()],
        isolation_profile_id: PROFILE.into(),
        supported_protocols: vec![EXECUTOR_PROTOCOL_V1.into()],
        max_clock_offset_ms: 0,
        max_one_way_delay_ms: 0,
        max_state_bytes: 256 * 1024 * 1024,
    }
}

fn coordinator_binding() -> CoordinatorEndpoint {
    CoordinatorEndpoint {
        installation_id: INSTALLATION.into(),
        endpoint_id: ENDPOINT.into(),
        expected_identity_digest: endpoint_identity_digest(ENDPOINT_CRED),
        endpoint_credential: ENDPOINT_CRED.to_vec(),
        coordinator_credential: COORDINATOR_CRED.to_vec(),
        allowed_protocols: vec![EXECUTOR_PROTOCOL_V1.into()],
    }
}

struct Fixture {
    service: ExecutorService,
    port: ScriptedProcessControl,
    _dir: PathBuf,
    state: EndpointState,
}

fn fixture(name: &str) -> Fixture {
    let dir = test_dir(name);
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        ExecutorService::boot_with_port(config, Arc::new(port.clone())).expect("service boots");
    let state = EndpointState::open(dir.join("state")).expect("state");
    Fixture {
        service,
        port,
        _dir: dir,
        state,
    }
}

fn launch_config(deadline_unix_seconds: i64) -> serde_json::Value {
    json!({
        "profile_id": PROFILE,
        "work_id": "w-fixture",
        "workspace_grant": "writable",
        "workspace_git": false,
        "worker_binding": {
            "adapter": "workloop-fixture-worker",
            "adapter_version": "1.0.0",
            "runtime_version": "fixture-1.0.0",
            "capability_digest": declaration_surface().capability_digest(),
        },
        "required_capabilities": [],
        "limits": {
            "max_input_bytes": 1048576,
            "max_output_bytes": 1048576,
            "max_item_count": 32,
            "operation_timeout_ms": 60000
        },
        "deadline_unix_seconds": deadline_unix_seconds,
        "stop_grace_ms": 250,
        "runtime_auth_mount": false
    })
}

fn launch_config_digest_of(config: &serde_json::Value) -> String {
    launch_config_digest(&executor_canonical_json(config).expect("canonical"))
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const REQUEST_ID: &str = "e-7:inv:2";
const INVOCATION_ID: &str = "inv-41";

fn request_body() -> Vec<u8> {
    br#"{"format_version":"taisan.worker.request/2","work":"fixture"}"#.to_vec()
}

fn request_digest() -> String {
    sha256_hex("taisan.test.request", &request_body())
}

fn identity(service: &ExecutorService) -> ExecutorIdentity {
    ExecutorIdentity {
        installation_id: INSTALLATION.into(),
        endpoint_id: ENDPOINT.into(),
        endpoint_identity_digest: endpoint_identity_digest(ENDPOINT_CRED),
        bound_incarnation_id: service.incarnation().to_string(),
        request_id: REQUEST_ID.into(),
        invocation_id: INVOCATION_ID.into(),
        request_digest: request_digest(),
    }
}

fn prepare_payload(launch_config: &serde_json::Value) -> serde_json::Value {
    let body = request_body();
    json!({
        "launch_config": launch_config,
        "launch_config_digest": launch_config_digest_of(launch_config),
        "items": [
            {
                "item_id": "request",
                "dest": "request",
                "path": "",
                "digest": item_digest(&body),
                "bytes": body.len(),
                "data_b64": workloop_executor::auth::base64_nopad_encode(&body),
            },
            {
                "item_id": "input-1",
                "dest": "input",
                "path": "inputs/seed.txt",
                "digest": item_digest(b"seed"),
                "bytes": 4,
                "data_b64": workloop_executor::auth::base64_nopad_encode(b"seed"),
            }
        ]
    })
}

/// Issue a command in-process (no session) and parse the receipt.
fn issue(
    service: &ExecutorService,
    op_id: &str,
    kind: OperationKind,
    payload: serde_json::Value,
) -> Result<ExecutorReceiptV1, EnvelopeError> {
    let command =
        ExecutorCommandV1::new(identity(service), op_id, kind, payload).expect("command seals");
    issue_command(service, &command)
}

fn issue_command(
    service: &ExecutorService,
    command: &ExecutorCommandV1,
) -> Result<ExecutorReceiptV1, EnvelopeError> {
    let bytes = service.handle(command)?;
    ExecutorReceiptV1::parse(&bytes)
}

fn issue_raw(
    service: &ExecutorService,
    command: &ExecutorCommandV1,
) -> Result<Vec<u8>, EnvelopeError> {
    service.handle(command)
}

fn prepare(service: &ExecutorService, op: &str, deadline: i64) -> ExecutorReceiptV1 {
    let config = launch_config(deadline);
    issue(
        service,
        op,
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("prepare receipt parses")
}

fn start(service: &ExecutorService, op: &str, deadline: i64) -> ExecutorReceiptV1 {
    let config = launch_config(deadline);
    issue(
        service,
        op,
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("start receipt parses")
}

// ---------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------

#[test]
fn full_lifecycle_over_one_session() {
    let fx = fixture("lifecycle");
    let request_dir = fx.state.request_path(REQUEST_ID);
    // One deadline shared by prepare and start: the launch_config_digest
    // must name byte-identical content on both commands.
    let deadline = now_seconds() + 3600;

    let receipt = prepare(&fx.service, "op-prepare", deadline);
    assert_eq!(receipt.result["status"], "prepared");
    let prepared_id = receipt.result["prepared_id"].as_str().unwrap().to_string();

    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");
    let evidence = receipt.result["process"]["evidence_ref"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prepared_id.starts_with("prep-"));

    // A second start names the recorded state, never a second tree.
    let receipt = start(&fx.service, "op-start-again", deadline);
    assert_eq!(receipt.result["status"], "started");
    assert_eq!(receipt.result["process"]["evidence_ref"], evidence);
    assert_eq!(fx.port.spawn_calls(), 1);

    let receipt = issue(
        &fx.service,
        "op-observe-1",
        OperationKind::Observe,
        json!({}),
    )
    .expect("observe");
    assert_eq!(receipt.result["status"], "running");

    // The Worker writes its report and exits.
    std::fs::create_dir_all(request_dir.join("out")).expect("out dir");
    std::fs::write(
        request_dir.join("out/report.json"),
        br#"{"format_version":"taisan.worker.report/2","status":"completed"}"#,
    )
    .expect("report");
    std::fs::write(request_dir.join("out/runtime.log"), b"log line\n").expect("log");
    fx.port.exit_all(0);

    let receipt = issue(
        &fx.service,
        "op-observe-2",
        OperationKind::Observe,
        json!({}),
    )
    .expect("observe");
    assert_eq!(receipt.result["status"], "stopped");
    assert!(receipt.result["proof"]["proof_digest"].is_string());
    let proof_receipt_digest = receipt.receipt_digest.clone();

    // collect names the stop-proof receipt that authorizes it.
    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": proof_receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "collected");
    let collection_id = receipt.result["collection_id"]
        .as_str()
        .unwrap()
        .to_string();
    let collection_digest = receipt.result["collection_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let item_ids: Vec<&str> = receipt.result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["item_id"].as_str().unwrap())
        .collect();
    assert!(item_ids.contains(&"report.json"));
    assert!(item_ids.contains(&"runtime.log"));

    let receipt = issue(
        &fx.service,
        "op-ack",
        OperationKind::Acknowledge,
        json!({"collection_id": collection_id, "collection_digest": collection_digest}),
    )
    .expect("acknowledge");
    assert_eq!(receipt.result["status"], "acknowledged");
    // The tombstone released the bodies; metadata stays.
    assert!(!request_dir.join("out").exists());
    assert!(request_dir.join("binding.json").exists());
    assert!(request_dir.join("ack.json").exists());
}

// ---------------------------------------------------------------------
// Replay and fencing
// ---------------------------------------------------------------------

#[test]
fn identical_replay_returns_the_stored_receipt_bytes() {
    let fx = fixture("replay");
    let deadline = now_seconds() + 3600;
    let config = launch_config(deadline);
    let command = ExecutorCommandV1::new(
        identity(&fx.service),
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");

    let first = issue_raw(&fx.service, &command).expect("first receipt");
    let replayed = issue_raw(&fx.service, &command).expect("replay receipt");
    assert_eq!(first, replayed, "the stored receipt replays byte-for-byte");
}

#[test]
fn same_operation_id_with_different_bytes_conflicts() {
    let fx = fixture("conflict");
    let deadline = now_seconds() + 3600;
    let mut config = launch_config(deadline);
    let first = ExecutorCommandV1::new(
        identity(&fx.service),
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &first).expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    config["stop_grace_ms"] = json!(999);
    let different = ExecutorCommandV1::new(
        identity(&fx.service),
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &different).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "conflicting_replay");
}

#[test]
fn durable_stop_intent_refuses_a_late_start() {
    let fx = fixture("stop-intent");
    let deadline = now_seconds() + 3600;
    let receipt = prepare(&fx.service, "op-prepare", deadline);
    assert_eq!(receipt.result["status"], "prepared");

    // A stop on a never-started request leaves durable intent; the
    // process inspection itself is honestly unknown.
    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "unknown");

    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(fx.port.spawn_calls(), 0);
}

#[test]
fn concurrent_invocation_refuses_the_single_slot() {
    let fx = fixture("one-slot");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");

    // A second request binding to the live incarnation prepares fine…
    let config = launch_config(deadline);
    let other_identity = ExecutorIdentity {
        request_id: "e-9:inv:9".into(),
        invocation_id: "inv-42".into(),
        request_digest: sha256_hex("taisan.test.request", b"other"),
        ..identity(&fx.service)
    };
    let prepare2 = ExecutorCommandV1::new(
        other_identity.clone(),
        "op-prepare-2",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &prepare2).expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    // …but its start cannot occupy the taken slot.
    fx.port.push_plan(SpawnPlan::Resident);
    let start2 = ExecutorCommandV1::new(
        other_identity,
        "op-start-2",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &sha256_hex("taisan.test.request", b"other")[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &start2).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "limit_exceeded");
    assert_eq!(fx.port.spawn_calls(), 1);
}

#[test]
fn a_corrupt_binding_record_fails_the_slot_check_closed() {
    let fx = fixture("corrupt-slot");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);

    // One unreadable binding record aborts the slot-check scan; the
    // start must refuse rather than let a second Invocation slip the
    // single execution slot.
    let corrupt = fx.state.request_path("corrupt-request");
    std::fs::create_dir_all(&corrupt).expect("corrupt dir");
    std::fs::write(corrupt.join("binding.json"), b"{ not json").expect("corrupt");

    let config = launch_config(deadline);
    let receipt = issue(
        &fx.service,
        "op-start",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "unavailable");
    assert_eq!(fx.port.spawn_calls(), 0);
}

// ---------------------------------------------------------------------
// Reconnect and restart
// ---------------------------------------------------------------------

#[test]
fn reconnect_recovers_durable_state_without_reissue() {
    let dir = test_dir("reconnect");
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        Arc::new(ExecutorService::boot_with_port(config, Arc::new(port.clone())).expect("service"));

    // Session 1: authenticate, prepare, start.
    let (mut end, client) = std::os::unix::net::UnixStream::pair().expect("pair");
    let served = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.serve_stream(&mut end))
    };
    let mut adapter =
        RemoteExecutor::connect(client, &coordinator_binding(), now_ms()).expect("session opens");
    assert_eq!(adapter.view().bound_incarnation_id, service.incarnation());

    let deadline = now_seconds() + 3600;
    let config = launch_config(deadline);
    let receipt = adapter
        .issue(
            "op-prepare",
            OperationKind::Prepare,
            REQUEST_ID,
            INVOCATION_ID,
            &request_digest(),
            prepare_payload(&config),
        )
        .expect("prepare");
    assert_eq!(receipt.result["status"], "prepared");
    port.push_plan(SpawnPlan::Resident);
    let receipt = adapter
        .issue(
            "op-start",
            OperationKind::Start,
            REQUEST_ID,
            INVOCATION_ID,
            &request_digest(),
            json!({
                "prepared_id": format!("prep-{}", &request_digest()[..16]),
                "launch_config_digest": launch_config_digest_of(&config),
            }),
        )
        .expect("start");
    assert_eq!(receipt.result["status"], "started");
    drop(adapter);
    served
        .join()
        .expect("session ended cleanly")
        .expect("stream");

    // Session 2: the Invocation's evidence is still reachable — the
    // transport session's death meant nothing to it.
    let (mut end, client) = std::os::unix::net::UnixStream::pair().expect("pair");
    let served = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.serve_stream(&mut end))
    };
    let mut adapter =
        RemoteExecutor::connect(client, &coordinator_binding(), now_ms()).expect("session opens");
    let receipt = adapter
        .issue(
            "op-observe",
            OperationKind::Observe,
            REQUEST_ID,
            INVOCATION_ID,
            &request_digest(),
            json!({}),
        )
        .expect("observe");
    assert_eq!(receipt.result["status"], "running");
    drop(adapter);
    served.join().expect("session ended").expect("stream");
}

#[test]
fn restart_rekeys_incarnation_and_keeps_typed_evidence() {
    let dir = test_dir("restart");
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        ExecutorService::boot_with_port(config.clone(), Arc::new(port.clone())).expect("service");
    let first_incarnation = service.incarnation().to_string();

    // A request prepared but never started under the first incarnation.
    let deadline = now_seconds() + 3600;
    let receipt = prepare(&service, "op-prepare", deadline);
    assert_eq!(receipt.result["status"], "prepared");

    // Reboot: a fresh service over the same state root mints a new
    // incarnation. A new incarnation may not create a start for the
    // bound request — typed refusal, never a liveness guess.
    let service2 =
        ExecutorService::boot_with_port(config, Arc::new(port.clone())).expect("service reboots");
    assert_ne!(service2.incarnation(), first_incarnation);

    let id = ExecutorIdentity {
        bound_incarnation_id: first_incarnation.clone(),
        ..identity(&service2)
    };
    let command = ExecutorCommandV1::new(id, "op-observe", OperationKind::Observe, json!({}))
        .expect("command");
    let receipt = issue_command(&service2, &command).expect("receipt");
    assert_eq!(receipt.result["status"], "unknown");
    assert_eq!(receipt.result["reason"], "incarnation_mismatch");

    let id = ExecutorIdentity {
        bound_incarnation_id: first_incarnation,
        ..identity(&service2)
    };
    let command = ExecutorCommandV1::new(
        id,
        "op-start-again",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&launch_config(deadline)),
        }),
    )
    .expect("command");
    let receipt = issue_command(&service2, &command).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "incarnation_mismatch");
    assert_eq!(port.spawn_calls(), 0);
}

#[test]
fn durable_stop_proof_survives_incarnation_change() {
    let dir = test_dir("restart-proof");
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        ExecutorService::boot_with_port(config.clone(), Arc::new(port.clone())).expect("service");
    let first_incarnation = service.incarnation().to_string();

    let deadline = now_seconds() + 3600;
    let _ = prepare(&service, "op-prepare", deadline);
    port.push_plan(SpawnPlan::Resident);
    let _ = start(&service, "op-start", deadline);
    let receipt = issue(&service, "op-stop", OperationKind::RequestStop, json!({})).expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    // Reboot and observe: durable proof answers, no inspection needed.
    let service2 =
        ExecutorService::boot_with_port(config, Arc::new(port)).expect("service reboots");
    let id = ExecutorIdentity {
        bound_incarnation_id: first_incarnation,
        ..identity(&service2)
    };
    let command = ExecutorCommandV1::new(id, "op-observe", OperationKind::Observe, json!({}))
        .expect("command");
    let receipt = issue_command(&service2, &command).expect("receipt");
    assert_eq!(receipt.result["status"], "stopped");
    assert_eq!(
        receipt.result["proof"]["incarnation_id"].as_str().unwrap(),
        receipt.bound_incarnation_id
    );
}

// ---------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------

#[test]
fn undeclared_required_capability_is_refused() {
    let fx = fixture("capability");
    let mut config = launch_config(now_seconds() + 3600);
    config["required_capabilities"] = json!(["cpu_hard_limit"]);
    let receipt = issue(
        &fx.service,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "capability_missing");
}

#[test]
fn read_only_grant_without_containment_is_refused() {
    let fx = fixture("read-only");
    let mut config = launch_config(now_seconds() + 3600);
    config["workspace_grant"] = json!("read_only");
    let receipt = issue(
        &fx.service,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "capability_missing");
}

#[test]
fn foreign_endpoint_identity_is_refused() {
    let fx = fixture("identity");
    let deadline = now_seconds() + 3600;
    let config = launch_config(deadline);
    let id = ExecutorIdentity {
        endpoint_identity_digest: "0".repeat(64),
        ..identity(&fx.service)
    };
    let command = ExecutorCommandV1::new(
        id,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &command).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "endpoint_identity_mismatch");
}

#[test]
fn tampered_item_body_is_refused_before_staging() {
    let fx = fixture("tamper");
    let deadline = now_seconds() + 3600;
    let config = launch_config(deadline);
    let mut payload = prepare_payload(&config);
    payload["items"][1]["data_b64"] =
        json!(workloop_executor::auth::base64_nopad_encode(b"tampered"));
    let receipt =
        issue(&fx.service, "op-prepare", OperationKind::Prepare, payload).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "input_invalid");
}

#[test]
fn start_with_drifting_worker_binding_is_refused() {
    let fx = fixture("drift");
    // Prepare and start must name the same launch configuration bytes:
    // a second `now_seconds()` call can cross a second boundary and
    // turn this binding-drift test into a request-identity mismatch.
    let deadline = now_seconds() + 3600;
    let mut drifted = launch_config(deadline);
    drifted["worker_binding"]["runtime_version"] = json!("fixture-9.9.9");
    let receipt = issue(
        &fx.service,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&drifted),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = issue(
        &fx.service,
        "op-start",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&drifted),
        }),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "known_not_started");
    assert_eq!(receipt.result["reason"], "configuration_mismatch");
    assert_eq!(fx.port.spawn_calls(), 0);
}

#[test]
fn a_dead_leader_still_enforces_the_deadline_on_surviving_descendants() {
    let fx = fixture("deadline-descendants");
    // Far enough out to start, close enough to expire in-test.
    let deadline = now_seconds() + 2;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");
    let leader = fx.port.spawned()[0].pid;

    // The leader exits but an in-group descendant survives — the case
    // the deadline sweep and the Exited observation used to skip.
    fx.port.adopt_child_in_group(leader, "descendant-writer");
    fx.port.exit(leader, 0);
    while now_ms() <= deadline * 1000 {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    let receipt =
        issue(&fx.service, "op-observe", OperationKind::Observe, json!({})).expect("observe");
    // The endpoint enforces the bound itself: the surviving member is
    // stopped and the durable record frees the execution slot.
    assert_eq!(receipt.result["status"], "stopped");
    let record = fx.state.request(REQUEST_ID).expect("read").expect("record");
    assert_eq!(record.state, "stopped");
    assert!(fx.port.signals().iter().any(|s| s.pgid == Some(leader)));
}

#[test]
fn expired_deadline_refuses_the_start() {
    let fx = fixture("deadline-past");
    let config = launch_config(now_seconds() - 60);
    let receipt = issue(
        &fx.service,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = issue(
        &fx.service,
        "op-start",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "known_not_started");
    assert_eq!(fx.port.spawn_calls(), 0);
}

// ---------------------------------------------------------------------
// Collect / acknowledge authorization
// ---------------------------------------------------------------------

#[test]
fn collect_requires_the_proven_stop_and_acknowledge_conflicts() {
    let fx = fixture("collect-gate");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);

    // Live Invocation: no stop proof exists.
    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": "0".repeat(64)}),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "invalid");
    assert_eq!(receipt.result["reason"], "collection_invalid");

    // Acknowledge without a collection is refused.
    let receipt = issue(
        &fx.service,
        "op-ack",
        OperationKind::Acknowledge,
        json!({"collection_id": "col-none", "collection_digest": "0".repeat(64)}),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "collection_incomplete");
}

#[test]
fn acknowledge_of_a_different_collection_conflicts() {
    let fx = fixture("ack-conflict");
    let request_dir = fx.state.request_path(REQUEST_ID);
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);
    std::fs::create_dir_all(request_dir.join("out")).expect("out");
    std::fs::write(request_dir.join("out/report.json"), b"{}").expect("report");
    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": receipt.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "collected");

    let receipt = issue(
        &fx.service,
        "op-ack",
        OperationKind::Acknowledge,
        json!({"collection_id": "col-foreign", "collection_digest": "f".repeat(64)}),
    )
    .expect("ack");
    assert_eq!(receipt.result["status"], "conflict");
    assert_eq!(receipt.result["reason"], "acknowledgement_conflict");
}

#[test]
fn an_overbound_output_member_is_refused_and_reclaimed() {
    let fx = fixture("output-bound");
    let request_dir = fx.state.request_path(REQUEST_ID);
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);
    std::fs::create_dir_all(request_dir.join("out")).expect("out");
    // Over the launch's max_output_bytes (1 MiB) — refused before the
    // body is buffered, and the retained bodies are reclaimed so the
    // storage bound holds on disk.
    std::fs::write(
        request_dir.join("out/report.json"),
        vec![b'x'; 2 * 1024 * 1024],
    )
    .expect("report");
    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": receipt.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "incomplete");
    assert_eq!(receipt.result["reason"], "limit_exceeded");
    assert!(!request_dir.join("out").exists());
}

#[test]
fn produced_output_shares_the_endpoint_storage_bound() {
    let dir = test_dir("state-bound");
    write_credentials(&dir);
    let mut config = endpoint_config(&dir);
    // Small enough that a modest produced body overruns it while
    // staying under the launch's max_output_bytes.
    config.max_state_bytes = 4096;
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        ExecutorService::boot_with_port(config, Arc::new(port.clone())).expect("service boots");
    let state = EndpointState::open(dir.join("state")).expect("state");
    let request_dir = state.request_path(REQUEST_ID);

    let deadline = now_seconds() + 3600;
    let _ = prepare(&service, "op-prepare", deadline);
    port.push_plan(SpawnPlan::Resident);
    let _ = start(&service, "op-start", deadline);
    std::fs::create_dir_all(request_dir.join("out")).expect("out");
    std::fs::write(request_dir.join("out/report.json"), vec![b'x'; 4096]).expect("report");
    let receipt = issue(&service, "op-stop", OperationKind::RequestStop, json!({})).expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    let receipt = issue(
        &service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": receipt.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "incomplete");
    assert_eq!(receipt.result["reason"], "limit_exceeded");
}

#[test]
fn acknowledge_reclaims_the_worker_scratch_tree() {
    let fx = fixture("scratch-reclaim");
    let request_dir = fx.state.request_path(REQUEST_ID);
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);
    // isolation::prepare built the worker HOME/TMPDIR/gitconfig here.
    assert!(request_dir.join("scratch").is_dir());
    std::fs::create_dir_all(request_dir.join("out")).expect("out");
    std::fs::write(request_dir.join("out/report.json"), b"{}").expect("report");
    fx.port.exit_all(0);

    let receipt =
        issue(&fx.service, "op-observe", OperationKind::Observe, json!({})).expect("observe");
    assert_eq!(receipt.result["status"], "stopped");
    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": receipt.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "collected");
    let collection_id = receipt.result["collection_id"]
        .as_str()
        .unwrap()
        .to_string();
    let collection_digest = receipt.result["collection_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let receipt = issue(
        &fx.service,
        "op-ack",
        OperationKind::Acknowledge,
        json!({"collection_id": collection_id, "collection_digest": collection_digest}),
    )
    .expect("ack");
    assert_eq!(receipt.result["status"], "acknowledged");
    assert!(!request_dir.join("scratch").exists());
}

// ---------------------------------------------------------------------
// Session authentication
// ---------------------------------------------------------------------

#[test]
fn wrong_credential_closes_the_session() {
    let dir = test_dir("auth");
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    let service =
        Arc::new(ExecutorService::boot_with_port(config, Arc::new(port)).expect("service"));
    let (mut end, client) = std::os::unix::net::UnixStream::pair().expect("pair");
    let served = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.serve_stream(&mut end))
    };
    let mut binding = coordinator_binding();
    binding.coordinator_credential = b"forged".to_vec();
    let outcome = RemoteExecutor::connect(client, &binding, now_ms());
    assert!(
        outcome.is_err(),
        "a forged credential never opens a session"
    );
    served.join().expect("session ended").expect_err("refused");
}

#[test]
fn credential_delivery_stages_and_binds() {
    let dir = test_dir("credential");
    write_credentials(&dir);
    let config = endpoint_config(&dir);
    let port = ScriptedProcessControl::new();
    port.set_default_probe(Ok(ProbeOutcome::Exited {
        stdout: DECLARATION.as_bytes().to_vec(),
        success: true,
    }));
    let service =
        Arc::new(ExecutorService::boot_with_port(config.clone(), Arc::new(port)).expect("service"));
    let (mut end, client) = std::os::unix::net::UnixStream::pair().expect("pair");
    let served = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.serve_stream(&mut end))
    };
    let mut adapter =
        RemoteExecutor::connect(client, &coordinator_binding(), now_ms()).expect("session");

    // A launch requiring the staged credential.
    let mut config_json = launch_config(now_seconds() + 3600);
    config_json["runtime_auth_mount"] = json!(true);
    let receipt = adapter
        .issue(
            "op-prepare",
            OperationKind::Prepare,
            REQUEST_ID,
            INVOCATION_ID,
            &request_digest(),
            prepare_payload(&config_json),
        )
        .expect("prepare");
    assert_eq!(receipt.result["status"], "prepared");

    let ack = adapter
        .deliver_credential(
            REQUEST_ID,
            INVOCATION_ID,
            &launch_config_digest_of(&config_json),
            b"runtime-secret",
        )
        .expect("ack");
    assert_eq!(ack.status, "staged");
    let staged = service_fixture_state(&dir)
        .request_path(REQUEST_ID)
        .join("runtime-auth");
    assert_eq!(
        std::fs::read(&staged).expect("credential"),
        b"runtime-secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&staged)
                .expect("credential metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the staged credential is owner-only from creation"
        );
    }

    drop(adapter);
    served.join().expect("session ended").expect("stream");
}

fn service_fixture_state(dir: &Path) -> EndpointState {
    EndpointState::open(dir.join("state")).expect("state")
}

// ---------------------------------------------------------------------
// Deadline enforcement (executor-side)
// ---------------------------------------------------------------------

#[test]
fn the_executor_enforces_the_authorized_runtime_itself() {
    let fx = fixture("deadline-enforce");
    // Far enough out to start, close enough to expire in-test. The
    // deadline is checked after start, not raced against the second
    // boundary the way a `now_seconds() + 1` window was.
    let deadline = now_seconds() + 2;
    let config = launch_config(deadline);
    let receipt = issue(
        &fx.service,
        "op-prepare",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = issue(
        &fx.service,
        "op-start",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &request_digest()[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("receipt");
    assert_eq!(receipt.result["status"], "started");

    while now_ms() <= deadline * 1000 {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let receipt =
        issue(&fx.service, "op-observe", OperationKind::Observe, json!({})).expect("observe");
    // The executor stopped it under the bound deadline — the worker
    // got the full signaled-escalation proof, no coordinator needed.
    assert_eq!(receipt.result["status"], "stopped");
    assert_eq!(receipt.result["proof"]["manner"], "signaled");
}

// ---------------------------------------------------------------------
// Start fencing after stop intent and unproven outcomes
// ---------------------------------------------------------------------

#[test]
fn a_delayed_start_after_stop_intent_is_refused_not_replayed() {
    let fx = fixture("stop-then-start");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");

    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    // A delayed start on a fresh operation id must be refused by the
    // durable stop intent — the recorded process evidence may not
    // answer `started` for an Invocation already proven stopped.
    let receipt = start(&fx.service, "op-start-late", deadline);
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(
        receipt.result["reason"], "request_identity_mismatch",
        "the durable stop intent refuses the delayed start"
    );
    assert_eq!(fx.port.spawn_calls(), 1);
}

#[test]
fn a_second_start_after_an_unknown_outcome_stays_unknown() {
    let fx = fixture("unknown-then-start");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    // Identity capture failed after a possible spawn — the record
    // advances to `live_unknown` and the workspace now exists.
    fx.port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "unknown");

    // A second start with a new operation id must replay the recorded
    // `unknown` — never re-dispatch (a second writer) and never
    // answer `known_not_started` (durable state cannot prove no
    // process exists).
    let receipt = start(&fx.service, "op-start-2", deadline);
    assert_eq!(receipt.result["status"], "unknown");
    assert_eq!(receipt.result["reason"], "process_identity_unavailable");
    assert_eq!(fx.port.spawn_calls(), 1);
}

#[test]
fn a_proven_stop_advances_the_record_and_frees_the_slot() {
    let fx = fixture("slot-freed");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");

    // A second request prepares while the first is live.
    let config = launch_config(deadline);
    let other_identity = ExecutorIdentity {
        request_id: "e-9:inv:9".into(),
        invocation_id: "inv-42".into(),
        request_digest: sha256_hex("taisan.test.request", b"other"),
        ..identity(&fx.service)
    };
    let prepare2 = ExecutorCommandV1::new(
        other_identity.clone(),
        "op-prepare-2",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &prepare2).expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    // The first Invocation exits; observe proves the stop. The proof
    // must advance the durable record — a stale `started` state would
    // pin the single execution slot until collect/ack.
    fx.port.exit_all(0);
    let receipt =
        issue(&fx.service, "op-observe", OperationKind::Observe, json!({})).expect("observe");
    assert_eq!(receipt.result["status"], "stopped");
    assert_eq!(
        fx.state
            .request(REQUEST_ID)
            .expect("request")
            .expect("record")
            .state,
        "stopped"
    );

    // The freed slot admits the second request's start.
    fx.port.push_plan(SpawnPlan::Resident);
    let start2 = ExecutorCommandV1::new(
        other_identity,
        "op-start-2",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &sha256_hex("taisan.test.request", b"other")[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &start2).expect("receipt");
    assert_eq!(receipt.result["status"], "started");
    assert_eq!(fx.port.spawn_calls(), 2);
}

#[cfg(unix)]
#[test]
fn a_start_intent_that_cannot_commit_spawns_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture("unwritable-binding");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);

    // The durable start intent must commit before any spawn: with the
    // request's binding directory unwritable the start answers
    // `unknown`, spawns nothing, and leaves the record `prepared` so a
    // repaired store retries cleanly.
    let request_dir = fx.state.request_path(REQUEST_ID);
    let saved = std::fs::metadata(&request_dir)
        .expect("metadata")
        .permissions();
    let mut readonly = saved.clone();
    readonly.set_mode(0o555);
    std::fs::set_permissions(&request_dir, readonly).expect("readonly");
    // A privileged runner ignores the mode bits — probe for that and
    // skip rather than assert a guarantee this environment lacks.
    if std::fs::write(request_dir.join(".perm-probe"), b"x").is_ok() {
        let _ = std::fs::remove_file(request_dir.join(".perm-probe"));
        std::fs::set_permissions(&request_dir, saved).expect("restore");
        return;
    }

    let receipt = start(&fx.service, "op-start", deadline);

    std::fs::set_permissions(&request_dir, saved).expect("restore");
    assert_eq!(receipt.result["status"], "unknown");
    assert_eq!(receipt.result["reason"], "unavailable");
    assert_eq!(fx.port.spawn_calls(), 0);
    assert_eq!(
        fx.state
            .request(REQUEST_ID)
            .expect("request")
            .expect("record")
            .state,
        "prepared"
    );

    let receipt = start(&fx.service, "op-start-retry", deadline);
    assert_eq!(receipt.result["status"], "started");
    assert_eq!(fx.port.spawn_calls(), 1);
}

#[test]
fn an_unproven_start_keeps_the_execution_slot() {
    let fx = fixture("unproven-slot");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    // Identity capture failed after a possible spawn — the outcome is
    // unproven, so the writer reservation must hold for everyone.
    fx.port.push_plan(SpawnPlan::Unknown {
        reason: "identity capture failed".into(),
    });
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "unknown");

    // A different request's start is refused by the held slot — a
    // second writer is never admitted while the first is unproven.
    let config = launch_config(deadline);
    let other_identity = ExecutorIdentity {
        request_id: "e-9:inv:9".into(),
        invocation_id: "inv-42".into(),
        request_digest: sha256_hex("taisan.test.request", b"other"),
        ..identity(&fx.service)
    };
    let prepare2 = ExecutorCommandV1::new(
        other_identity.clone(),
        "op-prepare-2",
        OperationKind::Prepare,
        prepare_payload(&config),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &prepare2).expect("receipt");
    assert_eq!(receipt.result["status"], "prepared");

    fx.port.push_plan(SpawnPlan::Resident);
    let start2 = ExecutorCommandV1::new(
        other_identity,
        "op-start-2",
        OperationKind::Start,
        json!({
            "prepared_id": format!("prep-{}", &sha256_hex("taisan.test.request", b"other")[..16]),
            "launch_config_digest": launch_config_digest_of(&config),
        }),
    )
    .expect("command");
    let receipt = issue_command(&fx.service, &start2).expect("receipt");
    assert_eq!(receipt.result["status"], "refused");
    assert_eq!(receipt.result["reason"], "limit_exceeded");
    assert_eq!(fx.port.spawn_calls(), 1);
}

#[test]
fn an_unverifiable_descendant_is_never_a_silent_stopped() {
    let fx = fixture("unknown-descendant");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");
    let leader = fx.port.spawned()[0].pid;

    // A setsid-escaped descendant is merged into the recorded set by
    // the stop-time snapshot; its liveness read is unverifiable, so
    // nothing may report `stopped` while it is unaccounted for.
    let child = fx.port.adopt_child(leader);
    fx.port
        .set_inspect_unknown(child, Some("liveness read unavailable".into()));

    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "unknown");
    assert!(
        fx.state.stop_proof(REQUEST_ID).expect("proof").is_none(),
        "no stop proof exists while a recorded identity is unverifiable"
    );
    assert_eq!(
        fx.state
            .request(REQUEST_ID)
            .expect("request")
            .expect("record")
            .state,
        "started"
    );
}

#[test]
fn an_unverifiable_group_member_is_never_a_silent_stopped() {
    let fx = fixture("unknown-group-member");
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let receipt = start(&fx.service, "op-start", deadline);
    assert_eq!(receipt.result["status"], "started");
    let leader = fx.port.spawned()[0].pid;

    // A group member adopted after the launch snapshot is only
    // visible through group_members; an unverifiable read on it must
    // keep the leader's own exit from releasing a stop proof.
    let member = fx.port.adopt_child_in_group(leader, "group-writer");
    fx.port
        .set_inspect_unknown(member, Some("liveness read unavailable".into()));
    fx.port.exit(leader, 0);

    let receipt =
        issue(&fx.service, "op-observe", OperationKind::Observe, json!({})).expect("observe");
    assert_eq!(receipt.result["status"], "unknown");
    assert!(
        fx.state.stop_proof(REQUEST_ID).expect("proof").is_none(),
        "no stop proof exists while a group member is unverifiable"
    );
    assert_eq!(
        fx.state
            .request(REQUEST_ID)
            .expect("request")
            .expect("record")
            .state,
        "started"
    );
}

// ---------------------------------------------------------------------
// Worker-controlled out/ entries
// ---------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlinked_out_member_is_refused_not_followed() {
    use std::os::unix::fs::symlink;
    let fx = fixture("out-symlink");
    let request_dir = fx.state.request_path(REQUEST_ID);
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);

    // The Worker controls out/: a symlinked report.json at a host
    // path would exfiltrate it into the collection.
    let out = request_dir.join("out");
    std::fs::create_dir_all(&out).expect("out");
    let sentinel = fx._dir.join("host-secret");
    std::fs::write(&sentinel, b"host-secret-bytes").expect("sentinel");
    symlink(&sentinel, out.join("report.json")).expect("symlink");

    let stop = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(stop.result["status"], "stopped");

    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": stop.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "invalid");
    assert_eq!(receipt.result["reason"], "collection_invalid");
    // The needle must be the sentinel's encoding under the same nopad
    // base64 `data_b64` carries — a padded literal can never occur.
    let sentinel_b64 = workloop_executor::auth::base64_nopad_encode(b"host-secret-bytes");
    assert!(
        !receipt
            .result
            .get("items")
            .and_then(|i| i.as_array())
            .is_some_and(|items| items.iter().any(|i| i["data_b64"]
                .as_str()
                .is_some_and(|b| b.contains(&sentinel_b64)))),
        "the symlink target's bytes never enter the collection"
    );

    // The same symlink at a name outside the collected whitelist is
    // never opened — a real `collected` receipt still must not carry
    // the target's bytes.
    std::fs::remove_file(out.join("report.json")).expect("remove symlink");
    std::fs::write(
        out.join("report.json"),
        br#"{"format_version":"taisan.worker.report/2","status":"completed"}"#,
    )
    .expect("report");
    symlink(&sentinel, out.join("unlisted")).expect("symlink");
    let receipt = issue(
        &fx.service,
        "op-collect-2",
        OperationKind::Collect,
        json!({"stop_proof_receipt": stop.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "collected");
    let items = receipt.result["items"].as_array().expect("items");
    assert!(!items.is_empty());
    assert!(
        !items.iter().any(|i| i["data_b64"]
            .as_str()
            .is_some_and(|b| b.contains(&sentinel_b64))),
        "the symlink target's bytes never enter the collection"
    );
}

#[cfg(unix)]
#[test]
fn a_fifo_out_member_is_refused_before_it_can_block() {
    use std::os::unix::ffi::OsStrExt;
    let fx = fixture("out-fifo");
    let request_dir = fx.state.request_path(REQUEST_ID);
    let deadline = now_seconds() + 3600;
    let _ = prepare(&fx.service, "op-prepare", deadline);
    fx.port.push_plan(SpawnPlan::Resident);
    let _ = start(&fx.service, "op-start", deadline);

    // A valid report plus a FIFO under an optional whitelist name —
    // an unguarded open would block the service lock forever.
    let out = request_dir.join("out");
    std::fs::create_dir_all(&out).expect("out");
    std::fs::write(out.join("report.json"), b"{}").expect("report");
    let fifo = out.join("runtime.log");
    let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("fifo path");
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

    let receipt = issue(
        &fx.service,
        "op-stop",
        OperationKind::RequestStop,
        json!({}),
    )
    .expect("stop");
    assert_eq!(receipt.result["status"], "stopped");

    let receipt = issue(
        &fx.service,
        "op-collect",
        OperationKind::Collect,
        json!({"stop_proof_receipt": receipt.receipt_digest}),
    )
    .expect("collect");
    assert_eq!(receipt.result["status"], "invalid");
    assert_eq!(receipt.result["reason"], "collection_invalid");
}
