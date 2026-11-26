//! The worker-neutral runtime binding contract a managed Worker
//! consumes at `/taisan/provider-config`
//! (`docs/runtime-bindings.md`, `docs/v0.3.md` §14.3).
//!
//! The contract document is a bounded JSON object authored by the
//! installation — model identity and provider endpoints, never a
//! credential. It is *versioned*: `format_version` must be exactly
//! [`BINDINGS_FORMAT_VERSION`], so a supervisor and a worker adapter
//! can never silently disagree on which contract the mount carries.
//! Members beyond the fixed set are the adapter-owned extension
//! space: Core does not name or interpret them, and neither do they
//! loosen the required bindings.
//!
//! Validation runs at the supervisor boundary before every dispatch
//! ([`crate::boundary`]): a missing, unreadable, oversized,
//! malformed, wrongly-versioned, or credential-bearing document is a
//! typed `provider_config.*` refusal with a bounded diagnostic — the
//! error names the rule that failed, never the document's content.
//! The contract is deliberately worker-neutral: no adapter, runtime,
//! or provider-specific key is required or interpreted here.

use std::path::Path;

use crate::credentials;

/// The only contract format version this supervisor mounts.
pub const BINDINGS_FORMAT_VERSION: &str = "taisan.worker.bindings/1";

/// Bound on the contract document's bytes — a provider configuration
/// is a settings file, not a payload. Larger input cannot be a valid
/// contract and is refused unread, never partially parsed.
pub const MAX_CONTRACT_BYTES: u64 = 64 * 1024;

/// Bound on each fixed member's string length — long enough for any
/// model identity or endpoint URL, short enough that a refusal stays
/// a bounded diagnostic surface.
const MAX_FIELD_CHARS: usize = 1024;

/// Validate the provider contract document at `path`. `Ok(())` means
/// the mount may proceed: a regular file, inside the size bound, a
/// JSON object carrying the exact `format_version` and the required
/// non-secret `model` member, and — when `credential_bytes` is
/// provided — free of the resolved credential value. Every `Err` is a
/// bounded `provider_config.*` refusal the caller records as a launch
/// repair; there is no tolerant parse.
///
/// Fixed members:
///
/// - `format_version` — required; exactly [`BINDINGS_FORMAT_VERSION`].
/// - `model` — required; a non-empty bounded string naming the model
///   the worker runs.
/// - `endpoint` — optional; a non-empty bounded string naming the
///   provider endpoint the runtime calls.
/// - `proxy_endpoint` — optional; a non-empty bounded string naming a
///   proxy endpoint the adapter may additionally honor. Worker-owned
///   override: the adapter may replace the supervisor-delivered
///   `HTTPS_PROXY` pair for the runtime child with this endpoint
///   (`docs/runtime-bindings.md`); under containment the override can
///   never widen egress past the supervisor's CONNECT allowlist.
pub fn validate(path: &Path, credential_bytes: Option<&[u8]>) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| format!("provider_config.unreadable: {e}"))?;
    if !metadata.is_file() {
        return Err(invalid(
            "not a regular file — the contract is exactly one document",
        ));
    }
    if metadata.len() > MAX_CONTRACT_BYTES {
        return Err(invalid("exceeds the contract size bound"));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("provider_config.unreadable: {e}"))?;
    // The metadata check and the read race: a file that grew between
    // them is still refused, never parsed past the bound.
    if bytes.len() as u64 > MAX_CONTRACT_BYTES {
        return Err(invalid("exceeds the contract size bound"));
    }
    let document = parse_contract(&bytes)?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("not a contract JSON object"))?;
    match object.get("format_version") {
        Some(serde_json::Value::String(version)) if version == BINDINGS_FORMAT_VERSION => {}
        _ => {
            return Err(format!(
                "provider_config.contract_version: format_version must be '{BINDINGS_FORMAT_VERSION}'"
            ));
        }
    }
    for (member, required) in [
        ("model", true),
        ("endpoint", false),
        ("proxy_endpoint", false),
    ] {
        validate_member(object, member, required)?;
    }
    // The resolved credential embedded anywhere in the document is a
    // configuration error, never a tolerated convenience: the
    // credential's only delivery path is the staged
    // `/taisan/runtime-auth` file. The check runs on the value as a
    // consumer reads it — adapters trim the staged file, so a `file:`
    // credential's trailing newline cannot slip the effective value
    // past the check — and on the decoded document, so a secret
    // spelled through JSON escapes still matches. An all-whitespace
    // credential trims to nothing deliverable: it cannot be embedded,
    // and matching its raw bytes would refuse ordinary indentation.
    if let Some(secret) = credential_bytes {
        let needle = effective_secret(secret);
        if !needle.is_empty()
            && (credentials::contains_secret(&bytes, needle)
                || document_contains(&document, needle))
        {
            return Err(
                "provider_config.contains_credential: the provider configuration embeds \
                 the resolved credential value"
                    .into(),
            );
        }
    }
    Ok(())
}

/// The credential as a consumer reads it: adapters trim the staged
/// value before use, so containment is checked against the trimmed
/// bytes — a `file:` credential's trailing newline would otherwise
/// let the effective value sit in the document unrefused. A non-UTF-8
/// value is checked raw (a consumer that cannot decode it cannot use
/// it); a value that trims to nothing is returned empty — the caller
/// skips the check because an undeliverable credential cannot be
/// embedded.
fn effective_secret(secret: &[u8]) -> &[u8] {
    match std::str::from_utf8(secret) {
        Ok(value) => value.trim().as_bytes(),
        Err(_) => secret,
    }
}

/// Whether the *decoded* document carries the credential: raw-byte
/// scanning misses a secret spelled through JSON escapes (`\uXXXX`,
/// `\n`) or inside a non-string leaf (`1.2e3` for `1200`), but the
/// adapter that parses the mounted document receives it. Member
/// names are document content too — the extension space is
/// adapter-owned and arbitrary.
fn document_contains(document: &serde_json::Value, needle: &[u8]) -> bool {
    match document {
        serde_json::Value::String(text) => credentials::contains_secret(text.as_bytes(), needle),
        serde_json::Value::Number(number) => {
            credentials::contains_secret(number.to_string().as_bytes(), needle)
        }
        serde_json::Value::Bool(flag) => {
            credentials::contains_secret(flag.to_string().as_bytes(), needle)
        }
        serde_json::Value::Array(items) => items.iter().any(|item| document_contains(item, needle)),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            credentials::contains_secret(key.as_bytes(), needle) || document_contains(value, needle)
        }),
        serde_json::Value::Null => false,
    }
}

/// One fixed member's shape rule: absent-or-null is allowed only for
/// optional members; a present member must be a non-empty bounded
/// string. The member name is part of the contract — the refusal may
/// name it; the value never enters the diagnostic.
fn validate_member(
    object: &serde_json::Map<String, serde_json::Value>,
    member: &'static str,
    required: bool,
) -> Result<(), String> {
    match object.get(member) {
        None | Some(serde_json::Value::Null) if required => {
            Err(invalid(format!("'{member}' is required")))
        }
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::String(value)) => {
            if value.is_empty() {
                return Err(invalid(format!("'{member}' must not be empty")));
            }
            if value.chars().count() > MAX_FIELD_CHARS {
                return Err(invalid(format!("'{member}' exceeds the field bound")));
            }
            Ok(())
        }
        Some(_) => Err(invalid(format!("'{member}' must be a string"))),
    }
}

fn invalid(detail: impl Into<String>) -> String {
    format!("provider_config.contract_invalid: {}", detail.into())
}

/// Parse the contract document, refusing duplicate member names at
/// every depth. A lenient last-wins parse would validate only the
/// final value while the document the Worker mounts still carries the
/// ambiguity — and a strict consumer (the adapter's own field parse)
/// then rejects the whole document and silently falls back to
/// defaults. The boundary fails closed instead: a repeated member
/// name anywhere in the document is not a contract document.
fn parse_contract(bytes: &[u8]) -> Result<serde_json::Value, String> {
    serde_json::from_slice::<UniqueMembers>(bytes)
        .map(|document| document.0)
        .map_err(|e| invalid(format!("not a contract JSON object: {e}")))
}

/// A `serde_json::Value` whose object member names are unique at
/// every depth — see [`parse_contract`].
struct UniqueMembers(serde_json::Value);

impl<'de> serde::Deserialize<'de> for UniqueMembers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> serde::de::Visitor<'de> for UniqueVisitor {
    type Value = UniqueMembers;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueMembers(serde_json::Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueMembers(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueMembers(value.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(|n| UniqueMembers(serde_json::Value::Number(n)))
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueMembers(value.into()))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueMembers(serde_json::Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueMembers(serde_json::Value::Null))
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        <UniqueMembers as serde::Deserialize>::deserialize(deserializer)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(UniqueMembers(item)) = seq.next_element::<UniqueMembers>()? {
            items.push(item);
        }
        Ok(UniqueMembers(serde_json::Value::Array(items)))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some((key, UniqueMembers(value))) = map.next_entry::<String, UniqueMembers>()? {
            if object.contains_key(&key) {
                // A member name is document content — an extension
                // key may carry the credential itself — so the
                // diagnostic names the rule and never echoes it.
                return Err(serde::de::Error::custom("duplicate member name"));
            }
            object.insert(key, value);
        }
        Ok(UniqueMembers(serde_json::Value::Object(object)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("workloop-bindings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    const VALID: &str = "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"model-a\",\"endpoint\":\"https://api.example\"}";

    #[test]
    fn a_valid_contract_is_accepted() {
        let dir = tmp("valid");
        let path = write(&dir, "bindings.json", VALID);
        validate(&path, None).unwrap();
        validate(&path, Some(b"credential")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_versioned_members_refuse_with_bounded_diagnostics() {
        let dir = tmp("invalid");
        for (content, expect) in [
            ("{}", "contract_version"),
            (
                "{\"format_version\":\"taisan.worker.bindings/2\",\"model\":\"m\"}",
                "contract_version",
            ),
            (
                "{\"format_version\":\"taisan.worker.bindings/1\"}",
                "'model' is required",
            ),
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"\"}",
                "must not be empty",
            ),
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":7}",
                "must be a string",
            ),
            ("[1,2]", "not a contract JSON object"),
            ("not json", "not a contract JSON object"),
        ] {
            let path = write(&dir, "bad.json", content);
            let err = validate(&path, None).unwrap_err();
            assert!(err.starts_with("provider_config."), "{err}");
            assert!(err.contains(expect), "{err} expected {expect}");
            assert!(err.len() < 240, "diagnostics stay bounded: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_and_directory_paths_refuse() {
        let dir = tmp("paths");
        let missing = dir.join("absent.json");
        let err = validate(&missing, None).unwrap_err();
        assert!(err.starts_with("provider_config.unreadable"), "{err}");
        let err = validate(&dir, None).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_embedded_credential_is_a_configuration_error() {
        let dir = tmp("secret");
        let path = write(
            &dir,
            "bindings.json",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"note\":\"xx-secret-yy\"}",
        );
        let err = validate(&path, Some(b"secret")).unwrap_err();
        assert!(
            err.starts_with("provider_config.contains_credential"),
            "{err}"
        );
        assert!(!err.contains("xx-secret-yy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check matches the credential as a consumer reads it: a
    /// `file:` credential's trailing newline must not let the trimmed
    /// value sit in the document, and a secret spelled through JSON
    /// escapes — absent from the raw bytes — is delivered verbatim
    /// when the adapter parses the mounted document.
    #[test]
    fn the_effective_credential_is_refused_whatever_its_spelling() {
        let dir = tmp("effective");
        for (content, credential) in [
            // Staged bytes carry a trailing newline; the document
            // embeds the trimmed value.
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"note\":\"x-sekrit-y\"}",
                "sekrit\n",
            ),
            // The document spells the secret through a JSON escape —
            // absent from the raw bytes, delivered verbatim on parse.
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"note\":\"x-sek\\u0072it-y\"}",
                "sekrit",
            ),
            // A member name is document content too.
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"x-sekrit-y\":1}",
                "sekrit",
            ),
            // A numeric leaf spells the secret in canonical form —
            // `1.2e3` parses to the number `1200`, which raw-byte
            // scanning never sees spelled this way.
            (
                "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"padding\":1.2e3}",
                "1200",
            ),
        ] {
            let path = write(&dir, "dirty.json", content);
            let err = validate(&path, Some(credential.as_bytes())).unwrap_err();
            assert!(
                err.starts_with("provider_config.contains_credential"),
                "{err} for {content}"
            );
            assert!(!err.contains("sekrit"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An all-whitespace credential trims to nothing deliverable —
    /// checking its raw bytes would match the document's own
    /// indentation and refuse a clean config.
    #[test]
    fn a_whitespace_only_credential_never_matches_indentation() {
        let dir = tmp("wscred");
        let path = write(
            &dir,
            "clean.json",
            "{\n    \"format_version\": \"taisan.worker.bindings/1\",\n    \"model\": \"m\"\n}",
        );
        validate(&path, Some(b"  \n  ")).expect("an undeliverable credential embeds nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repeated member name is a refusal — and the diagnostic names
    /// the rule, never the member: an extension key may carry the
    /// credential itself, and the error is recorded as a launch
    /// repair.
    #[test]
    fn a_duplicate_member_name_is_not_echoed() {
        let dir = tmp("dupname");
        let path = write(
            &dir,
            "dup.json",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"sekrit-key\":1,\"sekrit-key\":2}",
        );
        let err = validate(&path, None).unwrap_err();
        assert!(err.contains("duplicate member name"), "{err}");
        assert!(!err.contains("sekrit-key"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repeated member name — required, extension, or nested —
    /// refuses the document: a last-wins parse would validate only the
    /// final value while the mounted document still carries the
    /// ambiguity a strict consumer rejects wholesale.
    #[test]
    fn duplicate_member_names_refuse() {
        let dir = tmp("dup");
        for content in [
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"a\",\"model\":\"b\"}",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\"}",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"adapter_option\":1,\"adapter_option\":2}",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"nested\":{\"a\":1,\"a\":2}}",
        ] {
            let path = write(&dir, "dup.json", content);
            let err = validate(&path, None).unwrap_err();
            assert!(err.starts_with("provider_config."), "{err}");
            assert!(err.contains("duplicate member name"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_members_are_tolerated() {
        let dir = tmp("ext");
        let path = write(
            &dir,
            "bindings.json",
            "{\"format_version\":\"taisan.worker.bindings/1\",\"model\":\"m\",\"adapter_option\":{\"nested\":true}}",
        );
        validate(&path, None).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
