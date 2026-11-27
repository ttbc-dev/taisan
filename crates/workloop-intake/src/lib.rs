//! The intake coordinator (#203) and its restart recovery (#208):
//! one bounded invocation that preflights pending-queue capacity and
//! source exclusions, launches the configured selector argv through
//! the bounded exchange, validates the whole response under the
//! selector protocol, and admits each candidate through the ordinary
//! `submit` command path — plus the pass that reconciles durable runs
//! whose owner is proven gone.
//!
//! Durable contract: `docs/intake-identity-storage.md`. The
//! coordinator is the only writer of intake records; the selector
//! itself never sees the store. Every candidate submit travels
//! [`workloop_selector::admission::submit_command`] under its
//! deterministic replay key, so at most one Work exists per accepted
//! request identity across ticks, response loss, reordering, and
//! restart.
//!
//! [`scheduler`] holds the clock-driven due decision the `taisan run`
//! driver ticks every 60 seconds — process-local by design; durable
//! truth stays in the store.

pub mod scheduler;

use std::path::Path;
use std::time::Duration;

use workloop_core::command::{CommandDecision, CommandId, RejectionCode, payload_digest};
use workloop_core::domain::{
    Actor, EventSequence, Origin, ProcessIdentity, WorkId, canonical_json, new_id, sha256_hex,
};
use workloop_core::identity::{IssueRef, RepositoryRef};
use workloop_core::ports::ProcessControlPort;
use workloop_core::store::{
    Effect, IntakeCandidate, IntakeCandidateStatus, IntakeRun, IntakeRunState, IntakeTrigger,
    Store, StoreError,
};
use workloop_core::work::{DEFAULT_PENDING_QUEUE_LIMIT, WorkloopConfig, handle_command};
use workloop_exec::bounded::{self, ExchangeError, SELECTOR_EXCHANGE_BOUNDS};
use workloop_exec::supervisor::PosixProcessControl;
use workloop_selector::admission;
use workloop_selector::protocol::{self, FailureCode, SelectorReply, Violation};
use workloop_selector::{Proposal, proposal_semantic_json};
use workloop_store::sqlite::{RunLeaseError, RunLeaseOwner, SqliteStore};

/// The configured argv selector binding one installation carries —
/// the `[intake.selector]` profile section resolved to concrete
/// values.
#[derive(Debug, Clone)]
pub struct SelectorBinding {
    /// Selector adapter identity (`[a-z][a-z0-9-]*`).
    pub adapter: String,
    /// Selection-semantics version, part of request identity.
    pub selector_version: u32,
    /// Policy document revision, part of proposal identity.
    pub policy_revision: u64,
    /// The direct exec vector — never through a shell.
    pub argv: Vec<String>,
}

/// Everything one intake invocation needs from its caller. The
/// coordinator owns the lease, the durable records, the exchange, and
/// the per-candidate submits; the caller supplies identity, scope,
/// and bounds it cannot invent.
pub struct IntakeParams<'a> {
    /// Which path requested this invocation.
    pub trigger: IntakeTrigger,
    pub binding: &'a SelectorBinding,
    /// The installation actor selector submits replay under.
    pub actor: &'a Actor,
    /// The resolved installation configuration `submit` decides
    /// against — the same value manual commands use.
    pub work_config: &'a WorkloopConfig,
    /// The complete repository scope the request advertises.
    pub repositories: &'a [RepositoryRef],
    pub task_constraints: &'a [String],
    pub verification_argv: &'a [String],
    pub required_permissions: &'a [String],
    /// Opaque intake-settings revision for audit provenance.
    pub settings_revision: Option<&'a str>,
    /// The adapter's working directory.
    pub cwd: &'a Path,
    /// Adapter invocation deadline (the request's
    /// `deadline_unix_ms` derives from it).
    pub timeout: Duration,
}

/// Why an invocation produced no report. Every variant is bounded and
/// payload-free.
#[derive(Debug)]
pub enum IntakeError {
    /// Another invocation holds the installation intake lease.
    Busy { owner_pid: i32 },
    /// A durable `open`/`admitting` run already exists — its owner is
    /// either live or awaiting recovery; a second run is never
    /// recorded alongside it.
    ActiveRun { run_id: String },
    /// The lease could not be granted or this process's identity
    /// could not be recorded.
    Lease { reason: String },
    /// The request could not be built within protocol bounds — a
    /// configuration or caller error, never an adapter outcome.
    Request { rule: String },
    /// A store fault; the run record may or may not reflect it.
    Store(StoreError),
}

impl From<StoreError> for IntakeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// One candidate's recorded outcome, read back from the durable rows
/// after admission — never from in-memory intent.
#[derive(Debug, Clone)]
pub struct CandidateReport {
    pub ordinal: u32,
    pub proposal_key: String,
    pub replay_key: String,
    pub status: IntakeCandidateStatus,
    pub status_reason: Option<String>,
    pub command_id: Option<CommandId>,
    pub work_id: Option<WorkId>,
}

/// The durable outcome of one invocation.
#[derive(Debug)]
pub struct IntakeReport {
    pub run_id: String,
    /// The run's recorded terminal-or-active state after the call.
    pub state: IntakeRunState,
    pub detail_code: Option<String>,
    /// Whether the adapter process was actually launched.
    pub launched: bool,
    /// Candidates in original ordinal order.
    pub candidates: Vec<CandidateReport>,
}

const REQUEST_TAG: &str = "intake-request-v1";

/// Run one intake invocation end to end. Acquires the installation
/// intake lease, records the run, launches the adapter once through
/// the bounded exchange, validates the whole response, and admits
/// candidates in returned order through the ordinary submit path.
///
/// `now_unix_seconds` stamps durable records; `now_unix_ms` bounds
/// the request deadline. Both are caller-supplied so tests and the
/// scheduler drive one clock.
pub fn run_intake(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    now_unix_seconds: i64,
    now_unix_ms: i64,
) -> Result<IntakeReport, IntakeError> {
    let owner = new_lease_owner()?;
    run_intake_with_owner(store, params, now_unix_seconds, now_unix_ms, owner)
}

/// [`run_intake`] under a caller-supplied lease-owner identity. The
/// CLI driver records the identity before spawning the invocation
/// thread, so a run left non-terminal by a panicking or faulted
/// invocation can later be reconciled by its exact nonce rather than
/// by pid liveness alone — the owning process is still alive.
pub fn run_intake_with_owner(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    now_unix_seconds: i64,
    now_unix_ms: i64,
    owner: RunLeaseOwner,
) -> Result<IntakeReport, IntakeError> {
    let lease = IntakeLease::acquire(store, owner, now_unix_seconds)?;

    // The replay prefix identifies this selector binding's saved
    // decisions. Preflight asks the ledger only for keys under it, and
    // only as many as the protocol's candidate bound could use.
    let replay_prefix = format!(
        "{}{}:",
        admission::REPLAY_KEY_PREFIX,
        params.binding.adapter
    );

    // Preflight in one deferred read: an already-active run refuses
    // the invocation outright (the lease makes this rare — it covers
    // a run left by a dead owner awaiting recovery), the pending
    // queue's free positions size `max_candidates`, the live source
    // owners become the request's exclusions, and the ledger names
    // the replay identities whose decision is already saved.
    let preflight = store.transact_deferred(|tx| {
        if let Some(run) = tx.active_intake_run()? {
            return Ok(Preflight::Active(run.run_id));
        }
        let limit = tx
            .intake_settings()?
            .map(|s| s.pending_queue_limit)
            .unwrap_or(DEFAULT_PENDING_QUEUE_LIMIT);
        let occupied = tx.pending_queue_count()?;
        let owned = tx.intake_owned_issues()?;
        let decided = tx.decided_idempotency_keys(
            &Origin(admission::SELECTOR_ORIGIN.into()),
            params.actor,
            &replay_prefix,
            protocol::MAX_CANDIDATES,
        )?;
        Ok(Preflight::Ready {
            limit,
            occupied,
            owned,
            decided,
        })
    })?;

    let (limit, occupied, mut owned, decided) = match preflight {
        Preflight::Ready {
            limit,
            occupied,
            owned,
            decided,
        } => (limit, occupied, owned, decided),
        Preflight::Active(run_id) => return Err(IntakeError::ActiveRun { run_id }),
    };

    let free = u64::from(limit).saturating_sub(occupied);
    // Exclusions are scoped to the request's repositories and bounded
    // by the protocol's 4096-entry wire limit.
    owned.retain(|issue| params.repositories.contains(&issue.repository));
    owned.sort();
    owned.dedup();
    let exclusions = owned;

    // A candidate whose saved decision is re-presented is `replayed`
    // and creates no new pending position, but it still counts
    // against the adapter's returned-candidate bound. Size the ask
    // beyond the free positions by the known replay identities this
    // selector owns, so replays do not consume the budget new
    // proposals need. The ledger read is already scoped to the
    // binding's replay prefix and capped at `MAX_CANDIDATES`, so the
    // count is bounded and an upper bound on the replays a response
    // can carry.
    //
    // Residual limit: `max_candidates` is capped at the protocol's
    // 32, so once at least `MAX_CANDIDATES` replay identities exist
    // the ask is saturated and a selector that ranks every replay
    // ahead of every fresh Issue can fill the response with replays,
    // leaving no slot for new work regardless of the free count.
    // Below that bound the ask still leaves room for up to
    // `MAX_CANDIDATES - known_replays` fresh proposals. Eliminating
    // the saturation would require the selector protocol to accept
    // replay identities; raising `pending_queue_limit` helps only
    // while fewer than `MAX_CANDIDATES` replays exist.
    let known_replays = decided.len() as u64;
    // The run record copies the *held* lease's owner identity — the
    // storage contract's provenance for "who held this invocation" —
    // not a second, unrelated acquisition identity.
    let owner = lease.owner.clone();
    let request_id = new_id("intake-req");
    let deadline_unix_ms = now_unix_ms + params.timeout.as_millis() as i64;

    // Zero free positions, a repository list the wire cannot carry,
    // or an exclusion set beyond the bound: record the attempt and
    // never launch — "the queue was full" is durable evidence,
    // distinct from a failed or completed invocation.
    let skip_detail = if free == 0 {
        Some("queue_full")
    } else if params.repositories.is_empty()
        || params.repositories.len() > protocol::MAX_REPOSITORIES
    {
        Some("repositories_bound")
    } else if exclusions.len() > protocol::MAX_EXCLUSIONS {
        Some("exclusions_overflow")
    } else {
        None
    };
    if let Some(detail) = skip_detail {
        // The skipped run's request_payload still records the complete
        // exclusion snapshot — audit evidence is never truncated to
        // fit the wire bound the launch was refused for.
        return record_skipped(
            store,
            params,
            &owner,
            detail,
            &exclusions,
            deadline_unix_ms,
            now_unix_seconds,
        );
    }

    let max_candidates = free
        .saturating_add(known_replays)
        .min(u64::from(protocol::MAX_CANDIDATES)) as u32;
    let spec = protocol::RequestSpec {
        request_id: &request_id,
        selector: &params.binding.adapter,
        selector_version: params.binding.selector_version,
        policy_revision: params.binding.policy_revision,
        repositories: params.repositories,
        excluded_sources: &exclusions,
        max_candidates,
        deadline_unix_ms,
        task_constraints: params.task_constraints,
        verification_argv: params.verification_argv,
        required_permissions: params.required_permissions,
    };
    let request =
        protocol::prepare_request(&spec, now_unix_ms).map_err(|v| IntakeError::Request {
            rule: violation_rule(&v),
        })?;

    // B0→B1 boundary: the `open` run and its request context commit
    // before the adapter is launched, so a crash mid-exchange leaves
    // a recoverable record rather than an untracked process.
    let run_id = store.transact(|tx| {
        let request_payload =
            request_context_json(params, &exclusions, max_candidates, deadline_unix_ms);
        let run = IntakeRun {
            run_id: tx.alloc_id("intake")?,
            trigger: params.trigger,
            selector_id: workloop_core::identity::AdapterId::new(params.binding.adapter.clone())
                .map_err(|e| StoreError::Internal {
                    detail: format!("selector id: {e}"),
                })?,
            selector_version: params.binding.selector_version,
            policy_revision: params.binding.policy_revision,
            settings_revision: params.settings_revision.map(str::to_owned),
            state: IntakeRunState::Open,
            detail_code: None,
            request_digest: sha256_hex(REQUEST_TAG, &request_payload),
            request_payload,
            owner_pid: owner.pid,
            owner_started_at: owner.started_at.clone(),
            owner_nonce: owner.nonce.clone(),
            created_unix_seconds: now_unix_seconds,
            finished_unix_seconds: None,
            recorded_seq: tx.alloc_seq()?,
        };
        let run_id = run.run_id.clone();
        tx.apply(&Effect::InsertIntakeRun { run })?;
        Ok(run_id)
    })?;

    // Everything after the `open` record runs under one guard: a
    // store fault must never return with a non-terminal run still
    // owned by this live process — the next invocation would read it
    // as active forever and intake would wedge until restart.
    match complete_intake(store, params, &run_id, &request, now_unix_seconds) {
        Ok(report) => Ok(report),
        Err(error) => {
            interrupt_invocation(
                store,
                params.actor,
                &run_id,
                InterruptDetail::StoreError,
                now_unix_seconds,
            );
            Err(error)
        }
    }
}

/// The post-launch half of [`run_intake`]: the single bounded
/// exchange, whole-response validation, the atomic candidate batch,
/// and ordered admission. Any error propagates to the caller, which
/// aborts the still-open run — this function never leaves the run
/// non-terminal on its own error paths.
fn complete_intake(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    run_id: &str,
    request: &protocol::ValidatedRequest,
    now_unix_seconds: i64,
) -> Result<IntakeReport, IntakeError> {
    // The single Selector call. Transport-level outcomes map onto
    // bounded `detail_code`s; payload semantics belong to the
    // protocol layer, which already classified them.
    let exchange = bounded::exchange(
        &params.binding.argv,
        params.cwd,
        &bounded::selector_env(),
        &request.bytes,
        &SELECTOR_EXCHANGE_BOUNDS,
        params.timeout,
    );

    let outcome = match exchange {
        Err(error) => {
            finish_run(
                store,
                run_id,
                IntakeRunState::Failed,
                Some(exchange_detail(&error).to_string()),
                now_unix_seconds,
            )?;
            // Only `Launch` means no process ever ran; every other
            // exchange error happened after a successful spawn.
            let launched = !matches!(error, ExchangeError::Launch { .. });
            return Ok(report(store, run_id, launched)?);
        }
        Ok(output) => match protocol::validate_response(&output.stdout, request) {
            Err(violation) => {
                finish_run(
                    store,
                    run_id,
                    IntakeRunState::Failed,
                    Some(violation_detail(&violation)),
                    now_unix_seconds,
                )?;
                return Ok(report(store, run_id, true)?);
            }
            Ok(SelectorReply::Failure { code, .. }) => {
                finish_run(
                    store,
                    run_id,
                    IntakeRunState::Failed,
                    Some(failure_detail(code).to_string()),
                    now_unix_seconds,
                )?;
                return Ok(report(store, run_id, true)?);
            }
            Ok(SelectorReply::Success { candidates }) => candidates,
        },
    };

    // B1: the whole validated batch and the `open` → `admitting`
    // transition commit atomically — original order, all `pending`.
    admit_batch(store, run_id, params, &outcome)?;

    // Per-candidate admission in returned order. Ordinary refusals
    // are recorded and later candidates continue; capacity exhaustion
    // defers the unattempted remainder; store uncertainty stops all
    // further mutation — the live owner then closes the run
    // `interrupted` and reconciles unresolved candidates itself.
    let mut defer_rest = false;
    for (index, proposal) in outcome.iter().enumerate() {
        if defer_rest {
            // Capacity is exhausted for fresh work, but a candidate
            // whose decision is already saved is still a replay: the
            // ledger is authoritative and it holds no new pending
            // position. Resolve it rather than mislabeling it
            // `capacity_deferred`, which the contract reserves for a
            // candidate with no saved decision under its replay key.
            match resolve_or_defer(store, params, run_id, index as u32, proposal)? {
                AdmitStep::Continue | AdmitStep::Deferred => {}
                AdmitStep::Halted => break,
            }
            continue;
        }
        match admit_one(
            store,
            params,
            run_id,
            index as u32,
            proposal,
            now_unix_seconds,
        )? {
            AdmitStep::Continue => {}
            AdmitStep::Deferred => defer_rest = true,
            AdmitStep::Halted => break,
        }
    }

    // Terminal only when every candidate resolved. A live owner never
    // leaves the run `admitting`: unresolved candidates mean this
    // invocation cannot continue, so the run closes `interrupted` and
    // the recovery sweep reconciles them against the ledger now — the
    // same convergence a restart performs, without wedging every
    // later invocation behind the active-run guard.
    let unresolved = store.transact_deferred(|tx| {
        Ok(tx.intake_candidates(run_id)?.iter().any(|c| {
            matches!(
                c.status,
                IntakeCandidateStatus::Pending | IntakeCandidateStatus::Unknown
            )
        }))
    })?;
    if unresolved {
        interrupt_invocation(
            store,
            params.actor,
            run_id,
            InterruptDetail::UnresolvedCandidates,
            now_unix_seconds,
        );
    } else {
        finish_run(
            store,
            run_id,
            IntakeRunState::Completed,
            None,
            now_unix_seconds,
        )?;
    }
    Ok(report(store, run_id, true)?)
}

enum Preflight {
    Ready {
        limit: u32,
        occupied: u64,
        owned: Vec<IssueRef>,
        /// Idempotency keys with a saved selector decision; the
        /// coordinator filters them to this binding's replay prefix.
        decided: Vec<String>,
    },
    Active(String),
}

enum AdmitStep {
    Continue,
    /// This candidate and the unattempted remainder deferred for
    /// capacity — no decision exists under any of their keys.
    Deferred,
    /// Store uncertainty stopped further mutation.
    Halted,
}

/// The installation intake lease for one invocation, released on
/// drop. Same reclaim semantics as the run lease: a dead owner's row
/// is displaced only on proof, a live owner refuses `Busy`, and an
/// uninspectable owner fails closed.
struct IntakeLease<'a> {
    store: &'a SqliteStore,
    owner: RunLeaseOwner,
}

impl<'a> IntakeLease<'a> {
    fn acquire(
        store: &'a SqliteStore,
        owner: RunLeaseOwner,
        now: i64,
    ) -> Result<Self, IntakeError> {
        store
            .acquire_intake_lease(
                &owner,
                |recorded| PosixProcessControl.inspect(recorded),
                now,
            )
            .map_err(|e| match e {
                RunLeaseError::Held { owner_pid } => IntakeError::Busy { owner_pid },
                RunLeaseError::Unavailable { reason } => IntakeError::Lease { reason },
                RunLeaseError::Store(e) => IntakeError::Store(e),
            })?;
        Ok(Self { store, owner })
    }
}

impl Drop for IntakeLease<'_> {
    fn drop(&mut self) {
        let _ = self.store.release_intake_lease(&self.owner);
    }
}

/// This process's lease-owner identity: pid plus the `lstart`
/// verifier, with a fresh nonce per acquisition. The CLI driver calls
/// this before spawning an invocation so it can name the exact owner
/// it must reconcile if the thread ends without closing its run.
pub fn new_lease_owner() -> Result<RunLeaseOwner, IntakeError> {
    let identity =
        PosixProcessControl::identity_of(std::process::id() as i32).map_err(|reason| {
            IntakeError::Lease {
                reason: format!("cannot record this process's identity: {reason}"),
            }
        })?;
    Ok(RunLeaseOwner {
        pid: identity.pid,
        started_at: identity.started_at,
        nonce: format!("intake-{}", new_id("lease")),
    })
}

/// The canonical request-context record: everything the coordinator
/// asked for, including fields the wire request itself does not carry
/// (trigger, settings revision). Audit provenance — never proposal
/// identity.
fn request_context_json(
    params: &IntakeParams<'_>,
    exclusions: &[IssueRef],
    max_candidates: u32,
    deadline_unix_ms: i64,
) -> Vec<u8> {
    canonical_json(&serde_json::json!({
        "trigger": params.trigger,
        "selector": {
            "adapter": params.binding.adapter,
            "selector_version": params.binding.selector_version,
            "policy_revision": params.binding.policy_revision,
        },
        "settings_revision": params.settings_revision,
        "repositories": params.repositories,
        "excluded_sources": exclusions,
        "max_candidates": max_candidates,
        "task_constraints": params.task_constraints,
        "verification_argv": params.verification_argv,
        "required_permissions": params.required_permissions,
        "deadline_unix_ms": deadline_unix_ms,
    }))
}

/// Record a no-launch attempt: a `skipped` run is terminal at insert
/// and carries the same request-context shape with `max_candidates`
/// 0 — nothing was asked of the adapter.
fn record_skipped(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    owner: &RunLeaseOwner,
    detail: &str,
    exclusions: &[IssueRef],
    deadline_unix_ms: i64,
    now: i64,
) -> Result<IntakeReport, IntakeError> {
    let request_payload = request_context_json(params, exclusions, 0, deadline_unix_ms);
    let run_id = store.transact(|tx| {
        let run = IntakeRun {
            run_id: tx.alloc_id("intake")?,
            trigger: params.trigger,
            selector_id: workloop_core::identity::AdapterId::new(params.binding.adapter.clone())
                .map_err(|e| StoreError::Internal {
                    detail: format!("selector id: {e}"),
                })?,
            selector_version: params.binding.selector_version,
            policy_revision: params.binding.policy_revision,
            settings_revision: params.settings_revision.map(str::to_owned),
            state: IntakeRunState::Skipped,
            detail_code: Some(detail.to_string()),
            request_digest: sha256_hex(REQUEST_TAG, &request_payload),
            request_payload,
            owner_pid: owner.pid,
            owner_started_at: owner.started_at.clone(),
            owner_nonce: owner.nonce.clone(),
            created_unix_seconds: now,
            finished_unix_seconds: Some(now),
            recorded_seq: tx.alloc_seq()?,
        };
        let run_id = run.run_id.clone();
        tx.apply(&Effect::InsertIntakeRun { run })?;
        Ok(run_id)
    })?;
    report(store, &run_id, false).map_err(IntakeError::Store)
}

fn admit_batch(
    store: &SqliteStore,
    run_id: &str,
    params: &IntakeParams<'_>,
    proposals: &[Proposal],
) -> Result<(), StoreError> {
    store.transact(|tx| {
        let mut rows = Vec::with_capacity(proposals.len());
        for (index, proposal) in proposals.iter().enumerate() {
            let payload = canonical_json(&proposal_semantic_json(
                &params.binding.adapter,
                params.binding.selector_version,
                params.binding.policy_revision,
                &proposal.sources,
                &proposal.task,
                &proposal.required_permissions,
            ));
            rows.push(IntakeCandidate {
                run_id: run_id.to_string(),
                ordinal: index as u32,
                proposal_key: proposal.key.clone(),
                replay_key: admission::replay_key(proposal),
                payload_digest: proposal.key.clone(),
                payload,
                status: IntakeCandidateStatus::Pending,
                status_reason: None,
                command_id: None,
                work_id: None,
                committed_seq: None,
                recorded_seq: tx.alloc_seq()?,
            });
        }
        tx.apply(&Effect::AdmitIntakeBatch {
            run_id: run_id.to_string(),
            candidates: rows,
        })
    })
}

/// Admit one validated candidate: consult the command ledger before
/// any resend, submit fresh proposals through `handle_command`, and
/// record the outcome. Returns the loop's next step.
fn admit_one(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    run_id: &str,
    ordinal: u32,
    proposal: &Proposal,
    now: i64,
) -> Result<AdmitStep, IntakeError> {
    let command = admission::submit_command(proposal, params.actor.clone());
    let digest = payload_digest(
        &command.origin,
        &command.actor,
        &command.idempotency_key,
        &command.body,
    );

    // The ledger is authoritative: a saved decision under this replay
    // identity is replayed verbatim — never resent, never relabeled.
    let prior = store.transact_deferred(|tx| {
        tx.stored_decision(&command.origin, &command.actor, &command.idempotency_key)
    })?;
    // A same-key/different-payload conflict cannot arise from a
    // deterministic proposal; if it somehow does, the ordinary
    // path below records the conflict rejection.
    if let Some((stored_digest, decision)) = prior
        && stored_digest == digest
    {
        return match resolve_decided(store, run_id, ordinal, &decision, true)? {
            true => Ok(AdmitStep::Continue),
            false => Ok(AdmitStep::Halted),
        };
    }

    match handle_command(store, params.work_config, &command, now) {
        Ok(decision) => {
            if is_capacity_refusal(&decision) {
                // Transient: no decision was saved. This candidate and
                // every unattempted remainder defer — never a saved
                // rejection, so a later run decides them fresh.
                return match mark_candidate(
                    store,
                    run_id,
                    ordinal,
                    IntakeCandidateStatus::CapacityDeferred,
                    Some("capacity_exhausted".into()),
                    None,
                    None,
                    None,
                )? {
                    true => Ok(AdmitStep::Deferred),
                    false => Ok(AdmitStep::Halted),
                };
            }
            match resolve_decided(store, run_id, ordinal, &decision, false)? {
                true => Ok(AdmitStep::Continue),
                false => Ok(AdmitStep::Halted),
            }
        }
        Err(_) => {
            // The submit's commit state is undetermined: the candidate
            // is `unknown` and reconciliation goes through the saved
            // replay key — never a blind resend.
            let _ = mark_candidate(
                store,
                run_id,
                ordinal,
                IntakeCandidateStatus::Unknown,
                Some("submit_outcome_unknown".into()),
                None,
                None,
                None,
            );
            Ok(AdmitStep::Halted)
        }
    }
}

/// Resolve the unattempted remainder behind a capacity refusal
/// without submitting. A candidate with a saved decision under its
/// replay identity is still `replayed` — the ledger is authoritative
/// and it holds no new pending position — while every other candidate
/// is `capacity_deferred`, documenting that no decision exists under
/// its key so a later run decides it fresh. Marking a saved replay
/// `capacity_deferred` would promise the ledger holds nothing for it,
/// so replays must never be deferred blindly just because they trail
/// a fresh proposal that exhausted capacity.
fn resolve_or_defer(
    store: &SqliteStore,
    params: &IntakeParams<'_>,
    run_id: &str,
    ordinal: u32,
    proposal: &Proposal,
) -> Result<AdmitStep, IntakeError> {
    let command = admission::submit_command(proposal, params.actor.clone());
    let digest = payload_digest(
        &command.origin,
        &command.actor,
        &command.idempotency_key,
        &command.body,
    );
    let prior = store.transact_deferred(|tx| {
        tx.stored_decision(&command.origin, &command.actor, &command.idempotency_key)
    })?;
    if let Some((stored_digest, decision)) = prior
        && stored_digest == digest
    {
        return match resolve_decided(store, run_id, ordinal, &decision, true)? {
            true => Ok(AdmitStep::Continue),
            false => Ok(AdmitStep::Halted),
        };
    }
    match mark_candidate(
        store,
        run_id,
        ordinal,
        IntakeCandidateStatus::CapacityDeferred,
        Some("capacity_exhausted".into()),
        None,
        None,
        None,
    )? {
        true => Ok(AdmitStep::Deferred),
        false => Ok(AdmitStep::Halted),
    }
}

/// Whether the decision is the transient capacity refusal — the one
/// outcome `handle_command` deliberately does not persist.
fn is_capacity_refusal(decision: &CommandDecision) -> bool {
    matches!(
        decision,
        CommandDecision::Rejected {
            reason: RejectionCode::CapacityExhausted,
            ..
        }
    )
}

/// Write the candidate's decided status from a durable
/// `CommandDecision`. `replayed` marks a saved-decision return: the
/// status is `replayed` regardless of the saved outcome, with the
/// original rejection code preserved as the bounded reason when the
/// saved decision was a rejection.
fn resolve_decided(
    store: &SqliteStore,
    run_id: &str,
    ordinal: u32,
    decision: &CommandDecision,
    replayed: bool,
) -> Result<bool, IntakeError> {
    let (status, reason, work_id) = match (replayed, decision) {
        (true, CommandDecision::Accepted { work_id, .. }) => {
            (IntakeCandidateStatus::Replayed, None, work_id.clone())
        }
        (true, CommandDecision::Rejected { reason, .. }) => (
            IntakeCandidateStatus::Replayed,
            Some(rejection_tag(reason)),
            None,
        ),
        (false, CommandDecision::Accepted { work_id, .. }) => {
            if work_id.is_none() {
                // An accepted submit always creates Work; an accepted
                // decision without one cannot satisfy the links rule —
                // record the uncertainty rather than invent a link.
                (
                    IntakeCandidateStatus::Unknown,
                    Some("accepted_without_work".into()),
                    None,
                )
            } else {
                (IntakeCandidateStatus::Accepted, None, work_id.clone())
            }
        }
        (false, CommandDecision::Rejected { reason, .. }) => (
            IntakeCandidateStatus::Rejected,
            Some(rejection_tag(reason)),
            None,
        ),
    };
    let (command_id, committed_seq) = match decision {
        CommandDecision::Accepted {
            command_id,
            committed_at,
            ..
        }
        | CommandDecision::Rejected {
            command_id,
            committed_at,
            ..
        } => (Some(command_id.clone()), Some(*committed_at)),
    };
    // `unknown` and the deferred/unresolved statuses keep every link
    // NULL; decided statuses carry the command identity.
    let (command_id, committed_seq) = if matches!(
        status,
        IntakeCandidateStatus::Unknown | IntakeCandidateStatus::CapacityDeferred
    ) {
        (None, None)
    } else {
        (command_id, committed_seq)
    };
    mark_candidate(
        store,
        run_id,
        ordinal,
        status,
        reason,
        command_id,
        work_id,
        committed_seq,
    )
}

/// Guarded candidate transition `pending`/`unknown` → `status`.
/// Returns `false` when the store refused the write — the caller
/// stops rather than regress or overwrite.
#[allow(clippy::too_many_arguments)]
fn mark_candidate(
    store: &SqliteStore,
    run_id: &str,
    ordinal: u32,
    status: IntakeCandidateStatus,
    reason: Option<String>,
    command_id: Option<CommandId>,
    work_id: Option<WorkId>,
    committed_seq: Option<EventSequence>,
) -> Result<bool, IntakeError> {
    let current = store.transact_deferred(|tx| {
        Ok(tx
            .intake_candidates(run_id)?
            .into_iter()
            .find(|c| c.ordinal == ordinal))
    })?;
    let Some(current) = current else {
        return Ok(false);
    };
    let expected = current.status;
    let updated = IntakeCandidate {
        status,
        status_reason: reason,
        command_id,
        work_id,
        committed_seq,
        ..current
    };
    match store.transact(|tx| {
        tx.apply(&Effect::UpdateIntakeCandidate {
            candidate: updated,
            expected_status: expected,
        })
    }) {
        Ok(()) => Ok(true),
        Err(StoreError::ConstraintConflict { .. } | StoreError::StateConflict { .. }) => Ok(false),
        Err(e) => Err(IntakeError::Store(e)),
    }
}

/// Guarded run transition; `expected` is the state the caller
/// observed. Failures surface as store errors — a stale writer
/// conflicts rather than regressing a terminal run.
fn finish_run(
    store: &SqliteStore,
    run_id: &str,
    state: IntakeRunState,
    detail_code: Option<String>,
    now: i64,
) -> Result<(), StoreError> {
    let expected = match state {
        IntakeRunState::Failed => IntakeRunState::Open,
        IntakeRunState::Completed => IntakeRunState::Admitting,
        _ => {
            return Err(StoreError::Internal {
                detail: format!("unsupported finish state {state:?}"),
            });
        }
    };
    store.transact(|tx| {
        tx.apply(&Effect::UpdateIntakeRun {
            run_id: run_id.to_string(),
            state,
            detail_code,
            finished_unix_seconds: Some(now),
            expected_state: expected,
        })
    })
}

/// The bounded `detail_code` values a live owner records when it
/// closes its own run `interrupted` — distinct from the
/// recovery-origin codes (`owner_lost`, `no_conforming_response`)
/// a dead owner's sweep writes.
#[derive(Clone, Copy)]
enum InterruptDetail {
    /// A store fault propagated out of the invocation.
    StoreError,
    /// Admission halted with candidates still unresolved.
    UnresolvedCandidates,
}

impl InterruptDetail {
    fn code(self) -> &'static str {
        match self {
            Self::StoreError => "store_error",
            Self::UnresolvedCandidates => "unresolved_candidates",
        }
    }
}

/// Best-effort close of a run whose live-owner invocation ended
/// without reaching a terminal state — either a propagating store
/// fault or unresolved candidates after admission halted. Without
/// this, the run would stay `open`/`admitting` owned by *this live
/// process*: every later invocation reads it as the active run and
/// refuses, and recovery sees a live owner — intake wedges until
/// process restart. The guarded write only fires while the run is
/// still non-terminal, so an already-`completed`/`failed` run is
/// untouched; the recovery sweep then reconciles any persisted
/// candidates against the ledger instead of leaving them for the
/// next restart.
fn interrupt_invocation(
    store: &SqliteStore,
    actor: &Actor,
    run_id: &str,
    detail: InterruptDetail,
    now: i64,
) {
    let state = store
        .transact_deferred(|tx| Ok(tx.intake_run(run_id)?.map(|r| r.state)))
        .ok()
        .flatten();
    if let Some(state @ (IntakeRunState::Open | IntakeRunState::Admitting)) = state {
        let _ = store.transact(|tx| {
            tx.apply(&Effect::UpdateIntakeRun {
                run_id: run_id.to_string(),
                state: IntakeRunState::Interrupted,
                detail_code: Some(detail.code().to_string()),
                finished_unix_seconds: Some(now),
                expected_state: state,
            })
        });
    }
    let _ = recover_intake(store, actor, now);
}

fn report(store: &SqliteStore, run_id: &str, launched: bool) -> Result<IntakeReport, StoreError> {
    store.transact_deferred(|tx| {
        let run = tx.intake_run(run_id)?.ok_or_else(|| StoreError::Internal {
            detail: format!("intake run {run_id} is absent"),
        })?;
        let candidates = tx
            .intake_candidates(run_id)?
            .into_iter()
            .map(|c| CandidateReport {
                ordinal: c.ordinal,
                proposal_key: c.proposal_key,
                replay_key: c.replay_key,
                status: c.status,
                status_reason: c.status_reason,
                command_id: c.command_id,
                work_id: c.work_id,
            })
            .collect();
        Ok(IntakeReport {
            run_id: run.run_id,
            state: run.state,
            detail_code: run.detail_code,
            launched,
            candidates,
        })
    })
}

// --- Failure classification ------------------------------------------------

/// Bounded `detail_code` for a transport-level exchange failure.
fn exchange_detail(error: &ExchangeError) -> &'static str {
    match error {
        ExchangeError::Launch { .. } => "exchange_launch",
        ExchangeError::Deadline { .. } => "exchange_deadline",
        ExchangeError::OutputOverflow => "exchange_stdout_overflow",
        ExchangeError::StderrOverflow => "exchange_stderr_overflow",
        ExchangeError::Exit { .. } => "exchange_exit",
        ExchangeError::Unknown { .. } => "exchange_unknown",
    }
}

/// Bounded `detail_code` for a response that failed whole-response
/// validation — the contract rule tag, never payload text.
fn violation_detail(violation: &Violation) -> String {
    match violation {
        Violation::Parse(_) => "malformed_response".to_string(),
        Violation::Contract(rule) => format!("contract_{rule}"),
    }
}

fn violation_rule(violation: &Violation) -> String {
    violation_detail(violation)
}

/// Bounded `detail_code` for an adapter-declared failure response.
fn failure_detail(code: FailureCode) -> &'static str {
    match code {
        FailureCode::InvalidRequest => "adapter_invalid_request",
        FailureCode::UnsupportedVersion => "adapter_unsupported_version",
        FailureCode::UnsupportedCapability => "adapter_unsupported_capability",
        FailureCode::InputLimit => "adapter_input_limit",
        FailureCode::MemberLimit => "adapter_member_limit",
        FailureCode::SourceUnavailable => "adapter_source_unavailable",
        FailureCode::AuthenticationFailed => "adapter_authentication_failed",
        FailureCode::DeadlineExceeded => "adapter_deadline_exceeded",
        FailureCode::InternalError => "adapter_internal_error",
    }
}

/// The bounded reason stored on a rejected candidate — the
/// rejection's snake_case code, never evidence text.
fn rejection_tag(code: &RejectionCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "rejected".to_string())
}

// --- Recovery (#208) --------------------------------------------------------

/// What the recovery pass found and did.
#[derive(Debug)]
pub struct RecoveryReport {
    /// The active run inspected, if one existed.
    pub active_run: Option<String>,
    pub outcome: RecoveryOutcome,
    /// Candidates whose status was reconciled against the ledger.
    pub candidates_reconciled: usize,
}

#[derive(Debug)]
pub enum RecoveryOutcome {
    /// No `open`/`admitting` run exists.
    NoActiveRun,
    /// The recorded owner is provably alive — the run is untouched.
    OwnerAlive,
    /// Owner liveness could not be proven — exclusion retained, the
    /// run is untouched.
    OwnerUnknown,
    /// The owner is proven gone; the run was closed and its
    /// unresolved candidates reconciled against the command ledger.
    Recovered { final_state: IntakeRunState },
    /// Recovery stopped early — a guarded write conflicted. The run
    /// and its unresolved candidates remain for the next pass.
    Incomplete,
}

/// Reconcile durable intake state after a restart (#208). Consults
/// the command ledger before any resend: a saved decision fixes the
/// candidate's terminal status and links; an absent decision marks
/// the candidate `interrupted` — never silently acknowledged. Runs
/// whose candidates all reach terminal outcomes close `completed`;
/// the rest close `interrupted`. A live or uninspectable owner
/// retains exclusion untouched.
///
/// `actor` is the installation actor selector submits replay under —
/// the same value the coordinator used, needed to address ledger
/// lookups. The run record does not persist it: the installation
/// actor id is stable by design, and changing it re-keys the entire
/// command ledger system-wide — an operator identity change that is
/// outside intake's scope, not a per-run drift recovery must absorb.
pub fn recover_intake(
    store: &SqliteStore,
    actor: &Actor,
    now_unix_seconds: i64,
) -> Result<RecoveryReport, StoreError> {
    let active = store.transact_deferred(|tx| tx.active_intake_run())?;
    let Some(run) = active else {
        // No live run — but an earlier recovery may have died
        // mid-reconcile, leaving pending/unknown candidates under an
        // already-interrupted run. Finish those against the ledger.
        let mut reconciled = 0usize;
        for candidate in unresolved_candidates(store)? {
            let run_state = store
                .transact_deferred(|tx| Ok(tx.intake_run(&candidate.run_id)?.map(|r| r.state)))?;
            if !matches!(run_state, Some(IntakeRunState::Interrupted)) {
                continue;
            }
            if matches!(
                reconcile_candidate(store, actor, &candidate)?,
                Reconcile::Decided | Reconcile::Interrupted
            ) {
                reconciled += 1;
            }
        }
        return Ok(RecoveryReport {
            active_run: None,
            outcome: RecoveryOutcome::NoActiveRun,
            candidates_reconciled: reconciled,
        });
    };

    let identity = ProcessIdentity {
        pid: run.owner_pid,
        started_at: run.owner_started_at.clone(),
        pgid: None,
    };
    match PosixProcessControl.inspect(&identity) {
        workloop_core::ports::ProcessEvidence::Alive { .. } => {
            return Ok(RecoveryReport {
                active_run: Some(run.run_id),
                outcome: RecoveryOutcome::OwnerAlive,
                candidates_reconciled: 0,
            });
        }
        workloop_core::ports::ProcessEvidence::Unknown { .. } => {
            return Ok(RecoveryReport {
                active_run: Some(run.run_id),
                outcome: RecoveryOutcome::OwnerUnknown,
                candidates_reconciled: 0,
            });
        }
        workloop_core::ports::ProcessEvidence::Stopped { .. } => {}
    }

    // Owner proven gone. Reconcile and close exactly as an ended
    // invocation does, with the recovery-origin detail codes.
    reconcile_and_close_run(
        store,
        actor,
        &run,
        now_unix_seconds,
        "no_conforming_response",
        "owner_lost",
    )
}

/// Reconcile a run whose owning invocation has already ended inside a
/// still-live process: a panic unwound past the live-owner close
/// guard, or the same store fault that ended the invocation also
/// defeated its best-effort close and lease release. The match is
/// exact — pid, process start verifier, and the invocation's lease
/// nonce — so a run owned by any other identity is left to the
/// ordinary owner-liveness [`recover_intake`] path (which may also
/// sweep leftovers). A matching `open`/`admitting` run is closed
/// `interrupted` with `invocation_orphaned` (or `completed` when every
/// candidate reconciles to a decided terminal status), and its
/// unresolved candidates are reconciled against the ledger.
pub fn recover_ended_invocation(
    store: &SqliteStore,
    actor: &Actor,
    owner: &RunLeaseOwner,
    now_unix_seconds: i64,
) -> Result<RecoveryReport, StoreError> {
    let active = store.transact_deferred(|tx| tx.active_intake_run())?;
    match active {
        Some(run)
            if run.owner_pid == owner.pid
                && run.owner_started_at == owner.started_at
                && run.owner_nonce == owner.nonce =>
        {
            reconcile_and_close_run(
                store,
                actor,
                &run,
                now_unix_seconds,
                "invocation_orphaned",
                "invocation_orphaned",
            )
        }
        _ => recover_intake(store, actor, now_unix_seconds),
    }
}

/// The shared candidate reconciliation and guarded run close. The two
/// detail arguments are the bounded diagnostics the caller records for
/// an `open` run and for an `admitting` run that cannot close
/// `completed` (an unresolved or already-`interrupted` candidate).
fn reconcile_and_close_run(
    store: &SqliteStore,
    actor: &Actor,
    run: &IntakeRun,
    now_unix_seconds: i64,
    open_detail: &str,
    lost_detail: &str,
) -> Result<RecoveryReport, StoreError> {
    // Reconcile every unresolved candidate against the ledger first:
    // found decisions fix terminal status and links; absent decisions
    // become `interrupted`.
    let mut reconciled = 0usize;
    let candidates = store.transact_deferred(|tx| tx.intake_candidates(&run.run_id))?;
    // An `interrupted` candidate — whether this pass marks it now or
    // an earlier partial recovery already did — means the invocation
    // was cut short; the run can never close `completed` over one.
    let mut any_interrupted = candidates
        .iter()
        .any(|c| c.status == IntakeCandidateStatus::Interrupted);
    for candidate in candidates {
        if !matches!(
            candidate.status,
            IntakeCandidateStatus::Pending | IntakeCandidateStatus::Unknown
        ) {
            continue;
        }
        match reconcile_candidate(store, actor, &candidate)? {
            Reconcile::Decided => reconciled += 1,
            Reconcile::Interrupted => {
                reconciled += 1;
                any_interrupted = true;
            }
            Reconcile::Failed => {
                return Ok(RecoveryReport {
                    active_run: Some(run.run_id.clone()),
                    outcome: RecoveryOutcome::Incomplete,
                    candidates_reconciled: reconciled,
                });
            }
        }
    }

    // Close the run: `open` had no validated batch (nothing was
    // invented); `admitting` closes `completed` only when every
    // candidate reached a decided terminal outcome — an interrupted
    // candidate means the invocation itself was cut short.
    // `detail_code` is a failure diagnostic: a run recovered to
    // `completed` carries none, exactly like a live owner's close.
    let (final_state, detail) = match run.state {
        IntakeRunState::Open => (IntakeRunState::Interrupted, Some(open_detail)),
        IntakeRunState::Admitting if !any_interrupted => (IntakeRunState::Completed, None),
        _ => (IntakeRunState::Interrupted, Some(lost_detail)),
    };
    let close = store.transact(|tx| {
        tx.apply(&Effect::UpdateIntakeRun {
            run_id: run.run_id.clone(),
            state: final_state,
            detail_code: detail.map(str::to_string),
            finished_unix_seconds: Some(now_unix_seconds),
            expected_state: run.state,
        })
    });
    // A concurrent recovery pass may win the guarded transition —
    // recovery is idempotent, so a conflict is verified against the
    // run's actual state rather than reported as a store fault.
    let final_state = match close {
        Ok(()) => final_state,
        Err(StoreError::ConstraintConflict { .. } | StoreError::StateConflict { .. }) => {
            let current =
                store.transact_deferred(|tx| Ok(tx.intake_run(&run.run_id)?.map(|r| r.state)))?;
            match current {
                Some(
                    state @ (IntakeRunState::Completed
                    | IntakeRunState::Failed
                    | IntakeRunState::Interrupted
                    | IntakeRunState::Skipped),
                ) => state,
                _ => {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake run {} changed under a concurrent recovery",
                            run.run_id
                        ),
                    });
                }
            }
        }
        Err(e) => return Err(e),
    };

    Ok(RecoveryReport {
        active_run: Some(run.run_id.clone()),
        outcome: RecoveryOutcome::Recovered { final_state },
        candidates_reconciled: reconciled,
    })
}

enum Reconcile {
    /// A saved decision fixed the candidate's terminal status.
    Decided,
    /// No decision exists — the candidate is `interrupted`.
    Interrupted,
    /// The store refused the write; recovery stops, retry later.
    Failed,
}

/// Reconcile one `pending`/`unknown` candidate against the command
/// ledger under its replay key. Found decisions fix the terminal
/// status and links (a replayed proposal links the original decision,
/// never a new one); absent decisions mark the candidate
/// `interrupted` — never silently acknowledged as accepted.
fn reconcile_candidate(
    store: &SqliteStore,
    actor: &Actor,
    candidate: &IntakeCandidate,
) -> Result<Reconcile, StoreError> {
    let origin = Origin(admission::SELECTOR_ORIGIN.to_string());
    let decision =
        store.transact_deferred(|tx| tx.stored_decision(&origin, actor, &candidate.replay_key))?;
    let Some((_, decision)) = decision else {
        // No saved decision: the submit never committed (or its
        // outcome was never recorded). The candidate cannot be
        // acknowledged — it is interrupted, and a later run
        // re-proposing the same semantic proposal submits fresh.
        return match store.transact(|tx| {
            tx.apply(&Effect::UpdateIntakeCandidate {
                candidate: IntakeCandidate {
                    status: IntakeCandidateStatus::Interrupted,
                    status_reason: Some("no_saved_decision".to_string()),
                    ..candidate.clone()
                },
                expected_status: candidate.status,
            })
        }) {
            Ok(()) => Ok(Reconcile::Interrupted),
            Err(StoreError::ConstraintConflict { .. } | StoreError::StateConflict { .. }) => {
                Ok(Reconcile::Failed)
            }
            Err(e) => Err(e),
        };
    };
    // A found decision is by definition a replay for this candidate —
    // the ledger holds it because an earlier submit committed it.
    let updated = IntakeCandidate {
        status: IntakeCandidateStatus::Replayed,
        status_reason: match &decision {
            CommandDecision::Rejected { reason, .. } => Some(rejection_tag(reason)),
            _ => None,
        },
        command_id: Some(decision.command_id().clone()),
        work_id: match &decision {
            CommandDecision::Accepted { work_id, .. } => work_id.clone(),
            CommandDecision::Rejected { .. } => None,
        },
        committed_seq: Some(decision.committed_at()),
        ..candidate.clone()
    };
    // Rejected saved decisions keep `replayed` (the saved decision is
    // what replay returns) — but the links rule requires `rejected`
    // rows to forbid work_id and decided rows to carry command
    // identity; `replayed` allows work_id absent, which covers both.
    match store.transact(|tx| {
        tx.apply(&Effect::UpdateIntakeCandidate {
            candidate: updated,
            expected_status: candidate.status,
        })
    }) {
        Ok(()) => Ok(Reconcile::Decided),
        Err(StoreError::ConstraintConflict { .. } | StoreError::StateConflict { .. }) => {
            Ok(Reconcile::Failed)
        }
        Err(e) => Err(e),
    }
}

/// All `pending`/`unknown` candidates across runs — the recovery
/// sweep's work list.
fn unresolved_candidates(store: &SqliteStore) -> Result<Vec<IntakeCandidate>, StoreError> {
    store.transact_deferred(|tx| {
        let mut all = tx.intake_candidates_by_status(&[
            IntakeCandidateStatus::Pending,
            IntakeCandidateStatus::Unknown,
        ])?;
        all.sort_by(|a, b| (a.run_id.as_str(), a.ordinal).cmp(&(b.run_id.as_str(), b.ordinal)));
        Ok(all)
    })
}
