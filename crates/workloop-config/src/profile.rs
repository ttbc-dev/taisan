//! The versioned installation profile (`config.toml`): host and
//! repository bindings, role references, the egress allowlist, and the
//! approved recovery/retention policy settings — plus the fixed
//! flag → environment → XDG path precedence and root-separation
//! validation.
//!
//! Design sources: `docs/installation-config.md` (schema, validation
//! rules, installation layout), `docs/recovery-policy.md` and
//! `docs/retention-policy.md` (approved defaults and the zero /
//! `unlimited` semantics).
//!
//! Loading is read-only and fail-closed: an unknown key, an
//! unsupported `schema_version`, or an invalid value is refused with
//! the offending key named, and the file is never created, rewritten,
//! or migrated. Credential references are validated for shape only —
//! values resolve later at the adapter boundary and never enter this
//! module, Core, argv, logs, or evidence.
//!
//! Refusals draw one fixed line between identifiers and values:
//! identifiers (unknown keys, key paths, host/role/capability names)
//! are named so the operator's repair loop can locate the offending
//! entry — that naming is the CF-01/CF-05 contract — while rejected
//! scalar literals, `credential_ref` contents, and source lines are
//! always redacted. A load refusal therefore never carries a secret
//! *value* into a message, log, or evidence record.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use workloop_core::credential::CredentialProvider;
use workloop_core::domain::{canonical_json, sha256_hex};
use workloop_core::identity::{ActorRole, AdapterId, HostActorId, HostCapability, RepositoryId};
use workloop_core::work::DEFAULT_PENDING_QUEUE_LIMIT;

/// The single schema version this runtime understands. A missing,
/// older, newer, or corrupt version is refused without migration —
/// the same refusal model as the store's `user_version`.
pub const SCHEMA_VERSION: u32 = 1;

/// One limit as it appears in the file: an integer, or the literal
/// string `unlimited` where the defining document permits it. Values
/// are kept raw so validation can name the exact key that carried an
/// invalid unit, sign, or zero.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum LimitInput {
    Int(i64),
    Float(f64),
    Str(String),
}

/// Whether `unlimited` is an accepted spelling for a key.
#[derive(Debug, Clone, Copy)]
enum Unlimited {
    Permitted,
    Forbidden,
}

/// Whether `0` is an accepted value for a key. `0` is never silently
/// read as "no limit"; it is refused wherever a zero-length window or
/// zero-size pass cannot express intent.
#[derive(Debug, Clone, Copy)]
enum Zero {
    Allowed,
    Refused,
}

fn checked_limit(
    key: &str,
    value: &LimitInput,
    unlimited: Unlimited,
    zero: Zero,
) -> Result<(), String> {
    match value {
        LimitInput::Int(n) if *n < 0 => Err(format!("{key}: negative limit {n} is refused")),
        LimitInput::Int(0) if matches!(zero, Zero::Refused) => Err(format!(
            "{key}: 0 is never a valid limit — a zero-length window cannot express intent \
             (use 'unlimited' where the policy permits)"
        )),
        LimitInput::Int(_) => Ok(()),
        LimitInput::Float(f) => Err(format!("{key}: {f} is not an integer limit")),
        LimitInput::Str(s) if s == "unlimited" => match unlimited {
            Unlimited::Permitted => Ok(()),
            Unlimited::Forbidden => {
                Err(format!("{key}: 'unlimited' is not permitted for this key"))
            }
        },
        // The pasted string is redacted: an operator can put anything
        // here, and a load refusal must not carry it into logs or
        // evidence. The key and the expected shape stay visible.
        LimitInput::Str(_) => Err(format!(
            "{key}: unknown limit value <redacted> — expected a positive integer or 'unlimited' (no unit suffix)"
        )),
    }
}

/// A jitter ratio as written: an integer or a float, range `0.0`–`1.0`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum RatioInput {
    Int(i64),
    Float(f64),
}

impl Default for RatioInput {
    fn default() -> Self {
        Self::Float(0.0)
    }
}

impl RatioInput {
    /// The configured ratio as a float for range validation.
    pub fn value(&self) -> f64 {
        match self {
            Self::Int(i) => *i as f64,
            Self::Float(f) => *f,
        }
    }
}

/// The on-disk `config.toml`. Every table is refused on unknown keys,
/// and every field carries the documented default when omitted.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Required; must equal [`SCHEMA_VERSION`].
    pub schema_version: u32,
    #[serde(default)]
    pub installation: InstallationSection,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSection>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleSection>,
    /// Observed identity evidence per bound role actor (see
    /// [`IdentityEvidence`]). Absent is structurally valid — `check`
    /// stage 2 is what refuses a success claim without it.
    #[serde(default)]
    pub identity_evidence: BTreeMap<String, IdentityEvidence>,
    #[serde(default)]
    pub worker: WorkerSection,
    #[serde(default)]
    pub verification: VerificationSection,
    #[serde(default)]
    pub capabilities: CapabilitiesSection,
    #[serde(default)]
    pub recovery: RecoverySection,
    #[serde(default)]
    pub retention: RetentionSection,
    /// v0.2 scheduled and one-shot selector intake
    /// (`docs/installation-config.md`, `docs/execution-loop.md`
    /// intake scheduling). Absent means automatic intake disabled with
    /// the documented defaults; the resolved snapshot and its content
    /// revision come from [`Profile::intake_settings`].
    #[serde(default)]
    pub intake: Option<IntakeSection>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSection {
    #[serde(default = "default_capacity_slots")]
    pub capacity_slots: u32,
}

impl Default for InstallationSection {
    fn default() -> Self {
        Self {
            capacity_slots: default_capacity_slots(),
        }
    }
}

fn default_capacity_slots() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSection {
    pub adapter: Option<String>,
    pub endpoint: Option<String>,
    pub contract_version: Option<u32>,
    #[serde(default)]
    pub repositories: Vec<RepositorySection>,
}

/// One repository bound under a host. `use` selects the structural
/// role: `publication` targets accept publication effects and imply
/// source reads; `source` repositories grant admission reads only.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySection {
    pub id: Option<String>,
    pub display: Option<String>,
    #[serde(rename = "use")]
    pub use_: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSection {
    pub host: Option<String>,
    pub actor: Option<String>,
    pub credential_ref: Option<String>,
}

/// The persisted observed-identity evidence for one bound role actor,
/// written by `init` from explicit observed pointers and refreshed by
/// `check --probe`. Load validates structure and role coherence only;
/// whether the evidence exists and still matches the bound actor is
/// `check` stage 2's fail-closed question — a config carrying no
/// evidence must load (so `check` can name the missing stage) but can
/// never satisfy a success claim (docs/installation-config.md setup
/// contract; `check` never reports success from config declarations
/// alone).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityEvidence {
    /// Observed actor login or application slug. Must equal the bound
    /// role's `actor` — the bound identity is the verified adapter-side
    /// identity, never a display alias.
    pub login: String,
    /// Observed host instance identity (the role's host endpoint).
    pub host_instance: String,
    /// Observed repository stable id, declared under the role's host.
    pub repository: String,
    /// When the observation was made (unix seconds). Refreshed by a
    /// passing `check --probe`.
    pub observed_unix_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    #[serde(default)]
    pub argv: Vec<String>,
    /// The approved isolation profile every worker launch binds. Absent
    /// names the reference profile; an unknown name is refused at load
    /// — a misspelled profile must never degrade into an unprofiled
    /// launch (U2.3).
    pub isolation_profile: Option<String>,
    pub runtime_version: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationSection {
    #[serde(default)]
    pub egress: EgressSection,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressSection {
    #[serde(default)]
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesSection {
    #[serde(default)]
    pub required: RequiredCapabilities,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCapabilities {
    #[serde(default)]
    pub publication: Vec<String>,
    #[serde(default)]
    pub review: Vec<String>,
    #[serde(default)]
    pub discussion: Vec<String>,
}

/// Recovery settings with the owner-approved defaults from
/// `docs/recovery-policy.md`. `unlimited` is never accepted here: the
/// recovery document defines no unlimited spelling.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySection {
    #[serde(default)]
    pub transport: TransportSection,
    #[serde(default = "default_effect_reconcile_seconds")]
    pub effect_reconcile_seconds: LimitInput,
    #[serde(default = "default_max_fix_invocations")]
    pub max_fix_invocations: LimitInput,
    #[serde(default = "default_equivalent_failure_limit")]
    pub equivalent_failure_limit: LimitInput,
    #[serde(default = "default_no_progress_seconds")]
    pub no_progress_seconds: LimitInput,
    #[serde(default = "default_automatic_time_limit_seconds")]
    pub automatic_time_limit_seconds: LimitInput,
}

impl Default for RecoverySection {
    fn default() -> Self {
        Self {
            transport: TransportSection::default(),
            effect_reconcile_seconds: default_effect_reconcile_seconds(),
            max_fix_invocations: default_max_fix_invocations(),
            equivalent_failure_limit: default_equivalent_failure_limit(),
            no_progress_seconds: default_no_progress_seconds(),
            automatic_time_limit_seconds: default_automatic_time_limit_seconds(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportSection {
    #[serde(default = "default_max_retries")]
    pub max_retries: LimitInput,
    #[serde(default = "default_backoff_seconds")]
    pub backoff_seconds: Vec<LimitInput>,
    #[serde(default)]
    pub backoff_jitter_ratio: RatioInput,
}

impl Default for TransportSection {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            backoff_seconds: default_backoff_seconds(),
            backoff_jitter_ratio: RatioInput::default(),
        }
    }
}

/// Retention settings with the owner-approved defaults from
/// `docs/retention-policy.md`. Both axes accept `unlimited`; `0`,
/// negative, and unit-suffixed values are load errors.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionSection {
    #[serde(default)]
    pub workspace: AxisLimits,
    #[serde(default)]
    pub worker_output: AxisLimits,
    #[serde(default)]
    pub cas: AxisLimits,
    #[serde(default)]
    pub handoff_objects: AxisLimits,
    #[serde(default)]
    pub archive: AxisLimits,
    #[serde(default = "default_sweep")]
    pub sweep: SweepSection,
}

impl Default for RetentionSection {
    fn default() -> Self {
        Self {
            workspace: AxisLimits::default(),
            worker_output: AxisLimits::default(),
            cas: AxisLimits::default(),
            handoff_objects: AxisLimits::default(),
            archive: AxisLimits::default(),
            sweep: default_sweep(),
        }
    }
}

/// One retention category's two limit axes. A key omitted from a
/// present table still resolves to the category default at load.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisLimits {
    #[serde(default)]
    pub max_age_seconds: Option<LimitInput>,
    #[serde(default)]
    pub max_bytes: Option<LimitInput>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepSection {
    #[serde(default = "default_sweep_interval_seconds")]
    pub interval_seconds: LimitInput,
    #[serde(default = "default_sweep_max_bodies")]
    pub max_bodies: LimitInput,
    #[serde(default = "default_sweep_max_bytes")]
    pub max_bytes: LimitInput,
    #[serde(default = "default_delete_error_limit")]
    pub delete_error_limit: LimitInput,
}

impl Default for SweepSection {
    fn default() -> Self {
        Self {
            interval_seconds: default_sweep_interval_seconds(),
            max_bodies: default_sweep_max_bodies(),
            max_bytes: default_sweep_max_bytes(),
            delete_error_limit: default_delete_error_limit(),
        }
    }
}

/// The `[intake]` table: v0.2 scheduled and one-shot selector intake.
/// Every key carries its documented default when omitted; the section
/// itself may be absent entirely, which resolves to automatic intake
/// disabled with the same defaults. Numeric keys keep the raw
/// [`LimitInput`] so validation can name the exact key that carried an
/// invalid unit, sign, or zero.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeSection {
    /// Automatic intake on the scheduler's due times. Disabled by
    /// default; the one-shot `taisan intake` verb does not consult it.
    #[serde(default)]
    pub enabled: bool,
    /// Whole minutes between scheduled intake runs.
    #[serde(default = "default_intake_interval_minutes")]
    pub interval_minutes: LimitInput,
    /// Adapter invocation deadline in seconds; at most 300 so the
    /// request deadline stays inside the selector protocol's
    /// 300,000 ms bound.
    #[serde(default = "default_intake_timeout_seconds")]
    pub timeout_seconds: LimitInput,
    /// Installation-wide pending-Work admission bound, applied inside
    /// the submit transaction to manual and selector submissions
    /// alike.
    #[serde(default = "default_intake_pending_queue_limit")]
    pub pending_queue_limit: LimitInput,
    /// The single argv selector binding. Optional: `pending_queue_limit`
    /// applies without one, and `enabled = true` is what makes it
    /// required.
    #[serde(default)]
    pub selector: Option<IntakeSelectorSection>,
}

impl Default for IntakeSection {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: default_intake_interval_minutes(),
            timeout_seconds: default_intake_timeout_seconds(),
            pending_queue_limit: default_intake_pending_queue_limit(),
            selector: None,
        }
    }
}

/// The `[intake.selector]` table: exactly one adapter binding per
/// installation, launched as a direct argv vector — never through a
/// shell. `selector_version` and `policy_revision` are bound into every
/// request and candidate exactly as the selector protocol requires.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeSelectorSection {
    /// Selector adapter identity (`[a-z][a-z0-9-]*`, the protocol's
    /// adapter-id grammar — stricter than the opaque id rules).
    pub adapter: Option<String>,
    /// Selection-semantics version, part of request identity.
    #[serde(default = "default_selector_version")]
    pub selector_version: u32,
    /// Policy document revision, part of proposal identity.
    #[serde(default)]
    pub policy_revision: u64,
    /// The direct exec vector: 1–64 nonempty strings, at most 4,096
    /// bytes each and 16,384 bytes total, no NUL or control characters
    /// (docs/selector-protocol.md framing bounds).
    pub argv: Option<Vec<String>>,
}

/// The resolved selector binding inside an [`IntakeSettings`] snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntakeSelectorBinding {
    pub adapter: String,
    pub selector_version: u32,
    pub policy_revision: u64,
    pub argv: Vec<String>,
}

/// The validated intake-settings snapshot: concrete values resolved
/// from the `[intake]` section (or the documented defaults when the
/// section is absent). `revision()` is the content digest the
/// scheduler and the store's active-settings record share — identical
/// content yields the identical revision, any change yields a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntakeSettings {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub timeout_seconds: u64,
    pub pending_queue_limit: u32,
    pub selector: Option<IntakeSelectorBinding>,
}

impl IntakeSettings {
    /// The settings content revision: SHA-256 over the canonical JSON
    /// of the complete validated snapshot. It is opaque provenance —
    /// recorded on intake runs and the store's active-settings row,
    /// never part of proposal or replay identity.
    pub fn revision(&self) -> String {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        sha256_hex(
            "intake-settings-v1",
            &canonical_json(&value),
        )
    }
}

fn default_intake_interval_minutes() -> LimitInput {
    LimitInput::Int(30)
}
fn default_intake_timeout_seconds() -> LimitInput {
    LimitInput::Int(120)
}
fn default_intake_pending_queue_limit() -> LimitInput {
    LimitInput::Int(i64::from(DEFAULT_PENDING_QUEUE_LIMIT))
}
fn default_selector_version() -> u32 {
    1
}

/// The selector protocol's selection-deadline bound: the request's
/// `deadline_unix_ms` may be at most 300,000 ms after construction, so
/// a configured timeout above 300 seconds could never be honored.
const MAX_INTAKE_TIMEOUT_SECONDS: i64 = 300;
/// The protocol's argv bounds (docs/selector-protocol.md).
const MAX_ARGV_ITEMS: usize = 64;
const MAX_ARGV_ITEM_BYTES: usize = 4096;
const MAX_ARGV_TOTAL_BYTES: usize = 16_384;
/// The protocol's adapter-id grammar: `[a-z][a-z0-9-]*`, 1–64 ASCII
/// bytes — stricter than the generic opaque-id rules.
fn check_selector_adapter_id(adapter: &str) -> Result<(), String> {
    let valid = !adapter.is_empty()
        && adapter.len() <= 64
        && adapter.is_ascii()
        && adapter
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && adapter
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase());
    if valid {
        Ok(())
    } else {
        Err("must match [a-z][a-z0-9-]* and fit 64 ASCII bytes".into())
    }
}

fn positive_int(key: &str, value: &LimitInput) -> Result<i64, String> {
    checked_limit(key, value, Unlimited::Forbidden, Zero::Refused)?;
    match value {
        LimitInput::Int(n) => Ok(*n),
        _ => unreachable!("checked_limit admits only Int"),
    }
}

fn default_max_retries() -> LimitInput {
    LimitInput::Int(3)
}
fn default_backoff_seconds() -> Vec<LimitInput> {
    vec![
        LimitInput::Int(10),
        LimitInput::Int(30),
        LimitInput::Int(90),
    ]
}
fn default_effect_reconcile_seconds() -> LimitInput {
    LimitInput::Int(600)
}
fn default_max_fix_invocations() -> LimitInput {
    LimitInput::Int(3)
}
fn default_equivalent_failure_limit() -> LimitInput {
    LimitInput::Int(2)
}
fn default_no_progress_seconds() -> LimitInput {
    LimitInput::Int(1800)
}
fn default_automatic_time_limit_seconds() -> LimitInput {
    LimitInput::Int(14400)
}
fn default_sweep_interval_seconds() -> LimitInput {
    LimitInput::Int(3600)
}
fn default_sweep_max_bodies() -> LimitInput {
    LimitInput::Int(256)
}
fn default_sweep_max_bytes() -> LimitInput {
    LimitInput::Int(1073741824)
}
fn default_delete_error_limit() -> LimitInput {
    LimitInput::Int(3)
}
fn default_sweep() -> SweepSection {
    SweepSection::default()
}

fn unlimited() -> LimitInput {
    LimitInput::Str("unlimited".into())
}

fn fill_axis(axis: &mut AxisLimits, max_age: LimitInput, max_bytes: LimitInput) {
    if axis.max_age_seconds.is_none() {
        axis.max_age_seconds = Some(max_age);
    }
    if axis.max_bytes.is_none() {
        axis.max_bytes = Some(max_bytes);
    }
}

impl Profile {
    /// Resolve every retention axis omitted from the file to its
    /// documented category default. Called before validation so the
    /// whole profile carries concrete values.
    fn apply_documented_defaults(&mut self) {
        fill_axis(
            &mut self.retention.workspace,
            LimitInput::Int(2592000),
            LimitInput::Int(21474836480),
        );
        fill_axis(
            &mut self.retention.worker_output,
            LimitInput::Int(2592000),
            LimitInput::Int(10737418240),
        );
        fill_axis(
            &mut self.retention.cas,
            LimitInput::Int(7776000),
            LimitInput::Int(42949672960),
        );
        fill_axis(
            &mut self.retention.handoff_objects,
            LimitInput::Int(7776000),
            LimitInput::Int(10737418240),
        );
        fill_axis(&mut self.retention.archive, unlimited(), unlimited());
    }

    /// Validate the deserialized profile: schema version first, then
    /// every value against the rules its owning document defines.
    /// Failures name the exact key; nothing is written anywhere.
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}: this runtime understands {SCHEMA_VERSION} only — \
                 refusing without migration",
                self.schema_version
            ));
        }
        if self.installation.capacity_slots == 0 {
            return Err(
                "installation.capacity_slots: 0 is not a valid capacity — a zero-slot \
                 installation cannot dispatch"
                    .into(),
            );
        }
        self.validate_hosts()?;
        self.validate_roles()?;
        self.validate_identity_evidence()?;
        self.validate_worker()?;
        self.validate_allowlist()?;
        self.validate_capabilities()?;
        self.validate_recovery()?;
        self.validate_retention()?;
        self.validate_intake()?;
        Ok(())
    }

    fn validate_hosts(&self) -> Result<(), String> {
        for (name, host) in &self.hosts {
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err(format!("hosts: invalid host name '{name}'"));
            }
            let adapter = host
                .adapter
                .as_deref()
                .ok_or_else(|| format!("hosts.{name}: missing 'adapter'"))?;
            AdapterId::new(adapter).map_err(|e| format!("hosts.{name}.adapter: {e}"))?;
            let endpoint = host
                .endpoint
                .as_deref()
                .ok_or_else(|| format!("hosts.{name}: missing 'endpoint'"))?;
            if endpoint.is_empty()
                || endpoint
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(format!(
                    "hosts.{name}.endpoint: must be a non-empty endpoint identity without whitespace"
                ));
            }
            let contract_version = host
                .contract_version
                .ok_or_else(|| format!("hosts.{name}: missing 'contract_version'"))?;
            if contract_version == 0 {
                return Err(format!(
                    "hosts.{name}.contract_version: 0 is never a valid binding version"
                ));
            }
            for (index, repository) in host.repositories.iter().enumerate() {
                let id = repository
                    .id
                    .as_deref()
                    .ok_or_else(|| format!("hosts.{name}.repositories[{index}]: missing 'id'"))?;
                RepositoryId::new(id)
                    .map_err(|e| format!("hosts.{name}.repositories[{index}].id: {e}"))?;
                let use_ = repository
                    .use_
                    .as_deref()
                    .ok_or_else(|| format!("hosts.{name}.repositories[{index}]: missing 'use'"))?;
                match use_ {
                    "publication" | "source" => {}
                    other => {
                        return Err(format!(
                            "hosts.{name}.repositories[{index}] (id {id}): unknown use '{other}' — \
                             expected publication or source"
                        ));
                    }
                }
                if let Some(display) = &repository.display
                    && display.chars().any(char::is_control)
                {
                    return Err(format!(
                        "hosts.{name}.repositories[{index}].display: control characters are refused"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Structural coherence of the observed-identity evidence: the key
    /// is a known role that is actually bound, and every field is a
    /// non-empty identifier without control characters. The match
    /// against the bound actor (evidence completeness for success) is
    /// deliberately `check` stage 2's question, not a load refusal —
    /// otherwise a stale observation would surface as a parse failure
    /// instead of the stage-scoped diagnostic the preflight contract
    /// requires.
    fn validate_identity_evidence(&self) -> Result<(), String> {
        for (name, evidence) in &self.identity_evidence {
            if ActorRole::from_name(name).is_none() {
                return Err(format!(
                    "identity_evidence: unknown role '{name}' — expected publication, review, or discussion"
                ));
            }
            if !self.roles.contains_key(name) {
                return Err(format!(
                    "identity_evidence.{name}: role is not declared under [roles]"
                ));
            }
            for (field, value) in [
                ("login", &evidence.login),
                ("host_instance", &evidence.host_instance),
                ("repository", &evidence.repository),
            ] {
                if value.is_empty() {
                    return Err(format!(
                        "identity_evidence.{name}.{field}: must not be empty"
                    ));
                }
                if value.chars().any(char::is_control) {
                    return Err(format!(
                        "identity_evidence.{name}.{field}: control characters are refused"
                    ));
                }
            }
            if evidence.observed_unix_seconds < 0 {
                return Err(format!(
                    "identity_evidence.{name}.observed_unix_seconds: negative observation time {} is refused",
                    evidence.observed_unix_seconds
                ));
            }
        }
        Ok(())
    }

    fn validate_roles(&self) -> Result<(), String> {
        for (name, role) in &self.roles {
            if ActorRole::from_name(name).is_none() {
                return Err(format!(
                    "roles: unknown role '{name}' — expected publication, review, or discussion"
                ));
            }
            let host = role
                .host
                .as_deref()
                .ok_or_else(|| format!("roles.{name}: missing 'host'"))?;
            if !self.hosts.contains_key(host) {
                return Err(format!(
                    "roles.{name}: host '{host}' is not declared under [hosts]"
                ));
            }
            let actor = role
                .actor
                .as_deref()
                .ok_or_else(|| format!("roles.{name}: missing 'actor'"))?;
            HostActorId::new(actor).map_err(|e| format!("roles.{name}.actor: {e}"))?;
            let credential_ref = role
                .credential_ref
                .as_deref()
                .ok_or_else(|| format!("roles.{name}: missing 'credential_ref'"))?;
            check_credential_ref(name, credential_ref)?;
        }
        Ok(())
    }

    /// The named isolation profile must resolve against the approved
    /// registry — the same resolution the dispatch path uses, so a name
    /// refused here can never be launched under.
    fn validate_worker(&self) -> Result<(), String> {
        if let Some(id) = &self.worker.isolation_profile {
            workloop_core::worker::isolation_profile(id).ok_or_else(|| {
                let approved: Vec<&str> =
                    workloop_core::worker::approved_isolation_profile_ids().collect();
                format!(
                    "worker.isolation_profile: '{id}' is not an approved profile \
                     (approved: {})",
                    approved.join(", ")
                )
            })?;
        }
        Ok(())
    }

    fn validate_allowlist(&self) -> Result<(), String> {
        for (index, entry) in self.verification.egress.allowlist.iter().enumerate() {
            if entry.is_empty() {
                return Err(format!(
                    "verification.egress.allowlist[{index}]: must not be empty"
                ));
            }
            // Rejected entries are redacted like every other scalar a
            // refusal refuses: the operator may have pasted anything
            // here, so only the key and the rule leave this function.
            if entry.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(format!(
                    "verification.egress.allowlist[{index}]: <redacted> must be a bare host name \
                     without whitespace"
                ));
            }
            if entry.contains("://") {
                return Err(format!(
                    "verification.egress.allowlist[{index}]: <redacted> must be a bare host name, \
                     not a URL"
                ));
            }
            if !is_bare_host_name(entry) {
                return Err(format!(
                    "verification.egress.allowlist[{index}]: <redacted> must be a bare host name — \
                     labels of letters, digits, and hyphens separated by dots; never a wildcard, \
                     port, or path"
                ));
            }
        }
        Ok(())
    }

    fn validate_capabilities(&self) -> Result<(), String> {
        for (role, names) in [
            ("publication", &self.capabilities.required.publication),
            ("review", &self.capabilities.required.review),
            ("discussion", &self.capabilities.required.discussion),
        ] {
            for name in names {
                if HostCapability::from_name(name).is_none() {
                    return Err(format!(
                        "capabilities.required.{role}: unknown capability '{name}'"
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_recovery(&self) -> Result<(), String> {
        let transport = &self.recovery.transport;
        checked_limit(
            "recovery.transport.max_retries",
            &transport.max_retries,
            Unlimited::Forbidden,
            Zero::Allowed,
        )?;
        if transport.backoff_seconds.is_empty() {
            return Err(
                "recovery.transport.backoff_seconds: schedule must not be empty — provide at \
                 least one wait entry"
                    .into(),
            );
        }
        for (index, entry) in transport.backoff_seconds.iter().enumerate() {
            checked_limit(
                &format!("recovery.transport.backoff_seconds[{index}]"),
                entry,
                Unlimited::Forbidden,
                Zero::Allowed,
            )?;
        }
        let jitter = transport.backoff_jitter_ratio.value();
        if !(0.0..=1.0).contains(&jitter) {
            return Err(format!(
                "recovery.transport.backoff_jitter_ratio: {jitter} is outside the range 0.0–1.0"
            ));
        }
        for (key, value) in [
            (
                "recovery.effect_reconcile_seconds",
                &self.recovery.effect_reconcile_seconds,
            ),
            (
                "recovery.max_fix_invocations",
                &self.recovery.max_fix_invocations,
            ),
            (
                "recovery.equivalent_failure_limit",
                &self.recovery.equivalent_failure_limit,
            ),
            (
                "recovery.no_progress_seconds",
                &self.recovery.no_progress_seconds,
            ),
            (
                "recovery.automatic_time_limit_seconds",
                &self.recovery.automatic_time_limit_seconds,
            ),
        ] {
            checked_limit(key, value, Unlimited::Forbidden, Zero::Refused)?;
        }
        Ok(())
    }

    fn validate_retention(&self) -> Result<(), String> {
        for (category, axis) in [
            ("workspace", &self.retention.workspace),
            ("worker_output", &self.retention.worker_output),
            ("cas", &self.retention.cas),
            ("handoff_objects", &self.retention.handoff_objects),
            ("archive", &self.retention.archive),
        ] {
            let max_age = axis
                .max_age_seconds
                .as_ref()
                .ok_or_else(|| format!("retention.{category}: missing 'max_age_seconds'"))?;
            checked_limit(
                &format!("retention.{category}.max_age_seconds"),
                max_age,
                Unlimited::Permitted,
                Zero::Refused,
            )?;
            let max_bytes = axis
                .max_bytes
                .as_ref()
                .ok_or_else(|| format!("retention.{category}: missing 'max_bytes'"))?;
            checked_limit(
                &format!("retention.{category}.max_bytes"),
                max_bytes,
                Unlimited::Permitted,
                Zero::Refused,
            )?;
        }
        let sweep = &self.retention.sweep;
        for (key, value) in [
            ("retention.sweep.interval_seconds", &sweep.interval_seconds),
            ("retention.sweep.max_bodies", &sweep.max_bodies),
            ("retention.sweep.max_bytes", &sweep.max_bytes),
            (
                "retention.sweep.delete_error_limit",
                &sweep.delete_error_limit,
            ),
        ] {
            checked_limit(key, value, Unlimited::Forbidden, Zero::Refused)?;
        }
        Ok(())
    }

    /// The `[intake]` section resolves through the same fail-closed
    /// rules as every other table: unknown keys already failed
    /// deserialization, and every value is checked here so a load
    /// refusal names the exact key. An absent section is not an error —
    /// it resolves to automatic intake disabled with the documented
    /// defaults.
    fn validate_intake(&self) -> Result<(), String> {
        self.intake_settings().map(|_| ())
    }

    /// The validated intake-settings snapshot for this profile. Absent
    /// `[intake]` yields the documented defaults with automatic intake
    /// disabled; a present section resolves every key, and
    /// `enabled = true` requires a complete `[intake.selector]` binding
    /// while the reverse is not required — `pending_queue_limit` is an
    /// installation-wide admission bound that applies with or without
    /// a selector. The snapshot's [`IntakeSettings::revision`] is the
    /// single content revision every intake consumer shares.
    pub fn intake_settings(&self) -> Result<IntakeSettings, String> {
        let section = self.intake.clone().unwrap_or_default();
        let interval_minutes = positive_int("intake.interval_minutes", &section.interval_minutes)?;
        let timeout_seconds = positive_int("intake.timeout_seconds", &section.timeout_seconds)?;
        if timeout_seconds > MAX_INTAKE_TIMEOUT_SECONDS {
            return Err(format!(
                "intake.timeout_seconds: {timeout_seconds} exceeds the selector protocol's \
                 {MAX_INTAKE_TIMEOUT_SECONDS}-second deadline bound"
            ));
        }
        let pending_queue_limit =
            positive_int("intake.pending_queue_limit", &section.pending_queue_limit)?;
        if pending_queue_limit > i64::from(u32::MAX) {
            return Err(format!(
                "intake.pending_queue_limit: {pending_queue_limit} exceeds the supported range"
            ));
        }
        let selector = section
            .selector
            .as_ref()
            .map(resolve_intake_selector)
            .transpose()?;
        if section.enabled && selector.is_none() {
            return Err(
                "intake.enabled: true requires a complete [intake.selector] binding — \
                 adapter and argv at minimum"
                    .into(),
            );
        }
        Ok(IntakeSettings {
            enabled: section.enabled,
            interval_minutes: interval_minutes as u64,
            timeout_seconds: timeout_seconds as u64,
            pending_queue_limit: pending_queue_limit as u32,
            selector,
        })
    }

    /// Repositories usable as a publication target. `use = "source"`
    /// repositories never appear: source reads never authorize
    /// publication, so a publication intent aimed at one is refused
    /// structurally before any adapter call.
    ///
    /// The host half is the host instance identity — the declared
    /// `endpoint`, never the local `[hosts.<alias>]` table key. Host
    /// identity is distinct from a display alias and a changed
    /// endpoint cannot reuse it (docs/identity-storage.md), so two
    /// aliases on one endpoint name one target and a retargeted alias
    /// names a different one. A host without an endpoint (impossible
    /// after validation) contributes no target; the alias is never
    /// substituted, and duplicate identity pairs collapse.
    pub fn publication_targets(&self) -> Vec<(String, String)> {
        let mut targets = std::collections::BTreeSet::new();
        for host in self.hosts.values() {
            let Some(endpoint) = &host.endpoint else {
                continue;
            };
            for repository in &host.repositories {
                if repository.use_.as_deref() == Some("publication")
                    && let Some(id) = &repository.id
                {
                    targets.insert((endpoint.clone(), id.clone()));
                }
            }
        }
        targets.into_iter().collect()
    }
}

/// A bare host name for the egress allowlist: ASCII labels of
/// letters, digits, and hyphens joined by dots, each label at most 63
/// bytes and never hyphen-edged, the whole entry at most 253 bytes.
/// A URL, port, path, wildcard, or any other punctuation never
/// matches — the proxy resolves these entries as exact host names on
/// the fixed CONNECT port, so a wildcard-like entry must be refused
/// at the load boundary, never deferred to the proxy.
fn is_bare_host_name(entry: &str) -> bool {
    if entry.is_empty() || entry.len() > 253 {
        return false;
    }
    entry.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Resolve `[intake.selector]` into the binding every intake request
/// carries. The section is optional as a whole, but a present section
/// must be complete: `adapter` and `argv` are required, and the argv
/// vector is checked against the selector protocol's framing bounds —
/// count, per-item and total bytes, emptiness, and control characters.
/// Rejected argv entries are redacted like every other scalar: an
/// operator may have pasted anything here.
fn resolve_intake_selector(
    section: &IntakeSelectorSection,
) -> Result<IntakeSelectorBinding, String> {
    let adapter = section
        .adapter
        .as_deref()
        .ok_or("intake.selector.adapter: missing 'adapter'")?;
    check_selector_adapter_id(adapter)
        .map_err(|rule| format!("intake.selector.adapter: {rule}"))?;
    if section.selector_version == 0 {
        return Err(
            "intake.selector.selector_version: 0 is not a valid selector version — \
             the protocol requires a positive integer"
                .into(),
        );
    }
    let argv = section
        .argv
        .as_ref()
        .ok_or("intake.selector.argv: missing 'argv'")?;
    if argv.is_empty() || argv.len() > MAX_ARGV_ITEMS {
        return Err(format!(
            "intake.selector.argv: expected 1–{MAX_ARGV_ITEMS} entries, got {}",
            argv.len()
        ));
    }
    let mut total = 0usize;
    for (index, arg) in argv.iter().enumerate() {
        if arg.is_empty() {
            return Err(format!("intake.selector.argv[{index}]: must not be empty"));
        }
        if arg.len() > MAX_ARGV_ITEM_BYTES {
            return Err(format!(
                "intake.selector.argv[{index}]: exceeds {MAX_ARGV_ITEM_BYTES} bytes"
            ));
        }
        if arg.chars().any(char::is_control) {
            return Err(format!(
                "intake.selector.argv[{index}]: <redacted> contains control characters"
            ));
        }
        total += arg.len();
    }
    if total > MAX_ARGV_TOTAL_BYTES {
        return Err(format!(
            "intake.selector.argv: total exceeds {MAX_ARGV_TOTAL_BYTES} bytes"
        ));
    }
    Ok(IntakeSelectorBinding {
        adapter: adapter.to_string(),
        selector_version: section.selector_version,
        policy_revision: section.policy_revision,
        argv: argv.clone(),
    })
}

/// Validate a `credential_ref`'s shape only — never its value. The
/// three owner-approved providers are `env:`, `file:` (absolute path;
/// `~` is never expanded), and `command:`. Refusals name the role and
/// the provider rule but never echo the reference: an operator may
/// have pasted a live token here, and the load error must not carry
/// it out of this module.
fn check_credential_ref(role: &str, credential_ref: &str) -> Result<(), String> {
    CredentialProvider::parse_str(credential_ref)
        .map(|_| ())
        .map_err(|rule| format!("roles.{role}.credential_ref: {rule}"))
}

/// Describe a syntax failure by location and parser message only. The
/// parser's `Display` embeds the offending source line verbatim; that
/// line may carry an operator-pasted credential, so it is never
/// forwarded into a refusal, log, report, or evidence record.
fn describe_syntax_error(text: &str, error: &toml::de::Error) -> String {
    let message = error.message().trim_end();
    match error.span() {
        Some(span) => {
            let (line, column) = line_column(text, span.start);
            format!("config: invalid TOML at line {line}, column {column}: {message}")
        }
        None => format!("config: invalid TOML: {message}"),
    }
}

/// 1-based line and column for a byte offset into `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut line = 1;
    let mut column = 1;
    for (index, ch) in text.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Render a typed-deserialization failure with its key path but
/// without the scalar it rejected. Serde's message can quote the
/// offending value (`invalid type: string "secret"`), so only the
/// structural "what" and "expected" parts are kept.
fn describe_data_error(error: &toml::de::Error) -> String {
    let display = error.to_string();
    let (message, path) = match display.split_once("\nin `") {
        Some((message, path)) => (message, Some(path.trim().trim_matches('`').to_string())),
        None => (display.as_str(), None),
    };
    let message = redact_serde_values(message.trim_end());
    match path {
        Some(path) => format!("config: {message} (in `{path}`)"),
        None => format!("config: {message}"),
    }
}

/// Drop operator-authored literals from a serde error message. The
/// `unknown field` name is *input*, not a schema key, and is kept
/// deliberately: naming the offending key is the CF-01 contract, so
/// an unknown key is located from the refusal exactly like any other
/// config typo. `missing field` names a schema key. None of the
/// prefixes carries a rejected *value* — the quoted and backticked
/// literals after them are what this function removes.
fn redact_serde_values(message: &str) -> String {
    for prefix in ["unknown field ", "missing field ", "duplicate field "] {
        if message.starts_with(prefix) {
            return message.to_string();
        }
    }
    if let Some(tail) = message.split_once(", expected").map(|(_, tail)| tail) {
        let kind = message
            .split_once(": ")
            .map(|(kind, _)| kind)
            .unwrap_or("invalid value");
        return format!("{kind}: <redacted>, expected{tail}");
    }
    message
        .split(['"', '`'])
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 1 {
                "<redacted>".to_string()
            } else {
                part.to_string()
            }
        })
        .collect()
}

/// Parse and validate a `config.toml` document. Read-only: this
/// function never writes, migrates, or repairs the source text.
///
/// Parsing is two-stage so refusals never echo file source: the text
/// is first parsed into a TOML value (syntax errors are reported by
/// line/column plus the parser's own bounded message), then
/// deserialized from that value, whose errors name the offending key
/// path without the raw line or any surrounding file text.
pub fn parse_profile(text: &str) -> Result<Profile, String> {
    let document: toml::Value =
        toml::from_str(text).map_err(|e| describe_syntax_error(text, &e))?;
    let mut profile: Profile = document
        .try_into()
        .map_err(|e: toml::de::Error| describe_data_error(&e))?;
    profile.apply_documented_defaults();
    profile.validate()?;
    Ok(profile)
}

/// Read and validate the profile at `path`. Never mutates the file or
/// its directory: an absent file reports `config.absent`, and a
/// present file with an unknown key, unsupported version, or invalid
/// value is refused with the file byte-identical afterwards.
pub fn load_profile(path: &Path) -> Result<Profile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "config.absent: {} does not exist — run init or supply --config",
            path.display()
        ),
        _ => format!("config read: {e}"),
    })?;
    parse_profile(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The installation paths after the fixed precedence: explicit flag →
/// environment override → XDG default. The ambient working directory
/// is never consulted; every override must be an absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub config_path: PathBuf,
    pub state_root: PathBuf,
}

/// Resolve the config and state paths through the fixed precedence.
/// `lookup` supplies environment values (injectable so precedence is
/// testable without mutating the process environment).
pub fn resolve_paths(
    config_flag: Option<&Path>,
    state_flag: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<ResolvedPaths, String> {
    Ok(ResolvedPaths {
        config_path: resolve_config_path(config_flag, lookup)?,
        state_root: resolve_state_root(state_flag, lookup)?,
    })
}

/// [`resolve_paths`] against the real process environment.
pub fn resolve_paths_from_env(
    config_flag: Option<&Path>,
    state_flag: Option<&Path>,
) -> Result<ResolvedPaths, String> {
    resolve_paths(config_flag, state_flag, &|key| std::env::var_os(key))
}

fn absolute_override(source: &str, path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(format!(
        "{source}: path must be absolute, never the ambient working directory (got '{}')",
        path.display()
    ))
}

fn resolve_config_path(
    flag: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(flag) = flag {
        return absolute_override("--config", flag);
    }
    if let Some(value) = lookup("TAISAN_CONFIG") {
        return absolute_override("TAISAN_CONFIG", Path::new(&value));
    }
    Ok(xdg_base("XDG_CONFIG_HOME", ".config", lookup)?
        .join("taisan")
        .join("config.toml"))
}

fn resolve_state_root(
    flag: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(flag) = flag {
        return absolute_override("--state-dir", flag);
    }
    if let Some(value) = lookup("TAISAN_STATE_DIR") {
        return absolute_override("TAISAN_STATE_DIR", Path::new(&value));
    }
    Ok(xdg_base("XDG_DATA_HOME", ".local/share", lookup)?.join("taisan"))
}

/// The XDG base directory: the variable when it holds an absolute
/// path (relative values are ignored per the XDG Base Directory
/// specification), otherwise `$HOME/<fallback>`.
fn xdg_base(
    variable: &str,
    fallback: &str,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(value) = lookup(variable) {
        let path = Path::new(&value);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
    }
    let home = lookup("HOME").ok_or_else(|| {
        format!("{variable}: no absolute override and HOME is unset — set {variable} or HOME")
    })?;
    let home = Path::new(&home);
    if !home.is_absolute() {
        return Err(format!(
            "HOME must be absolute, never the ambient working directory (got '{}')",
            home.display()
        ));
    }
    Ok(home.join(fallback))
}

/// The documented managed workspace root: `state_root/workspaces`
/// (`~/.local/share/taisan/workspaces/` under the XDG default).
pub fn default_workspace_root(state_root: &Path) -> PathBuf {
    state_root.join("workspaces")
}

fn is_within(child: &Path, parent: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

/// Refuse overlapping installation roots
/// (`docs/installation-config.md` installation layout): the state
/// root must never sit inside a managed workspace, the workspace root
/// must be the documented child of the state root, and the config
/// path must stay outside both managed trees. Comparison is lexical
/// on absolute paths — nothing is created, canonicalized, or touched.
pub fn validate_root_separation(
    config_path: &Path,
    state_root: &Path,
    workspace_root: &Path,
) -> Result<(), String> {
    for (name, path) in [
        ("config path", config_path),
        ("state root", state_root),
        ("workspace root", workspace_root),
    ] {
        if !path.is_absolute() {
            return Err(format!(
                "{name} must be absolute, never the ambient working directory (got '{}')",
                path.display()
            ));
        }
    }
    if is_within(state_root, workspace_root) {
        return Err(format!(
            "state root '{}' is nested inside workspace root '{}' — setup refuses a state root \
             inside a managed workspace",
            state_root.display(),
            workspace_root.display()
        ));
    }
    let documented = default_workspace_root(state_root);
    if workspace_root != documented {
        return Err(format!(
            "workspace root '{}' must be the documented managed root '{}' — workspaces live \
             under the state root",
            workspace_root.display(),
            documented.display()
        ));
    }
    if is_within(config_path, workspace_root) {
        return Err(format!(
            "config path '{}' is inside the workspace root '{}' — configuration must stay \
             outside managed workspaces",
            config_path.display(),
            workspace_root.display()
        ));
    }
    if is_within(config_path, state_root) {
        return Err(format!(
            "config path '{}' is inside the state root '{}' — configuration and state roots \
             must be distinct",
            config_path.display(),
            state_root.display()
        ));
    }
    Ok(())
}
