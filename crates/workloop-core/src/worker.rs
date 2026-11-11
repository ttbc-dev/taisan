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
    /// Build the export for one Invocation. A `resume` kind is refused
    /// with [`Unsupported`]; nothing is built and nothing may launch.
    pub fn build(
        invocation_id: InvocationId,
        kind: WorkerRequestKind,
        profile_id: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Result<Self, Unsupported> {
        match kind {
            WorkerRequestKind::Start => Ok(Self {
                format_version: WORKER_REQUEST_FORMAT_VERSION.into(),
                kind,
                request_id: export_request_id(&invocation_id),
                invocation_id,
                profile_id: profile_id.into(),
                workspace_root: workspace_root.into(),
            }),
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
