//! The coordinator-side executor transport adapter: session-level
//! mutual authentication, the nonce-bound `hello` exchange that pins
//! the endpoint's incarnation and capability declaration, and framed
//! command/receipt issue for the six fixed operations.
//!
//! The adapter is a library surface, not a daemon: whatever runs it
//! holds one connected channel (the restricted-SSH helper's stdio in
//! the reference environment, a local socket in tests) and issues
//! operations against it. Receipts are verified against their command
//! before they leave this module — a foreign or stale receipt is a
//! protocol fault, never an answer.

use std::io::{Read, Write};

use workloop_core::executor::{
    ClockMeasurement, ExecutorCommandV1, ExecutorHelloReplyV1, ExecutorHelloV1, ExecutorIdentity,
    ExecutorReceiptV1, OperationKind, select_protocol_version, verify_receipt,
};

use crate::auth::{CredentialAck, CredentialDelivery, SessionAuth, fresh_nonce};
use crate::channel::{self, FrameClass, SessionError};
use crate::config::CoordinatorEndpoint;

/// Why an adapter call failed: the transport session broke, the peer
/// violated the protocol, or a receipt could not be verified against
/// its command. None of these are executor answers.
#[derive(Debug)]
pub enum AdapterError {
    Session(SessionError),
    /// A receipt arrived that does not verifiably answer the command
    /// issued — foreign or stale evidence. The violations name every
    /// broken echo field.
    ForeignReceipt(Vec<String>),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(e) => write!(f, "session: {e}"),
            Self::ForeignReceipt(violations) => {
                write!(f, "receipt violates its command: {}", violations.join("; "))
            }
        }
    }
}

impl std::error::Error for AdapterError {}

impl From<SessionError> for AdapterError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

/// What a connected adapter knows about the endpoint: the incarnation
/// bound for this session's lifetime, the honestly declared
/// capability set, and the clock measurement the coordinator's
/// deadline accounting needs (contract §8).
#[derive(Debug, Clone)]
pub struct EndpointView {
    /// The incarnation the hello-reply bound — commands issued over
    /// this adapter name it.
    pub bound_incarnation_id: String,
    /// The endpoint's declared capabilities, verbatim as bound by the
    /// capabilities digest.
    pub capabilities: Vec<String>,
    /// The protocol version selected for this binding.
    pub protocol_version: String,
    /// The handshake clock measurement: offset, uncertainty, and
    /// freshness timestamp.
    pub clock: ClockMeasurement,
}

/// The authenticated transport adapter for one configured endpoint.
/// Generic over the channel — the SSH forced-command's stdio or a
/// test's local socket both satisfy it.
pub struct RemoteExecutor<C> {
    channel: C,
    view: EndpointView,
    installation_id: String,
    endpoint_id: String,
    endpoint_identity_digest: String,
}

impl<C: Read + Write> RemoteExecutor<C> {
    /// Run the session-auth handshake, then `hello`: verify the
    /// endpoint's MAC and identity digest, echo-bind the nonce, and
    /// select the protocol version. A failed check is a refused
    /// session, never a degraded binding.
    pub fn connect(
        mut channel: C,
        binding: &CoordinatorEndpoint,
        now_unix_ms: i64,
    ) -> Result<Self, AdapterError> {
        binding
            .validate()
            .map_err(|e| AdapterError::Session(SessionError::Refused(e)))?;
        let auth: SessionAuth = channel::coordinator_handshake(
            &mut channel,
            &binding.installation_id,
            &binding.endpoint_id,
            &binding.endpoint_credential,
            &binding.coordinator_credential,
            &binding.expected_identity_digest,
            now_unix_ms,
        )?;
        let _ = auth;
        let hello = ExecutorHelloV1::new(
            &binding.installation_id,
            &binding.endpoint_id,
            fresh_nonce(),
            binding.allowed_protocols.clone(),
        );
        channel::write_frame(
            &mut channel,
            &hello.to_canonical_bytes(),
            FrameClass::Handshake,
        )?;
        let raw = channel::read_frame(&mut channel, FrameClass::Handshake)?;
        let reply = ExecutorHelloReplyV1::parse(&raw)
            .map_err(|e| AdapterError::Session(SessionError::Protocol(e.to_string())))?;
        let violations = reply.binding_violations(&hello, &binding.expected_identity_digest);
        if !violations.is_empty() {
            return Err(AdapterError::Session(SessionError::Protocol(format!(
                "hello-reply binding violations: {}",
                violations.join("; ")
            ))));
        }
        let Some(protocol_version) =
            select_protocol_version(&reply.supported_protocols, &binding.allowed_protocols)
        else {
            return Err(AdapterError::Session(SessionError::Refused(
                "protocol_unsupported".into(),
            )));
        };
        Ok(Self {
            view: EndpointView {
                bound_incarnation_id: reply.responder_incarnation_id,
                capabilities: reply.capabilities,
                protocol_version,
                clock: reply.clock,
            },
            installation_id: binding.installation_id.clone(),
            endpoint_id: binding.endpoint_id.clone(),
            endpoint_identity_digest: binding.expected_identity_digest.clone(),
            channel,
        })
    }

    /// The endpoint view this adapter's binding carries.
    pub fn view(&self) -> &EndpointView {
        &self.view
    }

    /// The incarnation this session bound at `hello` — the identity a
    /// reconcile pass compares against each binding's recorded
    /// `bound_incarnation_id` (contract §14 fencing).
    pub fn session_incarnation(&self) -> &str {
        &self.view.bound_incarnation_id
    }

    /// Issue a command the caller already committed to the durable
    /// operation ledger (contract §14 persist-before-send): the
    /// adapter sends the recorded canonical bytes verbatim — a
    /// restart replay and a first send travel the same path, and the
    /// receipt is verified against the recorded command before it is
    /// returned for durable attachment.
    pub fn issue_recorded(
        &mut self,
        command: &ExecutorCommandV1,
    ) -> Result<ExecutorReceiptV1, AdapterError> {
        let bytes = command
            .to_canonical_bytes()
            .map_err(|e| AdapterError::Session(SessionError::Protocol(e.to_string())))?;
        channel::write_frame(&mut self.channel, &bytes, FrameClass::Command)?;
        let raw = channel::read_frame(&mut self.channel, FrameClass::Receipt)?;
        verify_receipt(command, &raw).map_err(AdapterError::ForeignReceipt)
    }

    /// Issue one fixed operation. `operation_id` is the caller's
    /// durable idempotence key: re-issuing the same operation after a
    /// reconnect requires the identical id and payload so the
    /// endpoint's replay fence recognizes it.
    pub fn issue(
        &mut self,
        operation_id: &str,
        operation_kind: OperationKind,
        request_id: &str,
        invocation_id: &str,
        request_digest: &str,
        payload: serde_json::Value,
    ) -> Result<ExecutorReceiptV1, AdapterError> {
        let identity = ExecutorIdentity {
            installation_id: self.installation_id.clone(),
            endpoint_id: self.endpoint_id.clone(),
            endpoint_identity_digest: self.endpoint_identity_digest.clone(),
            bound_incarnation_id: self.view.bound_incarnation_id.clone(),
            request_id: request_id.into(),
            invocation_id: invocation_id.into(),
            request_digest: request_digest.into(),
        };
        let command = ExecutorCommandV1::new(identity, operation_id, operation_kind, payload)
            .map_err(|e| AdapterError::Session(SessionError::Protocol(e.to_string())))?;
        let bytes = command
            .to_canonical_bytes()
            .map_err(|e| AdapterError::Session(SessionError::Protocol(e.to_string())))?;
        channel::write_frame(&mut self.channel, &bytes, FrameClass::Command)?;
        let raw = channel::read_frame(&mut self.channel, FrameClass::Receipt)?;
        verify_receipt(&command, &raw).map_err(AdapterError::ForeignReceipt)
    }

    /// Deliver the one-Invocation runtime credential over the
    /// authenticated session (contract §7): a session message, never
    /// an executor operation, so the secret lands only in the
    /// endpoint's staged file — not in a receipt store.
    pub fn deliver_credential(
        &mut self,
        request_id: &str,
        invocation_id: &str,
        launch_config_digest: &str,
        secret: &[u8],
    ) -> Result<CredentialAck, AdapterError> {
        let delivery =
            CredentialDelivery::new(request_id, invocation_id, launch_config_digest, secret);
        channel::send_credential(&mut self.channel, &delivery).map_err(AdapterError::Session)
    }

    /// Release the underlying channel — the caller owns the
    /// transport's shutdown.
    pub fn into_inner(self) -> C {
        self.channel
    }
}

/// The adapter satisfies the reconcile seam (`contract §14, #378`):
/// `issue_recorded` sends the pre-persisted canonical command bytes
/// verbatim and verifies the receipt against them, so a coordinator
/// reconcile pass and a first send travel the identical path.
impl<C: Read + Write> workloop_exec::remote_reconcile::RemoteTransport for RemoteExecutor<C> {
    fn session_incarnation(&self) -> &str {
        RemoteExecutor::session_incarnation(self)
    }

    fn issue_recorded(
        &mut self,
        command: &ExecutorCommandV1,
    ) -> Result<ExecutorReceiptV1, workloop_exec::remote_reconcile::TransportError> {
        RemoteExecutor::issue_recorded(self, command).map_err(|e| {
            workloop_exec::remote_reconcile::TransportError {
                detail: e.to_string(),
            }
        })
    }
}
