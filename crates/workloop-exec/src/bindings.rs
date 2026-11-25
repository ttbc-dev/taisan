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
///   proxy endpoint the adapter may additionally honor. The
///   supervisor-delivered `HTTPS_PROXY` environment is authoritative
///   for egress either way.
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
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("not a contract JSON object: {e}")))?;
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
    // `/taisan/runtime-auth` file.
    if let Some(secret) = credential_bytes
        && !secret.is_empty()
        && credentials::contains_secret(&bytes, secret)
    {
        return Err(
            "provider_config.contains_credential: the provider configuration embeds \
             the resolved credential value"
                .into(),
        );
    }
    Ok(())
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
