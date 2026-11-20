//! Bounded collection of `taisan.worker.report/2` evidence
//! (`docs/v0.3.md` §6). Runs only after the Invocation's process tree
//! is proven stopped — `finish_invocation` is the single caller — so a
//! report is never read from a writer that could still change it.
//!
//! The collector reads `report.json` and `runtime.log` from the
//! Invocation-owned out directory under hard bounds taken from the
//! durable request's own budget (never current configuration), retains
//! the exact bytes in the CAS body store, classifies the envelope
//! against the recorded request binding, and returns the observation
//! fact the supervisor delivers. Claims inside the report stay claims:
//! the fact carries the supervisor's classification and body
//! references, never lifecycle authority.
//!
//! Idempotency and conflict: the first accepted `WorkerReport` fact is
//! never overwritten. A repeated collection that observes identical
//! report evidence replays as a store-level duplicate; different bytes
//! under the same Invocation identity are retained as a
//! `WorkerReportConflict` observation — the accepted evidence stands.

use std::io::Read;
use std::path::{Path, PathBuf};

use workloop_core::domain::{Invocation, InvocationRequest};
use workloop_core::observation::{ObservationFact, WorkerReportEvidence};
use workloop_core::store::{BodyCatalog, Store, StoreError};
use workloop_core::worker_v2::{
    ReportBinding, ReportClassification, WorkerRequestV2, classify_report,
};
use workloop_store::retention::CategoryLimits;

use crate::bodies::{BodyStore, CAS_CATEGORY};
use crate::isolation;
use crate::request_v2::invocation_io_root;
use crate::supervisor::ExecError;

/// What a bounded read of one worker-output file produced.
enum BoundedRead {
    /// No directory entry exists at the path.
    Absent,
    /// The entry exists but is not a regular file — a symlink, a
    /// directory, or any other non-file type. Never followed, never
    /// read: the worker's out surface is untrusted.
    Unsafe,
    /// A regular file was read. `bytes` holds at most `bound + 1`
    /// bytes so oversize is detectable without an unbounded read;
    /// `retained` is the bounded prefix actually kept as evidence.
    Present {
        bytes: Vec<u8>,
        retained: Vec<u8>,
        truncated: bool,
    },
    /// The path could not be read — a stat, open, or read failure on
    /// the worker-owned surface (an unreadable `out` tree, a
    /// permission-denied `report.json`). Classified like the unsafe
    /// case: the worker's output is invalid, never an infrastructure
    /// fault that would wedge the finish path on every pass.
    Unreadable,
}

/// Read `path` as a regular file capped at `bound + 1` bytes. The
/// metadata check runs on the link itself (`symlink_metadata`) so a
/// symlink is classified unsafe before any open follows it. Read
/// errors on the untrusted surface classify, they do not propagate —
/// a worker that made its report unreadable produced an invalid
/// report, and the Invocation still finishes.
fn read_bounded(path: &Path, bound: u64) -> BoundedRead {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BoundedRead::Absent,
        Err(_) => return BoundedRead::Unreadable,
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return BoundedRead::Unsafe;
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path)
        .and_then(|file| file.take(bound.saturating_add(1)).read_to_end(&mut bytes));
    if read.is_err() {
        return BoundedRead::Unreadable;
    }
    let truncated = bytes.len() as u64 > bound;
    let keep = (bound as usize).min(bytes.len());
    let retained = bytes[..keep].to_vec();
    BoundedRead::Present {
        bytes,
        retained,
        truncated,
    }
}

/// The out directory must itself be a real directory — a symlinked or
/// non-directory `out` would let the worker point collection outside
/// the Invocation-owned tree.
fn out_dir_safe(out_dir: &Path) -> Result<bool, ExecError> {
    match std::fs::symlink_metadata(out_dir) {
        Ok(meta) => Ok(meta.is_dir() && !meta.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(ExecError::Io(e)),
    }
}

/// Retain `bytes` in the CAS and return the evidence reference the
/// fact carries. The catalogue horizon matches the phase-1 snapshot
/// retention rule: the `cas` category's documented max-age from now.
fn retain_evidence(
    bodies: &BodyStore,
    catalog: &dyn BodyCatalog,
    bytes: &[u8],
    truncated: bool,
    now_unix_seconds: i64,
) -> Result<WorkerReportEvidence, ExecError> {
    let horizon = CategoryLimits::documented_default(CAS_CATEGORY)
        .and_then(|limits| limits.max_age)
        .map(|max_age| now_unix_seconds.saturating_add(max_age));
    let body_id = bodies
        .retain(catalog, CAS_CATEGORY, bytes, now_unix_seconds, horizon)
        .map_err(|e| ExecError::Fault(format!("retain worker report body: {e}")))?;
    Ok(WorkerReportEvidence {
        body_id,
        bytes: bytes.len() as u64,
        truncated,
    })
}

/// Collect the v2 report evidence for one finished Invocation and
/// return the observation fact to deliver — `WorkerReport` for the
/// first accepted collection, `WorkerReportConflict` when the bytes
/// under the Invocation's identity changed, `None` when the request
/// carries no v2 body (a v1 invocation) or identical evidence was
/// already accepted (idempotent replay).
///
/// `request` is the durable `InvocationRequest`; the v2-ness of the
/// dispatch is decided by its recorded `worker_request_body` row, so
/// collection follows the recorded binding, never the current
/// dispatch configuration.
pub fn collect(
    store: &impl Store,
    bodies: Option<&BodyStore>,
    workspace_root: &Path,
    invocation: &Invocation,
    request: &InvocationRequest,
    now_unix_seconds: i64,
) -> Result<Option<ObservationFact>, ExecError> {
    let body = store.transact(|tx| tx.worker_request_body(&request.request_id))?;
    let Some(body) = body else {
        // A legacy v1 dispatch has no `taisan.worker.report/2`
        // surface — nothing to collect.
        return Ok(None);
    };
    if !body.verifies() || body.format_version != workloop_core::worker_v2::WORKER_REQUEST_V2_FORMAT
    {
        return Err(ExecError::Store(StoreError::Internal {
            detail: format!(
                "worker request body for {} is corrupt or non-v2",
                request.request_id
            ),
        }));
    }
    let wire = WorkerRequestV2::parse(&body.canonical_body).map_err(|e| {
        ExecError::Store(StoreError::Internal {
            detail: format!("worker request body does not parse: {e}"),
        })
    })?;
    // Evidence retention needs the CAS filesystem root; without it the
    // classification could not be preserved — fail the pass rather
    // than record evidence that cannot resolve.
    let Some(bodies) = bodies else {
        return Err(ExecError::Fault(
            "worker report collection requires the CAS body store".into(),
        ));
    };
    let catalog = store.body_catalog();

    let out_dir: PathBuf = invocation_io_root(workspace_root)
        .join(isolation::sanitize_key(&invocation.id.0))
        .join("out");
    // The worker-owned out surface is untrusted: a stat failure on
    // the directory itself is the same invalid-report outcome as a
    // symlinked one — classified, never an abort that wedges the
    // finish path.
    let out_reason = match out_dir_safe(&out_dir) {
        Ok(true) => None,
        Ok(false) => Some("report.unsafe_path"),
        Err(ExecError::Io(_)) => Some("report.unreadable"),
        Err(e) => return Err(e),
    };

    let (classification, report_evidence) = if let Some(reason) = out_reason {
        (
            ReportClassification::Invalid {
                reasons: vec![reason.into()],
            },
            None,
        )
    } else {
        match read_bounded(&out_dir.join("report.json"), wire.budget.max_report_bytes) {
            BoundedRead::Absent => (ReportClassification::Absent, None),
            BoundedRead::Unsafe => (
                ReportClassification::Invalid {
                    reasons: vec!["report.unsafe_path".into()],
                },
                None,
            ),
            BoundedRead::Unreadable => (
                ReportClassification::Invalid {
                    reasons: vec!["report.unreadable".into()],
                },
                None,
            ),
            BoundedRead::Present {
                bytes,
                retained,
                truncated,
            } => {
                let classified = classify_report(
                    &bytes,
                    &ReportBinding {
                        request_id: &wire.request_id,
                        invocation_id: &invocation.id.0,
                        request_digest: &wire.request_digest,
                        role: wire.role,
                    },
                    wire.budget.max_report_bytes,
                );
                let evidence =
                    retain_evidence(bodies, catalog, &retained, truncated, now_unix_seconds)?;
                (classified.classification, Some(evidence))
            }
        }
    };

    // The log is auxiliary evidence collected under its own bound —
    // an absent, unsafe, or unreadable log never changes the report's
    // class; the measured retention is what the fact records.
    let log_evidence = if out_reason.is_none() {
        match read_bounded(&out_dir.join("runtime.log"), wire.budget.max_log_bytes) {
            BoundedRead::Present {
                retained,
                truncated,
                ..
            } => Some(retain_evidence(
                bodies,
                catalog,
                &retained,
                truncated,
                now_unix_seconds,
            )?),
            _ => None,
        }
    } else {
        None
    };

    // The first accepted report evidence owns the Invocation's report
    // slot. Identical evidence replays silently; different bytes —
    // including evidence appearing where `absent` was accepted or
    // disappearing after acceptance — are a retained conflict, never
    // an overwrite.
    let prior = store.transact(|tx| {
        Ok::<_, StoreError>(
            tx.observations_for_execution(&request.execution_id)?
                .into_iter()
                .find(|o| {
                    matches!(
                        &o.fact,
                        ObservationFact::WorkerReport { invocation_id, .. }
                            if invocation_id == &invocation.id
                    )
                }),
        )
    })?;
    if let Some(prior) = prior {
        let ObservationFact::WorkerReport {
            report: accepted, ..
        } = prior.fact
        else {
            unreachable!("filtered to WorkerReport facts")
        };
        if accepted == report_evidence {
            // Same report identity already accepted — idempotent
            // replay; a differing measured log is not a new report.
            return Ok(None);
        }
        return Ok(Some(ObservationFact::WorkerReportConflict {
            invocation_id: invocation.id.clone(),
            request_id: request.request_id.clone(),
            accepted_report: accepted,
            observed_report: report_evidence,
        }));
    }

    Ok(Some(ObservationFact::WorkerReport {
        invocation_id: invocation.id.clone(),
        request_id: request.request_id.clone(),
        classification,
        report: report_evidence,
        log: log_evidence,
    }))
}
