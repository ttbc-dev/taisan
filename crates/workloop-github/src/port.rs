//! The GitHub-integration port (`docs/module-contracts.md`): inbound
//! `ingest(Observation) → IngestReceipt`, outbound recorded
//! `HostActionIntent`s drained into result Observations. The slice-0
//! fixture and the real adapter both implement it, so one contract
//! test drives both through the same two calls.
//!
//! The port never mutates Execution state itself — every inbound fact
//! goes through `work::handle_observation`, so deduplication, outbox
//! reconciliation, and lifecycle assessment land in the same durable
//! pipeline regardless of which adapter delivered the fact.

use workloop_core::observation::Observation;
use workloop_core::store::{Store, StoreError};
use workloop_core::work::{IngestReceipt, WorkloopConfig};

/// One host integration behind the two-sided port. Implementations
/// perform no write that is not recorded as a `HostActionIntent` first,
/// and no inbound fact that is not an `Observation`.
pub trait IntegrationPort {
    /// The inbound half: deliver one Observation through the shared
    /// ingest pipeline. A repeated source event deduplicates to
    /// `IngestReceipt::Duplicate`; it never records a second effect.
    fn ingest(
        &self,
        store: &impl Store,
        config: &WorkloopConfig,
        observation: Observation,
        now_unix_seconds: i64,
    ) -> Result<IngestReceipt, StoreError>;

    /// The outbound half: attempt every outstanding (`pending`)
    /// `HostActionIntent` and record its result as a
    /// `host_action_result` Observation keyed by action ID. An `unknown`
    /// result stays outstanding until reconciled by action ID — a later
    /// pass never replays it blindly.
    fn apply_outbox(
        &mut self,
        store: &impl Store,
        config: &WorkloopConfig,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError>;
}
