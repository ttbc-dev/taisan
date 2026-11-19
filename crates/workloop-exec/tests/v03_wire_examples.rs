//! Consistency checks for the v0.3 wire examples in `docs/v0.3.md`.
//!
//! The labelled valid report must echo the identity and role of the
//! request it names: section 3.2 binds a report to the exact
//! `(request_id, invocation_id, request_digest)` and section 6.1
//! classifies a different role as `invalid`/`report.wrong_role`.
//! The canonical digest rule in section 3.1 is exercised directly so
//! an edit to a valid example cannot silently leave a stale digest.

use std::path::{Path, PathBuf};

use serde_json::Value;
use workloop_core::domain::{canonical_json, sha256_hex};

const DOC_PATH: &str = "docs/v0.3.md";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_doc() -> String {
    std::fs::read_to_string(repo_root().join(DOC_PATH))
        .unwrap_or_else(|e| panic!("read {DOC_PATH}: {e}"))
}

/// Body of one `###` subsection, up to the next `###` heading.
fn subsection<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("missing heading {heading}"));
    let rest = &doc[start + heading.len()..];
    match rest.find("\n### ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// First fenced `json` block in `body`.
fn json_block(body: &str) -> &str {
    let start = body
        .find("```json")
        .unwrap_or_else(|| panic!("missing json fence in section: {body}"))
        + "```json".len();
    let rest = &body[start..];
    let end = rest
        .find("```")
        .unwrap_or_else(|| panic!("unclosed json fence"));
    rest[..end].trim()
}

fn parse(body: &str) -> Value {
    serde_json::from_str(json_block(body)).unwrap_or_else(|e| panic!("parse example: {e}"))
}

/// SHA-256 over the tag, a colon, and the canonical body with `field`
/// omitted — the exact rule in section 3.1.
fn digest(value: &Value, tag: &str, field: &str) -> String {
    let mut body = value.clone();
    body.as_object_mut()
        .unwrap_or_else(|| panic!("{field} owner is not an object"))
        .remove(field);
    sha256_hex(tag, &canonical_json(&body))
}

fn string<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is not a string"))
}

#[test]
fn v03_valid_negative_report_echoes_its_request_role_and_identity() {
    let doc = read_doc();
    let request = parse(subsection(&doc, "### 7.2 Valid Coding request"));
    let report = parse(subsection(&doc, "### 7.3 Valid negative report"));

    for field in ["request_id", "invocation_id", "request_digest"] {
        assert_eq!(
            report[field], request[field],
            "valid report must echo the request {field}"
        );
    }
    assert_eq!(
        report["role"], request["role"],
        "a report labelled valid must echo its request role; a different role is invalid/report.wrong_role"
    );

    assert_eq!(
        digest(&request, "worker-request-v2", "request_digest"),
        string(&request, "request_digest"),
        "section 7.2 request_digest must verify under the section 3.1 rule"
    );
    assert_eq!(
        digest(&report, "worker-report-v2", "report_digest"),
        string(&report, "report_digest"),
        "section 7.3 report_digest must verify under the section 3.1 rule"
    );
}

#[test]
fn v03_invalid_report_differs_from_the_request_it_names() {
    let doc = read_doc();
    let request = parse(subsection(&doc, "### 7.2 Valid Coding request"));
    let invalid = parse(subsection(
        &doc,
        "### 7.4 Invalid report (supervisor classification)",
    ));

    assert_eq!(invalid["request_id"], request["request_id"]);
    assert_eq!(
        invalid["request_digest"], request["request_digest"],
        "the invalid example must echo the request digest so it isolates the invocation and role errors instead of also failing the request binding"
    );
    assert!(
        invalid["role"] != request["role"],
        "the invalid example must exercise report.wrong_role"
    );
    assert!(
        invalid["invocation_id"] != request["invocation_id"],
        "the invalid example must exercise report.wrong_invocation"
    );
    assert_eq!(
        digest(&invalid, "worker-report-v2", "report_digest"),
        string(&invalid, "report_digest"),
        "the invalid example must carry a verifying report_digest so report.digest_mismatch is not an additional error"
    );
}

/// The v0.3 operational profile is design-only, but its required
/// sections and selected decisions must stay in one place so a later
/// implementation cannot silently drop a decision or a non-claim.
#[test]
fn v03_operational_profile_records_the_single_host_decisions() {
    let doc = read_doc();
    let section = doc
        .split("## 14. Operational launch, verification, and host composition")
        .nth(1)
        .expect("v0.3 must keep the operational launch/verification/host section");

    for heading in [
        "### 14.1 Existing implementation mapped to the required boundary",
        "### 14.2 Selected operational decisions",
        "### 14.3 Runtime mount, credential, and provider-config delivery",
        "### 14.4 Model-API egress",
        "### 14.5 Verification toolchain, dependency setup, and host allowlist",
        "### 14.6 Managed process lifecycle and assessment outputs",
        "### 14.7 Production host-driver binding and composition",
        "### 14.8 Compatibility and cutover",
        "### 14.9 Operational acceptance matrix",
        "### 14.11 Non-claims",
    ] {
        assert!(
            section.contains(heading),
            "operational section omits {heading:?}"
        );
    }

    for decision in ["OP-1", "OP-2", "OP-3", "OP-4", "OP-5", "OP-6", "OP-7"] {
        assert!(
            section.contains(decision),
            "operational section omits decision {decision}"
        );
    }

    for required in [
        "Hygiene-only execution",
        "in_process",
        "model-API",
        "child runtime servers",
        "original actor binding",
        "OPS-19",
        "runtime setup, credential installation, model call",
    ] {
        assert!(
            section.contains(required),
            "operational section omits required phrase {required:?}"
        );
    }

    let config = std::fs::read_to_string(repo_root().join("docs/installation-config.md"))
        .expect("read installation-config.md");
    for required in [
        "## v0.3 operational profile (target schema revision 2)",
        "[host_driver]",
        "credential_ref",
        "model_api_hosts",
        "toolchain",
        "cache_paths",
    ] {
        assert!(
            config.contains(required),
            "installation config omits operational key {required:?}"
        );
    }

    let loop_doc = std::fs::read_to_string(repo_root().join("docs/execution-loop.md"))
        .expect("read execution-loop.md");
    for required in [
        "### Operational host-driver composition (v0.3)",
        "in_process",
        "Source polling",
        "Bounded outbox and reconciliation",
        "original actor/credential binding",
    ] {
        assert!(
            loop_doc.contains(required),
            "execution loop omits operational composition phrase {required:?}"
        );
    }

    let worker = std::fs::read_to_string(repo_root().join("docs/worker-protocol.md"))
        .expect("read worker-protocol.md");
    for required in [
        "### Operational delivery and managed lifecycle (v0.3)",
        "/taisan/provider-config",
        "Child runtime servers",
        "read-only workspace",
        "restart",
    ] {
        assert!(
            worker.contains(required),
            "worker protocol omits operational lifecycle phrase {required:?}"
        );
    }
}
