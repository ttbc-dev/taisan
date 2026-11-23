//! The reference `WorkerRequest` (slice U4.2): the durable export the
//! supervisor — acting in the taisan-owned shim role — produces for
//! exactly one Invocation before launch (`docs/worker-protocol.md`).
//!
//! The export is the shim boundary's only durable artifact: it is
//! committed to the store before any process is launched, and the
//! recorded `request_id` is bound to the spawned process handle in the
//! same commit that records the running Invocation. A crash between the
//! export commit and the spawn commit leaves an unbound export behind;
//! reconciliation then resolves nothing and the Execution holds
//! ownership — it is never read as stopped and never authorizes a
//! replacement.
//!
//! `resume` is not a supported request kind in v0.1. A request asking
//! for resume is refused with [`Unsupported`] before any process exists
//! and before any export is committed.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{InvocationId, canonical_json, sha256_hex};

/// The only format version this binary reads or writes.
pub const WORKER_REQUEST_FORMAT_VERSION: &str = "taisan.worker.request/1";

/// The approved reference profile recorded in an export when the
/// installation names none. Namespace and egress enforcement of the
/// profile is a separate slice; the export records the id, nothing
/// more is claimed here.
pub const DEFAULT_ISOLATION_PROFILE_ID: &str = "taisan-reference-v0.1";

/// The contract-harness profile: the fixture worker launches under the
/// same permission boundary as the reference profile. The distinct id
/// lets a suite distinguish "the installation named a profile" from the
/// absent-key default.
pub const FIXTURE_ISOLATION_PROFILE_ID: &str = "fixture-worker-v0.1";

/// One approved isolation profile. The registry is closed: a
/// `worker_isolation_profile` value outside this set is a
/// misconfiguration that must fail closed at validation and at launch,
/// never a silent downgrade to unprofiled execution
/// (`docs/worker-protocol.md` selected profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationProfile {
    /// The recorded profile identity — the export carries it verbatim.
    pub id: &'static str,
    /// Environment variable names a launch may pass through from the
    /// supervisor process. Everything not listed here and not generated
    /// is absent from the worker's environment — the environment is an
    /// allowlist, never an inheritance.
    pub env_passthrough: &'static [&'static str],
    /// The sandbox primitive this profile's launches must run under,
    /// when the profile declares one (`docs/v0.3.md` §14, OP-1). `None`
    /// marks the contract-harness profile: its launches are test
    /// evidence only and never prove namespace enforcement. A profile
    /// that names a primitive refuses dispatch when the primitive or
    /// its recorded preflight evidence is absent — never a hygiene-only
    /// substitute.
    pub sandbox_primitive: Option<&'static str>,
    /// Declared host CA trust-store locations bound read-only at their
    /// own path inside the sandbox, when they exist on the host —
    /// bundle files *and* hashed directories the runtimes' TLS stacks
    /// look up by compiled-in default. A declared path absent on the
    /// host is skipped, never mounted from elsewhere: the sandbox sees
    /// the trust store only at the path runtimes already expect.
    pub ca_trust_paths: &'static [&'static str],
    /// The declared commit identity `(name, email)` a launch's
    /// environment carries as `GIT_AUTHOR_*`/`GIT_COMMITTER_*` — the
    /// sandbox has no `/etc/passwd` and git config is sealed, so the
    /// runtime's local commits need an identity that arrives through
    /// declared configuration, not ambient git config. `None` leaves
    /// the role without a default identity.
    pub commit_identity: Option<(&'static str, &'static str)>,
}

/// The names every approved profile lets through: enough for process
/// and locale setup, none of them credential-bearing.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];

/// The conventional CA trust-store locations across the
/// distributions the reference profile supports: Debian/Ubuntu,
/// Alpine/OpenBSD, RHEL/Fedora (both layouts), openSUSE. Each is
/// bound at its own path read-only when it exists on the host —
/// absent entries are skipped, never relocated.
const REFERENCE_CA_TRUST_PATHS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/certs",
    "/etc/ssl/cert.pem",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/pki/tls/certs",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
];

/// The reference profile's declared commit identity: the shim's own
/// commits inside the sandbox name the role that made them.
const REFERENCE_COMMIT_IDENTITY: (&str, &str) = ("taisan-worker", "taisan-worker@localhost");

/// The closed registry of approved profiles. `isolation_profile` is the
/// single resolution point shared by config validation and the
/// supervisor so a name refused at load can never be launched under.
const APPROVED_PROFILES: &[IsolationProfile] = &[
    IsolationProfile {
        id: DEFAULT_ISOLATION_PROFILE_ID,
        env_passthrough: ENV_PASSTHROUGH,
        sandbox_primitive: Some("bwrap"),
        ca_trust_paths: REFERENCE_CA_TRUST_PATHS,
        commit_identity: Some(REFERENCE_COMMIT_IDENTITY),
    },
    IsolationProfile {
        id: FIXTURE_ISOLATION_PROFILE_ID,
        env_passthrough: ENV_PASSTHROUGH,
        sandbox_primitive: None,
        ca_trust_paths: &[],
        commit_identity: None,
    },
];

/// Resolve an approved profile id. `None` means "not approved" —
/// callers must refuse, never fall back to an unprofiled launch.
pub fn isolation_profile(id: &str) -> Option<IsolationProfile> {
    APPROVED_PROFILES
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
}

/// The id an installation that names no profile runs under.
pub fn default_isolation_profile() -> IsolationProfile {
    isolation_profile(DEFAULT_ISOLATION_PROFILE_ID).expect("the reference profile is registered")
}

/// Every approved profile id, for error messages and listings.
pub fn approved_isolation_profile_ids() -> impl Iterator<Item = &'static str> {
    APPROVED_PROFILES.iter().map(|profile| profile.id)
}

/// The request kinds the reference boundary accepts. `resume` is
/// listed so an incoming request asking for it is refused with
/// [`Unsupported`] rather than silently dropped or reinterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRequestKind {
    Start,
    Resume,
}

/// A refused request. v0.1 supports no runtime session resume: a
/// continuation is always a new Invocation whose inputs reference the
/// retained workspace and prior artifacts, never a runtime session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// Bounded reason code for the boundary; never runtime prose.
    pub reason: String,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported: {}", self.reason)
    }
}

impl std::error::Error for Unsupported {}

/// The deterministic request identity derived from the Invocation it
/// belongs to: the same `invocation_id` always yields the same
/// `request_id`, and distinct Invocations never collide.
pub fn export_request_id(invocation_id: &InvocationId) -> String {
    format!("worker:{}", invocation_id.0)
}

/// The durable request export. Four fields are the contract
/// (`invocation_id`, `request_id`, `profile_id`, `workspace_root`);
/// `kind` and `format_version` make the payload self-describing at the
/// shim boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub format_version: String,
    pub kind: WorkerRequestKind,
    pub invocation_id: InvocationId,
    /// Derived: [`export_request_id`]`(invocation_id)`. Stored rather
    /// than recomputed so a replay can read it back verbatim.
    pub request_id: String,
    pub profile_id: String,
    pub workspace_root: String,
}

impl WorkerRequest {
    /// Build the export for a `start` request. Infallible: the only
    /// failure mode of [`WorkerRequest::build`] is a `resume` kind,
    /// which this constructor cannot produce.
    pub fn start(
        invocation_id: InvocationId,
        profile_id: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        Self {
            format_version: WORKER_REQUEST_FORMAT_VERSION.into(),
            kind: WorkerRequestKind::Start,
            request_id: export_request_id(&invocation_id),
            invocation_id,
            profile_id: profile_id.into(),
            workspace_root: workspace_root.into(),
        }
    }

    /// Build the export for one Invocation. A `resume` kind is refused
    /// with [`Unsupported`]; nothing is built and nothing may launch.
    pub fn build(
        invocation_id: InvocationId,
        kind: WorkerRequestKind,
        profile_id: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Result<Self, Unsupported> {
        match kind {
            WorkerRequestKind::Start => Ok(Self::start(invocation_id, profile_id, workspace_root)),
            WorkerRequestKind::Resume => Err(Unsupported {
                reason: "resume is not supported in v0.1; a continuation is a new Invocation"
                    .into(),
            }),
        }
    }

    /// Canonical payload bytes: object keys sorted recursively, UTF-8,
    /// no insignificant whitespace.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_json(&serde_json::to_value(self).expect("WorkerRequest serializes"))
    }

    /// SHA-256 over the canonical payload, tagged for this schema.
    pub fn digest(&self) -> String {
        sha256_hex("worker-request-v1", &self.canonical_bytes())
    }
}

/// The store's durable record of one export. `state` advances
/// `exported` → `bound` when the launch commits the spawned identity;
/// an `exported` row whose Invocation never launched is the crash
/// window reconciliation must tolerate — never a stop proof, never a
/// release, never a reason to spawn a replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequestExport {
    pub invocation_id: InvocationId,
    pub request_id: String,
    pub profile_id: String,
    pub workspace_root: String,
    /// SHA-256 over the canonical `WorkerRequest` payload.
    pub digest: String,
    /// `exported` until the launch commits the spawned identity.
    pub state: WorkerRequestExportState,
    /// The spawned process identity (`<pid>@<started_at>`), present
    /// only once `state` is `bound`.
    pub process_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRequestExportState {
    Exported,
    Bound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation_id(n: &str) -> InvocationId {
        InvocationId(format!("inv-{n}"))
    }

    #[test]
    fn export_request_id_is_deterministic() {
        let id = invocation_id("1");
        assert_eq!(export_request_id(&id), export_request_id(&id));
    }

    #[test]
    fn export_request_id_does_not_collide() {
        assert_ne!(
            export_request_id(&invocation_id("1")),
            export_request_id(&invocation_id("2"))
        );
    }

    #[test]
    fn start_builds_the_export() {
        let request = WorkerRequest::build(
            invocation_id("7"),
            WorkerRequestKind::Start,
            "profile-a",
            "/srv/ws",
        )
        .expect("start is supported");
        assert_eq!(request.format_version, WORKER_REQUEST_FORMAT_VERSION);
        assert_eq!(request.request_id, export_request_id(&invocation_id("7")));
        assert_eq!(request.profile_id, "profile-a");
        assert_eq!(request.workspace_root, "/srv/ws");
        assert_eq!(request.digest(), request.digest());
        assert_ne!(
            request.digest(),
            request.canonical_bytes().len().to_string()
        );
    }

    #[test]
    fn start_is_infallible_and_matches_build_start() {
        let a = WorkerRequest::start(invocation_id("9"), "p", "/ws");
        let b = WorkerRequest::build(invocation_id("9"), WorkerRequestKind::Start, "p", "/ws")
            .expect("build start");
        assert_eq!(a, b);
    }

    #[test]
    fn resume_is_refused_with_unsupported() {
        let refused = WorkerRequest::build(
            invocation_id("8"),
            WorkerRequestKind::Resume,
            "profile-a",
            "/srv/ws",
        )
        .expect_err("resume is refused");
        assert!(
            matches!(&refused, Unsupported { reason } if reason.contains("resume")),
            "refusal names the unsupported kind: {refused:?}"
        );
    }
}
