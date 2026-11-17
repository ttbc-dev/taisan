//! U3.6e live-scenario preparation checks (slice issue #119 of #107).
//!
//! The separately authorized isolated live cases are gated on owner
//! authorization recorded per scenario. These named checks guard the
//! preparation slice — the `gh` CLI transport, the `live_evidenced` /
//! `not_authorized` evidence schema, the ignored live harness and the
//! runbook/authorization documents — without exercising any live
//! host:
//!
//! - `u36e_docs_cover_every_live_scenario` — every `live_*` test in
//!   `crates/workloop-github/tests/u36e_live.rs` is named in both
//!   `docs/u3.6e-live-runbook.md` and
//!   `docs/u3.6e-authorization.md`, and both documents cite #119.
//! - `u36e_runbook_documents_the_authorization_gate` — the runbook
//!   names every `TAISAN_U36E_*` variable the harness reads and the
//!   `live_evidenced` / `not_authorized` record shapes.
//! - `u36e_authorization_doc_scopes_each_scenario` — the
//!   authorization draft lists all five failure modes and bounds
//!   what each approval covers.
//!
//! The live tests themselves stay `#[ignore]`d: nothing here runs a
//! scenario, and no fixture result is ever presented as live
//! evidence.

use std::path::Path;

const HARNESS_PATH: &str = "crates/workloop-github/tests/u36e_live.rs";
const RUNBOOK_PATH: &str = "docs/u3.6e-live-runbook.md";
const AUTHORIZATION_PATH: &str = "docs/u3.6e-authorization.md";

/// The five #119 scenarios, one per failure mode.
const SCENARIOS: [&str; 5] = [
    "live_partial_push_then_create_failure",
    "live_lost_response_reconciles_by_marker",
    "live_stale_head_refused",
    "live_duplicate_delivery_single_effect",
    "live_denied_permission_refused",
];

const FAILURE_MODES: [&str; 5] = [
    "partial_effects",
    "lost_responses",
    "stale_head",
    "duplicate_events",
    "permission_drift",
];

/// Every `TAISAN_U36E_*` variable `live_env`/`remote_commit_pair`
/// read — the runbook must document the whole gate.
const ENV_VARS: [&str; 6] = [
    "TAISAN_U36E_REPO",
    "TAISAN_U36E_INSTALL",
    "TAISAN_U36E_AUTH",
    "TAISAN_U36E_GH",
    "TAISAN_U36E_SHA",
    "TAISAN_U36E_SHA_B",
];

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The `live_*` test functions the harness defines — the scenarios a
/// runbook must cover. Only `#[test]`-attributed functions count;
/// `live_env`/`live_config` helpers are not scenarios.
fn harness_scenarios() -> Vec<String> {
    let src = read(HARNESS_PATH);
    let lines: Vec<&str> = src.lines().collect();
    let mut scenarios = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("#[test]") {
            continue;
        }
        for next in &lines[i + 1..(i + 5).min(lines.len())] {
            if let Some(rest) = next.trim().strip_prefix("fn live_") {
                let name = rest.split('(').next().unwrap().trim();
                scenarios.push(format!("live_{name}"));
                break;
            }
        }
    }
    scenarios
}

#[test]
fn u36e_docs_cover_every_live_scenario() {
    let scenarios = harness_scenarios();
    for expected in SCENARIOS {
        assert!(
            scenarios.iter().any(|s| s == expected),
            "harness defines {expected}: {scenarios:?}"
        );
    }
    let runbook = read(RUNBOOK_PATH);
    let authorization = read(AUTHORIZATION_PATH);
    for scenario in &scenarios {
        assert!(
            runbook.contains(scenario),
            "runbook omits scenario {scenario}"
        );
        assert!(
            authorization.contains(scenario),
            "authorization draft omits scenario {scenario}"
        );
    }
    for doc in [&runbook, &authorization] {
        assert!(doc.contains("#119"), "document must cite issue #119");
    }
}

#[test]
fn u36e_runbook_documents_the_authorization_gate() {
    let runbook = read(RUNBOOK_PATH);
    for var in ENV_VARS {
        assert!(
            runbook.contains(var),
            "runbook omits environment variable {var}"
        );
    }
    for claim in [
        "live_evidenced",
        "not_authorized",
        "authorization_reference",
        "isolation_scope",
        "unexercised",
        "requires_live",
    ] {
        assert!(
            runbook.contains(claim),
            "runbook omits claim-class term {claim}"
        );
    }
}

#[test]
fn u36e_authorization_doc_scopes_each_scenario() {
    let authorization = read(AUTHORIZATION_PATH);
    for mode in FAILURE_MODES {
        assert!(
            authorization.contains(mode),
            "authorization draft omits failure mode {mode}"
        );
    }
    for term in [
        "GH_TOKEN",
        "read-only",
        "u36e/",
        "not_authorized",
        "isolation_scope",
        "TAISAN_U36E_AUTH",
    ] {
        assert!(
            authorization.contains(term),
            "authorization draft omits {term}"
        );
    }
}

/// The live tests stay `#[ignore]`d — the preparation slice must
/// never turn a fixture or CI run into a live claim.
#[test]
fn u36e_live_tests_stay_ignored() {
    let src = read(HARNESS_PATH);
    for scenario in SCENARIOS {
        let needle = format!("fn {scenario}(");
        let pos = src
            .find(&needle)
            .unwrap_or_else(|| panic!("harness defines {scenario}"));
        let before = &src[..pos];
        let tail = before.lines().rev().take(3).collect::<Vec<_>>().join("\n");
        assert!(
            tail.contains("#[ignore"),
            "{scenario} must stay #[ignore]d — found:\n{tail}"
        );
    }
}
