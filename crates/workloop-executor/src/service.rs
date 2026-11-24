//! The fixed-operation executor service: durable receipt store,
//! replay fences, the one-start slot, incarnation fencing, and the
//! session loop. It answers exactly the six contract operations plus
//! the handshake and the credential delivery — there is no
//! shell-string RPC surface anywhere in the dispatch path.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use workloop_core::executor::{
    ClockMeasurement, EnvelopeError, ExecutorCommandV1, ExecutorHelloReplyV1, ExecutorHelloV1,
    ExecutorReceiptV1, OperationKind, endpoint_identity_digest, launch_config_digest,
};
use workloop_core::ports::ProcessControlPort;

use crate::auth::{CredentialAck, CredentialDelivery};
use crate::channel::{self, FrameClass, SessionError};
use crate::config::EndpointConfig;
use crate::runner::{
    CollectOutcome, LaunchOutcome, ObserveOutcome, Runner, StageItem, StopOutcome,
};
use crate::state::{AckTombstone, EndpointState, OpRecord, RequestRecord, StopIntent, StopProof};

/// Capabilities the endpoint always declares — each one is enforced
/// by code in this crate, never merely claimed.
const BASE_CAPABILITIES: &[&str] = &[
    "bounded_transfer",
    "process_control",
    "runtime_deadline",
    "bounded_output",
    "bounded_storage",
    "runtime_auth_delivery",
];

/// Capabilities declared only when the configured profile carries a
/// sandbox primitive — the honest report the AC requires: an
/// uncontained profile never claims namespace or read-only
/// enforcement.
const CONTAINED_CAPABILITIES: &[&str] = &["containment_namespaces", "workspace_read_only"];

/// The executor service: one configured endpoint identity, one
/// incarnation, one execution slot, a durable receipt store.
pub struct ExecutorService {
    config: EndpointConfig,
    state: EndpointState,
    runner: Runner,
    endpoint_credential: Vec<u8>,
    coordinator_credential: Vec<u8>,
    endpoint_identity_digest: String,
    incarnation: String,
    /// Serializes durable-state access — the single execution slot is
    /// a fact about this lock, not about the session count.
    lock: Mutex<()>,
}

impl ExecutorService {
    /// Boot the service over a state root: mint a fresh incarnation
    /// (restart evidence), then answer sessions. The previous boot's
    /// bound requests keep their recorded incarnation — reconciliation
    /// reads them lazily through the typed refusals.
    pub fn boot(config: EndpointConfig) -> Result<Self, String> {
        config.validate().map_err(|e| e.to_string())?;
        let profile = config
            .profile()
            .ok_or_else(|| "no approved isolation profile configured".to_string())?;
        let state = EndpointState::open(config.state_root.clone()).map_err(|e| e.to_string())?;
        let endpoint_credential = config.endpoint_credential().map_err(|e| e.to_string())?;
        let coordinator_credential = config.coordinator_credential().map_err(|e| e.to_string())?;
        let service_record = state
            .boot_incarnation(Self::now_seconds())
            .map_err(|e| e.to_string())?;
        let runner = Runner::new(
            Arc::new(workloop_exec::supervisor::PosixProcessControl),
            state.clone(),
            profile,
            config.worker_argv.clone(),
            config.max_state_bytes,
            config.max_one_way_delay_ms,
            config.max_clock_offset_ms,
        );
        Ok(Self {
            endpoint_identity_digest: endpoint_identity_digest(&endpoint_credential),
            endpoint_credential,
            coordinator_credential,
            incarnation: service_record.incarnation_id,
            config,
            state,
            runner,
            lock: Mutex::new(()),
        })
    }

    /// Boot over an injected process-control port — the seam the
    /// contract fixtures use to prove lifecycle claims without a live
    /// process table.
    pub fn boot_with_port(
        config: EndpointConfig,
        port: std::sync::Arc<dyn ProcessControlPort + Send + Sync>,
    ) -> Result<Self, String> {
        let profile = config.profile().expect("profile resolves");
        let state = EndpointState::open(config.state_root.clone()).map_err(|e| e.to_string())?;
        let endpoint_credential = config.endpoint_credential().map_err(|e| e.to_string())?;
        let coordinator_credential = config.coordinator_credential().map_err(|e| e.to_string())?;
        let service_record = state
            .boot_incarnation(Self::now_seconds())
            .map_err(|e| e.to_string())?;
        let runner = Runner::new(
            port,
            state.clone(),
            profile,
            config.worker_argv.clone(),
            config.max_state_bytes,
            config.max_one_way_delay_ms,
            config.max_clock_offset_ms,
        );
        Ok(Self {
            endpoint_identity_digest: endpoint_identity_digest(&endpoint_credential),
            endpoint_credential,
            coordinator_credential,
            incarnation: service_record.incarnation_id,
            config,
            state,
            runner,
            lock: Mutex::new(()),
        })
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

    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }

    pub fn endpoint_identity_digest(&self) -> &str {
        &self.endpoint_identity_digest
    }

    /// The capabilities this endpoint honestly declares: the base
    /// enforced set plus the contained set only when the configured
    /// profile carries a sandbox primitive.
    pub fn declared_capabilities(&self) -> Vec<String> {
        let mut caps: Vec<String> = BASE_CAPABILITIES.iter().map(|s| s.to_string()).collect();
        if self
            .config
            .profile()
            .map(|p| p.sandbox_primitive.is_some())
            .unwrap_or(false)
        {
            caps.extend(CONTAINED_CAPABILITIES.iter().map(|s| s.to_string()));
        }
        caps
    }

    // -----------------------------------------------------------------
    // Handshake
    // -----------------------------------------------------------------

    /// Answer `taisan.executor.hello/1` with the endpoint's
    /// incarnation, protocols, honest capability set, and the clock
    /// measurement the coordinator's deadline accounting needs.
    pub fn hello_reply(
        &self,
        hello: &ExecutorHelloV1,
    ) -> Result<ExecutorHelloReplyV1, EnvelopeError> {
        if hello.installation_id != self.config.installation_id
            || hello.endpoint_id != self.config.endpoint_id
        {
            return Err(EnvelopeError::Invalid {
                reason: "endpoint_identity_mismatch",
                detail: "hello names a different installation or endpoint".into(),
            });
        }
        let capabilities = self.declared_capabilities();
        Ok(ExecutorHelloReplyV1 {
            format_version: workloop_core::executor::EXECUTOR_HELLO_REPLY_V1_FORMAT.into(),
            installation_id: self.config.installation_id.clone(),
            endpoint_id: self.config.endpoint_id.clone(),
            coordinator_nonce: hello.coordinator_nonce.clone(),
            endpoint_identity_digest: self.endpoint_identity_digest.clone(),
            responder_incarnation_id: self.incarnation.clone(),
            supported_protocols: self.config.supported_protocols.clone(),
            capabilities_digest: ExecutorHelloReplyV1::capabilities_digest_of(&capabilities)?,
            capabilities,
            clock: ClockMeasurement {
                offset_ms: self.runner.measured_clock_offset_ms(),
                uncertainty_ms: self.config.max_clock_offset_ms,
                round_trip_ms: 0,
                measured_at_unix_ms: Self::now_ms(),
            },
        })
    }

    // -----------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------

    /// Handle one parsed command: replay fence, binding checks, the
    /// durable operation ledger, then the operation's effect. Returns
    /// the canonical receipt bytes — stored-receipt replays come back
    /// byte-for-byte.
    pub fn handle(&self, command: &ExecutorCommandV1) -> Result<Vec<u8>, EnvelopeError> {
        let _serial = self.lock.lock().expect("service lock");
        self.runner.enforce_deadlines(&self.incarnation);

        let refused = |code: &'static str, detail: &str| {
            self.receipt(
                command,
                json!({
                    "status": "refused",
                    "reason": code,
                    "diagnostic": detail,
                }),
            )
        };

        // --- endpoint identity ----------------------------------------
        if command.endpoint_identity_digest != self.endpoint_identity_digest {
            return refused(
                "endpoint_identity_mismatch",
                "command names a different endpoint identity",
            );
        }
        if command.installation_id != self.config.installation_id
            || command.endpoint_id != self.config.endpoint_id
        {
            return refused(
                "request_identity_mismatch",
                "command names a different installation or endpoint",
            );
        }

        // --- replay fence ----------------------------------------------
        match self
            .state
            .operation(&command.request_id, &command.operation_id)
        {
            Ok(Some(op)) => {
                if op.command_digest == command.command_digest {
                    if let Some(body) = &op.receipt_body_b64 {
                        let bytes = crate::auth::base64_nopad_decode(body).map_err(|_| {
                            EnvelopeError::Invalid {
                                reason: "receipt.corrupt",
                                detail: "stored receipt body does not decode".into(),
                            }
                        })?;
                        return Ok(bytes);
                    }
                    // Recorded but unanswered: resume from durable
                    // state — never re-run an effect blindly.
                    let result = self.resume(command)?;
                    return self.finish(command, result);
                }
                return refused(
                    "conflicting_replay",
                    "operation id was reused with different command bytes",
                );
            }
            Ok(None) => {}
            Err(e) => {
                return refused("unavailable", &format!("operation ledger unreadable: {e}"));
            }
        }

        // --- commit the command before the effect -----------------------
        let op = OpRecord {
            operation_id: command.operation_id.clone(),
            operation_kind: command.operation_kind.as_str().into(),
            command_body_b64: crate::auth::base64_nopad_encode(
                &command
                    .to_canonical_bytes()
                    .map_err(|e| EnvelopeError::Malformed {
                        detail: format!("command does not serialize: {e}"),
                    })?,
            ),
            command_digest: command.command_digest.clone(),
            receipt_body_b64: None,
            receipt_digest: None,
        };
        if let Err(e) = self.state.put_operation(&command.request_id, &op) {
            return refused("unavailable", &format!("operation ledger: {e}"));
        }

        let result = self.dispatch(command);
        self.finish(command, result)
    }

    /// Run the effect and record the receipt against the operation.
    fn finish(
        &self,
        command: &ExecutorCommandV1,
        result: serde_json::Value,
    ) -> Result<Vec<u8>, EnvelopeError> {
        let bytes = self.receipt(command, result)?;
        // The operation ledger records the answer; a lost write is a
        // durable gap the coordinator reconciles by replay.
        if let Ok(Some(mut op)) = self
            .state
            .operation(&command.request_id, &command.operation_id)
        {
            let receipt_digest = ExecutorReceiptV1::parse(&bytes)
                .map(|r| r.receipt_digest)
                .unwrap_or_default();
            op.receipt_body_b64 = Some(crate::auth::base64_nopad_encode(&bytes));
            op.receipt_digest = Some(receipt_digest);
            let _ = self.state.put_operation(&command.request_id, &op);
        }
        Ok(bytes)
    }

    /// Build the canonical receipt bytes for a result value. The
    /// result is sanitized to the contract vocabulary first — a
    /// diagnostic over the character bound is truncated, and a result
    /// that fails the closed-vocabulary check degrades to `unknown`/
    /// `unavailable` rather than emitting a receipt the coordinator
    /// must reject as malformed.
    fn receipt(
        &self,
        command: &ExecutorCommandV1,
        mut result: serde_json::Value,
    ) -> Result<Vec<u8>, EnvelopeError> {
        if let Some(diagnostic) = result
            .get("diagnostic")
            .and_then(|d| d.as_str())
            .map(str::to_string)
            && diagnostic.chars().count() > workloop_core::executor::MAX_DIAGNOSTIC_CHARS
        {
            let truncated: String = diagnostic
                .chars()
                .take(workloop_core::executor::MAX_DIAGNOSTIC_CHARS)
                .collect();
            result["diagnostic"] = json!(truncated);
        }
        let build = |result: serde_json::Value| -> Result<Vec<u8>, EnvelopeError> {
            ExecutorReceiptV1::new(
                command_identity(command),
                self.incarnation.clone(),
                command.operation_id.clone(),
                command.operation_kind,
                command.payload_digest.clone(),
                command.command_digest.clone(),
                result,
            )?
            .to_canonical_bytes()
        };
        let bytes = build(result)?;
        if ExecutorReceiptV1::parse(&bytes).is_err() {
            return build(json!({
                "status": "unknown",
                "reason": "unavailable",
                "diagnostic": "operation result failed the contract vocabulary check",
            }));
        }
        Ok(bytes)
    }

    /// Resume an effect whose command committed before its receipt:
    /// consult durable state, never re-run a non-idempotent effect.
    fn resume(&self, command: &ExecutorCommandV1) -> Result<serde_json::Value, EnvelopeError> {
        let record = self.state.request(&command.request_id).ok().flatten();
        Ok(match command.operation_kind {
            OperationKind::Prepare => match record {
                Some(r) if r.state != "prepared" => json!({
                    "status": "refused",
                    "reason": "request_identity_mismatch",
                    "diagnostic": "request exists in a post-prepare state",
                }),
                Some(r) => json!({"status": "prepared", "prepared_id": r.prepared_id}),
                None => self.dispatch(command),
            },
            OperationKind::Start => match record {
                Some(r) => match self.state.process(&r.request_id).ok().flatten() {
                    Some(p) => json!({
                        "status": "started",
                        "process": {"evidence_ref": p.evidence_ref, "identity_digest": p.identity_digest},
                    }),
                    None => json!({
                        "status": "unknown",
                        "reason": "process_identity_unavailable",
                        "diagnostic": "start may have spawned before its receipt was lost",
                    }),
                },
                None => json!({
                    "status": "refused",
                    "reason": "request_identity_mismatch",
                    "diagnostic": "no prepared request",
                }),
            },
            OperationKind::RequestStop => {
                match self.state.stop_proof(&command.request_id).ok().flatten() {
                    Some(proof) => {
                        let sequence = self.next_sequence(&command.request_id);
                        self.stopped_result_with_seq(&proof, sequence)
                    }
                    None => self.dispatch(command),
                }
            }
            OperationKind::Collect => self.dispatch(command),
            OperationKind::Acknowledge => {
                match self
                    .state
                    .acknowledgement(&command.request_id)
                    .ok()
                    .flatten()
                {
                    Some(_) => json!({"status": "acknowledged"}),
                    None => self.dispatch(command),
                }
            }
            OperationKind::Observe => self.dispatch(command),
        })
    }

    /// The operation dispatch: every branch maps the runner's outcome
    /// to the closed result vocabulary.
    fn dispatch(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        match command.operation_kind {
            OperationKind::Prepare => self.op_prepare(command),
            OperationKind::Start => self.op_start(command),
            OperationKind::Observe => self.op_observe(command),
            OperationKind::RequestStop => self.op_request_stop(command),
            OperationKind::Collect => self.op_collect(command),
            OperationKind::Acknowledge => self.op_acknowledge(command),
        }
    }

    /// Load the request record and check the command's request
    /// identity against it. `Err` is the typed refusal result value —
    /// `refused` where the operation's vocabulary admits it, `unknown`
    /// with `request_identity_mismatch` where it does not
    /// (observe/request_stop/collect have no `refused` status).
    fn bound_request(
        &self,
        command: &ExecutorCommandV1,
    ) -> Result<RequestRecord, serde_json::Value> {
        let admits_refusal = matches!(
            command.operation_kind,
            OperationKind::Prepare | OperationKind::Start | OperationKind::Acknowledge
        );
        let mismatch = |detail: &str| {
            if admits_refusal {
                json!({
                    "status": "refused",
                    "reason": "request_identity_mismatch",
                    "diagnostic": detail,
                })
            } else {
                json!({
                    "status": "unknown",
                    "reason": "request_identity_mismatch",
                    "diagnostic": detail,
                })
            }
        };
        let record = match self.state.request(&command.request_id) {
            Ok(Some(record)) => record,
            Ok(None) => return Err(mismatch("no prepared request for this identity")),
            Err(e) => {
                return Err(json!({
                    "status": "unknown",
                    "reason": "unavailable",
                    "diagnostic": format!("request record unreadable: {e}"),
                }));
            }
        };
        if record.invocation_id != command.invocation_id
            || record.request_digest != command.request_digest
            || record.installation_id != command.installation_id
            || record.bound_incarnation_id != command.bound_incarnation_id
        {
            return Err(mismatch(
                "command identity disagrees with the bound request",
            ));
        }
        Ok(record)
    }

    // -----------------------------------------------------------------
    // prepare
    // -----------------------------------------------------------------

    fn op_prepare(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        // A new binding may only be made to the live incarnation.
        if command.bound_incarnation_id != self.incarnation {
            return json!({
                "status": "refused",
                "reason": "incarnation_mismatch",
                "diagnostic": "command names an incarnation this service does not run",
            });
        }
        // A bound request is immutable: identical content replays the
        // prepared answer, different content is a conflict.
        if let Ok(Some(existing)) = self.state.request(&command.request_id) {
            let same = existing.request_digest == command.request_digest
                && existing.invocation_id == command.invocation_id
                && existing.launch_config_digest
                    == command
                        .payload
                        .get("launch_config_digest")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
            if same {
                return json!({"status": "prepared", "prepared_id": existing.prepared_id});
            }
            return json!({
                "status": "refused",
                "reason": "request_identity_mismatch",
                "diagnostic": "request is already bound to different content",
            });
        }

        // --- launch configuration ------------------------------------
        let payload = &command.payload;
        let launch_config = match payload.get("launch_config").cloned() {
            Some(v) if v.is_object() => v,
            _ => {
                return json!({
                    "status": "refused",
                    "reason": "input_invalid",
                    "diagnostic": "payload lacks a launch_config object",
                });
            }
        };
        let canonical = match workloop_core::executor::executor_canonical_json(&launch_config) {
            Ok(bytes) => bytes,
            Err(e) => {
                return json!({
                    "status": "refused",
                    "reason": "input_invalid",
                    "diagnostic": format!("launch_config is not canonical JSON: {e}"),
                });
            }
        };
        let computed_digest = launch_config_digest(&canonical);
        let declared_digest = payload
            .get("launch_config_digest")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if declared_digest != computed_digest {
            return json!({
                "status": "refused",
                "reason": "configuration_mismatch",
                "diagnostic": "launch_config_digest does not cover the launch_config",
            });
        }
        let field = |name: &str| launch_config.get(name);
        let str_field = |name: &str| field(name).and_then(|v| v.as_str()).unwrap_or_default();

        // The endpoint runs exactly its configured profile — a config
        // naming another profile is a configuration refusal.
        if str_field("profile_id") != self.config.isolation_profile_id {
            return json!({
                "status": "refused",
                "reason": "configuration_mismatch",
                "diagnostic": "launch profile differs from the endpoint's configured profile",
            });
        }
        let workspace_grant = str_field("workspace_grant").to_string();
        if !matches!(workspace_grant.as_str(), "writable" | "read_only") {
            return json!({
                "status": "refused",
                "reason": "input_invalid",
                "diagnostic": "workspace_grant is not writable|read_only",
            });
        }
        let required: Vec<String> = field("required_capabilities")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let declared = self.declared_capabilities();
        let missing: Vec<&String> = required.iter().filter(|c| !declared.contains(c)).collect();
        if !missing.is_empty() {
            return json!({
                "status": "refused",
                "reason": "capability_missing",
                "diagnostic": format!("required capabilities are not declared: {missing:?}"),
            });
        }
        if workspace_grant == "read_only" && !declared.iter().any(|c| c == "workspace_read_only") {
            return json!({
                "status": "refused",
                "reason": "capability_missing",
                "diagnostic": "read_only workspace grant requires enforceable containment",
            });
        }
        let deadline_unix_seconds = field("deadline_unix_seconds")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        let stop_grace_ms = field("stop_grace_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(5_000);
        let limits = field("limits").and_then(|v| {
            serde_json::from_value::<workloop_core::executor::ExecutorLimits>(v.clone()).ok()
        });
        let Some(limits) = limits else {
            return json!({
                "status": "refused",
                "reason": "input_invalid",
                "diagnostic": "launch_config.limits is absent or malformed",
            });
        };
        if limits.max_input_bytes == 0
            || limits.max_output_bytes == 0
            || limits.max_item_count == 0
            || limits.operation_timeout_ms == 0
        {
            return json!({
                "status": "refused",
                "reason": "limit_exceeded",
                "diagnostic": "launch limits must all be finite and nonzero",
            });
        }
        let work_id = str_field("work_id").to_string();
        if work_id.is_empty() || work_id.len() > 256 {
            return json!({
                "status": "refused",
                "reason": "input_invalid",
                "diagnostic": "work_id is absent or overlong",
            });
        }
        let runtime_auth_mount = launch_config
            .get("runtime_auth_mount")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // --- staged items ----------------------------------------------
        let items_value = match payload.get("items").and_then(|v| v.as_array()) {
            Some(a) => a.clone(),
            None => {
                return json!({
                    "status": "refused",
                    "reason": "input_invalid",
                    "diagnostic": "payload lacks an items array",
                });
            }
        };
        let mut items = Vec::new();
        for value in &items_value {
            let get = |k: &str| value.get(k).and_then(|v| v.as_str()).unwrap_or_default();
            let bytes = match crate::auth::base64_nopad_decode(get("data_b64")) {
                Ok(b) => b,
                Err(_) => {
                    return json!({
                        "status": "refused",
                        "reason": "input_invalid",
                        "diagnostic": "item body does not decode",
                    });
                }
            };
            let declared_bytes = value
                .get("bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            if declared_bytes != bytes.len() as u64 {
                return json!({
                    "status": "refused",
                    "reason": "input_invalid",
                    "diagnostic": "item byte count disagrees with its body",
                });
            }
            items.push(StageItem {
                item_id: get("item_id").to_string(),
                dest: get("dest").to_string(),
                path: get("path").to_string(),
                digest: get("digest").to_string(),
                bytes,
            });
        }

        let record = RequestRecord {
            installation_id: command.installation_id.clone(),
            endpoint_id: command.endpoint_id.clone(),
            endpoint_identity_digest: command.endpoint_identity_digest.clone(),
            bound_incarnation_id: command.bound_incarnation_id.clone(),
            request_id: command.request_id.clone(),
            invocation_id: command.invocation_id.clone(),
            request_digest: command.request_digest.clone(),
            prepared_id: format!("prep-{}", &command.request_digest[..16]),
            launch_config_body: launch_config,
            launch_config_digest: computed_digest,
            input_manifest_digest: String::new(),
            workspace_grant,
            work_id,
            deadline_unix_seconds,
            limits,
            stop_grace_ms,
            required_capabilities: required,
            runtime_auth_mount,
            state: "prepared".into(),
        };
        // A torn prepare leaves `prepared/` without a binding; repair
        // it by clearing and re-staging under this command.
        self.runner.clear_torn_prepare(&record.request_id);
        let manifest_digest = match self.runner.stage(&record, &items) {
            Ok(digest) => digest,
            Err(refusal) => {
                return json!({
                    "status": "refused",
                    "reason": refusal.code,
                    "diagnostic": refusal.detail,
                });
            }
        };
        let mut record = record;
        record.input_manifest_digest = manifest_digest;
        // Identity before answer: the binding commits before the
        // `prepared` receipt exists.
        if let Err(e) = self.state.put_request(&record) {
            return json!({
                "status": "unknown",
                "reason": "unavailable",
                "diagnostic": format!("request record could not be committed: {e}"),
            });
        }
        json!({"status": "prepared", "prepared_id": record.prepared_id})
    }

    // -----------------------------------------------------------------
    // start
    // -----------------------------------------------------------------

    fn op_start(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        let record = match self.bound_request(command) {
            Ok(r) => r,
            Err(result) => return result,
        };
        let payload = &command.payload;
        let payload_str = |k: &str| payload.get(k).and_then(|v| v.as_str()).unwrap_or_default();
        if payload_str("prepared_id") != record.prepared_id
            || payload_str("launch_config_digest") != record.launch_config_digest
        {
            return json!({
                "status": "refused",
                "reason": "request_identity_mismatch",
                "diagnostic": "start names a different prepared set or launch configuration",
            });
        }
        // Durable stop intent fences any delayed start — checked
        // before the recorded process answer so an Invocation whose
        // stop already committed never reports live again (contract
        // §4: once stop intent is durable, a delayed start is
        // refused).
        if self
            .state
            .stop_intent(&record.request_id)
            .ok()
            .flatten()
            .is_some()
        {
            return json!({
                "status": "refused",
                "reason": "request_identity_mismatch",
                "diagnostic": "durable stop intent fences this Invocation from starting",
            });
        }
        // One start slot: a second start returns the recorded state,
        // never another process tree.
        if let Ok(Some(process)) = self.state.process(&record.request_id) {
            return json!({
                "status": "started",
                "process": {"evidence_ref": process.evidence_ref, "identity_digest": process.identity_digest},
            });
        }
        // A request past `prepared` without a process record holds an
        // unproven start outcome (`live_unknown`) or a torn record —
        // durable state cannot prove no process exists, so a second
        // start never re-dispatches and never answers
        // `known_not_started` (contract §4, §5). It replays the
        // recorded `unknown` state.
        if record.state != "prepared" {
            return json!({
                "status": "unknown",
                "reason": "process_identity_unavailable",
                "diagnostic": "a prior start's outcome is unproven; the executor cannot prove no process exists",
            });
        }
        // Only the bound incarnation may launch it.
        if record.bound_incarnation_id != self.incarnation {
            return json!({
                "status": "refused",
                "reason": "incarnation_mismatch",
                "diagnostic": "a new incarnation cannot adopt a bound request",
            });
        }
        // One execution slot: another live Invocation refuses a start.
        // The check fails closed — an enumeration error (one corrupt
        // binding record aborts the scan) cannot skip the guard and
        // admit a second live Invocation.
        match self.state.requests() {
            Ok(requests)
                if requests
                    .iter()
                    .any(|r| r.request_id != record.request_id && r.is_live()) =>
            {
                return json!({
                    "status": "refused",
                    "reason": "limit_exceeded",
                    "diagnostic": "the endpoint's single execution slot is occupied",
                });
            }
            Ok(_) => {}
            Err(e) => {
                return json!({
                    "status": "refused",
                    "reason": "unavailable",
                    "diagnostic": format!("request enumeration failed: {e}"),
                });
            }
        }
        let mut record = record;
        match self.runner.launch(&record) {
            LaunchOutcome::Started {
                evidence_ref,
                identity_digest,
            } => {
                record.state = "started".into();
                let _ = self.state.put_request(&record);
                json!({
                    "status": "started",
                    "process": {"evidence_ref": evidence_ref, "identity_digest": identity_digest},
                })
            }
            LaunchOutcome::KnownNotStarted { code, detail } => {
                // Durable proof no process exists — the request stays
                // prepared-but-fenced; the coordinator decides the next
                // step with typed evidence.
                json!({
                    "status": "known_not_started",
                    "reason": code,
                    "diagnostic": detail,
                })
            }
            LaunchOutcome::Unknown { code, detail } => {
                record.state = "live_unknown".into();
                let _ = self.state.put_request(&record);
                json!({
                    "status": "unknown",
                    "reason": code,
                    "diagnostic": detail,
                })
            }
        }
    }

    // -----------------------------------------------------------------
    // observe
    // -----------------------------------------------------------------

    /// The durable observation sequence for one request: every
    /// `observe` and every proven `stopped` answer advances it, so a
    /// reconnected coordinator can order what it missed.
    fn next_sequence(&self, request_id: &str) -> u64 {
        let mut observation = self
            .state
            .observation(request_id)
            .unwrap_or(crate::state::ObservationRecord { sequence: 0 });
        observation.sequence += 1;
        let _ = self.state.put_observation(request_id, &observation);
        observation.sequence
    }

    fn op_observe(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        let record = match self.bound_request(command) {
            Ok(r) => r,
            Err(result) => return result,
        };
        let outcome = self.runner.observe(&record, &self.incarnation);
        let sequence = self.next_sequence(&record.request_id);
        let receipts = self
            .state
            .operations_for(&record.request_id)
            .map(|ops| {
                ops.iter()
                    .filter_map(|o| o.receipt_digest.clone())
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut result = match outcome {
            ObserveOutcome::Running {
                evidence_ref,
                identity_digest,
            } => json!({
                "status": "running",
                "process": {"evidence_ref": evidence_ref, "identity_digest": identity_digest},
                "sequence": sequence,
            }),
            ObserveOutcome::Exited {
                evidence_ref,
                identity_digest,
            } => json!({
                "status": "exited",
                "process": {"evidence_ref": evidence_ref, "identity_digest": identity_digest},
                "sequence": sequence,
            }),
            ObserveOutcome::Stopped { proof } => {
                let proof = self
                    .runner
                    .finalize_proof(&record, proof, &command.operation_id);
                // A durable proof frees the execution slot — advance
                // the record even when this proof replays a write an
                // earlier observation lost.
                if record.is_live() {
                    let mut record = record;
                    record.state = "stopped".into();
                    let _ = self.state.put_request(&record);
                }
                self.stopped_result_with_seq(&proof, sequence)
            }
            ObserveOutcome::Unknown { code, detail } => json!({
                "status": "unknown",
                "reason": code,
                "diagnostic": detail,
                "sequence": sequence,
            }),
        };
        result["operation_receipts"] = json!(receipts);
        result
    }

    fn stopped_result_with_seq(&self, proof: &StopProof, sequence: u64) -> serde_json::Value {
        json!({
            "status": "stopped",
            "sequence": sequence,
            "proof": {
                "proof_digest": proof.proof_digest,
                "snapshot_digest": proof.snapshot_digest,
                "incarnation_id": proof.incarnation_id,
                "manner": proof.manner,
                "proven_at_unix_seconds": proof.proven_at_unix_seconds,
            },
        })
    }

    // -----------------------------------------------------------------
    // request_stop
    // -----------------------------------------------------------------

    fn op_request_stop(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        let record = match self.bound_request(command) {
            Ok(r) => r,
            Err(result) => return result,
        };
        // Intent before signal — durable, so a replay or restart never
        // loses the stop's authority.
        let intent = StopIntent {
            operation_id: command.operation_id.clone(),
            recorded_at_unix_seconds: Self::now_seconds(),
            state: "requested".into(),
        };
        if let Err(e) = self.state.put_stop_intent(&record.request_id, &intent) {
            return json!({
                "status": "unknown",
                "reason": "unavailable",
                "diagnostic": format!("stop intent could not be persisted: {e}"),
            });
        }
        match self.runner.stop(&record, &self.incarnation) {
            StopOutcome::Stopped { proof } => {
                let proof = self
                    .runner
                    .finalize_proof(&record, proof, &command.operation_id);
                let mut done = intent;
                done.state = "proven".into();
                let _ = self.state.put_stop_intent(&record.request_id, &done);
                let mut record = record;
                record.state = "stopped".into();
                let _ = self.state.put_request(&record);
                let sequence = self.next_sequence(&record.request_id);
                self.stopped_result_with_seq(&proof, sequence)
            }
            StopOutcome::StopRequested => json!({"status": "stop_requested"}),
            StopOutcome::Unknown { code, detail } => json!({
                "status": "unknown",
                "reason": code,
                "diagnostic": detail,
            }),
        }
    }

    // -----------------------------------------------------------------
    // collect
    // -----------------------------------------------------------------

    fn op_collect(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        let record = match self.bound_request(command) {
            Ok(r) => r,
            Err(result) => return result,
        };
        // The collection is authorized by the stop-proof receipt the
        // payload names — compare it to the proving operation's
        // recorded receipt digest.
        let named = command
            .payload
            .get("stop_proof_receipt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let authorized = match self.state.stop_proof(&record.request_id) {
            Ok(Some(proof)) => self
                .state
                .operation(&record.request_id, &proof.proving_operation_id)
                .ok()
                .flatten()
                .and_then(|op| op.receipt_digest)
                .map(|digest| digest == named)
                .unwrap_or(false),
            _ => false,
        };
        if !authorized {
            return json!({
                "status": "invalid",
                "reason": "collection_invalid",
                "diagnostic": "the named stop-proof receipt does not match the recorded proof",
            });
        }
        match self.runner.collect(&record) {
            CollectOutcome::Collected {
                collection_id,
                collection_digest,
                items,
            } => {
                let mut record = record;
                record.state = "collected".into();
                let _ = self.state.put_request(&record);
                let items_json: Vec<serde_json::Value> = items
                    .iter()
                    .map(|(item, body)| {
                        json!({
                            "item_id": item.item_id,
                            "bytes": item.bytes,
                            "content_digest": item.content_digest,
                            "source": item.source,
                            "data_b64": crate::auth::base64_nopad_encode(body),
                        })
                    })
                    .collect();
                json!({
                    "status": "collected",
                    "collection_id": collection_id,
                    "collection_digest": collection_digest,
                    "items": items_json,
                })
            }
            CollectOutcome::Incomplete { code, detail } => json!({
                "status": "incomplete",
                "reason": code,
                "diagnostic": detail,
            }),
            CollectOutcome::Invalid { code, detail } => json!({
                "status": "invalid",
                "reason": code,
                "diagnostic": detail,
            }),
            CollectOutcome::Unknown { code, detail } => json!({
                "status": "unknown",
                "reason": code,
                "diagnostic": detail,
            }),
        }
    }

    // -----------------------------------------------------------------
    // acknowledge
    // -----------------------------------------------------------------

    fn op_acknowledge(&self, command: &ExecutorCommandV1) -> serde_json::Value {
        let record = match self.bound_request(command) {
            Ok(r) => r,
            Err(result) => return result,
        };
        let collection = match self.state.collection(&record.request_id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return json!({
                    "status": "refused",
                    "reason": "collection_incomplete",
                    "diagnostic": "no completed collection exists to acknowledge",
                });
            }
            Err(e) => {
                return json!({
                    "status": "unknown",
                    "reason": "unavailable",
                    "diagnostic": format!("collection record unreadable: {e}"),
                });
            }
        };
        let named_id = command
            .payload
            .get("collection_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let named_digest = command
            .payload
            .get("collection_digest")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if named_id != collection.collection_id || named_digest != collection.collection_digest {
            return json!({
                "status": "conflict",
                "reason": "acknowledgement_conflict",
                "diagnostic": "acknowledgement does not match the durable collection identity",
            });
        }
        // Tombstone before any body removal — the durable record that
        // lets a lost acknowledgement replay without recollection.
        let tombstone = AckTombstone {
            collection_id: collection.collection_id.clone(),
            collection_digest: collection.collection_digest.clone(),
            acknowledged_at_unix_seconds: Self::now_seconds(),
            bodies: "present".into(),
        };
        if let Err(e) = self
            .state
            .put_acknowledgement(&record.request_id, &tombstone)
        {
            return json!({
                "status": "unknown",
                "reason": "unavailable",
                "diagnostic": format!("acknowledgement tombstone could not be committed: {e}"),
            });
        }
        let mut record = record;
        record.state = "acknowledged".into();
        let _ = self.state.put_request(&record);
        // Remote retention releases under the tombstone: bodies are
        // reclaimed, metadata stays.
        if self.state.reclaim_bodies(&record.request_id).is_ok() {
            let mut tombstone = tombstone;
            tombstone.bodies = "reclaimed".into();
            let _ = self
                .state
                .put_acknowledgement(&record.request_id, &tombstone);
        }
        json!({"status": "acknowledged"})
    }

    // -----------------------------------------------------------------
    // Credential delivery (contract §7) — session message, not an
    // executor operation.
    // -----------------------------------------------------------------

    /// Answer a credential delivery: stage it under the bound launch
    /// configuration, never into a durable operation record.
    pub fn handle_credential(&self, delivery: &CredentialDelivery) -> CredentialAck {
        let _serial = self.lock.lock().expect("service lock");
        let record = match self.state.request(&delivery.request_id) {
            Ok(Some(r)) => r,
            _ => return CredentialAck::refused("request_unknown"),
        };
        if record.invocation_id != delivery.invocation_id {
            return CredentialAck::refused("request_unknown");
        }
        match self.runner.stage_credential(
            &record,
            &delivery.launch_config_digest,
            &delivery.secret().unwrap_or_default(),
        ) {
            Ok(()) => CredentialAck::staged(),
            Err(reason) => CredentialAck::refused(reason),
        }
    }

    // -----------------------------------------------------------------
    // Serving
    // -----------------------------------------------------------------

    /// Serve one authenticated session over a connected stream: the
    /// endpoint-side handshake, then frames until close or protocol
    /// fault. Invocation state never depends on this session's life.
    pub fn serve_stream(&self, channel: &mut (impl Read + Write)) -> Result<(), SessionError> {
        let received_at = Self::now_ms();
        let challenge = channel::endpoint_handshake(
            channel,
            &self.config.installation_id,
            &self.config.endpoint_id,
            &self.endpoint_credential,
            &self.endpoint_identity_digest,
            &self.coordinator_credential,
        )?;
        self.runner
            .record_clock_offset(challenge.coordinator_time_unix_ms, received_at);
        loop {
            let raw = match channel::read_frame(channel, FrameClass::Command) {
                Ok(raw) => raw,
                Err(SessionError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let format = serde_json::from_slice::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| {
                    v.get("format_version")
                        .and_then(|f| f.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            match format.as_str() {
                workloop_core::executor::EXECUTOR_HELLO_V1_FORMAT => {
                    let hello = ExecutorHelloV1::parse(&raw)?;
                    let reply = self.hello_reply(&hello)?;
                    channel::write_frame(
                        channel,
                        &reply.to_canonical_bytes(),
                        FrameClass::Handshake,
                    )?;
                }
                workloop_core::executor::EXECUTOR_COMMAND_V1_FORMAT => {
                    let command = ExecutorCommandV1::parse(&raw)?;
                    let receipt = self.handle(&command)?;
                    channel::write_frame(channel, &receipt, FrameClass::Receipt)?;
                }
                crate::auth::CREDENTIAL_FORMAT => {
                    let delivery = CredentialDelivery::parse(&raw)?;
                    let ack = self.handle_credential(&delivery);
                    channel::write_frame(
                        channel,
                        &ack.to_canonical_bytes()?,
                        FrameClass::Handshake,
                    )?;
                }
                _ => {
                    return Err(SessionError::Protocol(format!(
                        "unrecognized frame format {format:?}"
                    )));
                }
            }
        }
    }

    /// Accept sessions on the configured unix socket until `stop`
    /// flips. Each connection is a disposable transport session; the
    /// Invocation custody lives in the state root and process table.
    /// `serve` also ticks the deadline sweep so authorized runtime is
    /// enforced without coordinator liveness.
    pub fn serve_unix(
        self: &Arc<Self>,
        socket_path: &PathBuf,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), String> {
        let _ = std::fs::remove_file(socket_path);
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let listener = UnixListener::bind(socket_path).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;

        // Deadline sweep: the executor enforces authorized runtime on
        // its own clock, independent of session traffic. The sweep
        // serializes with command handling on the same service lock —
        // an unsynchronized sweep could run stop escalation
        // concurrently with a session's `request_stop` and
        // double-signal the group or tear a shared record write.
        let ticker = {
            let service = Arc::clone(self);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    {
                        let _serial = service.lock.lock().expect("service lock");
                        service.runner.enforce_deadlines(&service.incarnation);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            })
        };

        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let service = Arc::clone(self);
                    std::thread::spawn(move || {
                        let _ = service.serve_stream(&mut stream);
                        let _ = stream.shutdown(Shutdown::Both);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let _ = ticker.join();
        Ok(())
    }
}

/// Rebuild the `ExecutorIdentity` a receipt needs from the answering
/// command.
fn command_identity(command: &ExecutorCommandV1) -> workloop_core::executor::ExecutorIdentity {
    workloop_core::executor::ExecutorIdentity {
        installation_id: command.installation_id.clone(),
        endpoint_id: command.endpoint_id.clone(),
        endpoint_identity_digest: command.endpoint_identity_digest.clone(),
        bound_incarnation_id: command.bound_incarnation_id.clone(),
        request_id: command.request_id.clone(),
        invocation_id: command.invocation_id.clone(),
        request_digest: command.request_digest.clone(),
    }
}
