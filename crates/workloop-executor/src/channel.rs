//! The bounded framed channel both sides share (contract §4): a
//! fixed-width unsigned 32-bit big-endian length before every body,
//! checked against the frame's class bound before allocation. The
//! session-auth handshake runs on top of it before any executor
//! envelope moves.

use std::io::{Read, Write};

use workloop_core::executor::{
    EnvelopeError, MAX_COMMAND_BYTES, MAX_HANDSHAKE_BYTES, MAX_RECEIPT_BYTES, frame_body, frame_len,
};

use crate::auth::{
    CredentialAck, CredentialDelivery, SessionAuth, SessionChallenge, SessionOpen, SessionRefusal,
};

/// Which side of the wire a frame belongs to, so each direction
/// applies its own bound before the body is even allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Session-auth, hello, and credential envelopes.
    Handshake,
    /// `taisan.executor.command/1`.
    Command,
    /// `taisan.executor.receipt/1`.
    Receipt,
}

impl FrameClass {
    pub fn limit(self) -> u64 {
        match self {
            Self::Handshake => MAX_HANDSHAKE_BYTES,
            Self::Command => MAX_COMMAND_BYTES,
            Self::Receipt => MAX_RECEIPT_BYTES,
        }
    }
}

/// Why a session exchange failed — transport faults are typed and
/// never become Worker report statuses or clean results.
#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    /// The peer sent a malformed, oversized, foreign, or replayed
    /// frame — the session is untrusted and closes.
    Protocol(String),
    /// Authentication failed; `reason` is the session-refusal code.
    Refused(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Protocol(detail) => write!(f, "protocol: {detail}"),
            Self::Refused(reason) => write!(f, "refused: {reason}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<EnvelopeError> for SessionError {
    fn from(e: EnvelopeError) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// Write one framed body at its class bound.
pub fn write_frame(w: &mut impl Write, body: &[u8], class: FrameClass) -> Result<(), SessionError> {
    let frame = frame_body(body, class.limit())?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Read one framed body: the 4-byte header's declared length gates
/// against the class bound *before* the body buffer exists.
pub fn read_frame(r: &mut impl Read, class: FrameClass) -> Result<Vec<u8>, SessionError> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let declared = frame_len(&header, class.limit())?;
    let mut body = vec![0u8; declared as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// If the next frame is a session refusal, return its reason;
/// anything else is a protocol fault the caller reports.
fn read_refusal_or<T>(
    frame: &[u8],
    parse: impl FnOnce(&[u8]) -> Result<T, EnvelopeError>,
) -> Result<T, SessionError> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(frame)
        && value.get("format_version").and_then(|v| v.as_str())
            == Some(crate::auth::SESSION_REFUSAL_FORMAT)
    {
        let reason = value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("session_invalid")
            .to_string();
        return Err(SessionError::Refused(reason));
    }
    Ok(parse(frame)?)
}

/// The coordinator side of the session-auth handshake: challenge,
/// verify the endpoint's MAC and declared identity, then counter-prove.
/// `endpoint_credential`/`coordinator_credential` are the configured
/// bootstrap secrets; `expected_identity_digest` is the configured
/// endpoint binding — a genuine endpoint must prove both.
pub fn coordinator_handshake(
    channel: &mut (impl Read + Write),
    installation_id: &str,
    endpoint_id: &str,
    endpoint_credential: &[u8],
    coordinator_credential: &[u8],
    expected_identity_digest: &str,
    now_unix_ms: i64,
) -> Result<SessionAuth, SessionError> {
    let challenge = SessionChallenge::new(installation_id, endpoint_id, now_unix_ms);
    write_frame(
        channel,
        &challenge.to_canonical_bytes()?,
        FrameClass::Handshake,
    )?;
    let raw = read_frame(channel, FrameClass::Handshake)?;
    let auth: SessionAuth = read_refusal_or(&raw, SessionAuth::parse)?;
    if auth.installation_id != installation_id
        || auth.endpoint_id != endpoint_id
        || auth.coordinator_nonce != challenge.coordinator_nonce
    {
        return Err(SessionError::Refused("session_invalid".into()));
    }
    if auth.endpoint_identity_digest != expected_identity_digest {
        return Err(SessionError::Refused("endpoint_mismatch".into()));
    }
    if auth.expected_mac(endpoint_credential)? != auth.endpoint_mac {
        return Err(SessionError::Refused("endpoint_auth_failed".into()));
    }
    let open = SessionOpen::new(&challenge, &auth, coordinator_credential)?;
    write_frame(channel, &open.to_canonical_bytes()?, FrameClass::Handshake)?;
    Ok(auth)
}

/// The endpoint side of the session-auth handshake: answer the
/// challenge, then demand the coordinator's counter-proof. A failed
/// check emits a bounded refusal frame before the close so a genuine
/// coordinator keeps the typed diagnostic.
pub fn endpoint_handshake(
    channel: &mut (impl Read + Write),
    installation_id: &str,
    endpoint_id: &str,
    endpoint_credential: &[u8],
    endpoint_identity_digest: &str,
    coordinator_credential: &[u8],
) -> Result<SessionChallenge, SessionError> {
    let raw = read_frame(channel, FrameClass::Handshake)?;
    let challenge = SessionChallenge::parse(&raw)?;
    if challenge.installation_id != installation_id || challenge.endpoint_id != endpoint_id {
        let _ = write_frame(
            channel,
            &SessionRefusal::new("endpoint_mismatch").to_canonical_bytes()?,
            FrameClass::Handshake,
        );
        return Err(SessionError::Refused("endpoint_mismatch".into()));
    }
    let auth = SessionAuth::new(&challenge, endpoint_credential, endpoint_identity_digest)?;
    write_frame(channel, &auth.to_canonical_bytes()?, FrameClass::Handshake)?;
    let raw = read_frame(channel, FrameClass::Handshake)?;
    let open = SessionOpen::parse(&raw)?;
    let expected = SessionOpen::expected_mac(&challenge, &auth, coordinator_credential)?;
    if open.coordinator_nonce != challenge.coordinator_nonce
        || open.endpoint_nonce != auth.endpoint_nonce
        || open.coordinator_mac != expected
    {
        let _ = write_frame(
            channel,
            &SessionRefusal::new("coordinator_auth_failed").to_canonical_bytes()?,
            FrameClass::Handshake,
        );
        return Err(SessionError::Refused("coordinator_auth_failed".into()));
    }
    Ok(challenge)
}

/// Coordinator-side send of the one-Invocation runtime credential;
/// returns the endpoint's typed ack.
pub fn send_credential(
    channel: &mut (impl Read + Write),
    delivery: &CredentialDelivery,
) -> Result<CredentialAck, SessionError> {
    write_frame(
        channel,
        &delivery.to_canonical_bytes()?,
        FrameClass::Handshake,
    )?;
    let raw = read_frame(channel, FrameClass::Handshake)?;
    CredentialAck::parse(&raw).map_err(SessionError::from)
}
