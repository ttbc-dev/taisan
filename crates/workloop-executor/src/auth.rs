//! Transport-session authentication for the reference adapter.
//!
//! The executor transport contract leaves *how* the coordinator and
//! endpoint authenticate to the transport (contract §3, §7): the
//! reference environment runs the framed channel over restricted SSH
//! or an equivalent authenticated session, and this module is the
//! reference implementation of that guarantee for channels that are
//! not themselves authenticated — a mutual nonce challenge keyed by
//! the two bootstrap credentials each side is configured with.
//!
//! Each direction's proof is a MAC over fresh nonces under a
//! directional domain tag, so a captured message can never be
//! replayed or reflected into the other role. `endpoint_identity_digest`
//! is derived from the endpoint credential bytes
//! (`workloop_core::executor::endpoint_identity_digest`), so the
//! identity the coordinator binds is the identity the MAC proves.
//!
//! The messages are transport-session envelopes, not executor
//! commands: they produce no durable operation record and are bounded
//! by the handshake limit.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use workloop_core::executor::{EnvelopeError, executor_canonical_json};

pub const SESSION_CHALLENGE_FORMAT: &str = "taisan.executor.session-challenge/1";
pub const SESSION_AUTH_FORMAT: &str = "taisan.executor.session-auth/1";
pub const SESSION_OPEN_FORMAT: &str = "taisan.executor.session-open/1";
pub const SESSION_REFUSAL_FORMAT: &str = "taisan.executor.session-refusal/1";
pub const CREDENTIAL_FORMAT: &str = "taisan.executor.credential/1";
pub const CREDENTIAL_ACK_FORMAT: &str = "taisan.executor.credential-ack/1";

const ENDPOINT_MAC_TAG: &[u8] = b"taisan.executor.session.endpoint.v1\0";
const COORDINATOR_MAC_TAG: &[u8] = b"taisan.executor.session.coordinator.v1\0";

/// RFC 2104 HMAC-SHA256. The two directional session credentials are
/// the only keys used; the construction is fixed so both sides of the
/// channel implement the identical function.
fn hmac_sha256(key: &[u8], tag: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        k[..digest.len()].copy_from_slice(&digest);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(tag);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

fn mac_hex(key: &[u8], tag: &[u8], msg: &[u8]) -> String {
    hmac_sha256(key, tag, msg)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fresh unpredictable 128-bit nonce rendered as 32 lowercase hex —
/// the same shape the contract's handshake nonce carries.
pub fn fresh_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn malformed(detail: impl Into<String>) -> EnvelopeError {
    EnvelopeError::Malformed {
        detail: detail.into(),
    }
}

/// `taisan.executor.session-challenge/1`: the coordinator opens a
/// session naming the configured endpoint and a fresh nonce. It also
/// carries the coordinator's wall-clock send time so the endpoint can
/// record the clock measurement the contract's deadline accounting
/// needs (contract §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionChallenge {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    /// 32 lowercase hex.
    pub coordinator_nonce: String,
    pub coordinator_time_unix_ms: i64,
}

impl SessionChallenge {
    pub fn new(installation_id: &str, endpoint_id: &str, now_unix_ms: i64) -> Self {
        Self {
            format_version: SESSION_CHALLENGE_FORMAT.into(),
            installation_id: installation_id.into(),
            endpoint_id: endpoint_id.into(),
            coordinator_nonce: fresh_nonce(),
            coordinator_time_unix_ms: now_unix_ms,
        }
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        let value = strict_json_bounded(raw)?;
        let this: Self =
            serde_json::from_value(value).map_err(|e| malformed(format!("challenge: {e}")))?;
        if !is_hex32(&this.coordinator_nonce) {
            return Err(EnvelopeError::Invalid {
                reason: "session.invalid",
                detail: "coordinator_nonce is not 32 lowercase hex".into(),
            });
        }
        Ok(this)
    }
}

/// `taisan.executor.session-auth/1`: the endpoint's proof — a MAC
/// over the coordinator's nonce, the endpoint's fresh nonce, and the
/// endpoint identity digest, keyed by the endpoint credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAuth {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    pub coordinator_nonce: String,
    /// 32 lowercase hex.
    pub endpoint_nonce: String,
    /// 64 lowercase hex.
    pub endpoint_identity_digest: String,
    /// 64 lowercase hex.
    pub endpoint_mac: String,
}

impl SessionAuth {
    pub fn new(
        challenge: &SessionChallenge,
        endpoint_credential: &[u8],
        endpoint_identity_digest: &str,
    ) -> Result<Self, EnvelopeError> {
        let endpoint_nonce = fresh_nonce();
        let covered = executor_canonical_json(&serde_json::json!({
            "coordinator_nonce": challenge.coordinator_nonce,
            "endpoint_nonce": endpoint_nonce,
            "endpoint_identity_digest": endpoint_identity_digest,
        }))?;
        Ok(Self {
            format_version: SESSION_AUTH_FORMAT.into(),
            installation_id: challenge.installation_id.clone(),
            endpoint_id: challenge.endpoint_id.clone(),
            coordinator_nonce: challenge.coordinator_nonce.clone(),
            endpoint_nonce,
            endpoint_identity_digest: endpoint_identity_digest.into(),
            endpoint_mac: mac_hex(endpoint_credential, ENDPOINT_MAC_TAG, &covered),
        })
    }

    /// The MAC this frame must carry for it to be genuine. Compared
    /// in the caller so the expected value never travels.
    pub fn expected_mac(&self, endpoint_credential: &[u8]) -> Result<String, EnvelopeError> {
        let covered = executor_canonical_json(&serde_json::json!({
            "coordinator_nonce": self.coordinator_nonce,
            "endpoint_nonce": self.endpoint_nonce,
            "endpoint_identity_digest": self.endpoint_identity_digest,
        }))?;
        Ok(mac_hex(endpoint_credential, ENDPOINT_MAC_TAG, &covered))
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        let value = strict_json_bounded(raw)?;
        let this: Self =
            serde_json::from_value(value).map_err(|e| malformed(format!("session-auth: {e}")))?;
        if !is_hex32(&this.coordinator_nonce)
            || !is_hex32(&this.endpoint_nonce)
            || !is_hex64(&this.endpoint_identity_digest)
            || !is_hex64(&this.endpoint_mac)
        {
            return Err(EnvelopeError::Invalid {
                reason: "session.invalid",
                detail: "session-auth member is not lowercase hex of the bound width".into(),
            });
        }
        Ok(this)
    }
}

/// `taisan.executor.session-open/1`: the coordinator's counter-proof
/// keyed by the coordinator credential. After this frame the channel
/// is mutually authenticated and carries the contract's framed
/// envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpen {
    pub format_version: String,
    pub coordinator_nonce: String,
    pub endpoint_nonce: String,
    /// 64 lowercase hex.
    pub coordinator_mac: String,
}

impl SessionOpen {
    pub fn new(
        challenge: &SessionChallenge,
        auth: &SessionAuth,
        coordinator_credential: &[u8],
    ) -> Result<Self, EnvelopeError> {
        let covered = executor_canonical_json(&serde_json::json!({
            "coordinator_nonce": challenge.coordinator_nonce,
            "endpoint_id": challenge.endpoint_id,
            "endpoint_nonce": auth.endpoint_nonce,
        }))?;
        Ok(Self {
            format_version: SESSION_OPEN_FORMAT.into(),
            coordinator_nonce: challenge.coordinator_nonce.clone(),
            endpoint_nonce: auth.endpoint_nonce.clone(),
            coordinator_mac: mac_hex(coordinator_credential, COORDINATOR_MAC_TAG, &covered),
        })
    }

    pub fn expected_mac(
        challenge: &SessionChallenge,
        auth: &SessionAuth,
        coordinator_credential: &[u8],
    ) -> Result<String, EnvelopeError> {
        let covered = executor_canonical_json(&serde_json::json!({
            "coordinator_nonce": challenge.coordinator_nonce,
            "endpoint_id": challenge.endpoint_id,
            "endpoint_nonce": auth.endpoint_nonce,
        }))?;
        Ok(mac_hex(
            coordinator_credential,
            COORDINATOR_MAC_TAG,
            &covered,
        ))
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        let value = strict_json_bounded(raw)?;
        let this: Self =
            serde_json::from_value(value).map_err(|e| malformed(format!("session-open: {e}")))?;
        if !is_hex32(&this.coordinator_nonce)
            || !is_hex32(&this.endpoint_nonce)
            || !is_hex64(&this.coordinator_mac)
        {
            return Err(EnvelopeError::Invalid {
                reason: "session.invalid",
                detail: "session-open member is not lowercase hex of the bound width".into(),
            });
        }
        Ok(this)
    }
}

/// `taisan.executor.session-refusal/1`: either side's typed refusal
/// before it closes the session. `reason` is a fixed diagnostic code,
/// never free-form peer text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRefusal {
    pub format_version: String,
    /// One of: `coordinator_auth_failed`, `endpoint_auth_failed`,
    /// `endpoint_mismatch`, `session_invalid`.
    pub reason: String,
}

impl SessionRefusal {
    pub fn new(reason: &str) -> Self {
        Self {
            format_version: SESSION_REFUSAL_FORMAT.into(),
            reason: reason.into(),
        }
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }
}

/// `taisan.executor.credential/1`: the one-Invocation runtime
/// credential delivered over the authenticated session (contract §7).
/// It is a session message, not an executor operation: it produces no
/// durable operation record, so the secret never lands in the receipt
/// store, a report, or a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialDelivery {
    pub format_version: String,
    pub request_id: String,
    pub invocation_id: String,
    /// The immutable launch configuration the credential is bound to.
    pub launch_config_digest: String,
    /// Base64url-no-pad secret bytes.
    pub secret_b64: String,
}

impl CredentialDelivery {
    pub fn new(
        request_id: &str,
        invocation_id: &str,
        launch_config_digest: &str,
        secret: &[u8],
    ) -> Self {
        Self {
            format_version: CREDENTIAL_FORMAT.into(),
            request_id: request_id.into(),
            invocation_id: invocation_id.into(),
            launch_config_digest: launch_config_digest.into(),
            secret_b64: base64_nopad_encode(secret),
        }
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        let value = strict_json_bounded(raw)?;
        let this: Self =
            serde_json::from_value(value).map_err(|e| malformed(format!("credential: {e}")))?;
        if !is_hex64(&this.launch_config_digest) {
            return Err(EnvelopeError::Invalid {
                reason: "session.invalid",
                detail: "launch_config_digest is not 64 lowercase hex".into(),
            });
        }
        Ok(this)
    }

    pub fn secret(&self) -> Result<Vec<u8>, EnvelopeError> {
        base64_nopad_decode(&self.secret_b64)
    }
}

/// `taisan.executor.credential-ack/1`: the endpoint's bounded answer
/// to a credential delivery — `staged` or `refused` with a closed
/// reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialAck {
    pub format_version: String,
    /// `staged` | `refused`.
    pub status: String,
    /// Closed reason when refused: `request_unknown`, `not_prepared`,
    /// `secret_invalid`, `secret_oversize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CredentialAck {
    pub fn staged() -> Self {
        Self {
            format_version: CREDENTIAL_ACK_FORMAT.into(),
            status: "staged".into(),
            reason: None,
        }
    }

    pub fn refused(reason: &str) -> Self {
        Self {
            format_version: CREDENTIAL_ACK_FORMAT.into(),
            status: "refused".into(),
            reason: Some(reason.into()),
        }
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).map_err(|e| malformed(e.to_string()))?)
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        let value = strict_json_bounded(raw)?;
        let this: Self =
            serde_json::from_value(value).map_err(|e| malformed(format!("credential-ack: {e}")))?;
        match this.status.as_str() {
            "staged" | "refused" => Ok(this),
            other => Err(EnvelopeError::Invalid {
                reason: "session.invalid",
                detail: format!("credential-ack status {other:?} is not in the closed set"),
            }),
        }
    }
}

fn strict_json_bounded(raw: &[u8]) -> Result<serde_json::Value, EnvelopeError> {
    if raw.len() as u64 > workloop_core::executor::MAX_HANDSHAKE_BYTES {
        return Err(EnvelopeError::Oversize {
            limit: workloop_core::executor::MAX_HANDSHAKE_BYTES,
            bytes: raw.len() as u64,
        });
    }
    // Session envelopes share the executor contract's strict JSON
    // rules: sorted-key canonical emission, no duplicates, floats, or
    // out-of-range integers.
    let text = std::str::from_utf8(raw).map_err(|_| EnvelopeError::Malformed {
        detail: "session frame is not UTF-8".into(),
    })?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| EnvelopeError::Malformed {
            detail: format!("session frame is not JSON: {e}"),
        })?;
    let declared = value
        .get("format_version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    const FORMATS: &[&str] = &[
        SESSION_CHALLENGE_FORMAT,
        SESSION_AUTH_FORMAT,
        SESSION_OPEN_FORMAT,
        SESSION_REFUSAL_FORMAT,
        CREDENTIAL_FORMAT,
        CREDENTIAL_ACK_FORMAT,
    ];
    if !FORMATS.contains(&declared.as_str()) {
        return Err(EnvelopeError::VersionUnknown { declared });
    }
    // Session frames must arrive canonical: re-emission must equal the
    // received bytes. A duplicate key, unsorted member, float, or
    // non-minimal escape can never satisfy that check.
    if executor_canonical_json(&value)? != raw {
        return Err(EnvelopeError::Malformed {
            detail: "session frame is not canonical JSON".into(),
        });
    }
    Ok(value)
}

// Base64url without padding — compact ASCII for canonical JSON
// transport. Encoding is symmetric with `base64_nopad_decode`.
pub fn base64_nopad_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

pub fn base64_nopad_decode(s: &str) -> Result<Vec<u8>, EnvelopeError> {
    fn val(b: u8) -> Result<u32, EnvelopeError> {
        match b {
            b'A'..=b'Z' => Ok(u32::from(b - b'A')),
            b'a'..=b'z' => Ok(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(b - b'0') + 52),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err(EnvelopeError::Invalid {
                reason: "base64.invalid",
                detail: "character outside the base64url alphabet".into(),
            }),
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(EnvelopeError::Invalid {
            reason: "base64.invalid",
            detail: "length cannot encode whole octets".into(),
        });
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            n |= val(b)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_case1() {
        // RFC 4231 test case 1: key = 20 * 0x0b, data = "Hi There".
        let out = hmac_sha256(&[0x0b; 20], b"", b"Hi There");
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case2() {
        // RFC 4231 test case 2: key = "Jefe", data = canonical test string.
        let out = hmac_sha256(b"Jefe", b"", b"what do ya want for nothing?");
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case6() {
        // RFC 4231 test case 6: 131-byte key exercises the key-hashing arm.
        let out = hmac_sha256(
            &[0xaau8; 131],
            b"",
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..64 {
            let bytes: Vec<u8> = (0..n)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
                .collect();
            let s = base64_nopad_encode(&bytes);
            assert_eq!(base64_nopad_decode(&s).unwrap(), bytes);
        }
        assert!(base64_nopad_decode("a").is_err());
        assert!(base64_nopad_decode("a===").is_err());
    }

    #[test]
    fn the_session_frames_round_trip_and_verify() {
        let challenge = SessionChallenge::new("install-a", "executor-a", 1_700_000_000_000);
        let raw = challenge.to_canonical_bytes().unwrap();
        let parsed = SessionChallenge::parse(&raw).unwrap();
        assert_eq!(parsed, challenge);

        let auth = SessionAuth::new(&challenge, b"endpoint-secret", &"ab".repeat(32)).unwrap();
        let raw = auth.to_canonical_bytes().unwrap();
        let parsed = SessionAuth::parse(&raw).unwrap();
        assert_eq!(
            parsed.endpoint_mac,
            parsed.expected_mac(b"endpoint-secret").unwrap()
        );
        assert_ne!(parsed.endpoint_mac, parsed.expected_mac(b"wrong").unwrap());

        let open = SessionOpen::new(&challenge, &auth, b"coord-secret").unwrap();
        let expected = SessionOpen::expected_mac(&challenge, &auth, b"coord-secret").unwrap();
        assert_eq!(open.coordinator_mac, expected);
        SessionOpen::parse(&open.to_canonical_bytes().unwrap()).unwrap();
    }

    #[test]
    fn a_foreign_frame_is_refused() {
        let raw = executor_canonical_json(&serde_json::json!({
            "format_version": "taisan.executor.session-challenge/9",
            "installation_id": "x", "endpoint_id": "y",
            "coordinator_nonce": "0".repeat(32), "coordinator_time_unix_ms": 0
        }))
        .unwrap();
        assert!(matches!(
            SessionChallenge::parse(&raw),
            Err(EnvelopeError::VersionUnknown { .. })
        ));
    }
}
