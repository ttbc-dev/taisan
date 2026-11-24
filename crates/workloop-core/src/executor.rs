//! The versioned executor transport wire types
//! (`docs/executor-transport-contract.md`, #374 contract, #375
//! implementation): `taisan.executor.command/1`,
//! `taisan.executor.receipt/1`, `taisan.executor.hello/1`,
//! `taisan.executor.hello-reply/1`, and the remote artifact-descriptor
//! `taisan.remote.handoff/1`, plus the coordinator-side durable
//! binding/operation records the contract's persist-before-send and
//! replay rules require.
//!
//! These are the wire types only. Every envelope is closed: unknown
//! fields are refused, a `format_version` other than the exact string
//! is refused, the class byte bound is checked before the body is
//! parsed, and readers never guess, downgrade, or partially parse.
//! Digests are SHA-256 over the NUL-terminated domain tag
//! (`taisan.executor.<subject>.v1\0`) and the contract-canonical bytes
//! of the object with its own digest member omitted.
//!
//! Canonical JSON here is the *contract* form, which is stricter than
//! [`crate::domain::canonical_json`]: object keys sort by UTF-8 byte
//! order, no insignificant whitespace is emitted, strings escape only
//! quotation marks, backslashes, and control characters — every control
//! character as a lowercase `\u00xx` escape — other code points emit as
//! UTF-8 without normalization, floating-point values and duplicate
//! keys are refused, and integers are base-10 signed 64-bit values.
//! The Worker wire types keep their own canonicalization; nothing here
//! changes a `taisan.worker.*` digest.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// Formats, limits, and digest tags (contract §4)
// ---------------------------------------------------------------------

/// The only command format this contract reads or writes.
pub const EXECUTOR_COMMAND_V1_FORMAT: &str = "taisan.executor.command/1";
/// The only receipt format this contract reads or writes.
pub const EXECUTOR_RECEIPT_V1_FORMAT: &str = "taisan.executor.receipt/1";
/// The only hello format this contract reads or writes.
pub const EXECUTOR_HELLO_V1_FORMAT: &str = "taisan.executor.hello/1";
/// The only hello-reply format this contract reads or writes.
pub const EXECUTOR_HELLO_REPLY_V1_FORMAT: &str = "taisan.executor.hello-reply/1";
/// The only remote handoff-manifest format this contract reads.
pub const REMOTE_HANDOFF_V1_FORMAT: &str = "taisan.remote.handoff/1";
/// The executor protocol version this slice negotiates.
pub const EXECUTOR_PROTOCOL_V1: &str = "taisan.executor/1";

/// Hard wire limit for handshake messages (contract §4).
pub const MAX_HANDSHAKE_BYTES: u64 = 64 * 1024;
/// Hard wire limit for command envelopes.
pub const MAX_COMMAND_BYTES: u64 = 1024 * 1024;
/// Hard wire limit for receipt envelopes.
pub const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
/// Hard wire limit for one `handoff.json` manifest.
pub const MAX_HANDOFF_BYTES: u64 = 1024 * 1024;
/// Hard wire limit for one data frame.
pub const MAX_DATA_FRAME_BYTES: u64 = 4 * 1024 * 1024;
/// Bound on free-form `diagnostic` text a result may carry: retained
/// diagnostics never steer state and never grow unbounded.
pub const MAX_DIAGNOSTIC_CHARS: usize = 2048;

/// Digest domain tags (contract §4): each is hashed exactly as written
/// — the NUL terminator is part of the tag.
pub const COMMAND_DIGEST_TAG: &[u8] = b"taisan.executor.command.v1\0";
pub const RECEIPT_DIGEST_TAG: &[u8] = b"taisan.executor.receipt.v1\0";
pub const CAPABILITIES_DIGEST_TAG: &[u8] = b"taisan.executor.capabilities.v1\0";
pub const PAYLOAD_DIGEST_TAG: &[u8] = b"taisan.executor.payload.v1\0";
pub const RESULT_DIGEST_TAG: &[u8] = b"taisan.executor.result.v1\0";
pub const ENDPOINT_IDENTITY_DIGEST_TAG: &[u8] = b"taisan.executor.endpoint-identity.v1\0";
pub const LAUNCH_CONFIG_DIGEST_TAG: &[u8] = b"taisan.executor.launch-config.v1\0";
pub const COLLECTION_DIGEST_TAG: &[u8] = b"taisan.executor.collection.v1\0";
pub const HANDOFF_DIGEST_TAG: &[u8] = b"taisan.remote.handoff.v1\0";
/// Coordinator-local tag for the durable binding record's self-check
/// digest; the binding never crosses the wire, so no contract tag
/// exists for it.
const BINDING_DIGEST_TAG: &[u8] = b"taisan.executor.binding.v1\0";

/// SHA-256 over a NUL-terminated domain tag and the payload bytes.
/// This is deliberately not [`crate::domain::sha256_hex`], which joins
/// tag and bytes with an ASCII colon — the executor contract's tags
/// carry their own `\0` terminator.
fn tagged_sha256(tag: &[u8], bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag);
    h.update(bytes);
    use std::fmt::Write;
    h.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// Why an executor envelope or durable record is not readable.
/// Codes mirror the closed `envelope.*` vocabulary; provider or
/// endpoint prose never enters a code — it stays in `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The body exceeds the class's hard bound; checked before the
    /// body is parsed or allocated from.
    Oversize { limit: u64, bytes: u64 },
    /// Not the closed schema: malformed JSON, a missing member, an
    /// unknown field, a duplicate key, a float or out-of-range integer,
    /// or a wrong member type.
    Malformed { detail: String },
    /// `format_version` is not the exact string this contract reads.
    VersionUnknown { declared: String },
    /// Internally inconsistent: a closed-vocabulary violation, an
    /// unsafe path, an unsorted manifest, a nonce/identity echo that
    /// does not match the message it answers.
    Invalid {
        reason: &'static str,
        detail: String,
    },
    /// A declared digest member does not match the body's own
    /// canonical digest — the record and the bytes disagree.
    DigestMismatch { member: &'static str },
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize { limit, bytes } => {
                write!(f, "envelope.oversize: {bytes} bytes over limit {limit}")
            }
            Self::Malformed { detail } => write!(f, "envelope.malformed: {detail}"),
            Self::VersionUnknown { declared } => {
                write!(f, "envelope.version_unknown: {declared}")
            }
            Self::Invalid { reason, detail } => write!(f, "{reason}: {detail}"),
            Self::DigestMismatch { member } => {
                write!(f, "envelope.digest_mismatch: {member}")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

// ---------------------------------------------------------------------
// Contract canonical JSON (contract §4 canonicalization rules)
// ---------------------------------------------------------------------

/// One JSON string in contract-canonical form: `"` and `\` escaped,
/// every control character as a lowercase `\u00xx` escape, all other
/// code points as raw UTF-8.
fn write_canonical_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for ch in s.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Contract-canonical bytes of a JSON value. Fails on a floating-point
/// number or an integer outside signed 64-bit range — values a valid
/// envelope may never carry.
pub fn executor_canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, EnvelopeError> {
    fn write(v: &serde_json::Value, out: &mut Vec<u8>) -> Result<(), EnvelopeError> {
        match v {
            serde_json::Value::Null => out.extend_from_slice(b"null"),
            serde_json::Value::Bool(b) => {
                out.extend_from_slice(if *b { b"true" } else { b"false" })
            }
            serde_json::Value::Number(n) => {
                let Some(i) = n.as_i64() else {
                    return Err(EnvelopeError::Malformed {
                        detail: format!("number {n} is not a signed 64-bit integer"),
                    });
                };
                out.extend_from_slice(i.to_string().as_bytes());
            }
            serde_json::Value::String(s) => write_canonical_string(s, out),
            serde_json::Value::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write(item, out)?;
                }
                out.push(b']');
            }
            serde_json::Value::Object(map) => {
                out.push(b'{');
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write_canonical_string(k, out);
                    out.push(b':');
                    write(map.get(*k).expect("key from map.keys()"), out)?;
                }
                out.push(b'}');
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    write(value, &mut out)?;
    Ok(out)
}

/// Canonical bytes of a JSON value minus one top-level member — the
/// exact bytes a self-referential digest covers.
fn canonical_minus(value: &serde_json::Value, member: &str) -> Result<Vec<u8>, EnvelopeError> {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove(member);
    }
    executor_canonical_json(&value)
}

// ---------------------------------------------------------------------
// Strict JSON parsing (contract §4): no duplicate keys, no floats,
// integers bounded to signed 64-bit.
// ---------------------------------------------------------------------

/// A `serde` wrapper that routes every nested value through
/// [`StrictVisitor`]; parsing a plain `serde_json::Value` would accept
/// duplicate keys and floats the contract refuses.
struct StrictValue(serde_json::Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a contract-canonical JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::from(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        i64::try_from(v)
            .map(serde_json::Value::from)
            .map_err(|_| E::custom(format!("integer {v} exceeds the signed 64-bit bound")))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Err(E::custom(format!(
            "floating-point value {v} is not contract-canonical"
        )))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(v))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictValue::deserialize(deserializer).map(|v| v.0)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<StrictValue>()? {
            items.push(item.0);
        }
        Ok(serde_json::Value::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut seen = std::collections::HashSet::new();
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = map.next_value::<StrictValue>()?;
            out.insert(key, value.0);
        }
        Ok(serde_json::Value::Object(out))
    }
}

/// Parse wire bytes under the contract grammar: duplicate keys,
/// floating-point numbers, and integers outside signed 64-bit range
/// are refused before any typed schema runs.
fn strict_json(raw: &[u8]) -> Result<serde_json::Value, EnvelopeError> {
    use serde::Deserializer as _;
    let mut de = serde_json::Deserializer::from_slice(raw);
    let value = de
        .deserialize_any(StrictVisitor)
        .map_err(|e| EnvelopeError::Malformed {
            detail: format!("not strict JSON: {e}"),
        })?;
    de.end().map_err(|e| EnvelopeError::Malformed {
        detail: format!("trailing content after JSON body: {e}"),
    })?;
    Ok(value)
}

/// Check the class byte bound before any parse: the receiver rejects a
/// frame before allocating from it.
fn check_bound(raw: &[u8], limit: u64) -> Result<(), EnvelopeError> {
    let bytes = raw.len() as u64;
    if bytes > limit {
        return Err(EnvelopeError::Oversize { limit, bytes });
    }
    Ok(())
}

/// The declared `format_version` member, or the empty string.
fn declared_format(value: &serde_json::Value) -> String {
    value
        .get("format_version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Whether a string is exactly 64 lowercase hex characters.
fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Whether a string is exactly 40 lowercase hex characters.
fn is_hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Whether a string is exactly 32 lowercase hex characters.
fn is_hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

// ---------------------------------------------------------------------
// Operation vocabulary (contract §5, §9)
// ---------------------------------------------------------------------

/// The six executor operations — the closed `operation_kind`
/// vocabulary of the command and receipt envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Prepare,
    Start,
    Observe,
    RequestStop,
    Collect,
    Acknowledge,
}

impl OperationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Start => "start",
            Self::Observe => "observe",
            Self::RequestStop => "request_stop",
            Self::Collect => "collect",
            Self::Acknowledge => "acknowledge",
        }
    }

    /// The closed `result.status` set this operation's receipt may
    /// carry (contract §5 "Main results").
    fn result_statuses(&self) -> &'static [&'static str] {
        match self {
            Self::Prepare => &["prepared", "pending", "refused", "unknown"],
            Self::Start => &["started", "known_not_started", "unknown", "refused"],
            Self::Observe => &["running", "exited", "stopped", "unknown", "unavailable"],
            Self::RequestStop => &["stop_requested", "stopped", "unknown", "unavailable"],
            Self::Collect => &[
                "collected",
                "pending",
                "incomplete",
                "invalid",
                "unknown",
                "unavailable",
            ],
            Self::Acknowledge => &["acknowledged", "conflict", "refused", "unknown"],
        }
    }
}

/// The closed refusal/diagnostic code vocabulary (contract §9).
/// Unknown codes are protocol errors — the coordinator retains the
/// receipt and fails closed.
pub const REFUSAL_CODES: &[&str] = &[
    "endpoint_identity_mismatch",
    "incarnation_mismatch",
    "protocol_unsupported",
    "capability_missing",
    "configuration_mismatch",
    "request_identity_mismatch",
    "conflicting_replay",
    "input_invalid",
    "limit_exceeded",
    "known_not_started",
    "process_identity_unavailable",
    "unknown",
    "unavailable",
    "collection_incomplete",
    "collection_invalid",
    "acknowledgement_conflict",
];

/// The opaque process-evidence reference a `started`, `running`, or
/// `exited` result carries (contract §1, §13): the endpoint-specific
/// identifier stays inside the executor; the coordinator sees only
/// this reference plus its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEvidenceRef {
    pub evidence_ref: String,
    /// 64 lowercase hex.
    pub identity_digest: String,
}

/// Validate one receipt `result` value against its operation's closed
/// vocabulary: `status` must be in the operation's set, `reason` —
/// where the contract requires or permits it — must be a §9 code, and
/// the members the contract names (`process`, `proof`, `sequence`,
/// `prepared_id`, `transfer_id`/`next_offset`, `collection_id`/
/// `collection_digest`) are checked where the status requires them.
/// Free-form `diagnostic` text is bounded. Members beyond the named
/// ones are tolerated: the contract pins the status/reason vocabulary
/// but leaves operation payload schemas to the operations themselves.
fn validate_result(kind: OperationKind, result: &serde_json::Value) -> Result<(), EnvelopeError> {
    let invalid = |detail: String| EnvelopeError::Invalid {
        reason: "result.invalid",
        detail,
    };
    let Some(map) = result.as_object() else {
        return Err(invalid("result is not an object".into()));
    };
    let status = map
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    if !kind.result_statuses().contains(&status) {
        return Err(invalid(format!(
            "status {status:?} is not a {} result",
            kind.as_str()
        )));
    }
    let reason = map.get("reason").and_then(|r| r.as_str());
    if reason.is_some_and(|code| !REFUSAL_CODES.contains(&code)) {
        return Err(invalid(format!("reason {reason:?} is not a refusal code")));
    }
    // `reason` is required where the contract makes it part of the
    // result's meaning: a typed refusal, a proven non-start, and the
    // unproven classes all name their code.
    if matches!(status, "refused" | "known_not_started" | "unknown") && reason.is_none() {
        return Err(invalid(format!("status {status:?} requires a reason code")));
    }
    if map
        .get("diagnostic")
        .and_then(|d| d.as_str())
        .is_some_and(|d| d.chars().count() > MAX_DIAGNOSTIC_CHARS)
    {
        return Err(invalid(format!(
            "diagnostic exceeds {MAX_DIAGNOSTIC_CHARS} chars"
        )));
    }
    let u64_member = |name: &str| -> Result<u64, EnvelopeError> {
        map.get(name)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| invalid(format!("missing or non-integer {name}")))
    };
    match status {
        "started" | "running" | "exited" => {
            let process = map.get("process").unwrap_or(&serde_json::Value::Null);
            let evidence: ProcessEvidenceRef = serde_json::from_value(process.clone())
                .map_err(|e| invalid(format!("process evidence ref malformed: {e}")))?;
            if !is_hex64(&evidence.identity_digest) {
                return Err(invalid(
                    "process identity_digest is not 64 lowercase hex".into(),
                ));
            }
        }
        _ => {}
    }
    match status {
        "running" | "exited" | "stopped" => {
            u64_member("sequence")?;
        }
        _ => {}
    }
    match status {
        "stopped" => {
            // The stop-proof reference's internal shape belongs to the
            // stop-proof evidence (#378); the envelope requires the
            // member to exist as an object so a bare claim can never
            // read as a proof.
            if !map.get("proof").is_some_and(|p| p.is_object()) {
                return Err(invalid("stopped result lacks a proof member".into()));
            }
        }
        "prepared" => {
            if !map.get("prepared_id").is_some_and(|p| p.is_string()) {
                return Err(invalid("prepared result lacks prepared_id".into()));
            }
        }
        "pending" => {
            // A pending receipt names the transfer it resumes
            // (contract §6).
            if !map.get("transfer_id").is_some_and(|t| t.is_string()) {
                return Err(invalid("pending result lacks transfer_id".into()));
            }
            u64_member("next_offset")?;
        }
        "collected"
            if !map.get("collection_id").is_some_and(|c| c.is_string())
                || !map
                    .get("collection_digest")
                    .is_some_and(|c| c.as_str().is_some_and(is_hex64)) =>
        {
            return Err(invalid(
                "collected result lacks collection_id/collection_digest".into(),
            ));
        }
        "collected" => {}
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Handshake envelopes (contract §3, §13)
// ---------------------------------------------------------------------

/// `taisan.executor.hello/1`: the coordinator's bounded handshake
/// request. The nonce binds the reply to this handshake so an old
/// capability declaration can never be reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorHelloV1 {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    /// 32 lowercase hex.
    pub coordinator_nonce: String,
    pub supported_protocols: Vec<String>,
}

impl ExecutorHelloV1 {
    pub fn new(
        installation_id: impl Into<String>,
        endpoint_id: impl Into<String>,
        coordinator_nonce: impl Into<String>,
        supported_protocols: Vec<String>,
    ) -> Self {
        Self {
            format_version: EXECUTOR_HELLO_V1_FORMAT.into(),
            installation_id: installation_id.into(),
            endpoint_id: endpoint_id.into(),
            coordinator_nonce: coordinator_nonce.into(),
            supported_protocols,
        }
    }

    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        executor_canonical_json(&serde_json::to_value(self).expect("hello serializes"))
            .expect("hello canonicalizes")
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        check_bound(raw, MAX_HANDSHAKE_BYTES)?;
        let value = strict_json(raw)?;
        let declared = declared_format(&value);
        if declared != EXECUTOR_HELLO_V1_FORMAT {
            return Err(EnvelopeError::VersionUnknown { declared });
        }
        let hello: Self = serde_json::from_value(value).map_err(|e| EnvelopeError::Malformed {
            detail: format!("hello malformed: {e}"),
        })?;
        if !is_hex32(&hello.coordinator_nonce) {
            return Err(EnvelopeError::Invalid {
                reason: "envelope.invalid",
                detail: "coordinator_nonce is not 32 lowercase hex".into(),
            });
        }
        Ok(hello)
    }
}

/// The handshake clock measurement (contract §8): the executor's
/// offset estimate and its uncertainty, valid only for the configured
/// freshness interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockMeasurement {
    pub offset_ms: i64,
    pub uncertainty_ms: u64,
    pub round_trip_ms: u64,
    pub measured_at_unix_ms: i64,
}

/// `taisan.executor.hello-reply/1`: incarnation, supported protocol
/// versions, capability declarations, and the capability digest the
/// coordinator binds to the request before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorHelloReplyV1 {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    pub coordinator_nonce: String,
    /// 64 lowercase hex — the authenticated principal's digest.
    pub endpoint_identity_digest: String,
    pub responder_incarnation_id: String,
    pub supported_protocols: Vec<String>,
    pub capabilities: Vec<String>,
    /// Digest over the canonical capability declaration.
    pub capabilities_digest: String,
    pub clock: ClockMeasurement,
}

impl ExecutorHelloReplyV1 {
    /// The digest `capabilities_digest` must equal: SHA-256 over the
    /// capabilities tag and the canonical capabilities array exactly
    /// as declared on the wire (declared order — the declaration is
    /// bound as made, never normalized).
    pub fn capabilities_digest_of(capabilities: &[String]) -> Result<String, EnvelopeError> {
        let canonical = executor_canonical_json(&serde_json::json!(capabilities))?;
        Ok(tagged_sha256(CAPABILITIES_DIGEST_TAG, &canonical))
    }

    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        executor_canonical_json(&serde_json::to_value(self).expect("reply serializes"))
            .expect("reply canonicalizes")
    }

    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        check_bound(raw, MAX_HANDSHAKE_BYTES)?;
        let value = strict_json(raw)?;
        let declared = declared_format(&value);
        if declared != EXECUTOR_HELLO_REPLY_V1_FORMAT {
            return Err(EnvelopeError::VersionUnknown { declared });
        }
        let reply: Self =
            serde_json::from_value(value.clone()).map_err(|e| EnvelopeError::Malformed {
                detail: format!("hello-reply malformed: {e}"),
            })?;
        if !is_hex32(&reply.coordinator_nonce) {
            return Err(EnvelopeError::Invalid {
                reason: "envelope.invalid",
                detail: "coordinator_nonce is not 32 lowercase hex".into(),
            });
        }
        if !is_hex64(&reply.endpoint_identity_digest) {
            return Err(EnvelopeError::Invalid {
                reason: "envelope.invalid",
                detail: "endpoint_identity_digest is not 64 lowercase hex".into(),
            });
        }
        // A capability declared twice is a malformed declaration — the
        // set semantics the digest binds admit no duplicates.
        let mut seen = std::collections::HashSet::new();
        if !reply.capabilities.iter().all(|c| seen.insert(c)) {
            return Err(EnvelopeError::Invalid {
                reason: "envelope.invalid",
                detail: "duplicate capability declaration".into(),
            });
        }
        if !is_hex64(&reply.capabilities_digest)
            || reply.capabilities_digest != Self::capabilities_digest_of(&reply.capabilities)?
        {
            return Err(EnvelopeError::DigestMismatch {
                member: "capabilities_digest",
            });
        }
        Ok(reply)
    }

    /// Violations of the handshake binding: the reply must echo the
    /// hello's installation, endpoint, and nonce, and carry the
    /// configured endpoint identity. An echo that does not match is a
    /// foreign or replayed handshake — never a binding. Returns every
    /// broken check so diagnosis retains all of them.
    pub fn binding_violations(
        &self,
        hello: &ExecutorHelloV1,
        expected_identity_digest: &str,
    ) -> Vec<&'static str> {
        let mut violations = Vec::new();
        if self.installation_id != hello.installation_id {
            violations.push("reply.wrong_installation");
        }
        if self.endpoint_id != hello.endpoint_id {
            violations.push("reply.wrong_endpoint");
        }
        if self.coordinator_nonce != hello.coordinator_nonce {
            violations.push("reply.wrong_nonce");
        }
        if self.endpoint_identity_digest != expected_identity_digest {
            violations.push("reply.identity_mismatch");
        }
        violations
    }
}

/// The authenticated endpoint principal's identity digest: SHA-256
/// over the canonical authenticated-principal bytes under the
/// endpoint-identity tag (contract §2). The principal bytes themselves
/// never leave the transport adapter.
pub fn endpoint_identity_digest(principal_bytes: &[u8]) -> String {
    tagged_sha256(ENDPOINT_IDENTITY_DIGEST_TAG, principal_bytes)
}

/// The immutable launch-configuration digest (contract §2): SHA-256
/// over the launch-config tag and the canonical configuration bytes.
/// Changing any field creates a different binding.
pub fn launch_config_digest(canonical_config: &[u8]) -> String {
    tagged_sha256(LAUNCH_CONFIG_DIGEST_TAG, canonical_config)
}

/// Select the protocol version for a request: the highest
/// `taisan.executor/<N>` the endpoint's declaration and the
/// installation's configured range both admit (contract §3). No common
/// version refuses dispatch — the caller surfaces
/// `protocol_unsupported`, never a guessed or downgraded version.
pub fn select_protocol_version(
    endpoint_supported: &[String],
    configured_allowed: &[String],
) -> Option<String> {
    let version_number = |s: &str| -> Option<u64> {
        s.strip_prefix("taisan.executor/")?
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= 1)
    };
    endpoint_supported
        .iter()
        .filter(|v| configured_allowed.contains(v))
        .filter_map(|v| version_number(v).map(|n| (n, v)))
        .max_by_key(|(n, _)| *n)
        .map(|(_, v)| v.clone())
}

// ---------------------------------------------------------------------
// Command and receipt envelopes (contract §4, §13)
// ---------------------------------------------------------------------

/// The identity tuple every command and receipt carries (contract §2):
/// the configured endpoint, its authenticated identity and bound
/// incarnation, the request/Invocation identity and digest, and the
/// operation identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorIdentity {
    pub installation_id: String,
    pub endpoint_id: String,
    pub endpoint_identity_digest: String,
    pub bound_incarnation_id: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
}

/// `taisan.executor.command/1`: one bounded operation against a
/// previously bound request. `payload_digest` covers only the
/// canonical `payload` value; `command_digest` covers the canonical
/// envelope with `command_digest` omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorCommandV1 {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    pub endpoint_identity_digest: String,
    pub bound_incarnation_id: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    pub operation_id: String,
    pub operation_kind: OperationKind,
    pub payload_digest: String,
    pub command_digest: String,
    /// The bounded operation payload. Operation-specific deadline
    /// transport fields (absolute deadline, coordinator send time,
    /// remaining runtime — contract §8) ride here: they are executor
    /// transport data, digested and bounded like the rest of the
    /// envelope, and never enter the Worker request body.
    pub payload: serde_json::Value,
}

impl ExecutorCommandV1 {
    /// Build and seal a command: computes `payload_digest` and
    /// `command_digest` over the contract-canonical value.
    pub fn new(
        identity: ExecutorIdentity,
        operation_id: impl Into<String>,
        operation_kind: OperationKind,
        payload: serde_json::Value,
    ) -> Result<Self, EnvelopeError> {
        let payload_digest = payload_digest(&payload)?;
        let mut command = Self {
            format_version: EXECUTOR_COMMAND_V1_FORMAT.into(),
            installation_id: identity.installation_id,
            endpoint_id: identity.endpoint_id,
            endpoint_identity_digest: identity.endpoint_identity_digest,
            bound_incarnation_id: identity.bound_incarnation_id,
            request_id: identity.request_id,
            invocation_id: identity.invocation_id,
            request_digest: identity.request_digest,
            operation_id: operation_id.into(),
            operation_kind,
            payload_digest,
            command_digest: String::new(),
            payload,
        };
        command.command_digest = command.digest()?;
        Ok(command)
    }

    /// The canonical body with `command_digest` omitted — the exact
    /// bytes [`Self::digest`] covers.
    fn canonical_body(&self) -> Result<Vec<u8>, EnvelopeError> {
        canonical_minus(
            &serde_json::to_value(self).expect("command serializes"),
            "command_digest",
        )
    }

    /// The full canonical command bytes — `command_digest` included —
    /// exactly what the durable operation row stores and what
    /// [`Self::parse`] accepts on verbatim replay.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).expect("command serializes"))
    }

    /// The digest of this command's canonical body.
    pub fn digest(&self) -> Result<String, EnvelopeError> {
        Ok(tagged_sha256(COMMAND_DIGEST_TAG, &self.canonical_body()?))
    }

    /// Parse a command body and validate it: bound first, strict JSON,
    /// exact format version, closed member set, then both digests.
    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        check_bound(raw, MAX_COMMAND_BYTES)?;
        let value = strict_json(raw)?;
        let declared = declared_format(&value);
        if declared != EXECUTOR_COMMAND_V1_FORMAT {
            return Err(EnvelopeError::VersionUnknown { declared });
        }
        // Digests verify over the raw body minus the covered member,
        // never a re-serialization.
        let payload_value = value
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_payload_digest = payload_digest(&payload_value)?;
        let expected_command_digest = tagged_sha256(
            COMMAND_DIGEST_TAG,
            &canonical_minus(&value, "command_digest")?,
        );
        let command: Self =
            serde_json::from_value(value).map_err(|e| EnvelopeError::Malformed {
                detail: format!("command malformed: {e}"),
            })?;
        if command.payload_digest != expected_payload_digest {
            return Err(EnvelopeError::DigestMismatch {
                member: "payload_digest",
            });
        }
        if command.command_digest != expected_command_digest {
            return Err(EnvelopeError::DigestMismatch {
                member: "command_digest",
            });
        }
        Ok(command)
    }

    /// Violations of the durable binding (contract §2): every command
    /// names the bound endpoint identity, incarnation, request, and
    /// Invocation. A command disagreeing with the recorded binding is
    /// foreign or stale and must never be sent or recorded.
    pub fn binding_violations(&self, binding: &ExecutorBindingFields) -> Vec<&'static str> {
        let mut violations = Vec::new();
        if self.installation_id != binding.installation_id {
            violations.push("command.wrong_installation");
        }
        if self.endpoint_id != binding.endpoint_id {
            violations.push("command.wrong_endpoint");
        }
        if self.endpoint_identity_digest != binding.endpoint_identity_digest {
            violations.push("command.wrong_endpoint_identity");
        }
        if self.bound_incarnation_id != binding.bound_incarnation_id {
            violations.push("command.stale_incarnation");
        }
        if self.request_id != binding.request_id {
            violations.push("command.wrong_request");
        }
        if self.invocation_id != binding.invocation_id {
            violations.push("command.wrong_invocation");
        }
        if self.request_digest != binding.request_digest {
            violations.push("command.wrong_request_digest");
        }
        violations
    }
}

/// Digest over a command payload value only (contract §4).
pub fn payload_digest(payload: &serde_json::Value) -> Result<String, EnvelopeError> {
    Ok(tagged_sha256(
        PAYLOAD_DIGEST_TAG,
        &executor_canonical_json(payload)?,
    ))
}

/// Digest over a result value only (contract §4).
pub fn result_digest(result: &serde_json::Value) -> Result<String, EnvelopeError> {
    Ok(tagged_sha256(
        RESULT_DIGEST_TAG,
        &executor_canonical_json(result)?,
    ))
}

/// `taisan.executor.receipt/1`: the executor's answer to one command —
/// the same identity tuple plus the producing incarnation, the echoed
/// `command_digest`, the operation result, and both digests. A receipt
/// is evidence of that operation only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorReceiptV1 {
    pub format_version: String,
    pub installation_id: String,
    pub endpoint_id: String,
    pub endpoint_identity_digest: String,
    pub bound_incarnation_id: String,
    pub responder_incarnation_id: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    pub operation_id: String,
    pub operation_kind: OperationKind,
    pub payload_digest: String,
    pub command_digest: String,
    pub result_digest: String,
    pub receipt_digest: String,
    pub result: serde_json::Value,
}

impl ExecutorReceiptV1 {
    /// Build and seal a receipt: computes `result_digest` and
    /// `receipt_digest` over the contract-canonical value.
    pub fn new(
        identity: ExecutorIdentity,
        responder_incarnation_id: impl Into<String>,
        operation_id: impl Into<String>,
        operation_kind: OperationKind,
        payload_digest: String,
        command_digest: String,
        result: serde_json::Value,
    ) -> Result<Self, EnvelopeError> {
        let result_digest = result_digest(&result)?;
        let mut receipt = Self {
            format_version: EXECUTOR_RECEIPT_V1_FORMAT.into(),
            installation_id: identity.installation_id,
            endpoint_id: identity.endpoint_id,
            endpoint_identity_digest: identity.endpoint_identity_digest,
            bound_incarnation_id: identity.bound_incarnation_id,
            responder_incarnation_id: responder_incarnation_id.into(),
            request_id: identity.request_id,
            invocation_id: identity.invocation_id,
            request_digest: identity.request_digest,
            operation_id: operation_id.into(),
            operation_kind,
            payload_digest,
            command_digest,
            result_digest,
            receipt_digest: String::new(),
            result,
        };
        receipt.receipt_digest = receipt.digest()?;
        Ok(receipt)
    }

    fn canonical_body(&self) -> Result<Vec<u8>, EnvelopeError> {
        canonical_minus(
            &serde_json::to_value(self).expect("receipt serializes"),
            "receipt_digest",
        )
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        executor_canonical_json(&serde_json::to_value(self).expect("receipt serializes"))
    }

    /// The digest of this receipt's canonical body — the durable
    /// receipt identity a `collect` names as stop proof or an
    /// `acknowledge` replays.
    pub fn digest(&self) -> Result<String, EnvelopeError> {
        Ok(tagged_sha256(RECEIPT_DIGEST_TAG, &self.canonical_body()?))
    }

    /// Parse a receipt body and validate it: bound first, strict JSON,
    /// exact format version, closed member set, both digests, then the
    /// closed result vocabulary for its operation kind.
    pub fn parse(raw: &[u8]) -> Result<Self, EnvelopeError> {
        check_bound(raw, MAX_RECEIPT_BYTES)?;
        let value = strict_json(raw)?;
        let declared = declared_format(&value);
        if declared != EXECUTOR_RECEIPT_V1_FORMAT {
            return Err(EnvelopeError::VersionUnknown { declared });
        }
        let result_value = value
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_result_digest = result_digest(&result_value)?;
        let expected_receipt_digest = tagged_sha256(
            RECEIPT_DIGEST_TAG,
            &canonical_minus(&value, "receipt_digest")?,
        );
        let receipt: Self =
            serde_json::from_value(value).map_err(|e| EnvelopeError::Malformed {
                detail: format!("receipt malformed: {e}"),
            })?;
        if receipt.result_digest != expected_result_digest {
            return Err(EnvelopeError::DigestMismatch {
                member: "result_digest",
            });
        }
        if receipt.receipt_digest != expected_receipt_digest {
            return Err(EnvelopeError::DigestMismatch {
                member: "receipt_digest",
            });
        }
        validate_result(receipt.operation_kind, &receipt.result)?;
        Ok(receipt)
    }

    /// Violations of the command this receipt answers (contract §4):
    /// every echo field must equal the command's — a receipt that
    /// answers a different installation, endpoint, identity,
    /// incarnation, request, Invocation, operation, or digest is
    /// foreign evidence. `responder_incarnation_id` is checked by the
    /// caller against liveness expectations, never here: a post-reboot
    /// receipt legitimately names a new responder incarnation.
    pub fn command_violations(&self, command: &ExecutorCommandV1) -> Vec<&'static str> {
        let mut violations = Vec::new();
        if self.installation_id != command.installation_id {
            violations.push("receipt.wrong_installation");
        }
        if self.endpoint_id != command.endpoint_id {
            violations.push("receipt.wrong_endpoint");
        }
        if self.endpoint_identity_digest != command.endpoint_identity_digest {
            violations.push("receipt.wrong_endpoint_identity");
        }
        if self.bound_incarnation_id != command.bound_incarnation_id {
            violations.push("receipt.wrong_bound_incarnation");
        }
        if self.request_id != command.request_id {
            violations.push("receipt.wrong_request");
        }
        if self.invocation_id != command.invocation_id {
            violations.push("receipt.wrong_invocation");
        }
        if self.request_digest != command.request_digest {
            violations.push("receipt.wrong_request_digest");
        }
        if self.operation_id != command.operation_id {
            violations.push("receipt.wrong_operation");
        }
        if self.operation_kind != command.operation_kind {
            violations.push("receipt.wrong_operation_kind");
        }
        if self.payload_digest != command.payload_digest {
            violations.push("receipt.wrong_payload_digest");
        }
        if self.command_digest != command.command_digest {
            violations.push("receipt.wrong_command_digest");
        }
        violations
    }
}

/// Parse a receipt and check it against the command it answers.
/// Returns the parsed receipt or every violated echo — a foreign or
/// stale receipt is evidence of neither the operation's effect nor a
/// clean answer.
pub fn verify_receipt(
    command: &ExecutorCommandV1,
    receipt_raw: &[u8],
) -> Result<ExecutorReceiptV1, Vec<String>> {
    let receipt =
        ExecutorReceiptV1::parse(receipt_raw).map_err(|e| vec![format!("receipt.invalid: {e}")])?;
    let violations = receipt.command_violations(command);
    if violations.is_empty() {
        Ok(receipt)
    } else {
        Err(violations.iter().map(|v| v.to_string()).collect())
    }
}

// ---------------------------------------------------------------------
// Remote handoff manifest — `taisan.remote.handoff/1` (contract §6,
// docs/handoff-manifest-custody.md)
// ---------------------------------------------------------------------

/// One artifact descriptor in the handoff manifest: the workspace
/// path, its byte count in the produced commit, and the Worker's
/// claimed digest — a claim only; the coordinator derives the expected
/// digest from the verified produced commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffArtifact {
    /// UTF-8 slash-separated relative path.
    pub path: String,
    pub bytes: u64,
    /// 64 lowercase hex — the Worker's claim, never authority.
    pub claimed_sha256: String,
}

/// A remote handoff manifest's repository identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRepository {
    pub host: String,
    pub repository: String,
}

/// `taisan.remote.handoff/1`: the per-Invocation transfer metadata a
/// producing Worker writes beside `report.json`. Exactly
/// `format_version`, `request_id`, `invocation_id`, `repository`,
/// `base_commit`, `produced_commit`, and `artifacts`; `artifacts` is
/// sorted by UTF-8 path bytes and each entry carries exactly `path`,
/// `bytes`, `claimed_sha256`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHandoffV1 {
    pub format_version: String,
    pub request_id: String,
    pub invocation_id: String,
    pub repository: HandoffRepository,
    /// 40 lowercase hex.
    pub base_commit: String,
    /// 40 lowercase hex.
    pub produced_commit: String,
    pub artifacts: Vec<HandoffArtifact>,
}

impl RemoteHandoffV1 {
    /// The manifest digest: SHA-256 over the handoff tag and the
    /// canonical JSON of the whole manifest (contract §6 — the
    /// manifest carries no digest member of its own).
    pub fn digest(&self) -> Result<String, EnvelopeError> {
        let canonical =
            executor_canonical_json(&serde_json::to_value(self).expect("manifest serializes"))?;
        Ok(tagged_sha256(HANDOFF_DIGEST_TAG, &canonical))
    }

    /// Whether a path is a legal manifest path: UTF-8 slash-separated
    /// relative, non-empty, no absolute form, backslash, NUL, empty
    /// component, or `.`/`..` component (contract §6).
    pub fn path_is_valid(path: &str) -> bool {
        if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
            return false;
        }
        path.split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
    }

    /// Parse a manifest body: bound first, strict JSON, exact format,
    /// closed member sets, then the contract's ordering and identity
    /// rules. `max_item_count` is the bound the Invocation's immutable
    /// limits carry.
    pub fn parse(raw: &[u8], max_item_count: u64) -> Result<Self, EnvelopeError> {
        check_bound(raw, MAX_HANDOFF_BYTES)?;
        let value = strict_json(raw)?;
        let declared = declared_format(&value);
        if declared != REMOTE_HANDOFF_V1_FORMAT {
            return Err(EnvelopeError::VersionUnknown { declared });
        }
        let manifest: Self =
            serde_json::from_value(value).map_err(|e| EnvelopeError::Malformed {
                detail: format!("handoff manifest malformed: {e}"),
            })?;
        manifest.validate(max_item_count)?;
        Ok(manifest)
    }

    /// The internal contract rules: commit/claim hex shapes, entry
    /// count, path grammar, and the sorted-by-path requirement — the
    /// descriptor set is refused, never normalized into order.
    pub fn validate(&self, max_item_count: u64) -> Result<(), EnvelopeError> {
        let invalid = |detail: String| EnvelopeError::Invalid {
            reason: "manifest.invalid",
            detail,
        };
        if !is_hex40(&self.base_commit) || !is_hex40(&self.produced_commit) {
            return Err(invalid("commit identity is not 40 lowercase hex".into()));
        }
        if self.artifacts.len() as u64 > max_item_count {
            return Err(invalid(format!(
                "{} artifacts exceed the bound {max_item_count}",
                self.artifacts.len()
            )));
        }
        for (i, artifact) in self.artifacts.iter().enumerate() {
            if !Self::path_is_valid(&artifact.path) {
                return Err(invalid(format!("unsafe manifest path {:?}", artifact.path)));
            }
            if !is_hex64(&artifact.claimed_sha256) {
                return Err(invalid(format!(
                    "claimed_sha256 for {:?} is not 64 lowercase hex",
                    artifact.path
                )));
            }
            if i > 0 && self.artifacts[i - 1].path.as_bytes() >= artifact.path.as_bytes() {
                let detail = if self.artifacts[i - 1].path == artifact.path {
                    format!("duplicate manifest path {:?}", artifact.path)
                } else {
                    "manifest artifacts are not sorted by path bytes".into()
                };
                return Err(invalid(detail));
            }
        }
        Ok(())
    }

    /// Violations of the request identity the manifest claims to serve:
    /// a foreign or stale `request_id`/`invocation_id` is rejected —
    /// never `Present` evidence (contract §6).
    pub fn identity_violations(&self, request_id: &str, invocation_id: &str) -> Vec<&'static str> {
        let mut violations = Vec::new();
        if self.request_id != request_id {
            violations.push("manifest.wrong_request");
        }
        if self.invocation_id != invocation_id {
            violations.push("manifest.wrong_invocation");
        }
        violations
    }
}

/// One collection item for the collection digest (contract §6): the
/// item identity, byte count, content digest, and source object
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionEntry {
    pub item_id: String,
    pub bytes: u64,
    /// 64 lowercase hex content digest.
    pub content_digest: String,
    /// Source object identity (e.g. the produced-commit object or CAS
    /// body the item came from).
    pub source: String,
}

/// The collection digest (contract §6): SHA-256 over the collection
/// tag and the canonical JSON of the entries sorted by `item_id` UTF-8
/// bytes. Covers the report, log, `handoff.json`, object closure, and
/// supervisor-issued verification evidence — never frame boundaries or
/// staging paths.
pub fn collection_digest(entries: &[CollectionEntry]) -> Result<String, EnvelopeError> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.item_id.as_bytes().cmp(b.item_id.as_bytes()));
    let canonical =
        executor_canonical_json(&serde_json::to_value(sorted).expect("collection serializes"))?;
    Ok(tagged_sha256(COLLECTION_DIGEST_TAG, &canonical))
}

// ---------------------------------------------------------------------
// Wire framing (contract §4): one fixed-width unsigned 32-bit
// big-endian length before each body. The receiver checks the length
// against the class bound before allocating or parsing the body.
// ---------------------------------------------------------------------

/// Frame a bounded body for the transport. The send side applies the
/// same bound the receiver enforces: a body over the class bound — or
/// wider than the 32-bit length prefix can declare — is refused rather
/// than emitted as a frame whose header disagrees with its body.
pub fn frame_body(body: &[u8], limit: u64) -> Result<Vec<u8>, EnvelopeError> {
    let limit = limit.min(u64::from(u32::MAX));
    let bytes = body.len() as u64;
    if bytes > limit {
        return Err(EnvelopeError::Oversize { limit, bytes });
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Read a frame header's declared length and gate it against the
/// class bound — the check runs before any body allocation. A declared
/// length past the bound is refused without reading the body.
pub fn frame_len(header: &[u8; 4], limit: u64) -> Result<u64, EnvelopeError> {
    let declared = u64::from(u32::from_be_bytes(*header));
    if declared > limit {
        return Err(EnvelopeError::Oversize {
            limit,
            bytes: declared,
        });
    }
    Ok(declared)
}

// ---------------------------------------------------------------------
// Durable coordinator-side records (contract §2, §4): the immutable
// binding and the operation ledger the persist-before-send and replay
// rules require. These never cross the wire.
// ---------------------------------------------------------------------

/// The immutable per-request transfer limits the binding records
/// (contract §2, §8): every value finite at bind time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorLimits {
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
    pub max_item_count: u64,
    pub operation_timeout_ms: u64,
}

/// The complete immutable first binding for one remote request
/// (contract §2): endpoint identity and incarnation, selected protocol
/// version, capability-set digest, launch-configuration digest,
/// request digest, and transfer limits. Serialized into the durable
/// `binding_body`; changing any field is a different binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBindingFields {
    pub installation_id: String,
    pub endpoint_id: String,
    pub endpoint_identity_digest: String,
    pub bound_incarnation_id: String,
    pub protocol_version: String,
    pub capabilities_digest: String,
    pub launch_config_digest: String,
    pub request_id: String,
    pub invocation_id: String,
    pub request_digest: String,
    pub limits: ExecutorLimits,
}

impl ExecutorBindingFields {
    /// The finite-limits rule (contract §8): a missing, zero, or
    /// unsupported required limit refuses binding.
    pub fn validates(&self) -> Result<(), EnvelopeError> {
        let invalid = |detail: String| EnvelopeError::Invalid {
            reason: "binding.invalid",
            detail,
        };
        for (name, value) in [
            ("max_input_bytes", self.limits.max_input_bytes),
            ("max_output_bytes", self.limits.max_output_bytes),
            ("max_item_count", self.limits.max_item_count),
            ("operation_timeout_ms", self.limits.operation_timeout_ms),
        ] {
            if value == 0 {
                return Err(invalid(format!("{name} must be finite and nonzero")));
            }
        }
        if !self
            .protocol_version
            .strip_prefix("taisan.executor/")
            .is_some_and(|n| n.parse::<u64>().is_ok_and(|v| v >= 1))
        {
            return Err(invalid(format!(
                "protocol_version {:?} is not a taisan.executor/<N> version",
                self.protocol_version
            )));
        }
        Ok(())
    }
}

/// The durable binding row: the canonical body plus its self-check
/// digest, keyed by `request_id`. Written before the first remote
/// operation is sent; a second insert with identical content is a
/// replay and with different content a conflict — a bound request is
/// never silently migrated to another endpoint, incarnation, payload,
/// or launch configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorBinding {
    pub request_id: String,
    /// The canonical JSON of [`ExecutorBindingFields`] — the verbatim
    /// record replayed across restarts.
    pub binding_body: Vec<u8>,
    /// `BINDING_DIGEST_TAG` over `binding_body`, re-checked on every
    /// read so a tampered row reads as corrupt.
    pub binding_digest: String,
}

impl ExecutorBinding {
    /// Seal the complete binding fields into the durable row.
    pub fn new(fields: ExecutorBindingFields) -> Result<Self, EnvelopeError> {
        fields.validates()?;
        let body = executor_canonical_json(
            &serde_json::to_value(&fields).expect("binding fields serialize"),
        )?;
        Ok(Self {
            request_id: fields.request_id,
            binding_digest: tagged_sha256(BINDING_DIGEST_TAG, &body),
            binding_body: body,
        })
    }

    /// The recorded fields, re-checked against the stored digest and
    /// the row key — `Err` marks a corrupt row the caller must refuse
    /// to send under, never a silently absent binding.
    pub fn fields(&self) -> Result<ExecutorBindingFields, EnvelopeError> {
        if tagged_sha256(BINDING_DIGEST_TAG, &self.binding_body) != self.binding_digest {
            return Err(EnvelopeError::DigestMismatch {
                member: "binding_digest",
            });
        }
        let fields: ExecutorBindingFields =
            serde_json::from_slice(&self.binding_body).map_err(|e| EnvelopeError::Malformed {
                detail: format!("binding body malformed: {e}"),
            })?;
        if fields.request_id != self.request_id {
            return Err(EnvelopeError::Invalid {
                reason: "binding.invalid",
                detail: "binding body names a different request_id".into(),
            });
        }
        Ok(fields)
    }

    /// Read-side integrity: digest and identity re-checked together.
    pub fn verifies(&self) -> bool {
        self.fields().is_ok()
    }
}

/// One durable operation record: the full command identity and its
/// canonical bytes committed *before* the operation is sent, plus the
/// receipt when one is recorded. The record keyed by
/// (`request_id`, `operation_id`) is the replay fence: identical bytes
/// replay idempotently, different bytes conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorOperation {
    pub request_id: String,
    pub operation_id: String,
    pub operation_kind: OperationKind,
    /// The canonical command bytes — digest member included — exactly
    /// as `ExecutorCommandV1::to_canonical_bytes` produced them.
    pub command_body: Vec<u8>,
    pub command_digest: String,
    /// The canonical receipt bytes and digest, once recorded.
    pub receipt_body: Option<Vec<u8>>,
    pub receipt_digest: Option<String>,
}

impl ExecutorOperation {
    /// Seal a parsed command into its durable record.
    pub fn new(command: &ExecutorCommandV1) -> Result<Self, EnvelopeError> {
        Ok(Self {
            request_id: command.request_id.clone(),
            operation_id: command.operation_id.clone(),
            operation_kind: command.operation_kind,
            command_body: command.to_canonical_bytes()?,
            command_digest: command.command_digest.clone(),
            receipt_body: None,
            receipt_digest: None,
        })
    }

    /// Read-side integrity: the stored bytes must parse as a complete
    /// v1 command whose embedded identity and digest agree with the
    /// row's own fields — a corrupt, incomplete, or foreign row is
    /// never replayed to an endpoint.
    pub fn verifies(&self) -> bool {
        let Ok(command) = ExecutorCommandV1::parse(&self.command_body) else {
            return false;
        };
        if command.request_id != self.request_id
            || command.operation_id != self.operation_id
            || command.operation_kind != self.operation_kind
            || command.command_digest != self.command_digest
        {
            return false;
        }
        match (&self.receipt_body, &self.receipt_digest) {
            (None, None) => true,
            (Some(body), Some(digest)) => verify_receipt(&command, body)
                .map(|receipt| &receipt.receipt_digest == digest)
                .unwrap_or(false),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ExecutorIdentity {
        ExecutorIdentity {
            installation_id: "install-a".into(),
            endpoint_id: "executor-a".into(),
            endpoint_identity_digest: "a".repeat(64),
            bound_incarnation_id: "boot-7f1c".into(),
            request_id: "e-7:inv:2".into(),
            invocation_id: "inv-41".into(),
            request_digest: "b".repeat(64),
        }
    }

    fn command() -> ExecutorCommandV1 {
        ExecutorCommandV1::new(
            identity(),
            "op-4f2a",
            OperationKind::Start,
            serde_json::json!({
                "prepared_id": "prep-41",
                "launch_config_digest": "c".repeat(64),
            }),
        )
        .expect("command seals")
    }

    fn receipt_for(command: &ExecutorCommandV1, result: serde_json::Value) -> ExecutorReceiptV1 {
        ExecutorReceiptV1::new(
            identity(),
            "boot-7f1c",
            command.operation_id.clone(),
            command.operation_kind,
            command.payload_digest.clone(),
            command.command_digest.clone(),
            result,
        )
        .expect("receipt seals")
    }

    // -- canonicalization ------------------------------------------

    #[test]
    fn canonical_json_uses_contract_escaping() {
        // Contract strings escape control characters as \u00xx — never
        // the \n/\t shorthand serde_json emits, which is why the
        // executor digest cannot reuse domain::canonical_json.
        let canonical = executor_canonical_json(&serde_json::json!({
            "key": "line one\nline two\t\"quoted\"\u{7}"
        }))
        .unwrap();
        assert_eq!(
            String::from_utf8(canonical).unwrap(),
            "{\"key\":\"line one\\u000aline two\\u0009\\\"quoted\\\"\\u0007\"}"
        );
    }

    #[test]
    fn canonical_json_sorts_keys_and_keeps_raw_unicode() {
        let canonical = executor_canonical_json(&serde_json::json!({
            "b": 2, "a": "日本語", "c": [true, null]
        }))
        .unwrap();
        assert_eq!(
            String::from_utf8(canonical).unwrap(),
            "{\"a\":\"日本語\",\"b\":2,\"c\":[true,null]}"
        );
    }

    #[test]
    fn strict_json_refuses_duplicates_floats_and_wide_ints() {
        assert!(strict_json(b"{\"a\":1,\"a\":2}").is_err());
        assert!(strict_json(b"{\"a\":1.5}").is_err());
        assert!(strict_json(b"{\"a\":1e3}").is_err());
        assert!(strict_json(b"{\"a\":9223372036854775807}").is_ok());
        assert!(strict_json(b"{\"a\":9223372036854775808}").is_err());
        assert!(strict_json(b"{\"a\":-9223372036854775808}").is_ok());
        // Nested values are strict too.
        assert!(strict_json(b"{\"a\":{\"b\":1,\"b\":2}}").is_err());
        assert!(strict_json(b"[{\"x\":0.1}]").is_err());
        // Trailing content is refused.
        assert!(strict_json(b"{} extra").is_err());
    }

    // -- commands ---------------------------------------------------

    #[test]
    fn a_valid_command_round_trips_and_verifies() {
        let command = command();
        let bytes = command.to_canonical_bytes().unwrap();
        let parsed = ExecutorCommandV1::parse(&bytes).expect("valid command parses");
        assert_eq!(parsed, command);
        assert_eq!(parsed.digest().unwrap(), parsed.command_digest);
    }

    #[test]
    fn malformed_oversize_and_foreign_commands_are_refused() {
        assert!(matches!(
            ExecutorCommandV1::parse(b"{not json"),
            Err(EnvelopeError::Malformed { .. })
        ));
        let mut foreign = command();
        foreign.format_version = "taisan.executor.command/9".into();
        foreign.command_digest = foreign.digest().unwrap();
        let bytes = executor_canonical_json(&serde_json::to_value(&foreign).unwrap()).unwrap();
        assert!(matches!(
            ExecutorCommandV1::parse(&bytes),
            Err(EnvelopeError::VersionUnknown { .. })
        ));
        // A body over the 1 MiB command bound is refused before parse.
        let mut big = serde_json::to_value(command()).unwrap();
        big["payload"] = serde_json::Value::String("x".repeat(1 << 20));
        let bytes = executor_canonical_json(&big).unwrap();
        assert!(matches!(
            ExecutorCommandV1::parse(&bytes),
            Err(EnvelopeError::Oversize { .. })
        ));
    }

    #[test]
    fn command_refusals_cover_fields_digests_and_key_rules() {
        let bytes = command().to_canonical_bytes().unwrap();
        let base: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Unknown member.
        let mut v = base.clone();
        v["operator_hint"] = serde_json::json!("skip");
        assert!(matches!(
            ExecutorCommandV1::parse(&executor_canonical_json(&v).unwrap()),
            Err(EnvelopeError::Malformed { .. })
        ));
        // Duplicate key — craft raw bytes; a Value cannot hold one.
        let dup = br#"{"format_version":"taisan.executor.command/1","format_version":"taisan.executor.command/1"}"#;
        assert!(matches!(
            ExecutorCommandV1::parse(dup),
            Err(EnvelopeError::Malformed { .. })
        ));
        // Float in payload.
        let mut v = base.clone();
        v["payload"]["ratio"] = serde_json::json!(0.5);
        assert!(matches!(
            ExecutorCommandV1::parse(serde_json::to_string(&v).unwrap().as_bytes()),
            Err(EnvelopeError::Malformed { .. })
        ));
        // Tampered payload without resealing fails payload_digest.
        let mut v = base.clone();
        v["payload"]["prepared_id"] = serde_json::json!("prep-99");
        assert!(matches!(
            ExecutorCommandV1::parse(&executor_canonical_json(&v).unwrap()),
            Err(EnvelopeError::DigestMismatch {
                member: "payload_digest"
            })
        ));
        // Resealed payload but stale command_digest fails command_digest.
        let mut v = base.clone();
        v["payload"]["prepared_id"] = serde_json::json!("prep-99");
        v["payload_digest"] = serde_json::json!(payload_digest(&v["payload"]).unwrap());
        assert!(matches!(
            ExecutorCommandV1::parse(&executor_canonical_json(&v).unwrap()),
            Err(EnvelopeError::DigestMismatch {
                member: "command_digest"
            })
        ));
        // An unknown operation kind is malformed.
        let mut v = base.clone();
        v["operation_kind"] = serde_json::json!("detonate");
        v["command_digest"] = serde_json::json!(tagged_sha256(
            COMMAND_DIGEST_TAG,
            &canonical_minus(&v, "command_digest").unwrap()
        ));
        assert!(matches!(
            ExecutorCommandV1::parse(&executor_canonical_json(&v).unwrap()),
            Err(EnvelopeError::Malformed { .. })
        ));
    }

    // -- receipts ---------------------------------------------------

    #[test]
    fn a_valid_receipt_round_trips_and_verifies_against_its_command() {
        let command = command();
        let receipt = receipt_for(
            &command,
            serde_json::json!({
                "status": "started",
                "process": {
                    "evidence_ref": "proc-evidence-41",
                    "identity_digest": "d".repeat(64),
                }
            }),
        );
        let bytes = receipt.to_canonical_bytes().unwrap();
        let parsed = verify_receipt(&command, &bytes).expect("receipt verifies");
        assert_eq!(parsed.receipt_digest, parsed.digest().unwrap());
    }

    #[test]
    fn receipt_refusals_cover_vocabulary_and_echo() {
        let command = command();
        let receipt = receipt_for(
            &command,
            serde_json::json!({
                "status": "started",
                "process": {"evidence_ref": "p-1", "identity_digest": "d".repeat(64)},
            }),
        );
        let base: serde_json::Value =
            serde_json::from_slice(&receipt.to_canonical_bytes().unwrap()).unwrap();
        let reseal = |v: &serde_json::Value| -> Vec<u8> {
            let mut v = v.clone();
            v["result_digest"] = serde_json::json!(result_digest(&v["result"]).unwrap());
            v["receipt_digest"] = serde_json::json!(tagged_sha256(
                RECEIPT_DIGEST_TAG,
                &canonical_minus(&v, "receipt_digest").unwrap()
            ));
            executor_canonical_json(&v).unwrap()
        };
        // Foreign version.
        let mut v = base.clone();
        v["format_version"] = serde_json::json!("taisan.executor.receipt/7");
        assert!(matches!(
            ExecutorReceiptV1::parse(&executor_canonical_json(&v).unwrap()),
            Err(EnvelopeError::VersionUnknown { .. })
        ));
        // Status outside the operation's closed set.
        let mut v = base.clone();
        v["result"]["status"] = serde_json::json!("maybe");
        assert!(matches!(
            ExecutorReceiptV1::parse(&reseal(&v)),
            Err(EnvelopeError::Invalid { .. })
        ));
        // A status valid for another operation but not `start`.
        let mut v = base.clone();
        v["result"]["status"] = serde_json::json!("prepared");
        v["result"]["prepared_id"] = serde_json::json!("prep-41");
        assert!(ExecutorReceiptV1::parse(&reseal(&v)).is_err());
        // Reason outside the refusal vocabulary.
        let mut v = base.clone();
        v["result"]["reason"] = serde_json::json!("it broke");
        assert!(ExecutorReceiptV1::parse(&reseal(&v)).is_err());
        // Echo violations: stale incarnation, foreign request, wrong digest.
        for (member, value) in [
            ("bound_incarnation_id", serde_json::json!("boot-dead")),
            ("request_id", serde_json::json!("e-8:inv:1")),
            ("command_digest", serde_json::json!("f".repeat(64))),
        ] {
            let mut v = base.clone();
            v[member] = value;
            let err = verify_receipt(&command, &reseal(&v)).expect_err("echo violation refuses");
            assert!(!err.is_empty());
        }
        // Oversize is refused before parse.
        let big = vec![b' '; (MAX_RECEIPT_BYTES + 1) as usize];
        assert!(matches!(
            ExecutorReceiptV1::parse(&big),
            Err(EnvelopeError::Oversize { .. })
        ));
    }

    #[test]
    fn result_shapes_the_contract_names_are_enforced() {
        let check = |kind: OperationKind, result: serde_json::Value| -> Result<(), EnvelopeError> {
            validate_result(kind, &result)
        };
        // `unknown` requires a refusal-coded reason.
        check(
            OperationKind::Start,
            serde_json::json!({"status": "unknown", "reason": "process_identity_unavailable"}),
        )
        .unwrap();
        assert!(
            check(
                OperationKind::Start,
                serde_json::json!({"status": "unknown"})
            )
            .is_err()
        );
        // `running`/`exited`/`stopped` require their evidence members.
        assert!(
            check(
                OperationKind::Observe,
                serde_json::json!({"status": "running", "sequence": 3})
            )
            .is_err()
        );
        check(
            OperationKind::Observe,
            serde_json::json!({
                "status": "running",
                "process": {"evidence_ref": "p", "identity_digest": "d".repeat(64)},
                "sequence": 3,
            }),
        )
        .unwrap();
        assert!(
            check(
                OperationKind::RequestStop,
                serde_json::json!({"status": "stopped", "sequence": 4})
            )
            .is_err()
        );
        check(
            OperationKind::RequestStop,
            serde_json::json!({
                "status": "stopped",
                "proof": {"receipt_digest": "e".repeat(64)},
                "sequence": 4,
            }),
        )
        .unwrap();
        // `pending` names the resumable transfer.
        check(
            OperationKind::Collect,
            serde_json::json!({"status": "pending", "transfer_id": "tr-1", "next_offset": 4096}),
        )
        .unwrap();
        assert!(
            check(
                OperationKind::Collect,
                serde_json::json!({"status": "pending"})
            )
            .is_err()
        );
        // `collected` binds the collection identity and digest.
        check(
            OperationKind::Collect,
            serde_json::json!({
                "status": "collected",
                "collection_id": "col-1",
                "collection_digest": "e".repeat(64),
            }),
        )
        .unwrap();
        assert!(
            check(
                OperationKind::Collect,
                serde_json::json!({"status": "collected"})
            )
            .is_err()
        );
        // Bounded diagnostics; a flood is refused.
        assert!(
            check(
                OperationKind::Start,
                serde_json::json!({
                    "status": "refused",
                    "reason": "capability_missing",
                    "diagnostic": "x".repeat(MAX_DIAGNOSTIC_CHARS + 1),
                })
            )
            .is_err()
        );
    }

    #[test]
    fn process_identity_digest_must_be_64_lowercase_hex() {
        // Every result variant carrying process evidence rejects a
        // malformed identity_digest — the same hex rule the other
        // contract digests are held to.
        for (kind, result) in [
            (
                OperationKind::Start,
                serde_json::json!({
                    "status": "started",
                    "process": {"evidence_ref": "p", "identity_digest": "not-hex"},
                }),
            ),
            (
                OperationKind::Observe,
                serde_json::json!({
                    "status": "running",
                    "process": {"evidence_ref": "p", "identity_digest": "D".repeat(64)},
                    "sequence": 3,
                }),
            ),
            (
                OperationKind::Observe,
                serde_json::json!({
                    "status": "exited",
                    "process": {"evidence_ref": "p", "identity_digest": "d".repeat(63)},
                    "sequence": 4,
                }),
            ),
        ] {
            assert!(
                validate_result(kind, &result).is_err(),
                "{kind:?} accepted a malformed identity_digest"
            );
        }
    }

    // -- handshake --------------------------------------------------

    #[test]
    fn hello_and_reply_round_trip_and_bind() {
        let hello = ExecutorHelloV1::new(
            "install-a",
            "executor-a",
            "0".repeat(32),
            vec![EXECUTOR_PROTOCOL_V1.into()],
        );
        let parsed_hello = ExecutorHelloV1::parse(&hello.to_canonical_bytes()).unwrap();
        assert_eq!(parsed_hello, hello);

        let capabilities = vec![
            "bounded_transfer".to_string(),
            "process_control".to_string(),
            "workspace_read_only".to_string(),
        ];
        let reply = ExecutorHelloReplyV1 {
            format_version: EXECUTOR_HELLO_REPLY_V1_FORMAT.into(),
            installation_id: "install-a".into(),
            endpoint_id: "executor-a".into(),
            coordinator_nonce: "0".repeat(32),
            endpoint_identity_digest: "a".repeat(64),
            responder_incarnation_id: "boot-7f1c".into(),
            supported_protocols: vec![EXECUTOR_PROTOCOL_V1.into()],
            capabilities_digest: ExecutorHelloReplyV1::capabilities_digest_of(&capabilities)
                .unwrap(),
            capabilities,
            clock: ClockMeasurement {
                offset_ms: 2,
                uncertainty_ms: 4,
                round_trip_ms: 8,
                measured_at_unix_ms: 1_800_000_000_000,
            },
        };
        let parsed = ExecutorHelloReplyV1::parse(&reply.to_canonical_bytes()).unwrap();
        assert_eq!(parsed, reply);
        assert!(
            parsed
                .binding_violations(&hello, &"a".repeat(64))
                .is_empty()
        );
        // A replayed nonce or foreign identity is caught.
        let mut stale = parsed.clone();
        stale.coordinator_nonce = "f".repeat(32);
        assert!(
            stale
                .binding_violations(&hello, &"a".repeat(64))
                .contains(&"reply.wrong_nonce")
        );
        assert!(
            parsed
                .binding_violations(&hello, &"b".repeat(64))
                .contains(&"reply.identity_mismatch")
        );
        // A tampered capability digest is refused at parse.
        let mut tampered: serde_json::Value =
            serde_json::from_slice(&reply.to_canonical_bytes()).unwrap();
        tampered["capabilities"] = serde_json::json!(["bounded_transfer"]);
        assert!(matches!(
            ExecutorHelloReplyV1::parse(&executor_canonical_json(&tampered).unwrap()),
            Err(EnvelopeError::DigestMismatch { .. })
        ));
        // The 64 KiB handshake bound is enforced before parse.
        assert!(matches!(
            ExecutorHelloV1::parse(&vec![b' '; (MAX_HANDSHAKE_BYTES + 1) as usize]),
            Err(EnvelopeError::Oversize { .. })
        ));
    }

    #[test]
    fn protocol_selection_picks_the_highest_mutual_version() {
        let allowed = vec![
            "taisan.executor/1".to_string(),
            "taisan.executor/2".to_string(),
        ];
        let endpoint = vec![
            "taisan.executor/1".to_string(),
            "taisan.executor/2".to_string(),
            "taisan.executor/9".to_string(),
            "other.proto/3".to_string(),
        ];
        assert_eq!(
            select_protocol_version(&endpoint, &allowed).as_deref(),
            Some("taisan.executor/2")
        );
        // No mutual version refuses — never a downgrade guess.
        assert!(select_protocol_version(&["taisan.executor/3".into()], &allowed).is_none());
        assert!(select_protocol_version(&[], &allowed).is_none());
    }

    // -- handoff manifest --------------------------------------------

    fn manifest() -> RemoteHandoffV1 {
        RemoteHandoffV1 {
            format_version: REMOTE_HANDOFF_V1_FORMAT.into(),
            request_id: "e-7:inv:2".into(),
            invocation_id: "inv-41".into(),
            repository: HandoffRepository {
                host: "host-1".into(),
                repository: "repo-a".into(),
            },
            base_commit: "0".repeat(40),
            produced_commit: "1".repeat(40),
            artifacts: vec![
                HandoffArtifact {
                    path: "dist/result.json".into(),
                    bytes: 128,
                    claimed_sha256: "2".repeat(64),
                },
                HandoffArtifact {
                    path: "docs/report.md".into(),
                    bytes: 64,
                    claimed_sha256: "3".repeat(64),
                },
            ],
        }
    }

    #[test]
    fn a_valid_manifest_round_trips_and_digests() {
        let manifest = manifest();
        let bytes = executor_canonical_json(&serde_json::to_value(&manifest).unwrap()).unwrap();
        let parsed = RemoteHandoffV1::parse(&bytes, 8).expect("valid manifest parses");
        assert_eq!(parsed, manifest);
        assert!(is_hex64(&parsed.digest().unwrap()));
        assert!(parsed.identity_violations("e-7:inv:2", "inv-41").is_empty());
        // Foreign or stale identity is flagged, never evidence.
        assert_eq!(
            parsed.identity_violations("e-9:inv:9", "inv-41"),
            vec!["manifest.wrong_request"]
        );
        assert_eq!(
            parsed.identity_violations("e-7:inv:2", "inv-99"),
            vec!["manifest.wrong_invocation"]
        );
    }

    #[test]
    fn manifest_refusals_cover_paths_order_bounds_and_members() {
        let manifest = manifest();
        let base = serde_json::to_value(&manifest).unwrap();
        let parse =
            |v: &serde_json::Value| RemoteHandoffV1::parse(&executor_canonical_json(v).unwrap(), 8);
        // Unknown envelope member.
        let mut v = base.clone();
        v["notes"] = serde_json::json!("extra");
        assert!(matches!(parse(&v), Err(EnvelopeError::Malformed { .. })));
        // Unknown descriptor member.
        let mut v = base.clone();
        v["artifacts"][0]["comment"] = serde_json::json!("hi");
        assert!(matches!(parse(&v), Err(EnvelopeError::Malformed { .. })));
        // Unsafe paths — each form refused.
        for bad in [
            "",
            "/abs/path",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "a/\u{0}/b",
            "..",
            ".",
        ] {
            let mut v = base.clone();
            v["artifacts"][0]["path"] = serde_json::json!(bad);
            assert!(parse(&v).is_err(), "path {bad:?} must be refused");
        }
        // Unsorted and duplicate paths refused.
        let mut v = base.clone();
        let mut arts = v["artifacts"].as_array().unwrap().clone();
        arts.reverse();
        v["artifacts"] = serde_json::json!(arts);
        assert!(parse(&v).is_err());
        let mut v = base.clone();
        let duplicate = v["artifacts"][0].clone();
        v["artifacts"].as_array_mut().unwrap().push(duplicate);
        assert!(parse(&v).is_err());
        // Entry count bound.
        assert!(RemoteHandoffV1::parse(&executor_canonical_json(&base).unwrap(), 1).is_err());
        // Bad hex shapes.
        let mut v = base.clone();
        v["produced_commit"] = serde_json::json!("zzz");
        assert!(parse(&v).is_err());
        let mut v = base.clone();
        v["artifacts"][0]["claimed_sha256"] = serde_json::json!("A".repeat(64));
        assert!(parse(&v).is_err());
        // Oversize refused before parse.
        assert!(matches!(
            RemoteHandoffV1::parse(&vec![b' '; (MAX_HANDOFF_BYTES + 1) as usize], 8),
            Err(EnvelopeError::Oversize { .. })
        ));
    }

    // -- durable records ----------------------------------------------

    fn binding_fields() -> ExecutorBindingFields {
        ExecutorBindingFields {
            installation_id: "install-a".into(),
            endpoint_id: "executor-a".into(),
            endpoint_identity_digest: "a".repeat(64),
            bound_incarnation_id: "boot-7f1c".into(),
            protocol_version: EXECUTOR_PROTOCOL_V1.into(),
            capabilities_digest: "e".repeat(64),
            launch_config_digest: "c".repeat(64),
            request_id: "e-7:inv:2".into(),
            invocation_id: "inv-41".into(),
            request_digest: "b".repeat(64),
            limits: ExecutorLimits {
                max_input_bytes: 4 * 1024 * 1024,
                max_output_bytes: 4 * 1024 * 1024,
                max_item_count: 64,
                operation_timeout_ms: 3_600_000,
            },
        }
    }

    #[test]
    fn the_binding_round_trips_and_rejects_corruption() {
        let binding = ExecutorBinding::new(binding_fields()).unwrap();
        assert!(binding.verifies());
        assert_eq!(binding.fields().unwrap(), binding_fields());
        let mut tampered = binding.clone();
        tampered.binding_body = b"{}".to_vec();
        assert!(!tampered.verifies());
        let mut tampered = binding.clone();
        tampered.binding_digest = "f".repeat(64);
        assert!(!tampered.verifies());
        // Zero or missing required limits refuse at seal time.
        let mut fields = binding_fields();
        fields.limits.max_output_bytes = 0;
        assert!(ExecutorBinding::new(fields).is_err());
        let mut fields = binding_fields();
        fields.protocol_version = "other.proto/1".into();
        assert!(ExecutorBinding::new(fields).is_err());
        // The command's binding check flags foreign and stale identity.
        let command = command();
        assert!(command.binding_violations(&binding_fields()).is_empty());
        let mut stale = binding_fields();
        stale.bound_incarnation_id = "boot-dead".into();
        assert!(
            command
                .binding_violations(&stale)
                .contains(&"command.stale_incarnation")
        );
        let mut foreign = binding_fields();
        foreign.endpoint_identity_digest = "9".repeat(64);
        assert!(
            command
                .binding_violations(&foreign)
                .contains(&"command.wrong_endpoint_identity")
        );
    }

    #[test]
    fn the_operation_record_verifies_command_and_receipt() {
        let command = command();
        let receipt = receipt_for(
            &command,
            serde_json::json!({
                "status": "started",
                "process": {"evidence_ref": "p", "identity_digest": "d".repeat(64)},
            }),
        );
        let mut operation = ExecutorOperation::new(&command).unwrap();
        assert!(operation.verifies());
        operation.receipt_body = Some(receipt.to_canonical_bytes().unwrap());
        operation.receipt_digest = Some(receipt.receipt_digest.clone());
        assert!(operation.verifies());
        // A foreign receipt or tampered command reads corrupt.
        operation.receipt_digest = Some("f".repeat(64));
        assert!(!operation.verifies());
        let mut operation = ExecutorOperation::new(&command).unwrap();
        operation.command_body = b"{}".to_vec();
        assert!(!operation.verifies());
    }

    #[test]
    fn framing_bounds_length_before_body() {
        let body = b"{}";
        let frame = frame_body(body, 1024).unwrap();
        assert_eq!(&frame[..4], &[0, 0, 0, 2]);
        assert_eq!(frame_len(frame[..4].try_into().unwrap(), 1024).unwrap(), 2);
        // A declared length over the class bound refuses before any
        // body is allocated or parsed.
        let header = (u32::MAX).to_be_bytes();
        assert!(matches!(
            frame_len(&header, MAX_DATA_FRAME_BYTES),
            Err(EnvelopeError::Oversize { .. })
        ));
        // The send side refuses the same way: a body over the class
        // bound is never emitted as a frame — a small bound exercises
        // the check without a real 4 GiB buffer.
        assert!(matches!(
            frame_body(body, 1),
            Err(EnvelopeError::Oversize { limit: 1, bytes: 2 })
        ));
    }
}
