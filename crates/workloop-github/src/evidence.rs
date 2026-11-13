//! U3.6 evidence records: the closed per-scenario record format defined
//! in `docs/repository-host-contract.md` (U3.6 evidence record), plus the
//! fail-closed revision stamping every U3.6 suite run shares.
//!
//! One record is retained per scenario per adapter. Fixture-produced
//! runs only ever stamp `claim_class = fixture_contract`; a
//! `live_evidenced` record is a separately authorized live run and must
//! cite its `authorization_reference` — the field is absent entirely on
//! a fixture record, never null.

use serde::{Deserialize, Serialize};
use workloop_core::domain::{canonical_json, sha256_hex};
use workloop_core::work::WorkloopConfig;

/// The format version stamped into every U3.6 record.
pub const SCHEMA_VERSION: u64 = 1;

/// The required top-level fields of a U3.6 record. A retained record
/// missing any of them is invalid; `authorization_reference` is
/// required additionally iff `claim_class` is `live_evidenced`.
pub const REQUIRED_FIELDS: &[&str] = &[
    "schema_version",
    "tested_revision",
    "base_revision",
    "core_revision",
    "adapter_revision",
    "config_identity",
    "command_or_scenario",
    "action_identities",
    "observation_identities",
    "read_back_receipts",
    "result",
    "claim_class",
];

/// What a retained record is allowed to claim. Fixture results support
/// contract claims only; live claims require separately authorized live
/// evidence under the implementation start gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimClass {
    /// Local fixture/contract evidence only — never a live claim.
    FixtureContract,
    /// A separately authorized live scenario — requires
    /// `authorization_reference`.
    LiveEvidenced,
}

/// The outcome a retained scenario row reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioResult {
    Pass,
    Fail,
    Skipped,
    Inconclusive,
}

/// The installation identity a run bound: which host instance,
/// repository, adapter implementation and contract version the
/// exercised configuration selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallIdentity {
    pub host_instance: String,
    pub repository: String,
    /// The configured adapter implementation id exercised by the run.
    pub adapter: String,
    pub contract_version: u32,
}

/// The configuration a record binds: the install identity plus the
/// digest of the canonical configuration projection the run exercised —
/// the record's config-file identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIdentity {
    pub install_identity: InstallIdentity,
    /// `sha256:`-tagged digest of the canonical configuration
    /// projection — deterministic for the same configuration.
    pub config_digest: String,
}

/// One read-back observation a record cites: which subject or delivery
/// the read answered for, and what the read-back established — the
/// receipt kind plus the recorded fact it left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadBackReceipt {
    /// The subject or delivery identity this read-back answered for.
    pub target: String,
    /// What the read-back observed: the receipt kind and the recorded
    /// fact it established.
    pub observed: String,
}

/// A retained U3.6 evidence record — the closed field set. One record
/// per exercised scenario per adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub schema_version: u64,
    /// The commit on the PR branch that contains the exercised slice
    /// tree — a base revision or patch digest never satisfies this
    /// field.
    pub tested_revision: String,
    /// The fork point the exercised tree was applied to.
    pub base_revision: String,
    /// The exact revision of the Core implementation exercised — a
    /// commit SHA for in-repo builds.
    pub core_revision: String,
    /// The exact revision of the adapter implementation exercised —
    /// a commit SHA for in-repo fixtures, an adapter-declared build
    /// identifier otherwise.
    pub adapter_revision: String,
    pub config_identity: ConfigIdentity,
    /// The named scenario or command the record answers for.
    pub command_or_scenario: String,
    /// The durable action identities the scenario exercised — empty
    /// for a scenario that performs no authorized writes.
    pub action_identities: Vec<String>,
    /// The delivery identities the scenario's observations recorded.
    pub observation_identities: Vec<String>,
    /// The read-back receipts the scenario's facts were re-observed
    /// through.
    pub read_back_receipts: Vec<ReadBackReceipt>,
    pub result: ScenarioResult,
    pub claim_class: ClaimClass,
    /// The authorization a `live_evidenced` record cites. Required iff
    /// `claim_class` is `live_evidenced`; absent — never null — on a
    /// `fixture_contract` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_reference: Option<String>,
}

impl EvidenceRecord {
    /// The field-presence check: serialize and run the raw-record
    /// validator so a typed record answers the same required-field
    /// contract as any externally produced JSON.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let value = serde_json::to_value(self).expect("EvidenceRecord serializes");
        validate_record_json(&value)
    }
}

/// The canonical configuration projection a run's `config_digest`
/// covers: the fields that determine host-adapter behavior — the
/// adapter/repository/contract binding, the declared capability and
/// effect grants, the required checks, and the role→actor bindings.
/// Never secret values: `credential_ref` is the configured source name
/// only.
fn config_projection(config: &WorkloopConfig) -> serde_json::Value {
    serde_json::json!({
        "adapter": config.host.adapter.as_str(),
        "repository": config.host.repository.to_string(),
        "contract_version": config.host.contract_version,
        "actors": config.host.actors.iter().map(|b| serde_json::json!({
            "role": b.role.name(),
            "actor": format!("{}/{}", b.actor.host, b.actor.actor.as_str()),
            "credential_ref": b.credential_ref.as_str(),
        })).collect::<Vec<_>>(),
        "declared_capabilities": config
            .declared_capabilities
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>(),
        "required_checks": config.required_checks,
        "allowed_effects": {
            "workspace_write": config.allowed_effects.workspace_write,
            "host_write": config.allowed_effects.host_write,
            "network": config.allowed_effects.network,
        },
    })
}

impl ConfigIdentity {
    /// The configuration identity a run exercised: install identity
    /// bound by the configuration plus the digest of its canonical
    /// projection. `adapter` names the adapter implementation the run
    /// drove (e.g. `fixture-github`, `sparse-host`) — the record's
    /// `adapter_revision` pins its build revision alongside it.
    pub fn for_config(config: &WorkloopConfig, adapter: &str) -> Self {
        let install_identity = InstallIdentity {
            host_instance: config.host.repository.host.as_str().to_string(),
            repository: config.host.repository.repository.as_str().to_string(),
            adapter: adapter.to_string(),
            contract_version: config.host.contract_version,
        };
        let config_digest = sha256_hex(
            "workloop.u3.6.config",
            &canonical_json(&config_projection(config)),
        );
        Self {
            install_identity,
            config_digest: format!("sha256:{config_digest}"),
        }
    }
}

/// One git query's trimmed stdout — `None` on any failure. Revision
/// stamping never substitutes a placeholder for it.
fn git_stdout(manifest_dir: &str, argv: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(argv)
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether a cited commit could describe this checkout: no tracked
/// file differs from `HEAD`. A git failure answers `false` rather than
/// panicking — the answer only decides whether a run may stamp and
/// retain records.
fn tracked_tree_is_pristine(manifest_dir: &str) -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(manifest_dir)
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// The base fork point of a normal checkout. A checkout that retains
/// no base history at all — a depth-1 clone or a detached source tree —
/// cites the deepest commit it does retain, the only fork point it can
/// prove. A checkout with no commit history at all yields `None`.
fn base_revision(manifest_dir: &str) -> Option<String> {
    for revs in [
        ["merge-base", "HEAD", "origin/main"],
        ["merge-base", "HEAD", "main"],
        ["merge-base", "HEAD", "origin/HEAD"],
    ] {
        if let Some(sha) = git_stdout(manifest_dir, &revs) {
            return Some(sha);
        }
    }
    git_stdout(manifest_dir, &["rev-list", "--max-parents=0", "HEAD"])
        .and_then(|roots| roots.lines().next().map(str::to_string))
}

/// The revision pair a CI run stamps, read from the workflow event
/// payload: the Actions checkout is depth-1, so the clone keeps no
/// reachable base history, but the payload records the real commits —
/// a pull_request run cites `head.sha` (the source-branch commit, not
/// the synthetic merge ref) and `base.sha`; a push run cites
/// `GITHUB_SHA` and `before`. `None` outside Actions or when the
/// payload carries neither pair.
fn ci_event_revisions() -> Option<(String, String)> {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return None;
    }
    let event: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::env::var("GITHUB_EVENT_PATH").ok()?).ok()?,
    )
    .ok()?;
    if let Some(pr) = event.get("pull_request") {
        return Some((
            pr.get("head")?.get("sha")?.as_str()?.to_string(),
            pr.get("base")?.get("sha")?.as_str()?.to_string(),
        ));
    }
    Some((
        std::env::var("GITHUB_SHA").ok()?,
        event.get("before")?.as_str()?.to_string(),
    ))
}

/// Whether `sha` is a full commit id — `unknown`, a truncated id, or
/// any non-hex output is no stamp.
fn is_commit_id(sha: &str) -> bool {
    sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// The revision pair a recorded run stamps: `tested` is the commit
/// that contains the exercised slice tree, `base` the fork point it
/// was applied to. Both are full commit ids over a pristine tracked
/// tree — `None` whenever no trustworthy stamp exists (a dirty
/// checkout, an unreadable repository, missing base history), so a
/// run without a stamp executes its assertions but retains no records
/// rather than stamping a guess.
pub fn record_revisions(manifest_dir: &str) -> Option<(String, String)> {
    if !tracked_tree_is_pristine(manifest_dir) {
        return None;
    }
    let (tested, base) = match ci_event_revisions() {
        Some(pair) => pair,
        None => (
            git_stdout(manifest_dir, &["rev-parse", "HEAD"])?,
            base_revision(manifest_dir)?,
        ),
    };
    if !is_commit_id(&tested) || !is_commit_id(&base) {
        return None;
    }
    Some((tested, base))
}

/// The field-presence check every retained record must pass. Asserts:
///
/// - every field of `REQUIRED_FIELDS` is present (non-null);
/// - `claim_class` is one of `fixture_contract` / `live_evidenced`;
/// - a `live_evidenced` record additionally carries a non-empty
///   `authorization_reference`, and a `fixture_contract` record does
///   not carry the key at all — its absence invalidates a live record
///   and its presence invalidates a fixture record;
/// - `config_identity` carries its own required fields
///   (`install_identity` with `host_instance`, `repository`,
///   `adapter`, `contract_version`, plus `config_digest`);
/// - `result` is one of `pass` / `fail` / `skipped` / `inconclusive`;
/// - the array fields are arrays.
///
/// Returns the list of violations; an empty list is a valid record.
pub fn validate_record_json(value: &serde_json::Value) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();
    let Some(object) = value.as_object() else {
        return Err(vec!["record is not a JSON object".into()]);
    };
    for field in REQUIRED_FIELDS {
        match object.get(*field) {
            Some(v) if !v.is_null() => {}
            _ => violations.push(format!("missing required field: {field}")),
        }
    }
    for field in [
        "action_identities",
        "observation_identities",
        "read_back_receipts",
    ] {
        match object.get(field) {
            Some(v) if v.is_array() => {}
            Some(_) => violations.push(format!("{field} is not an array")),
            None => {}
        }
    }
    match object.get("claim_class").and_then(|v| v.as_str()) {
        Some("fixture_contract") => {
            if object.contains_key("authorization_reference") {
                violations.push(
                    "authorization_reference must be absent on a fixture_contract record".into(),
                );
            }
        }
        Some("live_evidenced") => match object.get("authorization_reference") {
            Some(v) if v.as_str().is_some_and(|s| !s.is_empty()) => {}
            _ => violations.push("live_evidenced record missing authorization_reference".into()),
        },
        Some(other) => violations.push(format!("unknown claim_class: {other}")),
        None => {}
    }
    match object.get("result").and_then(|v| v.as_str()) {
        Some("pass" | "fail" | "skipped" | "inconclusive") | None => {}
        Some(other) => violations.push(format!("unknown result: {other}")),
    }
    if let Some(config) = object.get("config_identity") {
        match config.as_object() {
            Some(c) => {
                match c.get("config_digest") {
                    Some(v) if v.is_string() => {}
                    _ => violations.push("config_identity missing config_digest".into()),
                }
                match c.get("install_identity").and_then(|i| i.as_object()) {
                    Some(i) => {
                        for field in ["host_instance", "repository", "adapter", "contract_version"]
                        {
                            match i.get(field) {
                                Some(v) if !v.is_null() => {}
                                _ => violations.push(format!("install_identity missing {field}")),
                            }
                        }
                    }
                    _ => violations.push("config_identity missing install_identity".into()),
                }
            }
            None => violations.push("config_identity is not an object".into()),
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use workloop_core::domain::{AllowedEffects, ExecutionBudget, LanePolicy, WorkPolicy};
    use workloop_core::identity::*;
    use workloop_core::work::{HostTemplate, WorkloopConfig};

    fn config() -> WorkloopConfig {
        WorkloopConfig {
            host: HostTemplate {
                adapter: AdapterId::trusted("fixture-github"),
                repository: RepositoryRef::new(
                    HostInstanceId::trusted("test-host"),
                    RepositoryId::trusted("local-repo"),
                ),
                contract_version: 1,
                actors: vec![RoleBinding {
                    role: ActorRole::Publication,
                    actor: HostActorRef {
                        host: HostInstanceId::trusted("test-host"),
                        actor: HostActorId::trusted("push-agent"),
                    },
                    credential_ref: CredentialRef::trusted("cred-publication"),
                }],
            },
            declared_capabilities: BTreeSet::new(),
            lanes: LanePolicy::default(),
            required_checks: vec![],
            allowed_effects: AllowedEffects {
                workspace_write: true,
                host_write: true,
                network: false,
            },
            budget: ExecutionBudget::default(),
            policy: WorkPolicy::default(),
        }
    }

    fn record() -> EvidenceRecord {
        EvidenceRecord {
            schema_version: SCHEMA_VERSION,
            tested_revision: "a".repeat(40),
            base_revision: "b".repeat(40),
            core_revision: "a".repeat(40),
            adapter_revision: "a".repeat(40),
            config_identity: ConfigIdentity::for_config(&config(), "fixture-github"),
            command_or_scenario: "u36a::stale_head_observation_never_rolls_back".into(),
            action_identities: vec![],
            observation_identities: vec![
                "local:fixture.poll:pr:test-host/local-repo/cr-1@2".into(),
            ],
            read_back_receipts: vec![ReadBackReceipt {
                target: "pr:test-host/local-repo/cr-1".into(),
                observed: "ingest:Recorded; current_read head-2 @v2".into(),
            }],
            result: ScenarioResult::Pass,
            claim_class: ClaimClass::FixtureContract,
            authorization_reference: None,
        }
    }

    #[test]
    fn a_complete_fixture_record_validates() {
        record().validate().unwrap();
    }

    #[test]
    fn a_fixture_record_serializes_without_authorization_reference() {
        let value = serde_json::to_value(record()).unwrap();
        assert!(
            !value
                .as_object()
                .unwrap()
                .contains_key("authorization_reference"),
            "a fixture_contract record carries no authorization_reference key — absent, never null"
        );
    }

    #[test]
    fn a_record_missing_any_required_field_is_invalid() {
        for field in REQUIRED_FIELDS {
            let mut value = serde_json::to_value(record()).unwrap();
            value.as_object_mut().unwrap().remove(*field);
            let violations = validate_record_json(&value).unwrap_err();
            assert!(
                violations.iter().any(|v| v.contains(field)),
                "missing {field} is a violation: {violations:?}"
            );
        }
    }

    #[test]
    fn a_live_record_requires_authorization_reference() {
        let mut value = serde_json::to_value(record()).unwrap();
        value.as_object_mut().unwrap().insert(
            "claim_class".into(),
            serde_json::Value::String("live_evidenced".into()),
        );
        let violations = validate_record_json(&value).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("authorization_reference")),
            "a live_evidenced record without authorization_reference is invalid: {violations:?}"
        );
        value.as_object_mut().unwrap().insert(
            "authorization_reference".into(),
            serde_json::Value::String("owner approval 2026-11-09, issue #119".into()),
        );
        validate_record_json(&value).unwrap();
    }

    #[test]
    fn a_fixture_record_with_authorization_reference_is_invalid() {
        let mut value = serde_json::to_value(record()).unwrap();
        value.as_object_mut().unwrap().insert(
            "authorization_reference".into(),
            serde_json::Value::String("should not be here".into()),
        );
        let violations = validate_record_json(&value).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("authorization_reference")),
            "a fixture_contract record carrying authorization_reference is invalid"
        );
    }

    #[test]
    fn an_unknown_claim_class_is_invalid() {
        let mut value = serde_json::to_value(record()).unwrap();
        value.as_object_mut().unwrap().insert(
            "claim_class".into(),
            serde_json::Value::String("live_proven".into()),
        );
        assert!(validate_record_json(&value).is_err());
    }

    #[test]
    fn a_broken_config_identity_is_invalid() {
        let mut value = serde_json::to_value(record()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .get_mut("config_identity")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("config_digest");
        assert!(validate_record_json(&value).is_err());
    }

    #[test]
    fn config_digest_is_deterministic_and_config_bound() {
        let a = ConfigIdentity::for_config(&config(), "fixture-github");
        let b = ConfigIdentity::for_config(&config(), "fixture-github");
        assert_eq!(a, b, "the same configuration digests identically");
        assert!(a.config_digest.starts_with("sha256:"));

        let mut changed = config();
        changed.host.contract_version = 2;
        let c = ConfigIdentity::for_config(&changed, "fixture-github");
        assert_ne!(
            a.config_digest, c.config_digest,
            "a different configuration digests differently"
        );
    }

    #[test]
    fn install_identity_binds_the_configured_install() {
        let identity = ConfigIdentity::for_config(&config(), "sparse-host");
        assert_eq!(identity.install_identity.host_instance, "test-host");
        assert_eq!(identity.install_identity.repository, "local-repo");
        assert_eq!(identity.install_identity.adapter, "sparse-host");
        assert_eq!(identity.install_identity.contract_version, 1);
    }
}
