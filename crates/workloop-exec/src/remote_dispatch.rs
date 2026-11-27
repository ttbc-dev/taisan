//! The executor-placement dispatch seam (`docs/remote-executor.md`
//! §2.2, #379): the port one remote-bound request's first launch
//! crosses. The bound endpoint descriptor's credential references
//! resolve at session use inside `workloop-executor`
//! (`CoordinatorEndpoint::resolve`) — never into a durable record,
//! log, or inspect output.
//!
//! Placement decides *where* an Invocation runs; this module owns
//! only the seam that decision crosses. Everything after the decision
//! — session drivers, transfer, prepare/start, reconnect, fencing,
//! stop reconciliation — is the remote-lifecycle machinery #378 and
//! #380 build behind the same port. Until a driver is wired, a
//! remote-bound request refuses as `request.placement_unavailable`:
//! the bound verdict is an Attention, never a Fault-retry and never a
//! silent local launch under the same request identity.

use std::path::Path;

use workloop_core::domain::{InvocationId, InvocationRequest};
use workloop_core::store::{Store, StoreError, TxView};
use workloop_core::worker_v2::WorkerRequestBody;

use crate::request_v2::V2Dispatch;

/// The durable-store surface the dispatch port records against:
/// transactions only — the supervisor drives the port per request,
/// so scanning stays where it already lives. `Store::transact` is
/// generic and not object-safe; this erased-result shape is what the
/// trait object needs — the closure carries any result out through
/// its own capture.
pub trait RemoteStore {
    fn transact(
        &self,
        f: &mut dyn FnMut(&mut dyn TxView) -> Result<(), StoreError>,
    ) -> Result<(), StoreError>;
}

impl<S: Store + ?Sized> RemoteStore for S {
    fn transact(
        &self,
        f: &mut dyn FnMut(&mut dyn TxView) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        Store::transact(self, |tx| f(tx))
    }
}

/// The launch context one remote-bound request carries across the
/// port — coordinator-side configuration that is *not* part of the
/// stored binding. The binding's endpoint descriptor travels on the
/// request itself, so a configuration reload can never retarget a
/// bound request.
pub struct RemoteLaunch<'a> {
    /// The v2 dispatch profile the body was built under — the
    /// complete bound-request transport is the only remote dispatch
    /// path.
    pub dispatch: &'a V2Dispatch,
    /// The installation's handoff repository, when the request binds
    /// an acknowledged predecessor the export bundle resolves from.
    pub handoff_repo: Option<&'a Path>,
    /// Coordinator-side bounded scratch root for export staging —
    /// transferred input crosses as content-addressed bodies, never a
    /// coordinator path.
    pub scratch_root: &'a Path,
    pub now_unix_seconds: i64,
}

/// What a committed remote dispatch produced.
#[derive(Debug)]
pub struct RemoteDispatchOutcome {
    /// The Invocation the remote run answers to — recorded by the
    /// port inside its own commit.
    pub invocation_id: InvocationId,
    /// The remote process identity the start receipt proved,
    /// verbatim — reconcile (#378) inspects only what the endpoint's
    /// receipts carry, never a locally imagined identity.
    pub process_identity: String,
    /// The incarnation the session bound — the fencing input an
    /// executor-reboot detection compares against (contract §8).
    pub bound_incarnation_id: String,
}

/// Why a remote dispatch did not commit. The two arms are the
/// placement failure semantics #379 specifies: `Unavailable` is the
/// terminal bound verdict for this `request_id`; `Fault` is
/// coordinator-side infrastructure the next pass resumes from the
/// durable executor rows under.
#[derive(Debug)]
pub enum RemoteDispatchError {
    /// The bound endpoint cannot be reached or bound: the session
    /// refused, a credential reference would not resolve, or the
    /// transport failed before the remote Invocation was proven
    /// started. The caller records a `placement_unavailable` dispatch
    /// refusal — a named Attention, never a local re-dispatch.
    Unavailable { detail: String },
    /// Coordinator-side infrastructure (store, filesystem, git)
    /// failed before the verdict was reachable. The request stays
    /// pending; the durable executor rows make a later pass a resume,
    /// never a second remote start under a new identity.
    Fault(String),
}

impl std::fmt::Display for RemoteDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { detail } => write!(f, "endpoint unavailable: {detail}"),
            Self::Fault(detail) => write!(f, "remote dispatch fault: {detail}"),
        }
    }
}

impl std::error::Error for RemoteDispatchError {}

/// The port a remote-bound request's first launch crosses. The
/// driver behind it owns the whole remote-launch sequence — session
/// authentication under the *stored* endpoint descriptor, the durable
/// executor binding, input transfer, `prepare` and `start` — and
/// records every command/receipt against the durable executor tables.
///
/// `Ok` means the commit landed: the `Invocation` row, the consumed
/// mark, and the durable executor records exist, and the returned
/// outcome carries the proven remote identities. A call that fails
/// after partial commits must leave the durable rows such that a
/// later `dispatch` of the same request resumes, never double-spawns —
/// the endpoint's replay fence recognizes a re-issued operation id.
pub trait RemoteDispatch: Send + Sync {
    fn dispatch(
        &self,
        store: &dyn RemoteStore,
        request: &InvocationRequest,
        invocation_id: &InvocationId,
        body: &WorkerRequestBody,
        launch: &RemoteLaunch<'_>,
    ) -> Result<RemoteDispatchOutcome, RemoteDispatchError>;
}
