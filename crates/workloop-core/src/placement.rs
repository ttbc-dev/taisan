//! Executor placement (#379, docs/remote-executor.md §2.2): the
//! validated per-role policy the installation configuration resolves
//! to, the endpoint descriptor the policy names, and the immutable
//! binding every durable request records at admission.
//!
//! The decision and the record are Core-owned; Worker implementation
//! selection is independent. A binding written at admission is never
//! mutated or re-resolved — a configuration reload binds only requests
//! admitted after it, and the remote-dispatch path resolves the
//! endpoint from the stored binding, never from live configuration.
//! Credentials exist only as opaque provider references (`env:`/
//! `file:`/`command:`) resolved at use by the dispatch port; no
//! resolved value ever reaches a durable record, a log, or inspect
//! output.

use serde::{Deserialize, Serialize};

use crate::domain::InvocationRole;
use crate::domain::{canonical_json, sha256_hex};

/// The only placement schema version this build reads; an unknown or
/// missing version is a hard configuration error, never a silent
/// default.
pub const PLACEMENT_SCHEMA_VERSION: u32 = 1;

/// The executor identity the implicit all-local policy binds — the
/// coordinator host's local executor.
pub const LOCAL_EXECUTOR: &str = "local";

/// The transport kinds a static endpoint descriptor may name. The
/// restricted-SSH channel (docs/executor-transport-contract.md §3) is
/// the reference transport; the set is closed so an unknown spelling
/// is a configuration error, and extending it is a schema decision,
/// never silent acceptance.
pub const TRANSPORT_KINDS: &[&str] = &["ssh"];

/// The resolved executor target for one role or for verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementTarget {
    /// The coordinator host's local executor — the v0.3 launch path.
    Local,
    /// The configured static remote endpoint.
    Remote,
}

/// The one static remote endpoint descriptor (contract §2, #379):
/// transport kind, host, port, endpoint identity, the pinned identity
/// digest, and the credential *references* the session resolves at
/// use. Carried both by the resolved policy and verbatim by each
/// remote binding, so a bound request stays bound to the endpoint it
/// was admitted under even if the configuration later changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundEndpoint {
    /// The installation identity the session challenge presents —
    /// the same value the endpoint's configuration pins (contract §2).
    pub installation_id: String,
    /// The configured endpoint identity — the executor name the
    /// binding records and session authentication binds.
    pub endpoint_id: String,
    /// Transport kind, one of [`TRANSPORT_KINDS`].
    pub kind: String,
    /// Bare host name or IP literal the transport dials.
    pub host: String,
    pub port: u16,
    /// The pinned `endpoint_identity_digest` — the authenticated
    /// principal the installation expects (64 lowercase hex; a digest,
    /// never a secret).
    pub expected_identity_digest: String,
    /// Opaque provider reference the endpoint credential resolves
    /// through at use — never a secret value.
    pub endpoint_credential_ref: String,
    /// Opaque provider reference the coordinator credential resolves
    /// through at use — never a secret value.
    pub coordinator_credential_ref: String,
    /// Executor protocol versions the installation accepts.
    pub allowed_protocols: Vec<String>,
}

/// The validated per-role placement policy `admit_queued_role`
/// consults to stamp each request's [`PlacementBinding`]. The
/// default is all-local — the v0.3 behavior an installation with no
/// `placement` section keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementPolicy {
    /// The placement section's schema version; `0` marks the implicit
    /// all-local default no section produced.
    pub schema_version: u32,
    /// Digest over the canonical resolved policy — recorded on each
    /// binding so a later reload is auditable against what each
    /// in-flight request was bound to.
    pub config_digest: String,
    /// The one static remote endpoint, when configured.
    pub endpoint: Option<BoundEndpoint>,
    pub coding: PlacementTarget,
    pub fix: PlacementTarget,
    pub invariant_check: PlacementTarget,
    pub review: PlacementTarget,
    /// The separate verification placement (remote-executor.md §2.2).
    /// Remote verification machinery is #383's; a remote-placed
    /// verification must never run silently local meanwhile.
    pub verification: PlacementTarget,
}

impl Default for PlacementPolicy {
    /// The implicit all-local policy: `schema_version` 0 and the
    /// digest of the canonical default descriptor.
    fn default() -> Self {
        let mut policy = Self {
            schema_version: 0,
            config_digest: String::new(),
            endpoint: None,
            coding: PlacementTarget::Local,
            fix: PlacementTarget::Local,
            invariant_check: PlacementTarget::Local,
            review: PlacementTarget::Local,
            verification: PlacementTarget::Local,
        };
        policy.config_digest = policy.digest();
        policy
    }
}

impl PlacementPolicy {
    /// The placement a request for `role` binds.
    pub fn placement_for(&self, role: &InvocationRole) -> PlacementTarget {
        match role {
            InvocationRole::Coding => self.coding,
            InvocationRole::Fix => self.fix,
            InvocationRole::InvariantCheck => self.invariant_check,
            InvocationRole::Review => self.review,
        }
    }

    /// `true` when any role or verification runs on the endpoint —
    /// validation requires the descriptor exactly then.
    pub fn uses_remote(&self) -> bool {
        self.endpoint.is_some()
            && [
                self.coding,
                self.fix,
                self.invariant_check,
                self.review,
                self.verification,
            ]
            .contains(&PlacementTarget::Remote)
    }

    /// The canonical descriptor the `config_digest` covers: the
    /// resolved policy, so a differently-spelled section resolving to
    /// the same policy digests identically — the digest records what
    /// the decision was made under, not how it was written.
    fn digest_descriptor(&self) -> serde_json::Value {
        let role = |t: PlacementTarget| match t {
            PlacementTarget::Local => "local",
            PlacementTarget::Remote => "remote",
        };
        serde_json::json!({
            "schema_version": self.schema_version,
            "endpoint": self.endpoint,
            "coding": role(self.coding),
            "fix": role(self.fix),
            "invariant_check": role(self.invariant_check),
            "review": role(self.review),
            "verification": role(self.verification),
        })
    }

    /// SHA-256 over the canonical resolved-policy descriptor, under
    /// the placement tag so it can never alias another record's digest.
    pub fn digest(&self) -> String {
        sha256_hex(
            "taisan.placement.v1",
            &canonical_json(&self.digest_descriptor()),
        )
    }

    /// The immutable binding one admitted request records. The
    /// endpoint descriptor rides along self-contained for a remote
    /// target so the dispatch path never re-resolves configuration.
    pub fn binding_for(&self, role: &InvocationRole) -> PlacementBinding {
        let target = self.placement_for(role);
        let endpoint = match target {
            PlacementTarget::Remote => self.endpoint.clone(),
            PlacementTarget::Local => None,
        };
        PlacementBinding {
            executor: match &endpoint {
                Some(e) => e.endpoint_id.clone(),
                None => LOCAL_EXECUTOR.to_string(),
            },
            target,
            endpoint,
            schema_version: self.schema_version,
            config_digest: self.config_digest.clone(),
        }
    }
}

/// The immutable executor-placement decision one durable request
/// records at admission (#379): the bound executor identity, the
/// resolved target, the self-contained endpoint descriptor for a
/// remote target, and the schema version/config digest the decision
/// was made under. Like the adapter binding it is written once and
/// never retargeted — including while the request is still `pending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementBinding {
    /// Executor identity this request runs on: `local` or the
    /// endpoint's `endpoint_id`.
    pub executor: String,
    /// The resolved decision.
    pub target: PlacementTarget,
    /// The bound endpoint descriptor when `target == Remote`. `None`
    /// under a remote target is a corrupt record, never a local
    /// fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<BoundEndpoint>,
    /// Placement schema version + validated-policy digest the
    /// decision resolved under.
    pub schema_version: u32,
    pub config_digest: String,
}
