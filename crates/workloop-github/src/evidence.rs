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
    /// The base revision the run was exercised against — the
    /// merge-base fork point where the checkout retains history, the
    /// platform-recorded event base in CI.
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_authorization_reference"
    )]
    pub authorization_reference: Option<String>,
}

/// Present-but-wrong-typed guard: an `authorization_reference` key may
/// only ever hold a non-empty string. A `null` or non-string value must
/// fail deserialization rather than normalize to `None`, which would
/// silently turn a present key into an absent one — typed records and
/// externally produced JSON answer the same contract.
fn deserialize_authorization_reference<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) if !s.is_empty() => Ok(Some(s)),
        other => Err(serde::de::Error::custom(format!(
            "authorization_reference must be a non-empty string when present, got {other}"
        ))),
    }
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

/// The base fork point of a normal checkout. A candidate equal to HEAD
/// is no fork point — `merge-base HEAD <ref>` answers HEAD itself when
/// `<ref>` is the checked-out commit (a run on the base branch, or the
/// only commit a shallow clone retains) — so it is rejected. When no
/// merge-base is provable, only a *truncated* checkout may cite the
/// deepest commit it retains — a shallow clone's boundary commit is the
/// deepest base it can prove. A full clone whose base refs are all
/// unresolved (no origin, a run on the base branch itself) has no
/// provable base: the repository root is not a base the run was
/// exercised against, so the answer is `None` and the run retains no
/// records.
fn base_revision(manifest_dir: &str) -> Option<String> {
    let head = git_stdout(manifest_dir, &["rev-parse", "HEAD"])?;
    for revs in [
        ["merge-base", "HEAD", "origin/main"],
        ["merge-base", "HEAD", "main"],
        ["merge-base", "HEAD", "origin/HEAD"],
    ] {
        match git_stdout(manifest_dir, &revs) {
            Some(sha) if sha != head => return Some(sha),
            _ => {}
        }
    }
    // The deepest-retained-commit citation is only meaningful when the
    // checkout is truncated — otherwise the repository root would
    // masquerade as a fork point it is not.
    if git_stdout(manifest_dir, &["rev-parse", "--is-shallow-repository"]).as_deref()
        != Some("true")
    {
        return None;
    }
    match git_stdout(manifest_dir, &["rev-list", "--max-parents=0", "HEAD"])
        .and_then(|roots| roots.lines().next().map(str::to_string))
    {
        Some(sha) if sha != head => Some(sha),
        _ => None,
    }
}

/// The revision pair a CI run stamps, read from the workflow event
/// payload: the Actions checkout is depth-1, so the clone keeps no
/// reachable base history, but the payload records the real commits —
/// a pull_request run cites `head.sha` (the source-branch commit
/// carrying the slice tree) and `base.sha` (the base tip the run was
/// exercised against — the commit the merge ref combined with head);
/// the default `pull_request` checkout is the synthetic merge of the
/// two, so the (`tested_revision`, `base_revision`) pair describes the
/// exercised merge tree; a push run cites `GITHUB_SHA` and `before`
/// (the tip the push was applied over). The CI base is therefore the
/// platform-recorded event
/// base, which differs from a merge-base fork point once the base
/// branch has advanced; both describe the base the run was exercised
/// against in its own environment. `None` outside Actions or when the
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

/// Whether `sha` is a full commit id — `unknown`, a truncated id, any
/// non-hex output, or GitHub's all-zero absent-revision sentinel (the
/// `before` of a pushed new ref) is no stamp.
fn is_commit_id(sha: &str) -> bool {
    sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) && !sha.bytes().all(|b| b == b'0')
}

/// The revision pair a recorded run stamps: `tested` is the commit
/// that contains the exercised slice tree, `base` the base revision
/// the run was exercised against — the merge-base fork point where the
/// checkout retains history, the platform-recorded event base in CI.
/// Both are full commit ids over a pristine tracked
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
    if !is_commit_id(&tested) || !is_commit_id(&base) || tested == base {
        return None;
    }
    Some((tested, base))
}

/// The field-presence check every retained record must pass. Asserts:
///
/// - every field of `REQUIRED_FIELDS` is present (non-null);
/// - `schema_version` is exactly `SCHEMA_VERSION` — a record of an
///   unknown schema cannot be interpreted;
/// - `claim_class` is the string `fixture_contract` or `live_evidenced`
///   — a present-but-non-string value is a violation, never fail-open;
/// - a `live_evidenced` record additionally carries a non-empty
///   `authorization_reference`, and a `fixture_contract` record does
///   not carry the key at all — its absence invalidates a live record
///   and its presence invalidates a fixture record;
/// - the revision/scenario fields and the `install_identity` strings
///   are strings, `contract_version` a non-negative integer — a
///   present-but-wrong-typed scalar is a violation, so a record the
///   typed form cannot deserialize never passes the raw check;
/// - `config_identity` carries its own required fields
///   (`install_identity` with `host_instance`, `repository`,
///   `adapter`, `contract_version`, plus `config_digest`);
/// - `result` is the string `pass` / `fail` / `skipped` /
///   `inconclusive` — a present-but-non-string value is a violation;
/// - the array fields are arrays of the typed element shape: identity
///   arrays hold strings, `read_back_receipts` holds objects with
///   string `target` and `observed`.
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
    for field in ["action_identities", "observation_identities"] {
        match object.get(field) {
            Some(v) if v.is_array() => {
                for (i, element) in v.as_array().unwrap().iter().enumerate() {
                    if !element.is_string() {
                        violations.push(format!("{field}[{i}] is not a string"));
                    }
                }
            }
            Some(_) => violations.push(format!("{field} is not an array")),
            None => {}
        }
    }
    match object.get("read_back_receipts") {
        Some(v) if v.is_array() => {
            for (i, element) in v.as_array().unwrap().iter().enumerate() {
                match element.as_object() {
                    Some(receipt) => {
                        for field in ["target", "observed"] {
                            match receipt.get(field) {
                                Some(v) if v.is_string() => {}
                                _ => violations.push(format!(
                                    "read_back_receipts[{i}] missing string {field}"
                                )),
                            }
                        }
                    }
                    None => violations.push(format!("read_back_receipts[{i}] is not an object")),
                }
            }
        }
        Some(_) => violations.push("read_back_receipts is not an array".into()),
        None => {}
    }
    // String scalars: presence is the required-field check; a
    // present-but-non-string value is a type violation the typed form
    // could never deserialize.
    for field in [
        "tested_revision",
        "base_revision",
        "core_revision",
        "adapter_revision",
        "command_or_scenario",
    ] {
        match object.get(field) {
            Some(v) if v.is_string() => {}
            Some(_) => violations.push(format!("{field} is not a string")),
            None => {}
        }
    }
    match object.get("schema_version") {
        Some(v) if v.as_u64() == Some(SCHEMA_VERSION) => {}
        Some(_) => violations.push(format!("schema_version must be {SCHEMA_VERSION}")),
        None => {} // absent is already flagged by the required-field check
    }
    // Absent claim_class/result are already flagged by the
    // required-field check; a present-but-non-string value is a
    // distinct violation, never fail-open.
    if let Some(v) = object.get("claim_class") {
        match v.as_str() {
            Some("fixture_contract") => {
                if object.contains_key("authorization_reference") {
                    violations.push(
                        "authorization_reference must be absent on a fixture_contract record"
                            .into(),
                    );
                }
            }
            Some("live_evidenced") => match object.get("authorization_reference") {
                Some(v) if v.as_str().is_some_and(|s| !s.is_empty()) => {}
                _ => {
                    violations.push("live_evidenced record missing authorization_reference".into())
                }
            },
            Some(other) => violations.push(format!("unknown claim_class: {other}")),
            None => violations.push("claim_class is not a string".into()),
        }
    }
    if let Some(v) = object.get("result") {
        match v.as_str() {
            Some("pass" | "fail" | "skipped" | "inconclusive") => {}
            Some(other) => violations.push(format!("unknown result: {other}")),
            None => violations.push("result is not a string".into()),
        }
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
                        for field in ["host_instance", "repository", "adapter"] {
                            match i.get(field) {
                                Some(v) if v.is_string() => {}
                                Some(_) => violations
                                    .push(format!("install_identity.{field} is not a string")),
                                None => {
                                    violations.push(format!("install_identity missing {field}"))
                                }
                            }
                        }
                        match i.get("contract_version") {
                            Some(v) if v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()) => {}
                            Some(_) => violations
                                .push("install_identity.contract_version is not a u32".into()),
                            None => {
                                violations.push("install_identity missing contract_version".into())
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
    use std::path::{Path, PathBuf};
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

    #[test]
    fn wrong_typed_closed_set_fields_are_invalid() {
        // A present-but-wrong-typed or out-of-set closed-field value is
        // a violation — the string matches must not fail open.
        for (field, injected) in [
            ("claim_class", serde_json::json!(7)),
            ("claim_class", serde_json::json!(null)),
            ("result", serde_json::json!(7)),
            ("result", serde_json::json!(null)),
            ("schema_version", serde_json::json!(SCHEMA_VERSION + 1)),
            ("schema_version", serde_json::json!("1")),
            ("tested_revision", serde_json::json!(7)),
            ("command_or_scenario", serde_json::json!({"nested": true})),
        ] {
            let mut value = serde_json::to_value(record()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(field.into(), injected.clone());
            assert!(
                validate_record_json(&value).is_err(),
                "{field} = {injected} must invalidate the record"
            );
        }
    }

    #[test]
    fn wrong_typed_array_elements_are_invalid() {
        // Element shapes must match the typed form: identity arrays
        // hold strings, receipts hold {target, observed} objects —
        // anything else is a record the typed form cannot deserialize.
        for (field, injected) in [
            ("action_identities", serde_json::json!([7])),
            ("observation_identities", serde_json::json!([null])),
            ("read_back_receipts", serde_json::json!([{}])),
            ("read_back_receipts", serde_json::json!([{"target": "t"}])),
            (
                "read_back_receipts",
                serde_json::json!([{"target": 7, "observed": "o"}]),
            ),
            ("read_back_receipts", serde_json::json!([5])),
        ] {
            let mut value = serde_json::to_value(record()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(field.into(), injected.clone());
            assert!(
                validate_record_json(&value).is_err(),
                "{field} = {injected} must invalidate the record"
            );
        }
    }

    #[test]
    fn wrong_typed_install_identity_fields_are_invalid() {
        // Nested scalars follow the same rule: present-but-wrong-typed
        // is a violation, never a pass.
        for (field, injected) in [
            ("host_instance", serde_json::json!(7)),
            ("repository", serde_json::json!(null)),
            ("adapter", serde_json::json!(true)),
            ("contract_version", serde_json::json!("1")),
            // One past u32::MAX fits u64 but not the typed u32 field —
            // parity with the typed form rejects it.
            (
                "contract_version",
                serde_json::json!(u64::from(u32::MAX) + 1),
            ),
        ] {
            let mut value = serde_json::to_value(record()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .get_mut("config_identity")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .get_mut("install_identity")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.into(), injected.clone());
            assert!(
                validate_record_json(&value).is_err(),
                "install_identity.{field} = {injected} must invalidate the record"
            );
        }
    }

    #[test]
    fn a_present_but_invalid_authorization_reference_cannot_deserialize() {
        // A `null` or wrong-typed key must not normalize to absent —
        // typed records and raw JSON answer the same contract.
        for injected in [
            serde_json::Value::Null,
            serde_json::json!(7),
            serde_json::json!(""),
        ] {
            let mut value = serde_json::to_value(record()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("authorization_reference".into(), injected.clone());
            assert!(
                serde_json::from_value::<EvidenceRecord>(value.clone()).is_err(),
                "authorization_reference = {injected} must fail deserialization"
            );
            assert!(
                validate_record_json(&value).is_err(),
                "authorization_reference = {injected} is also a raw-record violation"
            );
        }
    }

    #[test]
    fn an_all_zero_sha_is_not_a_commit_id() {
        // GitHub's absent-revision sentinel — a pushed new ref's
        // `before` — must not stamp as a base.
        assert!(!is_commit_id(&"0".repeat(40)));
        assert!(!is_commit_id("abc"));
        assert!(!is_commit_id(&"g".repeat(40)));
        assert!(is_commit_id(&"a".repeat(40)));
    }

    fn git(dir: &Path, argv: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(argv)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {argv:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A scratch git repo under the OS temp dir with `commits` empty
    /// commits — `commits == 1` is the shallow-clone shape: HEAD is
    /// the deepest commit the checkout retains.
    fn git_repo(name: &str, commits: usize) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("workloop-u36a-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "test"]);
        for i in 0..commits {
            git(
                &dir,
                &["commit", "-q", "--allow-empty", "-m", &format!("c{i}")],
            );
        }
        dir
    }

    /// A scratch clone of `src` at `--depth n` — a real shallow
    /// checkout, so `--is-shallow-repository` and grafted boundary
    /// commits behave as they do in CI.
    fn shallow_clone(name: &str, src: &Path, depth: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("workloop-u36a-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        git(
            std::env::temp_dir().as_path(),
            &[
                "clone",
                "-q",
                "--depth",
                depth,
                &format!("file://{}", src.display()),
                dir.to_str().unwrap(),
            ],
        );
        dir
    }

    #[test]
    fn base_revision_is_none_when_the_only_retained_commit_is_head() {
        // A depth-1 clone outside CI: HEAD is also the deepest (grafted)
        // commit the checkout retains — no fork point is provable, so
        // the stamp must not cite HEAD as its own base.
        let src = git_repo("src-shallow", 2);
        let dir = shallow_clone("depth1", &src, "1");
        assert_eq!(
            base_revision(dir.to_str().unwrap()),
            None,
            "a checkout retaining only HEAD proves no fork point"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&src).ok();
    }

    #[test]
    fn base_revision_is_none_for_a_full_clone_without_base_refs() {
        // A full clone whose base refs are all unresolved: the
        // repository root is not a base the run was exercised against —
        // retain no stamp rather than citing it as a fork point.
        let dir = git_repo("deep", 2);
        assert_eq!(
            base_revision(dir.to_str().unwrap()),
            None,
            "a full clone without base refs proves no base"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base_revision_cites_the_deepest_commit_a_shallow_clone_retains() {
        // A --depth 2 clone keeps the tip and its grafted parent:
        // merge-base is unprovable across the boundary, so the boundary
        // commit is the deepest base the checkout can cite.
        let src = git_repo("src", 3);
        let dir = shallow_clone("clone", &src, "2");
        let boundary = git(&dir, &["rev-parse", "HEAD~1"]);
        assert_eq!(
            base_revision(dir.to_str().unwrap()).as_deref(),
            Some(boundary.as_str()),
            "the shallow boundary commit is the deepest provable base"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&src).ok();
    }
}
