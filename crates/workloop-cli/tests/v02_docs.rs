//! v0.2 installation/operation documentation checks.
//!
//! These checks guard the published v0.2 material without running any
//! live scenario:
//!
//! - `v02_walkthrough_covers_install_and_operations` asserts the
//!   install/configure, one-shot-with-automation-disabled,
//!   enable/disable, startup, interval-change, configuration-only
//!   adapter-swap, and history-inspection steps are present, together
//!   with the 30-minute interval, 60-second settings tick,
//!   120-second timeout, and 10-pending-Work limit and the no-OS-timer
//!   statement.
//! - `v02_owner_checklist_exercises_every_required_scenario` asserts
//!   the checklist names two real Issues and the restart, duplicate
//!   presentation, full-queue, outage, and configuration-reload
//!   scenarios with expected results.
//! - `v02_evidence_template_labels_and_redacts` asserts the template
//!   captures revisions, profiles, commands, observed results, durable
//!   run/proposal/command/Work links, redaction, unexercised rows, and
//!   both evidence labels.
//! - `v02_sanitized_profile_parses_with_documented_defaults` loads the
//!   published example through the same profile parser and asserts one
//!   selector binding, the documented intake defaults, and ref-only
//!   credentials.
//! - `v02_public_docs_do_not_name_private_integrations` keeps the
//!   publication surface free of private source-role names and keeps
//!   the separately validated optional-adapter boundary stated.
//! - `readme_links_the_v02_guide` keeps the public entry point linked.
//!
//! No test here creates an Issue, enables automation, or launches a
//! Worker; they read repository files only.

use std::path::Path;

const GUIDE_PATH: &str = "docs/v0.2.md";
const CHECKLIST_PATH: &str = "docs/v0.2-owner-run-checklist.md";
const TEMPLATE_PATH: &str = "docs/v0.2-evidence-template.md";
const EXAMPLE_CONFIG_PATH: &str = "docs/examples/v0.2/config.toml";
const FIXTURE_LABEL: &str = "automated fixture/contract evidence";
const LIVE_LABEL: &str = "owner-run live evidence";

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"))
}

/// Collapse Markdown line wrapping so a phrase that spans lines still
/// matches as one string.
fn flattened(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn v02_walkthrough_covers_install_and_operations() {
    let guide = flattened(&read(GUIDE_PATH));

    for required in [
        "## Clean install",
        "### Build and install",
        "### Configure",
        "## Operation walkthrough",
        "Run one explicit intake with automation disabled",
        "Enable and disable automatic intake",
        "Observe startup, interval change, and restart",
        "Replace the adapter by configuration only",
        "Inspect history and Work",
    ] {
        assert!(guide.contains(required), "guide omits section {required:?}");
    }

    for required in [
        "One Selector adapter per installation",
        "60-second settings tick",
        "interval_minutes",
        "timeout_seconds = 120",
        "pending_queue_limit",
        "configuration-only",
        "configuration-only adapter",
        "No cron",
        "no OS timer",
        "taisan intake",
        "taisan intake-runs",
        "taisan intake-detail",
        "taisan run",
        "taisan work-list",
        "taisan work-detail",
        "capacity",
        "restart",
        "duplicate",
        "cargo install --path crates/workloop-selector --bin public-issue-selector",
        "absent (no `[intake.selector]`)",
        "joins it before exiting",
    ] {
        assert!(guide.contains(required), "guide omits {required:?}");
    }

    assert!(
        guide.contains("interval_minutes = 30"),
        "guide must document the 30-minute default"
    );
    assert!(
        guide.contains("pending_queue_limit = 10"),
        "guide must document the 10-pending-Work limit"
    );
    assert!(
        guide.contains("30 minutes apart") || guide.contains("30-minute"),
        "guide must state the 30-minute schedule"
    );
}

#[test]
fn v02_owner_checklist_exercises_every_required_scenario() {
    let checklist = flattened(&read(CHECKLIST_PATH));

    for required in [
        "Issue A",
        "Issue B",
        "existing execution",
        "scheduled intake",
        "restart",
        "Duplicate presentation",
        "full queue",
        "selector outage",
        "configuration reload",
        "Expected results summary",
        "unexercised",
        "adapter_source_unavailable",
        "adapter_authentication_failed",
        "adapter_deadline_exceeded",
        "exchange_*",
    ] {
        assert!(checklist.contains(required), "checklist omits {required:?}");
    }

    for scenario in ["L-1", "L-2", "L-3", "L-4", "L-5", "L-6", "L-7", "L-8"] {
        assert!(
            checklist.contains(scenario),
            "checklist omits scenario {scenario}"
        );
    }

    assert!(checklist.contains(FIXTURE_LABEL));
    assert!(checklist.contains(LIVE_LABEL));
    assert!(
        checklist.contains("never marked passed from a fixture")
            || checklist.contains("never marked passed by fixture"),
        "checklist must forbid fixture-only live passes"
    );
}

#[test]
fn v02_evidence_template_labels_and_redacts() {
    let template = flattened(&read(TEMPLATE_PATH));

    for required in [
        "Tested revision",
        "profile",
        "Command and observation log",
        "Run / proposal / command / Work links",
        "Recorded identities",
        "Redaction checklist",
        "Unexercised scenarios",
        FIXTURE_LABEL,
        LIVE_LABEL,
        "never marked passed from an automated fixture",
        "credential",
        "redact",
        "unexercised",
        "owner-run live",
        "automated fixture/contract",
    ] {
        assert!(template.contains(required), "template omits {required:?}");
    }

    for result in ["pass", "fail", "deviation", "unexercised"] {
        assert!(
            template.contains(result),
            "template omits result class {result}"
        );
    }
}

#[test]
fn v02_sanitized_profile_parses_with_documented_defaults() {
    let text = read(EXAMPLE_CONFIG_PATH);
    let profile = workloop_config::profile::parse_profile(&text)
        .expect("the published sanitized profile must parse");

    let intake = profile
        .intake_settings()
        .expect("the published intake section must validate");
    assert!(!intake.enabled, "clean install must start disabled");
    assert_eq!(intake.interval_minutes, 30);
    assert_eq!(intake.timeout_seconds, 120);
    assert_eq!(intake.pending_queue_limit, 10);

    let selector = intake
        .selector
        .expect("the published profile must carry one selector binding");
    assert_eq!(selector.adapter, "public-issue-selector");
    assert_eq!(selector.selector_version, 1);
    assert!(!selector.argv.is_empty());

    for (role, binding) in &profile.roles {
        let credential_ref = binding
            .credential_ref
            .as_deref()
            .unwrap_or_else(|| panic!("role {role} must carry a credential_ref"));
        assert!(
            credential_ref.starts_with("env:")
                || credential_ref.starts_with("file:")
                || credential_ref.starts_with("command:"),
            "role {role} must use a credential reference, got {credential_ref:?}"
        );
    }
    assert!(
        !text.contains("ghp_") && !text.contains("github_pat_"),
        "the sanitized example must not carry a token-shaped value"
    );
}

#[test]
fn v02_public_docs_do_not_name_private_integrations() {
    let documents = [
        read(GUIDE_PATH),
        read(CHECKLIST_PATH),
        read(TEMPLATE_PATH),
        read("docs/installation-config.md"),
        read(EXAMPLE_CONFIG_PATH),
        read("README.md"),
    ];
    for text in &documents {
        let lowered = flattened(text).to_lowercase();
        for forbidden in [
            "ops repo",
            "worker repo",
            "selector repo",
            "infra repo",
            "private source repository",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "public documentation names a private source role: {forbidden:?}"
            );
        }
    }

    let guide = flattened(&read(GUIDE_PATH));
    let checklist = flattened(&read(CHECKLIST_PATH));
    for required in [
        "no dependency on private integrations",
        "validated separately by its own owner",
        "not a release dependency",
    ] {
        assert!(
            guide.contains(required) || checklist.contains(required),
            "public guidance omits the private-integration boundary {required:?}"
        );
    }
}

#[test]
fn readme_links_the_v02_guide() {
    let readme = read("README.md");
    for link in [GUIDE_PATH, CHECKLIST_PATH, TEMPLATE_PATH] {
        assert!(readme.contains(link), "README omits link {link}");
    }
}
