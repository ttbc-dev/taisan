//! The SQLite store: current-state tables plus an append-only `event_log`.
//! Structural invariants live in constraints and partial unique indexes;
//! transition legality stays in `work`. One connection behind a mutex
//! serializes transactions within a process; across processes (every CLI
//! invocation is one) immediate transactions reserve the writer before reading.
//! Contenders wait on the busy timeout before observing a snapshot, so a later
//! `transact` write never needs to upgrade a stale WAL snapshot. Queries and
//! `preview` instead use `transact_deferred`: they hold no writer lock while
//! only reading, and a write they perform upgrades the snapshot — failing
//! with `SQLITE_BUSY`/`SQLITE_BUSY_SNAPSHOT` when a writer or a newer commit
//! intervened rather than waiting on the lock.
//!
//! `PRAGMA user_version` is the format gate: `10` is the current format
//! (see `FORMAT_VERSION`). A file stamped `1` through `9`
//! must carry the complete recognized schema of its version and
//! upgrades additively at open; a stamped file whose schema is foreign
//! or incomplete, an unversioned database with user tables (a slice-0
//! file — archive-only), and a newer version are all refused at open.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use workloop_core::command::{CommandDecision, CommandId};
use workloop_core::domain::*;
use workloop_core::identity::{AdapterId, HostInstanceId, StoreId, canonical_host_instance};
use workloop_core::observation::{DeliveryId, DeliveryScope, Observation};
use workloop_core::ports::ProcessEvidence;
use workloop_core::store::{
    CommandRecord, Deferral, Effect, HostActionIntent, IntakeCandidate, IntakeCandidateStatus,
    IntakeRun, IntakeRunState, IntakeSettingsRecord, IntakeSourceOwner, ReadView, RecoveryIntent,
    RecoveryIntentState, Store, StoreError, TxView,
};
use workloop_core::worker::WorkerRequestExport;

const SCHEMA: &str = include_str!("schema.sql");

/// The only store format this binary reads or writes. `2` added the
/// `invocations.descendants` column and the artifact-handoff and
/// verification tables; `3` adds the `worker_request_exports` table;
/// `4` adds the body catalogue's explicit `pin_state`; `5` adds the
/// `reclaim_plans` / `reclaim_plan_entries` tables; `6` adds the
/// request's durable Worker adapter binding; `7` adds the usage row's
/// durable `last_progress_basis`; `8` adds the `intake_runs` /
/// `intake_candidates` tables (`docs/intake-identity-storage.md`);
/// `9` adds the `intake_settings` admission record (#213);
/// `10` adds the `recovery_intents.retirement_generation` column (#139).
/// Older files upgrade additively at open.
pub(crate) const FORMAT_VERSION: i64 = 10;

/// The tables `SCHEMA` installs. A file stamped `user_version=1` must
/// hold all of them before any mutating pragma runs — the version stamp
/// alone is not proof the file is this store's format — unless it is the
/// previous release's v1 store, which holds every table except
/// [`ADDITIVE_TABLES`] and is upgraded in place.
const EXPECTED_TABLES: &[&str] = &[
    "artifact_handoffs",
    "attention_acks",
    "body_catalog",
    "body_pins",
    "capacity_slots",
    "commands",
    "deferrals",
    "event_log",
    "executions",
    "finding_occurrences",
    "host_action_intents",
    "id_counters",
    "intake_candidates",
    "intake_runs",
    "intake_settings",
    "invocations",
    "invocation_requests",
    "issue_claims",
    "observations",
    "reclaim_plan_entries",
    "reclaim_plans",
    "recovery_decisions",
    "recovery_intents",
    "resource_reservations",
    "review_rounds",
    "round_judgments",
    "run_lease",
    "specs",
    "stop_intents",
    "store_meta",
    "verification_results",
    "worker_request_exports",
    "work_issue_refs",
    "work_usage",
    "works",
];

/// The tables the run-lease slice added on top of the previous
/// release's v1 store while it was still stamped `1`. Together with
/// [`V2_ADDED_TABLES`], [`V3_ADDED_TABLES`], [`V5_ADDED_TABLES`], and
/// [`V8_ADDED_TABLES`] they are the set a stamped file may legitimately
/// lack and still be upgraded in place by the additive migration.
const ADDITIVE_TABLES: &[&str] = &["body_catalog", "body_pins", "recovery_intents", "run_lease"];

/// Tables the v1 → v2 migration adds. Everything else in
/// `EXPECTED_TABLES` must already be present for a `user_version=1`
/// file to migrate — that check is what keeps a foreign stamped file
/// refused instead of silently reshaped.
const V2_ADDED_TABLES: &[&str] = &["artifact_handoffs", "verification_results"];

/// Tables the v2 → v3 migration adds (same additive shape as v1 → v2).
const V3_ADDED_TABLES: &[&str] = &["worker_request_exports"];

/// The tables the U2.6 two-phase cleanup slice adds on top of the
/// format-4 store while it is still stamped `4`. Like the additive set
/// they are installed by the same `SCHEMA` batch on upgrade.
const V5_ADDED_TABLES: &[&str] = &["reclaim_plan_entries", "reclaim_plans"];

/// Tables the v7 → v8 migration adds (the v0.2 intake records). Like
/// the earlier additive sets, they are installed by the same `SCHEMA`
/// batch on upgrade.
const V8_ADDED_TABLES: &[&str] = &["intake_candidates", "intake_runs"];

/// The table the v8 → v9 migration adds (the intake-settings admission
/// record). Installed by the same `SCHEMA` batch on upgrade.
const V9_ADDED_TABLES: &[&str] = &["intake_settings"];

/// How long a writer waits on the file lock before `SQLITE_BUSY` surfaces.
/// Connection-local (applies inside `transact` and during `init` itself) and
/// set explicitly so the guarantee does not depend on the binding's default.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SqliteStore {
    pub(crate) conn: Mutex<Connection>,
}

/// The process claiming the run lease: pid plus the opaque start-time
/// verifier captured for it, and a per-acquisition nonce so two
/// acquisitions by one process (or one test process standing in for two
/// drivers) never read as the same holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLeaseOwner {
    pub pid: i32,
    pub started_at: String,
    pub nonce: String,
}

/// Why the run lease could not be granted. Every variant fails closed:
/// the caller starts no pass.
#[derive(Debug)]
pub enum RunLeaseError {
    /// The recorded owner is provably alive — a second driver is
    /// refused with `lock.held`.
    Held {
        owner_pid: i32,
    },
    /// The recorded owner's liveness could not be inspected — never
    /// treated as stop proof, never granted.
    Unavailable {
        reason: String,
    },
    Store(StoreError),
}

impl std::fmt::Display for RunLeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { owner_pid } => write!(f, "lock.held: run lease held by pid {owner_pid}"),
            Self::Unavailable { reason } => {
                write!(
                    f,
                    "lock.unavailable: run lease owner uninspectable: {reason}"
                )
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunLeaseError {}

/// Nonces of lease grants held by live guards in this process, with a
/// reference count so one holder's release cannot mark dead a nonce
/// another live holder still owns. A recorded owner row naming this
/// process (same pid and started_at) with a nonce absent from the map
/// belongs to a guard that was dropped while its release write failed
/// — provably gone, so the lease is reclaimable without waiting for
/// the process to die. A nonce still present belongs to a live guard
/// in this process and must report `Held` like any other live owner.
static LIVE_LEASE_NONCES: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-wide gate held across the whole lease acquire window —
/// row read, ownership write, commit, and nonce registration. The
/// committed row and the holder's nonce registration must be atomic
/// with respect to other connections in this process: without the
/// gate, a second connection can observe the committed row in the
/// gap before `register_live_nonce` runs, find the recorded nonce
/// absent from [`LIVE_LEASE_NONCES`], and reclaim a lease a live
/// same-process guard still holds.
static LEASE_ACQUIRE_GATE: Mutex<()> = Mutex::new(());

fn register_live_nonce(nonce: &str) {
    if let Ok(mut nonces) = LIVE_LEASE_NONCES.lock() {
        *nonces.entry(nonce.to_string()).or_insert(0) += 1;
    }
}

fn renew_live_nonce(nonce: &str) {
    // A renewal is the same logical grant re-entering: it must not
    // double-count its own presence, but it re-arms the nonce when an
    // earlier release write forgot it while the row survived.
    if let Ok(mut nonces) = LIVE_LEASE_NONCES.lock() {
        nonces.entry(nonce.to_string()).or_insert(1);
    }
}

fn forget_live_nonce(nonce: &str) {
    if let Ok(mut nonces) = LIVE_LEASE_NONCES.lock()
        && let Some(count) = nonces.get_mut(nonce)
    {
        *count = count.saturating_sub(1);
        if *count == 0 {
            nonces.remove(nonce);
        }
    }
}

fn is_live_nonce(nonce: &str) -> bool {
    // An uninspectable registry fails closed: the nonce reads as live
    // so a recorded owner is never reclaimed on doubt.
    LIVE_LEASE_NONCES
        .lock()
        .map(|nonces| nonces.contains_key(nonce))
        .unwrap_or(true)
}

/// A granted run lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLeaseGrant {
    /// No prior owner (or the same holder re-entering).
    Acquired,
    /// A previous owner's lease was reclaimed after inspection proved
    /// it gone.
    Reclaimed { previous_pid: i32 },
}

impl SqliteStore {
    pub fn open(path: &std::path::Path, capacity: u32) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(internal)?;
        Self::init(conn, capacity)
    }

    /// In-memory store for contract tests.
    pub fn in_memory(capacity: u32) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        Self::init(conn, capacity)
    }

    fn init(conn: Connection, capacity: u32) -> Result<Self, StoreError> {
        // Install the busy handler first so even the WAL/schema batch below
        // waits on a concurrent initializer instead of failing with
        // SQLITE_BUSY; a residual timeout still maps to `StoreError::Internal`,
        // which callers propagate without persisting a rejection.
        conn.busy_timeout(BUSY_TIMEOUT).map_err(internal)?;
        // foreign_keys is per-connection state; journal_mode persists in
        // the file header, so it is deferred until after the format gate
        // — a refused file must not be mutated.
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(internal)?;

        // Format gate (identity-storage): refuse anything that is not the
        // current format or an empty file. An unversioned database with
        // user tables is a pre-v1 slice-0 file — archive-only.
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(internal)?;
        if version != 0 && !(1..=FORMAT_VERSION).contains(&version) {
            return Err(StoreError::UnsupportedFormat {
                detail: format!("store format {version} is newer than supported {FORMAT_VERSION}"),
            });
        }
        // A version stamp alone is not proof this is our store: verify
        // the schema shape before any mutating pragma. A foreign or
        // truncated file carrying the stamp is refused as unsupported —
        // never WAL-flipped, migrated, or seeded first.
        let table_set =
            |conn: &Connection| -> Result<std::collections::HashSet<String>, StoreError> {
                let mut stmt = conn
                    .prepare(
                        "SELECT name FROM sqlite_master \
                     WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                    )
                    .map_err(internal)?;
                stmt.query_map([], |r| r.get(0))
                    .map_err(internal)?
                    .collect::<Result<_, _>>()
                    .map_err(internal)
            };

        let mut bootstrapping = false;
        if version == 0 {
            if !table_set(&conn)?.is_empty() {
                // The stamp read above and this table scan are two
                // separate autocommit reads: a concurrent first-opener can
                // commit its bootstrap between them, leaving this opener
                // with version==0 and the winner's tables. Re-read the
                // stamp before refusing — a stamp still 0 is the genuine
                // pre-v1 archive, refused untouched; a stamp that changed
                // is the winner's store, reopened through the ordinary
                // gate instead of being misread as a legacy file.
                let stamped_now: i64 = conn
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .map_err(internal)?;
                if stamped_now == 0 {
                    return Err(StoreError::UnsupportedFormat {
                        detail: "unversioned store file (pre-v1 format); \
                                 archive it and start a new store"
                            .into(),
                    });
                }
                // The stamp only ever moves 0 -> FORMAT_VERSION (one
                // atomic bootstrap commit), so the re-run reads a nonzero
                // stamp, skips this branch, and cannot recurse again.
                return Self::init(conn, capacity);
            }
            bootstrapping = true;
        } else if version < FORMAT_VERSION {
            // An upgrade is legitimate only for a file that holds the
            // complete schema of its stamped version: a v1 file must
            // hold every expected table except the ones added since —
            // the run-lease slice's additive set, the v2, v3, v5, v8,
            // and v9 additions — a v2 file everything except the v3,
            // v5, v8, and v9 additions, a v3 or v4 file everything
            // except the v5, v8, and v9 additions (v4 adds a column,
            // not a table), a v5–v7 file everything except the v8 and
            // v9 additions (v6 and v7 each add a column, not a table),
            // and a v8 file everything except the v9 addition. A
            // foreign or
            // truncated stamped file fails this check untouched, and
            // the accepted difference is a previous release's store: it
            // holds every base table and none of the additions, so it
            // is upgraded in place instead of being locked out with a
            // misleading "not the recognized schema" refusal.
            let present = table_set(&conn)?;
            let required_complete = if version == 1 {
                EXPECTED_TABLES
                    .iter()
                    .filter(|t| {
                        !V2_ADDED_TABLES.contains(t)
                            && !ADDITIVE_TABLES.contains(t)
                            && !V3_ADDED_TABLES.contains(t)
                            && !V5_ADDED_TABLES.contains(t)
                            && !V8_ADDED_TABLES.contains(t)
                            && !V9_ADDED_TABLES.contains(t)
                    })
                    .all(|t| present.contains(*t))
            } else if version == 2 {
                EXPECTED_TABLES
                    .iter()
                    .filter(|t| {
                        !V3_ADDED_TABLES.contains(t)
                            && !V5_ADDED_TABLES.contains(t)
                            && !V8_ADDED_TABLES.contains(t)
                            && !V9_ADDED_TABLES.contains(t)
                    })
                    .all(|t| present.contains(*t))
            } else if version < 5 {
                EXPECTED_TABLES
                    .iter()
                    .filter(|t| {
                        !V5_ADDED_TABLES.contains(t)
                            && !V8_ADDED_TABLES.contains(t)
                            && !V9_ADDED_TABLES.contains(t)
                    })
                    .all(|t| present.contains(*t))
            } else if version < 8 {
                EXPECTED_TABLES
                    .iter()
                    .filter(|t| !V8_ADDED_TABLES.contains(t) && !V9_ADDED_TABLES.contains(t))
                    .all(|t| present.contains(*t))
            } else if version < 9 {
                EXPECTED_TABLES
                    .iter()
                    .filter(|t| !V9_ADDED_TABLES.contains(t))
                    .all(|t| present.contains(*t))
            } else {
                EXPECTED_TABLES.iter().all(|t| present.contains(*t))
            };
            if !required_complete {
                return Err(StoreError::UnsupportedFormat {
                    detail: format!(
                        "store file claims the v{version} format but does \\
                         not hold the recognized schema"
                    ),
                });
            }
            // invocations.descendants, body_catalog.pin_state,
            // invocation_requests.adapter_binding,
            // work_usage.last_progress_basis,
            // recovery_intents.retirement_generation, and the pin-ledger
            // reconstruction. A crash leaves the old stamp and the
            // migration reruns idempotently (CREATE IF NOT EXISTS; each
            // column check guards a partial ALTER; the ledger insert is
            // guarded by NOT EXISTS).
            let tx = conn.unchecked_transaction().map_err(internal)?;
            tx.execute_batch(SCHEMA).map_err(internal)?;
            let has_descendants: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('invocations') \
                     WHERE name='descendants')",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if !has_descendants {
                tx.execute_batch(
                    "ALTER TABLE invocations ADD COLUMN descendants TEXT NOT NULL DEFAULT '[]'",
                )
                .map_err(internal)?;
            }
            let has_pin_state: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('body_catalog') \
                     WHERE name='pin_state')",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if !has_pin_state {
                tx.execute_batch(
                    "ALTER TABLE body_catalog ADD COLUMN pin_state TEXT NOT NULL \
                     DEFAULT 'none' CHECK (pin_state IN \
                     ('none', 'active', 'paused', 'recovering', 'unknown-effect'))",
                )
                .map_err(internal)?;
            }
            // Existing protected rows were expressed as the bare boolean;
            // materialize them as the catalogue's `active` pin state.
            tx.execute(
                "UPDATE body_catalog SET pin_state='active' \
                 WHERE pinned=1 AND pin_state='none'",
                [],
            )
            .map_err(internal)?;
            let has_adapter_binding: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('invocation_requests') \
                     WHERE name='adapter_binding')",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if !has_adapter_binding {
                tx.execute_batch("ALTER TABLE invocation_requests ADD COLUMN adapter_binding TEXT")
                    .map_err(internal)?;
            }
            let has_progress_basis: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_usage') \
                     WHERE name='last_progress_basis')",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if !has_progress_basis {
                tx.execute_batch("ALTER TABLE work_usage ADD COLUMN last_progress_basis TEXT")
                    .map_err(internal)?;
            }
            let has_retirement_generation: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('recovery_intents') \
                     WHERE name='retirement_generation')",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if !has_retirement_generation {
                tx.execute_batch(
                    "ALTER TABLE recovery_intents ADD COLUMN retirement_generation INTEGER",
                )
                .map_err(internal)?;
            }
            // A pre-ledger store recorded protection as the bare
            // `pinned` boolean. Reconstruct the ledger row under the
            // unnamed `registered` dependency so the materialized flag
            // and the row set agree, and so clearing an unrelated
            // dependency can never silently un-protect the body.
            tx.execute(
                "INSERT INTO body_pins (body_id, dependency) \
                 SELECT body_id, 'registered' FROM body_catalog AS c \
                 WHERE c.pinned = 1 \
                   AND NOT EXISTS (SELECT 1 FROM body_pins AS p \
                                   WHERE p.body_id = c.body_id)",
                [],
            )
            .map_err(internal)?;
            tx.pragma_update(None, "user_version", FORMAT_VERSION)
                .map_err(internal)?;
            tx.commit().map_err(internal)?;
        }
        if !bootstrapping {
            let present = table_set(&conn)?;
            if !EXPECTED_TABLES.iter().all(|t| present.contains(*t)) {
                return Err(StoreError::UnsupportedFormat {
                    detail: "store file claims the current format but does \
                             not hold the recognized schema"
                        .into(),
                });
            }
        }
        // journal_mode cannot change inside a transaction, so it is set
        // here — only for a file the gate accepted.
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(internal)?;
        if bootstrapping {
            // Bootstrap is one transaction: schema, format version, and
            // the store_meta row land together or not at all, so a crash
            // can never leave a table-bearing version-0 file (refused as
            // a legacy archive on next open) or a stamped store without a
            // store_id. The gate above runs before this transaction, so a
            // second first-opener can have passed it too; begin IMMEDIATE
            // to take the writer lock before touching the file. Re-read the
            // stamp under that lock: if a winner committed while this
            // connection waited, reopen through the ordinary gate so an
            // older winner is upgraded and a foreign one is refused rather
            // than overwriting its commit.
            let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
                .map_err(internal)?;
            let stamped: i64 = tx
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(internal)?;
            if stamped != 0 {
                tx.rollback().map_err(internal)?;
                return Self::init(conn, capacity);
            }
            tx.execute_batch(SCHEMA).map_err(internal)?;
            tx.pragma_update(None, "user_version", FORMAT_VERSION)
                .map_err(internal)?;
            // The insert is still OR IGNORE: a duplicate store_id must never
            // surface as a bootstrap failure.
            tx.execute(
                "INSERT OR IGNORE INTO store_meta (key, value) VALUES ('store_id', ?1)",
                params![new_id("store")],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)?;
        }
        // The evidence epoch is a durable per-file identity: minted
        // once, never touched by additive migrations, and carried by
        // the file itself so tokens from another store generation
        // mismatch rather than silently continuing a diverged history.
        conn.execute(
            "INSERT OR IGNORE INTO store_meta (key, value) VALUES ('history_epoch', ?1)",
            params![new_id("epoch")],
        )
        .map_err(internal)?;
        // Seed the fixed slot rows once; claiming is an UPDATE inside the
        // caller's transaction. The configured capacity is the effective
        // limit, recorded here so a later open with a smaller value
        // shrinks admission: free rows beyond it are trimmed, rows still
        // held are honored until released but can never be re-claimed —
        // the claim picks only `slot_id < configured_capacity`.
        for slot in 0..capacity {
            conn.execute(
                "INSERT OR IGNORE INTO capacity_slots (slot_id) VALUES (?1)",
                params![i64::from(slot)],
            )
            .map_err(internal)?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO store_meta (key, value) \
             VALUES ('configured_capacity', ?1)",
            params![capacity.to_string()],
        )
        .map_err(internal)?;
        conn.execute(
            "DELETE FROM capacity_slots \
             WHERE slot_id >= ?1 AND execution_id IS NULL",
            params![i64::from(capacity)],
        )
        .map_err(internal)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Claim the installation's single-flight run lease. Acquisition is
    /// one immediate transaction, so contenders serialize on the writer
    /// lock and at most one driver can hold the row. A recorded owner is
    /// displaced only when the supplied inspection proves it gone; a
    /// live owner is refused and an uninspectable owner fails closed —
    /// lock contention and restart can never create two drivers.
    pub fn acquire_run_lease(
        &self,
        owner: &RunLeaseOwner,
        inspect: impl Fn(&ProcessIdentity) -> ProcessEvidence,
        now_unix_seconds: i64,
    ) -> Result<RunLeaseGrant, RunLeaseError> {
        self.acquire_lease(owner, inspect, now_unix_seconds, "default")
    }

    /// The loop-independent installation maintenance lease for explicit
    /// tooling (cleanup, sweep-style passes). A second maintenance
    /// writer is refused with `lock.held`; the loop's driver lease is
    /// unaffected and vice versa.
    pub fn acquire_maintenance_lease(
        &self,
        owner: &RunLeaseOwner,
        inspect: impl Fn(&ProcessIdentity) -> ProcessEvidence,
        now_unix_seconds: i64,
    ) -> Result<RunLeaseGrant, RunLeaseError> {
        self.acquire_lease(owner, inspect, now_unix_seconds, "maintenance")
    }

    fn acquire_lease(
        &self,
        owner: &RunLeaseOwner,
        inspect: impl Fn(&ProcessIdentity) -> ProcessEvidence,
        now_unix_seconds: i64,
        lease_key: &str,
    ) -> Result<RunLeaseGrant, RunLeaseError> {
        let _acquire_gate = LEASE_ACQUIRE_GATE.lock().map_err(|_| {
            RunLeaseError::Store(StoreError::Internal {
                detail: "lease acquire gate poisoned".into(),
            })
        })?;
        let mut guard = self.conn.lock().map_err(|_| {
            RunLeaseError::Store(StoreError::Internal {
                detail: "store lock poisoned".into(),
            })
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| RunLeaseError::Store(internal(e)))?;
        let existing: Option<(i32, String, String)> = tx
            .query_row(
                "SELECT owner_pid, owner_started_at, owner_nonce \
                 FROM run_lease WHERE lease_key=?1",
                params![lease_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| RunLeaseError::Store(map_err(e)))?;
        let mut renewed = false;
        let grant = match existing {
            None => {
                insert_lease(&tx, owner, now_unix_seconds, lease_key)
                    .map_err(RunLeaseError::Store)?;
                RunLeaseGrant::Acquired
            }
            Some((pid, _started_at, nonce)) if nonce == owner.nonce && pid == owner.pid => {
                // The same holder re-entering (a repeated pass of one
                // driver): refresh the acquisition stamp, keep ownership.
                // The nonce is renewed, not registered — this grant
                // already counted its presence when it was granted.
                renewed = true;
                tx.execute(
                    "UPDATE run_lease SET acquired_unix_seconds=?1 \
                     WHERE lease_key=?2",
                    params![now_unix_seconds, lease_key],
                )
                .map_err(|e| RunLeaseError::Store(map_err(e)))?;
                RunLeaseGrant::Acquired
            }
            Some((pid, started_at, nonce)) => {
                let dead_guard_here =
                    pid == owner.pid && started_at == owner.started_at && !is_live_nonce(&nonce);
                if dead_guard_here {
                    // The recorded owner is this very process but its
                    // nonce is held by no live guard here: the guard
                    // was dropped while its release write failed.
                    // Reclaiming cannot create concurrency — a live
                    // same-process holder keeps its nonce registered
                    // and reads `Alive` below instead.
                    tx.execute(
                        "UPDATE run_lease SET owner_pid=?1, \
                         owner_started_at=?2, owner_nonce=?3, \
                         acquired_unix_seconds=?4 \
                         WHERE lease_key=?5",
                        params![
                            owner.pid,
                            owner.started_at,
                            owner.nonce,
                            now_unix_seconds,
                            lease_key
                        ],
                    )
                    .map_err(|e| RunLeaseError::Store(map_err(e)))?;
                    RunLeaseGrant::Reclaimed { previous_pid: pid }
                } else {
                    let evidence = inspect(&ProcessIdentity {
                        pid,
                        started_at,
                        pgid: None,
                    });
                    match evidence {
                        ProcessEvidence::Alive { .. } => {
                            return Err(RunLeaseError::Held { owner_pid: pid });
                        }
                        ProcessEvidence::Stopped { .. } => {
                            // The recorded driver is provably gone (or its
                            // pid was reused under a different start time):
                            // its lease is reclaimable, never concurrent.
                            tx.execute(
                                "UPDATE run_lease SET owner_pid=?1, \
                             owner_started_at=?2, owner_nonce=?3, \
                             acquired_unix_seconds=?4 \
                             WHERE lease_key=?5",
                                params![
                                    owner.pid,
                                    owner.started_at,
                                    owner.nonce,
                                    now_unix_seconds,
                                    lease_key
                                ],
                            )
                            .map_err(|e| RunLeaseError::Store(map_err(e)))?;
                            RunLeaseGrant::Reclaimed { previous_pid: pid }
                        }
                        ProcessEvidence::Unknown { reason } => {
                            return Err(RunLeaseError::Unavailable { reason });
                        }
                    }
                }
            }
        };
        tx.commit().map_err(|e| RunLeaseError::Store(internal(e)))?;
        if renewed {
            renew_live_nonce(&owner.nonce);
        } else {
            register_live_nonce(&owner.nonce);
        }
        Ok(grant)
    }

    /// Release only this owner's maintenance lease.
    pub fn release_maintenance_lease(&self, owner: &RunLeaseOwner) -> Result<(), StoreError> {
        self.release_lease(owner, "maintenance")
    }

    /// Release only this owner's lease. A stale holder's release can
    /// never clear a lease that was reclaimed by another driver.
    pub fn release_run_lease(&self, owner: &RunLeaseOwner) -> Result<(), StoreError> {
        self.release_lease(owner, "default")
    }

    /// Claim the installation-wide intake invocation lease
    /// (`docs/intake-identity-storage.md`): at most one automatic or
    /// manual intake owner at a time. Same reclaim semantics as the
    /// run lease — a recorded owner is displaced only on proof it is
    /// gone; a live owner yields `Held`, an uninspectable one
    /// `Unavailable`.
    pub fn acquire_intake_lease(
        &self,
        owner: &RunLeaseOwner,
        inspect: impl Fn(&ProcessIdentity) -> ProcessEvidence,
        now_unix_seconds: i64,
    ) -> Result<RunLeaseGrant, RunLeaseError> {
        self.acquire_lease(owner, inspect, now_unix_seconds, "intake")
    }

    /// Release only this owner's intake lease. A stale holder's release
    /// can never clear a lease reclaimed by another invocation.
    pub fn release_intake_lease(&self, owner: &RunLeaseOwner) -> Result<(), StoreError> {
        self.release_lease(owner, "intake")
    }

    fn release_lease(&self, owner: &RunLeaseOwner, lease_key: &str) -> Result<(), StoreError> {
        // The guard is dead once release runs, whatever the write's
        // outcome: forgetting the nonce first means a failed delete
        // leaves a reclaimable row instead of a same-process wedge.
        forget_live_nonce(&owner.nonce);
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "DELETE FROM run_lease WHERE lease_key=?3 \
             AND owner_nonce=?1 AND owner_pid=?2",
            params![owner.nonce, owner.pid, lease_key],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// How many events of one kind the audit log holds — used to verify
    /// durable per-pass records (e.g. `tick_report`) after a run.
    pub fn count_events(&self, kind: &str) -> Result<usize, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let n: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind=?1",
                params![kind],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        Ok(n as usize)
    }

    /// The audit-log payloads for one kind, in sequence order — used to
    /// inspect durable per-pass records (e.g. `tick_report`) after a run.
    pub fn events(&self, kind: &str) -> Result<Vec<String>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let mut stmt = guard
            .prepare("SELECT payload FROM event_log WHERE kind=?1 ORDER BY seq")
            .map_err(map_err)?;
        stmt.query_map(params![kind], |r| r.get(0))
            .map_err(map_err)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(map_err)
    }

    /// Append one non-effect event to the audit log (e.g. a durable tick
    /// report). Returns the assigned sequence.
    pub fn append_event(
        &self,
        kind: &str,
        payload_json: &str,
    ) -> Result<EventSequence, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "INSERT INTO event_log (kind, payload) VALUES (?1, ?2)",
            params![kind, payload_json],
        )
        .map_err(map_err)?;
        let seq = tx.last_insert_rowid();
        tx.commit().map_err(internal)?;
        Ok(seq)
    }
}

fn insert_lease(
    tx: &rusqlite::Transaction<'_>,
    owner: &RunLeaseOwner,
    now_unix_seconds: i64,
    lease_key: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO run_lease (lease_key, owner_pid, owner_started_at, \
         owner_nonce, acquired_unix_seconds) VALUES (?5, ?1, ?2, ?3, ?4)",
        params![
            owner.pid,
            owner.started_at,
            owner.nonce,
            now_unix_seconds,
            lease_key
        ],
    )
    .map_err(map_err)?;
    Ok(())
}

pub(crate) fn internal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Internal {
        detail: e.to_string(),
    }
}

/// Map a rusqlite failure to the store's conflict vocabulary.
pub(crate) fn map_err(e: rusqlite::Error) -> StoreError {
    match e {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                ..
            },
            Some(msg),
        ) => StoreError::ConstraintConflict { detail: msg },
        other => internal(other),
    }
}

fn json<T: serde::Serialize>(v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(internal)
}

/// Unit-variant enums are stored as their plain serde tag (`queued`, not
/// `"queued"`) so SQL constraints and partial indexes match on text.
fn tag<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => panic!("expected a string-tagged enum"),
    }
}

fn untag<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, rusqlite::Error> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(conv)
}

fn opt_untag<T: serde::de::DeserializeOwned>(
    s: Option<String>,
) -> Result<Option<T>, rusqlite::Error> {
    s.map(|v| untag(&v)).transpose()
}

/// Row-closure error conversion: inside `query_row`/`query_map` callbacks the
/// error type is `rusqlite::Error`, so `StoreError` helpers map through this.
fn conv(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(RowConv(e.to_string())))
}

fn de_row<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, rusqlite::Error> {
    serde_json::from_str(s).map_err(conv)
}

fn opt_de_row<T: serde::de::DeserializeOwned>(
    s: Option<String>,
) -> Result<Option<T>, rusqlite::Error> {
    s.map(|v| de_row(&v)).transpose()
}

/// A stored command's idempotency key belongs to `replay_key` when it
/// is that key itself or its idempotency-conflict key
/// (`<replay_key>#<digest>`), where the suffix is exactly that
/// command's own payload digest — the form `handle_command` derives
/// from the same origin/actor/key payload. A key that merely shares
/// the prefix (an operator-chosen key, or a key whose selector itself
/// contains `#`) is not this identity and cannot back the candidate.
fn command_key_matches_replay(stored_key: &str, stored_digest: &str, replay_key: &str) -> bool {
    stored_key == replay_key
        || stored_key
            .strip_prefix(replay_key)
            .and_then(|suffix| suffix.strip_prefix('#'))
            .is_some_and(|suffix| suffix == stored_digest)
}

/// The identity two spellings of one Issue share: the canonical host
/// instance replaces whatever endpoint spelling was persisted. Rows
/// written before host canonicalization carry the raw endpoint; every
/// claim and membership read compares this form so an upgraded store
/// still enforces one-Issue-one-claim.
fn canonical_issue(issue: &workloop_core::identity::IssueRef) -> workloop_core::identity::IssueRef {
    let mut issue = issue.clone();
    let canonical = canonical_host_instance(issue.repository.host.as_str());
    if !canonical.is_empty() {
        issue.repository.host = HostInstanceId::trusted(canonical);
    }
    issue
}

/// Issue identity key: repository + opaque issue id — never the display
/// number, which is presentational and must not participate in claims.
/// The key is computed over the canonical host instance; rows written
/// before canonicalization keep their original key and are matched by
/// the tolerant reads, not by this value.
fn issue_key(issue: &workloop_core::identity::IssueRef) -> String {
    let issue = canonical_issue(issue);
    json(&(issue.repository, issue.issue.as_str()))
        .unwrap_or_else(|_| unreachable!("issue key serializes"))
}

struct Tx<'t> {
    tx: rusqlite::Transaction<'t>,
}

impl SqliteStore {
    fn run<R>(
        &self,
        behavior: TransactionBehavior,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(behavior)
            .map_err(internal)?;
        let mut view = Tx { tx };
        let result = f(&mut view);
        match result {
            Ok(r) => {
                view.tx.commit().map_err(internal)?;
                Ok(r)
            }
            Err(e) => {
                let _ = view.tx.rollback();
                Err(e)
            }
        }
    }
}

impl Store for SqliteStore {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.run(TransactionBehavior::Immediate, f)
    }

    fn transact_deferred<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.run(TransactionBehavior::Deferred, f)
    }
}

impl Tx<'_> {
    fn exec(&self, sql: &str, p: impl rusqlite::Params) -> Result<usize, StoreError> {
        self.tx.execute(sql, p).map_err(map_err)
    }

    fn query_opt<T>(
        &self,
        sql: &str,
        p: impl rusqlite::Params,
        f: impl FnOnce(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error>,
    ) -> Result<Option<T>, StoreError> {
        self.tx.query_row(sql, p, f).optional().map_err(map_err)
    }

    fn query_all<T>(
        &self,
        sql: &str,
        p: impl rusqlite::Params,
        f: impl FnMut(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error>,
    ) -> Result<Vec<T>, StoreError> {
        let mut stmt = self.tx.prepare(sql).map_err(internal)?;
        let rows = stmt.query_map(p, f).map_err(map_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_err)
    }

    /// The decision a candidate may link: the command row must belong
    /// to the candidate's own replay identity, so a candidate can never
    /// borrow an unrelated proposal's accepted command or Work. An
    /// idempotency conflict is durable under
    /// `<replay_key>#<payload_digest>`, which is the same identity with
    /// the payload-derived suffix; a prefix-sharing key whose suffix is
    /// not the command's own digest is not that identity.
    fn command_decision_for_replay(
        &self,
        command_id: &CommandId,
        replay_key: &str,
    ) -> Result<Option<CommandDecision>, StoreError> {
        let row: Option<(String, String, String)> = self.query_opt(
            "SELECT idempotency_key, payload_digest, decision FROM commands \
             WHERE command_id=?1",
            params![command_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        match row {
            Some((key, digest, decision))
                if command_key_matches_replay(&key, &digest, replay_key) =>
            {
                Ok(Some(de_row(&decision).map_err(map_err)?))
            }
            _ => Ok(None),
        }
    }
}

#[derive(Debug)]
struct RowConv(String);
impl std::fmt::Display for RowConv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RowConv {}

fn execution_from_row(row: &rusqlite::Row<'_>) -> Result<Execution, rusqlite::Error> {
    Ok(Execution {
        id: ExecutionId(row.get::<_, String>(0)?),
        work_id: WorkId(row.get::<_, String>(1)?),
        ordinal: row.get::<_, u32>(2)?,
        state: untag(&row.get::<_, String>(3)?)?,
        waiting_reason: opt_untag(row.get::<_, Option<String>>(4)?)?,
        desired_action: opt_untag(row.get::<_, Option<String>>(5)?)?,
        terminal_code: row.get(6)?,
        pause_reason: row.get(7)?,
        repair_prerequisites: de_row(&row.get::<_, String>(8)?)?,
        outcome: opt_de_row(row.get::<_, Option<String>>(9)?)?,
        enqueued_seq: row.get(10)?,
        allowed_effects: de_row(&row.get::<_, String>(11)?)?,
        input: de_row(&row.get::<_, String>(12)?)?,
        budget: de_row(&row.get::<_, String>(13)?)?,
        invocations_used: row.get(14)?,
        review_rounds_used: row.get(15)?,
        started_seq: row.get(16)?,
        started_unix_seconds: row.get(17)?,
        recovery_attempts_used: row.get(18)?,
        recovery_deadline_unix_seconds: row.get(19)?,
        uncertain_since_unix_seconds: row.get(20)?,
        paused_since_unix_seconds: row.get(21)?,
        next_role: opt_untag(row.get::<_, Option<String>>(22)?)?,
    })
}

const EXECUTION_COLS: &str = "id, work_id, ordinal, state, waiting_reason, \
    desired_action, terminal_code, pause_reason, repair_prerequisites, outcome, \
    enqueued_seq, allowed_effects, input, budget, invocations_used, \
    review_rounds_used, started_seq, started_unix_seconds, \
    recovery_attempts_used, recovery_deadline_unix_seconds, \
    uncertain_since_unix_seconds, paused_since_unix_seconds, next_role";

const INVOCATION_COLS: &str = "id, request_id, execution_id, role, state, \
    process_identity, descendants, required_artifacts, evidence_refs";

fn invocation_from_row(row: &rusqlite::Row<'_>) -> Result<Invocation, rusqlite::Error> {
    Ok(Invocation {
        id: InvocationId(row.get::<_, String>(0)?),
        request_id: row.get(1)?,
        execution_id: ExecutionId(row.get::<_, String>(2)?),
        role: untag(&row.get::<_, String>(3)?)?,
        state: untag(&row.get::<_, String>(4)?)?,
        process_identity: opt_de_row(row.get::<_, Option<String>>(5)?)?,
        descendants: de_row(&row.get::<_, String>(6)?)?,
        required_artifacts: de_row(&row.get::<_, String>(7)?)?,
        evidence_refs: de_row(&row.get::<_, String>(8)?)?,
    })
}

const HANDOFF_COLS: &str = "artifact_id, invocation_id, request_id, work_id, \
    execution_id, base, produced, manifest_digest, state, invalid_reason, \
    attempt, recorded_seq";

fn handoff_from_row(row: &rusqlite::Row<'_>) -> Result<ArtifactHandoff, rusqlite::Error> {
    Ok(ArtifactHandoff {
        artifact_id: row.get(0)?,
        invocation_id: InvocationId(row.get::<_, String>(1)?),
        request_id: row.get(2)?,
        work_id: WorkId(row.get::<_, String>(3)?),
        execution_id: ExecutionId(row.get::<_, String>(4)?),
        base: row.get(5)?,
        produced: row.get(6)?,
        manifest_digest: row.get(7)?,
        state: untag(&row.get::<_, String>(8)?)?,
        invalid_reason: row.get(9)?,
        attempt: row.get::<_, u32>(10)?,
        recorded_seq: row.get(11)?,
    })
}

const WORKER_REQUEST_EXPORT_COLS: &str =
    "invocation_id, request_id, profile_id, workspace_root, digest, state, process_identity";

fn worker_request_export_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<WorkerRequestExport, rusqlite::Error> {
    Ok(WorkerRequestExport {
        invocation_id: InvocationId(row.get::<_, String>(0)?),
        request_id: row.get(1)?,
        profile_id: row.get(2)?,
        workspace_root: row.get(3)?,
        digest: row.get(4)?,
        state: untag(&row.get::<_, String>(5)?)?,
        process_identity: row.get(6)?,
    })
}

const VERIFICATION_COLS: &str = "id, artifact_id, requirement, argv, produced, \
    environment_digest, result, output, recorded_seq";

fn verification_from_row(row: &rusqlite::Row<'_>) -> Result<VerificationResult, rusqlite::Error> {
    Ok(VerificationResult {
        id: row.get(0)?,
        artifact_id: row.get(1)?,
        requirement: row.get(2)?,
        argv: de_row(&row.get::<_, String>(3)?)?,
        produced: row.get(4)?,
        environment_digest: row.get(5)?,
        result: untag(&row.get::<_, String>(6)?)?,
        output: row.get(7)?,
        recorded_seq: row.get(8)?,
    })
}

fn request_from_row(row: &rusqlite::Row<'_>) -> Result<InvocationRequest, rusqlite::Error> {
    Ok(InvocationRequest {
        request_id: row.get(0)?,
        work_id: WorkId(row.get::<_, String>(1)?),
        execution_id: ExecutionId(row.get::<_, String>(2)?),
        role: untag(&row.get::<_, String>(3)?)?,
        spec_revision: row.get(4)?,
        payload: de_row(&row.get::<_, String>(5)?)?,
        adapter_binding: opt_de_row(row.get::<_, Option<String>>(6)?)?,
    })
}

fn observation_from_row(row: &rusqlite::Row<'_>) -> Result<Observation, rusqlite::Error> {
    let scope: Option<String> = row.get(0)?;
    let host: Option<String> = row.get(1)?;
    let source: Option<String> = row.get(2)?;
    let event_id: Option<String> = row.get(3)?;
    let delivery = match (scope.as_deref(), host, source, event_id) {
        (Some("host"), Some(h), Some(s), Some(e)) => Some(DeliveryId {
            scope: DeliveryScope::Host {
                host: HostInstanceId::trusted(h),
            },
            source: s,
            event_id: e,
        }),
        (Some("local"), _, Some(s), Some(e)) => Some(DeliveryId {
            scope: DeliveryScope::Local,
            source: s,
            event_id: e,
        }),
        _ => None,
    };
    Ok(Observation {
        delivery,
        subject_work_id: row.get::<_, Option<String>>(4)?.map(WorkId),
        subject_execution_id: row.get::<_, Option<String>>(5)?.map(ExecutionId),
        fact: de_row(&row.get::<_, String>(6)?)?,
        observed_unix_seconds: row.get(7)?,
    })
}

const OBSERVATION_COLS: &str = "delivery_scope, delivery_host, delivery_source, \
    delivery_event_id, subject_work_id, subject_execution_id, fact, \
    observed_unix_seconds";

fn work_from_row(row: &rusqlite::Row<'_>) -> Result<Work, rusqlite::Error> {
    Ok(Work {
        id: WorkId(row.get::<_, String>(0)?),
        version: row.get::<_, u64>(1)?,
        binding: de_row(&row.get::<_, String>(2)?)?,
        spec: row.get(3)?,
        approved_spec: row.get(4)?,
        source_gate: untag(&row.get::<_, String>(5)?)?,
        policy: de_row(&row.get::<_, String>(6)?)?,
        created_seq: row.get(7)?,
    })
}

const WORK_COLS: &str =
    "id, version, binding, spec, approved_spec, source_gate, policy, created_seq";

fn spec_from_row(row: &rusqlite::Row<'_>) -> Result<SpecSnapshot, rusqlite::Error> {
    Ok(SpecSnapshot {
        work_id: WorkId(row.get::<_, String>(0)?),
        revision: row.get(1)?,
        payload: de_row(&row.get::<_, String>(2)?)?,
        digest: row.get(3)?,
        approved_by: de_row(&row.get::<_, String>(4)?)?,
        recorded_seq: row.get(5)?,
    })
}

fn intent_from_row(row: &rusqlite::Row<'_>) -> Result<HostActionIntent, rusqlite::Error> {
    Ok(HostActionIntent {
        action_id: row.get(0)?,
        work_id: WorkId(row.get::<_, String>(1)?),
        // The recorded host is an endpoint spelling on rows written
        // before host-instance unification; reads reduce it to the
        // canonical identity the bound adapter compares against.
        host: HostInstanceId::trusted(canonical_host_instance(&row.get::<_, String>(2)?)),
        repository: de_row(&row.get::<_, String>(3)?)?,
        role: untag(&row.get::<_, String>(4)?)?,
        actor: de_row(&row.get::<_, String>(5)?)?,
        action: de_row(&row.get::<_, String>(6)?)?,
        payload_digest: row.get(7)?,
        status: de_row(&row.get::<_, String>(8)?)?,
        authenticated_actor: opt_de_row(row.get::<_, Option<String>>(9)?)?,
        effects: de_row(&row.get::<_, String>(10)?)?,
        created_seq: row.get(11)?,
    })
}

fn round_from_row(row: &rusqlite::Row<'_>) -> Result<ReviewRound, rusqlite::Error> {
    Ok(ReviewRound {
        id: row.get(0)?,
        work_id: WorkId(row.get::<_, String>(1)?),
        target: de_row(&row.get::<_, String>(2)?)?,
        source: row.get(3)?,
        source_revision: row.get(4)?,
        completeness: row.get(5)?,
        set_digest: row.get(6)?,
        generation: row.get(7)?,
        seq: row.get(8)?,
    })
}

fn judgment_from_row(row: &rusqlite::Row<'_>) -> Result<RoundJudgment, rusqlite::Error> {
    Ok(RoundJudgment {
        id: row.get(0)?,
        work_id: WorkId(row.get::<_, String>(1)?),
        round_id: row.get(2)?,
        set_digest: row.get(3)?,
        decisions: de_row(&row.get::<_, String>(4)?)?,
        actor_id: row.get(5)?,
        seq: row.get(6)?,
    })
}

fn usage_from_row(row: &rusqlite::Row<'_>) -> Result<WorkUsage, rusqlite::Error> {
    Ok(WorkUsage {
        work_id: WorkId(row.get::<_, String>(0)?),
        fix_invocations_used: row.get(1)?,
        transport_retries_used: row.get(2)?,
        equivalent_failure_streak: row.get(3)?,
        equivalent_signature: row.get(4)?,
        automatic_seconds_used: row.get(5)?,
        first_dispatch_unix_seconds: row.get(6)?,
        last_progress_unix_seconds: row.get(7)?,
        human_wait_seconds_total: row.get(8)?,
        last_progress_basis: row
            .get::<_, Option<String>>(9)?
            .and_then(|s| MaterialProgressBasis::parse(&s)),
    })
}

const USAGE_COLS: &str = "work_id, fix_invocations_used, transport_retries_used, \
    equivalent_failure_streak, equivalent_signature, automatic_seconds_used, \
    first_dispatch_unix_seconds, last_progress_unix_seconds, \
    human_wait_seconds_total, last_progress_basis";

fn write_execution(tx: &Tx<'_>, e: &Execution) -> Result<(), StoreError> {
    tx.exec(
        "INSERT INTO executions (id, work_id, ordinal, state, waiting_reason, \
         desired_action, terminal_code, pause_reason, repair_prerequisites, \
         outcome, enqueued_seq, allowed_effects, input, budget, \
         invocations_used, review_rounds_used, started_seq, \
         started_unix_seconds, recovery_attempts_used, \
         recovery_deadline_unix_seconds, uncertain_since_unix_seconds, \
         paused_since_unix_seconds, next_role) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
        params![
            e.id.0,
            e.work_id.0,
            e.ordinal,
            tag(&e.state),
            e.waiting_reason.as_ref().map(tag),
            e.desired_action.as_ref().map(tag),
            e.terminal_code,
            e.pause_reason,
            json(&e.repair_prerequisites)?,
            e.outcome.as_ref().map(json).transpose()?,
            e.enqueued_seq,
            json(&e.allowed_effects)?,
            json(&e.input)?,
            json(&e.budget)?,
            e.invocations_used,
            e.review_rounds_used,
            e.started_seq,
            e.started_unix_seconds,
            e.recovery_attempts_used,
            e.recovery_deadline_unix_seconds,
            e.uncertain_since_unix_seconds,
            e.paused_since_unix_seconds,
            e.next_role.as_ref().map(tag),
        ],
    )?;
    Ok(())
}

fn update_execution(
    tx: &Tx<'_>,
    e: &Execution,
    expected: &ExecutionState,
) -> Result<(), StoreError> {
    let n = tx.exec(
        "UPDATE executions SET state=?4, waiting_reason=?5, desired_action=?6, \
         terminal_code=?7, pause_reason=?8, repair_prerequisites=?9, outcome=?10, \
         enqueued_seq=?11, allowed_effects=?12, input=?13, budget=?14, \
         invocations_used=?15, review_rounds_used=?16, started_seq=?17, \
         started_unix_seconds=?18, recovery_attempts_used=?19, \
         recovery_deadline_unix_seconds=?20, uncertain_since_unix_seconds=?21, \
         paused_since_unix_seconds=?22, next_role=?23 \
         WHERE id=?1 AND work_id=?2 AND ordinal=?3 AND state=?24",
        params![
            e.id.0,
            e.work_id.0,
            e.ordinal,
            tag(&e.state),
            e.waiting_reason.as_ref().map(tag),
            e.desired_action.as_ref().map(tag),
            e.terminal_code,
            e.pause_reason,
            json(&e.repair_prerequisites)?,
            e.outcome.as_ref().map(json).transpose()?,
            e.enqueued_seq,
            json(&e.allowed_effects)?,
            json(&e.input)?,
            json(&e.budget)?,
            e.invocations_used,
            e.review_rounds_used,
            e.started_seq,
            e.started_unix_seconds,
            e.recovery_attempts_used,
            e.recovery_deadline_unix_seconds,
            e.uncertain_since_unix_seconds,
            e.paused_since_unix_seconds,
            e.next_role.as_ref().map(tag),
            tag(expected),
        ],
    )?;
    if n == 0 {
        let current = tx
            .tx
            .query_row(
                "SELECT state FROM executions WHERE id=?1",
                params![e.id.0],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(map_err)?
            .map(|s| untag::<ExecutionState>(&s).map_err(internal))
            .transpose()?
            .unwrap_or(ExecutionState::Cancelled);
        return Err(StoreError::StateConflict {
            execution_id: e.id.clone(),
            current_state: current,
        });
    }
    Ok(())
}

fn update_work(tx: &Tx<'_>, work: &Work, expected: u64) -> Result<(), StoreError> {
    let n = tx.exec(
        "UPDATE works SET version=?2, binding=?3, spec=?4, approved_spec=?5, \
         source_gate=?6, policy=?7 WHERE id=?1 AND version=?8",
        params![
            work.id.0,
            work.version,
            json(&work.binding)?,
            work.spec,
            work.approved_spec,
            tag(&work.source_gate),
            json(&work.policy)?,
            expected,
        ],
    )?;
    if n == 0 {
        let current: u64 = tx
            .tx
            .query_row(
                "SELECT version FROM works WHERE id=?1",
                params![work.id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .unwrap_or(0);
        return Err(StoreError::VersionConflict {
            work_id: work.id.clone(),
            current_version: current,
        });
    }
    Ok(())
}

fn insert_spec(tx: &Tx<'_>, spec: &SpecSnapshot) -> Result<(), StoreError> {
    tx.exec(
        "INSERT INTO specs (work_id, revision, payload, digest, approved_by, \
         recorded_seq) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            spec.work_id.0,
            spec.revision,
            json(&spec.payload)?,
            spec.digest,
            json(&spec.approved_by)?,
            spec.recorded_seq,
        ],
    )?;
    Ok(())
}

fn insert_claims(
    tx: &Tx<'_>,
    work_id: &WorkId,
    issues: &[workloop_core::identity::IssueRef],
) -> Result<(), StoreError> {
    for issue in issues {
        tx.exec(
            "INSERT INTO issue_claims (issue_key, issue, work_id) \
             VALUES (?1,?2,?3)",
            params![issue_key(issue), json(issue)?, work_id.0],
        )?;
    }
    Ok(())
}

impl ReadView for Tx<'_> {
    fn store_id(&self) -> Result<StoreId, StoreError> {
        let s: String = self
            .tx
            .query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        Ok(StoreId::trusted(s))
    }

    fn stored_decision(
        &self,
        origin: &Origin,
        actor: &Actor,
        idempotency_key: &str,
    ) -> Result<Option<(String, CommandDecision)>, StoreError> {
        let actor_kind = tag(&actor.kind);
        self.query_opt(
            "SELECT payload_digest, decision FROM commands \
             WHERE origin=?1 AND actor_kind=?2 AND actor_id=?3 AND idempotency_key=?4",
            params![origin.0, actor_kind, actor.external_id, idempotency_key],
            |row| Ok((row.get::<_, String>(0)?, de_row(&row.get::<_, String>(1)?)?)),
        )
    }

    fn command_decision(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<CommandDecision>, StoreError> {
        self.query_opt(
            "SELECT decision FROM commands WHERE command_id=?1",
            params![command_id.0],
            |row| de_row(&row.get::<_, String>(0)?),
        )
    }

    fn work(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError> {
        self.query_opt(
            &format!("SELECT {WORK_COLS} FROM works WHERE id=?1"),
            params![work_id.0],
            work_from_row,
        )
    }

    fn spec(
        &self,
        work_id: &WorkId,
        revision: Option<SpecRevision>,
    ) -> Result<Option<SpecSnapshot>, StoreError> {
        match revision {
            Some(rev) => self.query_opt(
                "SELECT work_id, revision, payload, digest, approved_by, \
                 recorded_seq FROM specs WHERE work_id=?1 AND revision=?2",
                params![work_id.0, rev],
                spec_from_row,
            ),
            None => self.query_opt(
                "SELECT s.work_id, s.revision, s.payload, s.digest, \
                 s.approved_by, s.recorded_seq FROM specs s \
                 JOIN works w ON w.id = s.work_id AND w.spec = s.revision \
                 WHERE s.work_id=?1",
                params![work_id.0],
                spec_from_row,
            ),
        }
    }

    fn execution(&self, execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError> {
        self.query_opt(
            &format!("SELECT {EXECUTION_COLS} FROM executions WHERE id=?1"),
            params![execution_id.0],
            execution_from_row,
        )
    }

    fn active_execution(&self, work_id: &WorkId) -> Result<Option<Execution>, StoreError> {
        self.query_opt(
            &format!(
                "SELECT {EXECUTION_COLS} FROM executions WHERE work_id=?1 \
                 AND state NOT IN ('succeeded','failed','cancelled')"
            ),
            params![work_id.0],
            execution_from_row,
        )
    }

    fn executions_for_work(&self, work_id: &WorkId) -> Result<Vec<Execution>, StoreError> {
        self.query_all(
            &format!("SELECT {EXECUTION_COLS} FROM executions WHERE work_id=?1 ORDER BY ordinal"),
            params![work_id.0],
            execution_from_row,
        )
    }

    fn queued_executions(&self) -> Result<Vec<Execution>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {EXECUTION_COLS} FROM executions WHERE state='queued' \
                 ORDER BY enqueued_seq"
            ),
            [],
            execution_from_row,
        )
    }

    fn issue_refs(
        &self,
        work_id: &WorkId,
    ) -> Result<Vec<workloop_core::identity::IssueRef>, StoreError> {
        self.query_all(
            "SELECT issue FROM work_issue_refs WHERE work_id=?1 ORDER BY issue_key",
            params![work_id.0],
            |row| de_row(&row.get::<_, String>(0)?),
        )
    }

    fn issue_claim(
        &self,
        issue: &workloop_core::identity::IssueRef,
    ) -> Result<Option<WorkId>, StoreError> {
        // Rows persisted before host canonicalization carry the raw
        // endpoint spelling in both the key and the document, so the
        // match is on the canonical identity rather than the stored
        // key bytes. `issue_claims` holds only active claims — the
        // scan stays small.
        let want = canonical_issue(issue);
        let rows = self.query_all("SELECT issue, work_id FROM issue_claims", [], |row| {
            Ok((
                de_row::<workloop_core::identity::IssueRef>(&row.get::<_, String>(0)?)?,
                WorkId(row.get::<_, String>(1)?),
            ))
        })?;
        Ok(rows
            .into_iter()
            .find(|(stored, _)| canonical_issue(stored) == want)
            .map(|(_, work_id)| work_id))
    }

    fn work_usage(&self, work_id: &WorkId) -> Result<Option<WorkUsage>, StoreError> {
        self.query_opt(
            &format!("SELECT {USAGE_COLS} FROM work_usage WHERE work_id=?1"),
            params![work_id.0],
            usage_from_row,
        )
    }

    fn reservation(&self, resource_key: &str) -> Result<Option<ResourceReservation>, StoreError> {
        self.query_opt(
            "SELECT resource_key, resource, holder, expires_at_unix_seconds \
             FROM resource_reservations WHERE resource_key=?1",
            params![resource_key],
            |row| {
                Ok(ResourceReservation {
                    resource_key: row.get(0)?,
                    resource: de_row(&row.get::<_, String>(1)?)?,
                    holder: de_row(&row.get::<_, String>(2)?)?,
                    expires_at_unix_seconds: row.get(3)?,
                })
            },
        )
    }

    fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError> {
        self.query_all(
            "SELECT resource_key, resource, holder, expires_at_unix_seconds \
             FROM resource_reservations",
            [],
            |row| {
                Ok(ResourceReservation {
                    resource_key: row.get(0)?,
                    resource: de_row(&row.get::<_, String>(1)?)?,
                    holder: de_row(&row.get::<_, String>(2)?)?,
                    expires_at_unix_seconds: row.get(3)?,
                })
            },
        )
    }

    fn slot_limit(&self) -> Result<u32, StoreError> {
        // The effective limit is the last configured capacity — the
        // row count can exceed it while slots beyond the shrink point
        // remain held.
        let configured: Option<String> = self
            .tx
            .query_row(
                "SELECT value FROM store_meta WHERE key='configured_capacity'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        match configured {
            Some(value) => value.parse::<u32>().map_err(|_| StoreError::Internal {
                detail: format!("corrupt configured_capacity: {value:?}"),
            }),
            None => {
                let n: i64 = self
                    .tx
                    .query_row("SELECT COUNT(*) FROM capacity_slots", [], |r| r.get(0))
                    .map_err(map_err)?;
                Ok(n as u32)
            }
        }
    }

    fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError> {
        self.query_all(
            "SELECT s.execution_id, e.enqueued_seq FROM capacity_slots s \
             JOIN executions e ON e.id = s.execution_id \
             WHERE s.execution_id IS NOT NULL",
            [],
            |row| Ok((ExecutionId(row.get::<_, String>(0)?), row.get::<_, i64>(1)?)),
        )
    }

    fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError> {
        self.query_all(
            "SELECT request_id, work_id, execution_id, role, spec_revision, \
             payload, adapter_binding FROM invocation_requests \
             WHERE consumed=0 ORDER BY rowid",
            [],
            request_from_row,
        )
    }

    fn request(&self, request_id: &str) -> Result<Option<InvocationRequest>, StoreError> {
        self.query_opt(
            "SELECT request_id, work_id, execution_id, role, spec_revision, \
             payload, adapter_binding FROM invocation_requests WHERE request_id=?1",
            params![request_id],
            request_from_row,
        )
    }

    fn invocations_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<Invocation>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {INVOCATION_COLS} FROM invocations \
                      WHERE execution_id=?1 ORDER BY rowid"
            ),
            params![execution_id.0],
            invocation_from_row,
        )
    }

    fn invocation_for_request(&self, request_id: &str) -> Result<Option<Invocation>, StoreError> {
        self.query_opt(
            &format!("SELECT {INVOCATION_COLS} FROM invocations WHERE request_id=?1"),
            params![request_id],
            invocation_from_row,
        )
    }

    fn unresolved_invocations(&self) -> Result<Vec<Invocation>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {INVOCATION_COLS} FROM invocations \
                      WHERE state IN ('prepared','running','unknown') ORDER BY rowid"
            ),
            [],
            invocation_from_row,
        )
    }

    fn works(&self) -> Result<Vec<Work>, StoreError> {
        self.query_all(
            &format!("SELECT {WORK_COLS} FROM works ORDER BY created_seq"),
            [],
            work_from_row,
        )
    }

    fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {EXECUTION_COLS} FROM executions \
                      WHERE state NOT IN ('succeeded','failed','cancelled') \
                      ORDER BY enqueued_seq"
            ),
            [],
            execution_from_row,
        )
    }

    fn stop_intent(&self, invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError> {
        self.query_opt(
            "SELECT invocation_id, identities, recorded_seq \
             FROM stop_intents WHERE invocation_id=?1",
            params![invocation_id.0],
            |row| {
                Ok(StopIntent {
                    invocation_id: InvocationId(row.get::<_, String>(0)?),
                    identities: de_row(&row.get::<_, String>(1)?)?,
                    recorded_seq: row.get(2)?,
                })
            },
        )
    }

    fn worker_request_export(
        &self,
        request_id: &str,
    ) -> Result<Option<WorkerRequestExport>, StoreError> {
        self.query_opt(
            &format!(
                "SELECT {WORKER_REQUEST_EXPORT_COLS} FROM worker_request_exports \
                 WHERE request_id=?1"
            ),
            params![request_id],
            worker_request_export_from_row,
        )
    }

    fn artifact_handoff(&self, artifact_id: &str) -> Result<Option<ArtifactHandoff>, StoreError> {
        self.query_opt(
            &format!("SELECT {HANDOFF_COLS} FROM artifact_handoffs WHERE artifact_id=?1"),
            params![artifact_id],
            handoff_from_row,
        )
    }

    fn handoff_for_invocation(
        &self,
        invocation_id: &InvocationId,
    ) -> Result<Option<ArtifactHandoff>, StoreError> {
        self.query_opt(
            &format!(
                "SELECT {HANDOFF_COLS} FROM artifact_handoffs \
                 WHERE invocation_id=?1 ORDER BY artifact_id"
            ),
            params![invocation_id.0],
            handoff_from_row,
        )
    }

    fn incomplete_handoffs(&self) -> Result<Vec<ArtifactHandoff>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {HANDOFF_COLS} FROM artifact_handoffs \
                 WHERE state NOT IN ('acknowledged','invalid') ORDER BY recorded_seq"
            ),
            [],
            handoff_from_row,
        )
    }

    fn verification_results(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<VerificationResult>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {VERIFICATION_COLS} FROM verification_results \
                 WHERE artifact_id=?1 ORDER BY recorded_seq"
            ),
            params![artifact_id],
            verification_from_row,
        )
    }

    fn deferrals(&self) -> Result<Vec<Deferral>, StoreError> {
        self.query_all(
            "SELECT execution_id, reason, diagnostic, since_seq FROM deferrals",
            [],
            |row| {
                Ok(Deferral {
                    execution_id: ExecutionId(row.get::<_, String>(0)?),
                    reason: row.get(1)?,
                    diagnostic: row.get(2)?,
                    since_seq: row.get(3)?,
                })
            },
        )
    }

    fn attention_acks(&self, work_id: &WorkId) -> Result<Vec<String>, StoreError> {
        self.query_all(
            "SELECT attention_id FROM attention_acks WHERE work_id=?1",
            params![work_id.0],
            |row| row.get(0),
        )
    }

    fn observations_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<Observation>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {OBSERVATION_COLS} FROM observations \
                 WHERE subject_execution_id=?1 ORDER BY id"
            ),
            params![execution_id.0],
            observation_from_row,
        )
    }

    fn observations_for_work(&self, work_id: &WorkId) -> Result<Vec<Observation>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {OBSERVATION_COLS} FROM observations \
                 WHERE subject_work_id=?1 \
                    OR subject_execution_id IN (SELECT id FROM executions WHERE work_id=?1) \
                 ORDER BY id"
            ),
            params![work_id.0],
            observation_from_row,
        )
    }

    fn all_observations(&self) -> Result<Vec<Observation>, StoreError> {
        self.query_all(
            &format!("SELECT {OBSERVATION_COLS} FROM observations ORDER BY id"),
            [],
            observation_from_row,
        )
    }

    fn action_intents(&self) -> Result<Vec<HostActionIntent>, StoreError> {
        self.query_all(
            "SELECT action_id, work_id, host, repository, role, actor, action, \
             payload_digest, status, authenticated_actor, effects, created_seq \
             FROM host_action_intents ORDER BY rowid",
            [],
            intent_from_row,
        )
    }

    fn latest_round(&self, work_id: &WorkId) -> Result<Option<ReviewRound>, StoreError> {
        self.query_opt(
            "SELECT id, work_id, target, source, source_revision, completeness, \
             set_digest, generation, seq FROM review_rounds \
             WHERE work_id=?1 ORDER BY seq DESC LIMIT 1",
            params![work_id.0],
            round_from_row,
        )
    }

    fn rounds_for_work(&self, work_id: &WorkId) -> Result<Vec<ReviewRound>, StoreError> {
        self.query_all(
            "SELECT id, work_id, target, source, source_revision, completeness, \
             set_digest, generation, seq FROM review_rounds \
             WHERE work_id=?1 ORDER BY seq",
            params![work_id.0],
            round_from_row,
        )
    }

    fn occurrences(&self, round_id: &str) -> Result<Vec<FindingOccurrence>, StoreError> {
        self.query_all(
            "SELECT id, round_id, source_key, content_digest, severity, \
             position, thread_id FROM finding_occurrences \
             WHERE round_id=?1 ORDER BY id",
            params![round_id],
            |row| {
                Ok(FindingOccurrence {
                    id: row.get(0)?,
                    round_id: row.get(1)?,
                    source_key: row.get(2)?,
                    content_digest: row.get(3)?,
                    severity: row.get(4)?,
                    position: row.get(5)?,
                    thread_id: row.get(6)?,
                })
            },
        )
    }

    fn judgment_for_round(&self, round_id: &str) -> Result<Option<RoundJudgment>, StoreError> {
        self.query_opt(
            "SELECT id, work_id, round_id, set_digest, decisions, actor_id, seq \
             FROM round_judgments WHERE round_id=?1",
            params![round_id],
            judgment_from_row,
        )
    }

    fn latest_judgment(&self, work_id: &WorkId) -> Result<Option<RoundJudgment>, StoreError> {
        self.query_opt(
            "SELECT id, work_id, round_id, set_digest, decisions, actor_id, seq \
             FROM round_judgments WHERE work_id=?1 ORDER BY seq DESC LIMIT 1",
            params![work_id.0],
            judgment_from_row,
        )
    }

    fn recovery_decisions(&self, work_id: &WorkId) -> Result<Vec<RecoveryDecision>, StoreError> {
        self.query_all(
            "SELECT id, work_id, execution_id, classification, signature, \
             prerequisites, detail, usage_after, seq FROM recovery_decisions \
             WHERE work_id=?1 ORDER BY seq",
            params![work_id.0],
            |row| {
                Ok(RecoveryDecision {
                    id: row.get(0)?,
                    work_id: WorkId(row.get::<_, String>(1)?),
                    execution_id: row.get::<_, Option<String>>(2)?.map(ExecutionId),
                    classification: row.get(3)?,
                    signature: row.get(4)?,
                    prerequisites: de_row(&row.get::<_, String>(5)?)?,
                    detail: row.get(6)?,
                    usage_after: de_row(&row.get::<_, String>(7)?)?,
                    seq: row.get(8)?,
                })
            },
        )
    }

    fn last_seq(&self) -> Result<EventSequence, StoreError> {
        let s: Option<i64> = self
            .tx
            .query_row("SELECT MAX(seq) FROM event_log", [], |r| r.get(0))
            .map_err(map_err)?;
        Ok(s.unwrap_or(0))
    }

    fn recovery_intents(&self) -> Result<Vec<RecoveryIntent>, StoreError> {
        self.query_all(
            "SELECT intent_id, work_id, execution_id, kind, \
             due_at_unix_seconds, generation, evidence_basis, state, \
             retirement_reason, retirement_generation, scheduled_seq \
             FROM recovery_intents \
             ORDER BY due_at_unix_seconds, intent_id",
            [],
            |row| {
                Ok(RecoveryIntent {
                    intent_id: row.get(0)?,
                    work_id: WorkId(row.get::<_, String>(1)?),
                    execution_id: row.get::<_, Option<String>>(2)?.map(ExecutionId),
                    kind: row.get(3)?,
                    due_at_unix_seconds: row.get(4)?,
                    generation: row.get::<_, i64>(5)? as u64,
                    evidence_basis: row.get(6)?,
                    state: untag(&row.get::<_, String>(7)?)?,
                    retirement_reason: row.get(8)?,
                    retirement_generation: row.get::<_, Option<i64>>(9)?.map(|g| g as u64),
                    scheduled_seq: row.get(10)?,
                })
            },
        )
    }

    fn intake_run(&self, run_id: &str) -> Result<Option<IntakeRun>, StoreError> {
        self.query_opt(
            &format!("SELECT {INTAKE_RUN_COLS} FROM intake_runs WHERE run_id=?1"),
            params![run_id],
            intake_run_from_row,
        )
    }

    fn intake_runs(&self) -> Result<Vec<IntakeRun>, StoreError> {
        self.query_all(
            &format!("SELECT {INTAKE_RUN_COLS} FROM intake_runs ORDER BY recorded_seq"),
            [],
            intake_run_from_row,
        )
    }

    fn active_intake_run(&self) -> Result<Option<IntakeRun>, StoreError> {
        self.query_opt(
            &format!(
                "SELECT {INTAKE_RUN_COLS} FROM intake_runs \
                 WHERE state IN ('open', 'admitting')"
            ),
            [],
            intake_run_from_row,
        )
    }

    fn intake_candidates(&self, run_id: &str) -> Result<Vec<IntakeCandidate>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {INTAKE_CANDIDATE_COLS} FROM intake_candidates \
                 WHERE run_id=?1 ORDER BY ordinal"
            ),
            params![run_id],
            intake_candidate_from_row,
        )
    }

    fn intake_candidates_for_proposal(
        &self,
        proposal_key: &str,
    ) -> Result<Vec<IntakeCandidate>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {INTAKE_CANDIDATE_COLS} FROM intake_candidates \
                 WHERE proposal_key=?1 ORDER BY recorded_seq, run_id, ordinal"
            ),
            params![proposal_key],
            intake_candidate_from_row,
        )
    }

    fn intake_candidates_for_command(
        &self,
        command_id: &CommandId,
    ) -> Result<Vec<IntakeCandidate>, StoreError> {
        self.query_all(
            &format!(
                "SELECT {INTAKE_CANDIDATE_COLS} FROM intake_candidates \
                 WHERE command_id=?1 ORDER BY recorded_seq, run_id, ordinal"
            ),
            params![command_id.0],
            intake_candidate_from_row,
        )
    }

    fn intake_candidates_by_status(
        &self,
        statuses: &[IntakeCandidateStatus],
    ) -> Result<Vec<IntakeCandidate>, StoreError> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {INTAKE_CANDIDATE_COLS} FROM intake_candidates \
             WHERE status IN ({placeholders}) ORDER BY recorded_seq, run_id, ordinal"
        );
        let tags: Vec<String> = statuses.iter().map(tag).collect();
        self.query_all(
            &sql,
            rusqlite::params_from_iter(tags.iter()),
            intake_candidate_from_row,
        )
    }

    fn intake_settings(&self) -> Result<Option<IntakeSettingsRecord>, StoreError> {
        self.query_opt(
            "SELECT settings_revision, pending_queue_limit, recorded_seq \
             FROM intake_settings WHERE singleton=1",
            [],
            |row| {
                Ok(IntakeSettingsRecord {
                    settings_revision: row.get(0)?,
                    pending_queue_limit: row.get::<_, i64>(1)? as u32,
                    recorded_seq: row.get(2)?,
                })
            },
        )
    }

    fn pending_queue_count(&self) -> Result<u64, StoreError> {
        // The pending queue is the set of distinct Works whose active
        // (nonterminal) Execution is `queued`; executing states and
        // `paused` hold no position. At most one nonterminal Execution
        // exists per Work, so the DISTINCT guard is belt-and-braces
        // over the documented semantics.
        let n: i64 = self
            .tx
            .query_row(
                "SELECT COUNT(DISTINCT work_id) FROM executions \
                 WHERE state='queued'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        Ok(n as u64)
    }

    fn intake_source_owners(
        &self,
        issues: &[workloop_core::identity::IssueRef],
    ) -> Result<Vec<IntakeSourceOwner>, StoreError> {
        if issues.is_empty() {
            return Ok(Vec::new());
        }
        // Immutable membership (work_issue_refs), not live claims: a
        // paused Work holds no claim yet still owns its issues for
        // exclusion purposes. The active Execution's state decides
        // whether the owner is live for the selector's scope —
        // Every non-terminal state: `ready`, `waiting`, `stopping`,
        // and `uncertain` own their issues exactly like `queued`,
        // `running`, and `paused` — only the three terminal states
        // release them.
        // The match is on canonical identity, not stored key bytes:
        // membership rows written before host canonicalization carry
        // the raw endpoint spelling. The candidate set is bounded by
        // the preflight scope and the owner set by live Works, so the
        // filter runs in memory over the joined rows.
        let wanted: std::collections::BTreeSet<workloop_core::identity::IssueRef> =
            issues.iter().map(canonical_issue).collect();
        let owners = self.query_all(
            "SELECT wir.issue, wir.work_id, e.state \
             FROM work_issue_refs wir \
             JOIN executions e ON e.work_id = wir.work_id \
             WHERE e.state NOT IN ('succeeded','failed','cancelled') \
             ORDER BY wir.issue_key, wir.work_id",
            [],
            |row| {
                let issue: workloop_core::identity::IssueRef = de_row(&row.get::<_, String>(0)?)?;
                Ok(IntakeSourceOwner {
                    issue: canonical_issue(&issue),
                    work_id: WorkId(row.get::<_, String>(1)?),
                    execution_state: untag(&row.get::<_, String>(2)?)?,
                })
            },
        )?;
        Ok(owners
            .into_iter()
            .filter(|o| wanted.contains(&o.issue))
            .collect())
    }

    fn intake_owned_issues(&self) -> Result<Vec<workloop_core::identity::IssueRef>, StoreError> {
        // The unfiltered owner set: every issue whose immutable
        // membership belongs to a Work holding a non-terminal
        // Execution — the same live-owner rule `intake_source_owners`
        // applies, minus the candidate filter.
        // Dedup is by scoped identity (`IssueRef` ordering ignores
        // `display_number`): two Works may record the same issue under
        // different display numbers, and exclusions must see each
        // identity once. The stored `issue` document's display number
        // is kept — the protocol's exclusion rule also matches a
        // non-null display number within the same repository — but a
        // zero is normalized away because the wire requires a display
        // number to be positive or absent.
        let rows: Vec<workloop_core::identity::IssueRef> = self.query_all(
            "SELECT DISTINCT wir.issue FROM work_issue_refs wir \
             JOIN executions e ON e.work_id = wir.work_id \
             WHERE e.state NOT IN ('succeeded','failed','cancelled') \
             ORDER BY wir.issue_key, wir.issue",
            [],
            |row| {
                let mut issue: workloop_core::identity::IssueRef =
                    de_row(&row.get::<_, String>(0)?)?;
                // Membership written before host canonicalization keeps
                // the raw endpoint spelling; consumers compare against
                // canonical request repositories, so the read returns
                // the canonical identity.
                issue = canonical_issue(&issue);
                if issue.display_number == Some(0) {
                    issue.display_number = None;
                }
                Ok(issue)
            },
        )?;
        Ok(rows
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    fn commands(&self) -> Result<Vec<CommandRecord>, StoreError> {
        self.query_all(
            "SELECT command_id, canonical_payload, decision, seq \
             FROM commands ORDER BY seq, command_id",
            [],
            |row| {
                let (kind, work_id, execution_id) = command_target(&row.get::<_, Vec<u8>>(1)?);
                let decision: String = row.get(2)?;
                // An undecodable decision can never prove acceptance:
                // it reads as not-accepted so a census never treats an
                // unreadable record as a recorded release. The check
                // decodes the full CommandDecision — a bare
                // `{"status":"accepted"}` envelope is not a decision.
                let parsed = serde_json::from_str::<CommandDecision>(&decision).ok();
                Ok(CommandRecord {
                    command_id: CommandId(row.get::<_, String>(0)?),
                    kind,
                    work_id,
                    execution_id,
                    accepted: parsed
                        .as_ref()
                        .is_some_and(|d| matches!(d, CommandDecision::Accepted { .. })),
                    committed_at: parsed.as_ref().map(CommandDecision::committed_at),
                    seq: row.get(3)?,
                })
            },
        )
    }
}

const INTAKE_RUN_COLS: &str = "run_id, trigger, selector_id, selector_version, \
    policy_revision, settings_revision, state, detail_code, request_payload, \
    request_digest, owner_pid, owner_started_at, owner_nonce, \
    created_unix_seconds, finished_unix_seconds, recorded_seq";

const INTAKE_CANDIDATE_COLS: &str = "run_id, ordinal, proposal_key, replay_key, \
    payload, payload_digest, status, status_reason, command_id, work_id, \
    committed_seq, recorded_seq";

fn intake_run_from_row(row: &rusqlite::Row<'_>) -> Result<IntakeRun, rusqlite::Error> {
    Ok(IntakeRun {
        run_id: row.get(0)?,
        trigger: untag(&row.get::<_, String>(1)?)?,
        selector_id: AdapterId::trusted(row.get::<_, String>(2)?),
        selector_version: row.get::<_, u32>(3)?,
        policy_revision: row.get::<_, i64>(4)? as u64,
        settings_revision: row.get(5)?,
        state: untag(&row.get::<_, String>(6)?)?,
        detail_code: row.get(7)?,
        request_payload: row.get(8)?,
        request_digest: row.get(9)?,
        owner_pid: row.get(10)?,
        owner_started_at: row.get(11)?,
        owner_nonce: row.get(12)?,
        created_unix_seconds: row.get(13)?,
        finished_unix_seconds: row.get(14)?,
        recorded_seq: row.get(15)?,
    })
}

fn intake_candidate_from_row(row: &rusqlite::Row<'_>) -> Result<IntakeCandidate, rusqlite::Error> {
    Ok(IntakeCandidate {
        run_id: row.get(0)?,
        ordinal: row.get::<_, u32>(1)?,
        proposal_key: row.get(2)?,
        replay_key: row.get(3)?,
        payload: row.get(4)?,
        payload_digest: row.get(5)?,
        status: untag(&row.get::<_, String>(6)?)?,
        status_reason: row.get(7)?,
        command_id: row.get::<_, Option<String>>(8)?.map(CommandId),
        work_id: row.get::<_, Option<String>>(9)?.map(WorkId),
        committed_seq: row.get(10)?,
        recorded_seq: row.get(11)?,
    })
}

/// Reduce a stored canonical command payload to the census fields: the
/// Command kind tag, the Work target, and the Execution target
/// (`execution_id` for resume/cancel, `expected_execution_id` for
/// retry). An undecodable payload yields a `None` kind — the caller
/// surfaces it as an unreadable record rather than dropping it.
fn command_target(payload: &[u8]) -> (Option<String>, Option<WorkId>, Option<ExecutionId>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return (None, None, None);
    };
    let Some(body) = v.get("body") else {
        return (None, None, None);
    };
    let Some(kind) = body.get("kind").and_then(|k| k.as_str()) else {
        return (None, None, None);
    };
    let work_id = body
        .get("work_id")
        .and_then(|w| w.as_str())
        .map(|s| WorkId(s.to_string()));
    let execution_id = body
        .get("execution_id")
        .or_else(|| body.get("expected_execution_id"))
        .and_then(|e| e.as_str())
        .map(|s| ExecutionId(s.to_string()));
    (Some(kind.to_string()), work_id, execution_id)
}

impl TxView for Tx<'_> {
    fn alloc_seq(&mut self) -> Result<EventSequence, StoreError> {
        self.exec(
            "INSERT INTO event_log (kind, payload) VALUES ('tick', '{}')",
            [],
        )?;
        Ok(self.tx.last_insert_rowid())
    }

    fn alloc_id(&mut self, prefix: &str) -> Result<String, StoreError> {
        self.exec(
            "INSERT INTO id_counters (prefix, next) VALUES (?1, 1) \
             ON CONFLICT(prefix) DO UPDATE SET next = next + 1",
            params![prefix],
        )?;
        let n: i64 = self
            .tx
            .query_row(
                "SELECT next FROM id_counters WHERE prefix=?1",
                params![prefix],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        Ok(format!("{prefix}-{n}"))
    }

    fn begin_attempt(&mut self) -> Result<(), StoreError> {
        self.exec("SAVEPOINT attempt", [])?;
        Ok(())
    }

    fn revert_attempt(&mut self) -> Result<(), StoreError> {
        self.exec("ROLLBACK TO SAVEPOINT attempt", [])?;
        self.exec("RELEASE SAVEPOINT attempt", [])?;
        Ok(())
    }

    fn commit_attempt(&mut self) -> Result<(), StoreError> {
        self.exec("RELEASE SAVEPOINT attempt", [])?;
        Ok(())
    }

    fn apply(&mut self, effect: &Effect) -> Result<(), StoreError> {
        // Every applied effect is an event: the log is the audit trail.
        self.exec(
            "INSERT INTO event_log (kind, payload) VALUES (?1, ?2)",
            params!["effect", json(effect)?],
        )?;
        match effect {
            Effect::InsertWork {
                work,
                spec,
                issue_refs,
                claims,
                usage,
            } => {
                self.exec(
                    "INSERT INTO works (id, version, binding, spec, \
                     approved_spec, source_gate, policy, created_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        work.id.0,
                        work.version,
                        json(&work.binding)?,
                        work.spec,
                        work.approved_spec,
                        tag(&work.source_gate),
                        json(&work.policy)?,
                        work.created_seq,
                    ],
                )?;
                insert_spec(self, spec)?;
                for r in issue_refs {
                    self.exec(
                        "INSERT INTO work_issue_refs (work_id, issue_key, issue) \
                         VALUES (?1,?2,?3)",
                        params![r.work_id.0, issue_key(&r.issue), json(&r.issue)?],
                    )?;
                }
                insert_claims(self, &work.id, claims)?;
                self.exec(
                    "INSERT INTO work_usage (work_id, fix_invocations_used, \
                     transport_retries_used, equivalent_failure_streak, \
                     equivalent_signature, automatic_seconds_used, \
                     first_dispatch_unix_seconds, last_progress_unix_seconds, \
                     human_wait_seconds_total, last_progress_basis) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        usage.work_id.0,
                        usage.fix_invocations_used,
                        usage.transport_retries_used,
                        usage.equivalent_failure_streak,
                        usage.equivalent_signature,
                        usage.automatic_seconds_used,
                        usage.first_dispatch_unix_seconds,
                        usage.last_progress_unix_seconds,
                        usage.human_wait_seconds_total,
                        usage.last_progress_basis.map(|b| b.as_str()),
                    ],
                )?;
            }
            Effect::UpdateWork { work, expected } => {
                update_work(self, work, *expected)?;
            }
            Effect::InsertSpec { spec } => {
                insert_spec(self, spec)?;
            }
            Effect::InsertExecution { execution } => {
                write_execution(self, execution)?;
            }
            Effect::UpdateExecution {
                execution,
                expected_state,
            } => {
                update_execution(self, execution, expected_state)?;
            }
            Effect::ClaimSlot { execution_id } => {
                let limit = self.slot_limit()?;
                let n = self.exec(
                    "UPDATE capacity_slots SET execution_id=?1 \
                     WHERE slot_id = (SELECT slot_id FROM capacity_slots \
                                      WHERE execution_id IS NULL AND slot_id < ?2 \
                                      LIMIT 1)",
                    params![execution_id.0, i64::from(limit)],
                )?;
                if n == 0 {
                    return Err(StoreError::NoFreeSlot);
                }
            }
            Effect::ReleaseSlot { execution_id } => {
                self.exec(
                    "UPDATE capacity_slots SET execution_id=NULL WHERE execution_id=?1",
                    params![execution_id.0],
                )?;
            }
            Effect::InsertReservation { reservation } => {
                self.exec(
                    "INSERT INTO resource_reservations (resource_key, resource, \
                     holder, expires_at_unix_seconds) VALUES (?1,?2,?3,?4)",
                    params![
                        reservation.resource_key,
                        json(&reservation.resource)?,
                        json(&reservation.holder)?,
                        reservation.expires_at_unix_seconds,
                    ],
                )?;
            }
            Effect::DeleteReservations {
                resource_keys,
                holder,
            } => {
                let holder_json = json(holder)?;
                for key in resource_keys {
                    self.exec(
                        "DELETE FROM resource_reservations \
                         WHERE resource_key=?1 AND holder=?2",
                        params![key, holder_json],
                    )?;
                }
            }
            Effect::RenewReservation {
                resource_key,
                holder,
                expires_at_unix_seconds,
            } => {
                self.exec(
                    "UPDATE resource_reservations SET expires_at_unix_seconds=?1 \
                     WHERE resource_key=?2 AND holder=?3",
                    params![expires_at_unix_seconds, resource_key, json(holder)?],
                )?;
            }
            Effect::InsertRequest { request } => {
                self.exec(
                    "INSERT INTO invocation_requests (request_id, work_id, \
                     execution_id, role, spec_revision, payload, adapter_binding) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        request.request_id,
                        request.work_id.0,
                        request.execution_id.0,
                        tag(&request.role),
                        request.spec_revision,
                        json(&request.payload)?,
                        request.adapter_binding.as_ref().map(json).transpose()?,
                    ],
                )?;
            }
            Effect::BindRequestAdapter {
                request_id,
                binding,
            } => {
                // First binding wins: a request already dispatched under
                // one adapter is never migrated to another.
                self.exec(
                    "UPDATE invocation_requests SET adapter_binding=?2 \
                     WHERE request_id=?1 AND adapter_binding IS NULL",
                    params![request_id, json(&binding)?],
                )?;
            }
            Effect::InsertInvocation { invocation } => {
                self.exec(
                    "INSERT INTO invocations (id, request_id, execution_id, role, \
                     state, process_identity, descendants, required_artifacts, \
                     evidence_refs) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        invocation.id.0,
                        invocation.request_id,
                        invocation.execution_id.0,
                        tag(&invocation.role),
                        tag(&invocation.state),
                        invocation.process_identity.as_ref().map(json).transpose()?,
                        json(&invocation.descendants)?,
                        json(&invocation.required_artifacts)?,
                        json(&invocation.evidence_refs)?,
                    ],
                )?;
            }
            Effect::InsertWorkerRequestExport { export } => {
                self.exec(
                    "INSERT INTO worker_request_exports \
                     (invocation_id, request_id, profile_id, workspace_root, \
                      digest, state, process_identity) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        export.invocation_id.0,
                        export.request_id,
                        export.profile_id,
                        export.workspace_root,
                        export.digest,
                        tag(&export.state),
                        export.process_identity,
                    ],
                )?;
            }
            Effect::BindWorkerRequestExport {
                request_id,
                process_identity,
            } => {
                let n = self.exec(
                    "UPDATE worker_request_exports \
                     SET state='bound', process_identity=?2 \
                     WHERE request_id=?1 AND state='exported'",
                    params![request_id, process_identity],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "worker request export {request_id} is absent or already bound"
                        ),
                    });
                }
            }
            Effect::SetIntakeSettings {
                settings,
                expected_revision,
            } => {
                // The singleton row is compare-and-set on the
                // caller-observed revision: the first write requires an
                // explicit absent expectation (`None`), every later
                // write the exact revision it read. Anything else —
                // a stale reload snapshot, a missing row, a forged
                // expectation — conflicts instead of overwriting the
                // active limit (#213).
                let current: Option<String> = self
                    .tx
                    .query_row(
                        "SELECT settings_revision FROM intake_settings \
                         WHERE singleton=1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_err)?;
                let guard_ok = match (current.as_deref(), expected_revision.as_deref()) {
                    (None, None) => true,
                    (Some(c), Some(e)) => c == e,
                    _ => false,
                };
                if !guard_ok {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake settings revision guard failed: record is {}, \
                             caller expected {}",
                            current.as_deref().unwrap_or("<absent>"),
                            expected_revision.as_deref().unwrap_or("<absent>")
                        ),
                    });
                }
                self.exec(
                    "INSERT INTO intake_settings (singleton, settings_revision, \
                     pending_queue_limit, recorded_seq) \
                     VALUES (1, ?1, ?2, ?3) \
                     ON CONFLICT(singleton) DO UPDATE SET \
                     settings_revision=excluded.settings_revision, \
                     pending_queue_limit=excluded.pending_queue_limit, \
                     recorded_seq=excluded.recorded_seq",
                    params![
                        settings.settings_revision,
                        i64::from(settings.pending_queue_limit),
                        settings.recorded_seq,
                    ],
                )?;
            }
            Effect::UpdateInvocation {
                invocation,
                expected_state,
            } => {
                let n = self.exec(
                    "UPDATE invocations SET state=?2, process_identity=?3, \
                     descendants=?4, required_artifacts=?5, evidence_refs=?6 \
                     WHERE id=?1 AND state=?7",
                    params![
                        invocation.id.0,
                        tag(&invocation.state),
                        invocation.process_identity.as_ref().map(json).transpose()?,
                        json(&invocation.descendants)?,
                        json(&invocation.required_artifacts)?,
                        json(&invocation.evidence_refs)?,
                        tag(expected_state),
                    ],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "invocation {} no longer in state {:?}",
                            invocation.id.0, expected_state
                        ),
                    });
                }
            }
            Effect::InsertObservation { observation } => {
                let (scope, host, source, event_id) = match &observation.delivery {
                    Some(DeliveryId {
                        scope: DeliveryScope::Host { host },
                        source,
                        event_id,
                    }) => (
                        Some("host".to_string()),
                        Some(host.as_str().to_string()),
                        Some(source.clone()),
                        Some(event_id.clone()),
                    ),
                    Some(DeliveryId {
                        scope: DeliveryScope::Local,
                        source,
                        event_id,
                    }) => (
                        Some("local".to_string()),
                        // A NULL host key would dedupe nothing: SQLite
                        // treats NULLs as distinct in UNIQUE indexes, so
                        // every local redelivery would insert again.
                        Some(String::new()),
                        Some(source.clone()),
                        Some(event_id.clone()),
                    ),
                    None => (None, None, None, None),
                };
                self.exec(
                    "INSERT INTO observations (delivery_scope, delivery_host, \
                     delivery_source, delivery_event_id, subject_work_id, \
                     subject_execution_id, fact, observed_unix_seconds) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        scope,
                        host,
                        source,
                        event_id,
                        observation.subject_work_id.as_ref().map(|w| w.0.clone()),
                        observation
                            .subject_execution_id
                            .as_ref()
                            .map(|e| e.0.clone()),
                        json(&observation.fact)?,
                        observation.observed_unix_seconds,
                    ],
                )?;
            }
            Effect::InsertStopIntent { intent } => {
                self.exec(
                    "INSERT INTO stop_intents (invocation_id, identities, \
                     recorded_seq) VALUES (?1,?2,?3)",
                    params![
                        intent.invocation_id.0,
                        json(&intent.identities)?,
                        intent.recorded_seq,
                    ],
                )?;
            }
            Effect::InsertHostActionIntent { intent } => {
                self.exec(
                    "INSERT INTO host_action_intents (action_id, work_id, host, \
                     repository, role, actor, action, payload_digest, status, \
                     authenticated_actor, effects, created_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![
                        intent.action_id,
                        intent.work_id.0,
                        intent.host.as_str(),
                        json(&intent.repository)?,
                        tag(&intent.role),
                        json(&intent.actor)?,
                        json(&intent.action)?,
                        intent.payload_digest,
                        json(&intent.status)?,
                        intent.authenticated_actor.as_ref().map(json).transpose()?,
                        json(&intent.effects)?,
                        intent.created_seq,
                    ],
                )?;
            }
            Effect::UpdateHostActionIntent {
                action_id,
                status,
                authenticated_actor,
                effects,
            } => {
                self.exec(
                    "UPDATE host_action_intents SET status=?2, \
                     authenticated_actor=?3, effects=?4 WHERE action_id=?1",
                    params![
                        action_id,
                        json(status)?,
                        authenticated_actor.as_ref().map(json).transpose()?,
                        json(effects)?,
                    ],
                )?;
            }
            Effect::InsertReviewRound { round, occurrences } => {
                self.exec(
                    "INSERT INTO review_rounds (id, work_id, target, source, \
                     source_revision, completeness, set_digest, generation, seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        round.id,
                        round.work_id.0,
                        json(&round.target)?,
                        round.source,
                        round.source_revision,
                        round.completeness,
                        round.set_digest,
                        round.generation,
                        round.seq,
                    ],
                )?;
                for o in occurrences {
                    self.exec(
                        "INSERT INTO finding_occurrences (id, round_id, \
                         source_key, content_digest, severity, position, \
                         thread_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                        params![
                            o.id,
                            o.round_id,
                            o.source_key,
                            o.content_digest,
                            o.severity,
                            o.position,
                            o.thread_id,
                        ],
                    )?;
                }
            }
            Effect::InsertRoundJudgment { judgment } => {
                self.exec(
                    "INSERT INTO round_judgments (id, work_id, round_id, \
                     set_digest, decisions, actor_id, seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        judgment.id,
                        judgment.work_id.0,
                        judgment.round_id,
                        judgment.set_digest,
                        json(&judgment.decisions)?,
                        judgment.actor_id,
                        judgment.seq,
                    ],
                )?;
            }
            Effect::InsertIssueClaims { work_id, issues } => {
                insert_claims(self, work_id, issues)?;
            }
            Effect::ReleaseIssueClaims { work_id } => {
                self.exec(
                    "DELETE FROM issue_claims WHERE work_id=?1",
                    params![work_id.0],
                )?;
            }
            Effect::UpdateWorkUsage { usage } => {
                self.exec(
                    "UPDATE work_usage SET fix_invocations_used=?2, \
                     transport_retries_used=?3, equivalent_failure_streak=?4, \
                     equivalent_signature=?5, automatic_seconds_used=?6, \
                     first_dispatch_unix_seconds=?7, \
                     last_progress_unix_seconds=?8, \
                     human_wait_seconds_total=?9, last_progress_basis=?10 \
                     WHERE work_id=?1",
                    params![
                        usage.work_id.0,
                        usage.fix_invocations_used,
                        usage.transport_retries_used,
                        usage.equivalent_failure_streak,
                        usage.equivalent_signature,
                        usage.automatic_seconds_used,
                        usage.first_dispatch_unix_seconds,
                        usage.last_progress_unix_seconds,
                        usage.human_wait_seconds_total,
                        usage.last_progress_basis.map(|b| b.as_str()),
                    ],
                )?;
            }
            Effect::InsertRecoveryDecision { decision } => {
                self.exec(
                    "INSERT INTO recovery_decisions (id, work_id, execution_id, \
                     classification, signature, prerequisites, detail, \
                     usage_after, seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        decision.id,
                        decision.work_id.0,
                        decision.execution_id.as_ref().map(|e| e.0.clone()),
                        decision.classification,
                        decision.signature,
                        json(&decision.prerequisites)?,
                        decision.detail,
                        json(&decision.usage_after)?,
                        decision.seq,
                    ],
                )?;
            }
            Effect::InsertRecoveryIntent { intent } => {
                self.exec(
                    "INSERT INTO recovery_intents (intent_id, work_id, \
                     execution_id, kind, due_at_unix_seconds, generation, \
                     evidence_basis, state, retirement_reason, \
                     retirement_generation, scheduled_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params![
                        intent.intent_id,
                        intent.work_id.0,
                        intent.execution_id.as_ref().map(|e| e.0.clone()),
                        intent.kind,
                        intent.due_at_unix_seconds,
                        intent.generation as i64,
                        intent.evidence_basis,
                        tag(&intent.state),
                        intent.retirement_reason,
                        intent.retirement_generation.map(|g| g as i64),
                        intent.scheduled_seq,
                    ],
                )?;
            }
            Effect::UpdateRecoveryIntent {
                intent_id,
                state,
                retirement_reason,
                evidence_generation,
            } => {
                // Closed lifecycle with idempotent retirement: a repeat
                // of the recorded transition is a no-op, any other
                // transition is refused.
                let current: Option<(String, i64, String)> = self
                    .tx
                    .query_row(
                        "SELECT state, generation, work_id FROM recovery_intents \
                         WHERE intent_id=?1",
                        params![intent_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(map_err)?;
                let Some((current, intent_generation, work_id)) = current else {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("recovery intent {intent_id} is absent"),
                    });
                };
                let target = tag(state);
                if current == target {
                    return Ok(());
                }
                if current != tag(&RecoveryIntentState::Pending) {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("recovery intent {intent_id} is {current}, not pending"),
                    });
                }
                let retirement_generation = if matches!(state, RecoveryIntentState::Retired) {
                    let evidence = evidence_generation.ok_or_else(|| StoreError::Internal {
                        detail: format!(
                            "recovery intent {intent_id} retirement carries no evidence generation"
                        ),
                    })?;
                    // A decision made from evidence older than the
                    // intent's own scheduling generation, or older than
                    // the Work's current generation, is stale: it cannot
                    // retire an intent newer activity owns.
                    let work_generation: Option<i64> = self
                        .tx
                        .query_row(
                            "SELECT version FROM works WHERE id=?1",
                            params![work_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(map_err)?;
                    let required = work_generation
                        .map(|g| (g as u64).max(intent_generation as u64))
                        .unwrap_or(intent_generation as u64);
                    if evidence < required {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "recovery intent {intent_id} cannot be retired by evidence \
                                 generation {evidence} older than generation {required}"
                            ),
                        });
                    }
                    Some(evidence as i64)
                } else {
                    None
                };
                self.exec(
                    "UPDATE recovery_intents SET state=?2, retirement_reason=?3, \
                     retirement_generation=?4 WHERE intent_id=?1 AND state='pending'",
                    params![intent_id, target, retirement_reason, retirement_generation],
                )?;
            }
            Effect::InsertIntakeRun { run } => {
                // Runs are born `open` (invocation owned, adapter call
                // in flight) or `skipped` (recorded without a launch,
                // terminal at insert) — the only insert states the spec
                // table allows. Every other state is reached through
                // the guarded UpdateIntakeRun transition.
                if !matches!(run.state, IntakeRunState::Open | IntakeRunState::Skipped) {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake run {} cannot be inserted in state {}",
                            run.run_id,
                            tag(&run.state)
                        ),
                    });
                }
                // Field invariants match the state table: a live run
                // keeps `detail_code`/`finished_unix_seconds` NULL; a
                // terminal insert (`skipped`) records both its bounded
                // diagnostic and its finish time.
                match run.state {
                    IntakeRunState::Open
                        if run.detail_code.is_some() || run.finished_unix_seconds.is_some() =>
                    {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "live intake run {} must keep detail_code and finished_unix_seconds NULL",
                                run.run_id
                            ),
                        });
                    }
                    IntakeRunState::Skipped
                        if run.detail_code.is_none() || run.finished_unix_seconds.is_none() =>
                    {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "terminal intake run {} must record detail_code and finished_unix_seconds",
                                run.run_id
                            ),
                        });
                    }
                    _ => {}
                }
                // Single-active backstop: the intake lease is the real
                // mutex, but a second live run must conflict even if a
                // caller bypassed it — the partial unique index makes
                // the same guarantee structural. Terminal records
                // (`skipped`, `failed`, …) never occupy the active slot
                // and stay recordable while another run is live.
                if matches!(run.state, IntakeRunState::Open | IntakeRunState::Admitting) {
                    let active: i64 = self
                        .tx
                        .query_row(
                            "SELECT COUNT(*) FROM intake_runs \
                             WHERE state IN ('open', 'admitting')",
                            [],
                            |r| r.get(0),
                        )
                        .map_err(map_err)?;
                    if active > 0 {
                        return Err(StoreError::ConstraintConflict {
                            detail: "an intake run is already active".into(),
                        });
                    }
                }
                self.exec(
                    "INSERT INTO intake_runs (run_id, trigger, selector_id, \
                     selector_version, policy_revision, settings_revision, \
                     state, detail_code, request_payload, request_digest, \
                     owner_pid, owner_started_at, owner_nonce, \
                     created_unix_seconds, finished_unix_seconds, \
                     recorded_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                    params![
                        run.run_id,
                        tag(&run.trigger),
                        run.selector_id.as_str(),
                        i64::from(run.selector_version),
                        run.policy_revision as i64,
                        run.settings_revision,
                        tag(&run.state),
                        run.detail_code,
                        run.request_payload,
                        run.request_digest,
                        run.owner_pid,
                        run.owner_started_at,
                        run.owner_nonce,
                        run.created_unix_seconds,
                        run.finished_unix_seconds,
                        run.recorded_seq,
                    ],
                )?;
            }
            Effect::AdmitIntakeBatch { run_id, candidates } => {
                // Commit boundary B1 in one apply: the validated batch
                // and the run's `open` → `admitting` transition commit
                // together, so a crash can never leave candidates
                // without their run state, an `open` run holding
                // candidates, or `admitting` with a partial batch
                // (`docs/intake-identity-storage.md`).
                let run_state: Option<String> = self
                    .tx
                    .query_row(
                        "SELECT state FROM intake_runs WHERE run_id=?1",
                        params![run_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_err)?;
                let Some(state_tag) = run_state else {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("intake run {run_id} is absent"),
                    });
                };
                let state: IntakeRunState = untag(&state_tag).map_err(map_err)?;
                if state != IntakeRunState::Open {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("intake run {run_id} is {}, not open", tag(&state)),
                    });
                }
                for c in candidates {
                    // A validated batch is persisted all-`pending` with
                    // no decision links; every other status and every
                    // link is reached through the guarded
                    // UpdateIntakeCandidate transition after its submit
                    // resolves.
                    if c.run_id != run_id.as_str() {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "intake candidate ordinal {} belongs to run {}, not {run_id}",
                                c.ordinal, c.run_id
                            ),
                        });
                    }
                    if c.status != IntakeCandidateStatus::Pending
                        || c.command_id.is_some()
                        || c.work_id.is_some()
                        || c.committed_seq.is_some()
                    {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "intake candidate {}/{} must be pending with no decision links",
                                c.run_id, c.ordinal
                            ),
                        });
                    }
                    self.exec(
                        "INSERT INTO intake_candidates (run_id, ordinal, \
                         proposal_key, replay_key, payload, payload_digest, \
                         status, status_reason, command_id, work_id, \
                         committed_seq, recorded_seq) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                        params![
                            run_id,
                            i64::from(c.ordinal),
                            c.proposal_key,
                            c.replay_key,
                            c.payload,
                            c.payload_digest,
                            tag(&c.status),
                            c.status_reason,
                            c.command_id.as_ref().map(|id| id.0.clone()),
                            c.work_id.as_ref().map(|id| id.0.clone()),
                            c.committed_seq,
                            c.recorded_seq,
                        ],
                    )?;
                }
                self.exec(
                    "UPDATE intake_runs SET state='admitting' \
                     WHERE run_id=?1 AND state='open'",
                    params![run_id],
                )?;
            }
            Effect::UpdateIntakeRun {
                run_id,
                state,
                detail_code,
                finished_unix_seconds,
                expected_state,
            } => {
                // Closed lifecycle plus an expected-state guard: the
                // store enforces both so a coordinator bug cannot
                // regress a terminal run or skip the admitting phase.
                // `docs/intake-identity-storage.md` is the table.
                let current: Option<String> = self
                    .tx
                    .query_row(
                        "SELECT state FROM intake_runs WHERE run_id=?1",
                        params![run_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_err)?;
                let Some(current) = current else {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("intake run {run_id} is absent"),
                    });
                };
                let current_state: IntakeRunState = untag(&current).map_err(map_err)?;
                if current_state != *expected_state {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("intake run {run_id} is {current}, not the expected state"),
                    });
                }
                let legal = matches!(
                    (current_state, *state),
                    (IntakeRunState::Open, IntakeRunState::Admitting)
                        | (IntakeRunState::Open, IntakeRunState::Failed)
                        | (IntakeRunState::Open, IntakeRunState::Interrupted)
                        | (IntakeRunState::Admitting, IntakeRunState::Completed)
                        | (IntakeRunState::Admitting, IntakeRunState::Interrupted)
                );
                if !legal {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake run {run_id} cannot move {current} -> {}",
                            tag(state)
                        ),
                    });
                }
                // Field invariants match the state table: live runs
                // keep both columns NULL, every terminal transition
                // records a finish time, and only `failed`/`interrupted`
                // carry a failure diagnostic.
                let fields_ok = match state {
                    IntakeRunState::Open | IntakeRunState::Admitting => {
                        detail_code.is_none() && finished_unix_seconds.is_none()
                    }
                    IntakeRunState::Completed => {
                        detail_code.is_none() && finished_unix_seconds.is_some()
                    }
                    IntakeRunState::Failed | IntakeRunState::Interrupted => {
                        detail_code.is_some() && finished_unix_seconds.is_some()
                    }
                    IntakeRunState::Skipped => false,
                };
                if !fields_ok {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake run {run_id} transition to {} carries an inconsistent detail/finish field set",
                            tag(state)
                        ),
                    });
                }
                // `completed` means every candidate reached a resolved
                // terminal status; a coordinator that still holds a
                // `pending`/`unknown` candidate must close the run
                // `interrupted` instead. An `interrupted` candidate is
                // terminal but unresolved: recovery never closes such a
                // run `completed`, so the store refuses it too.
                if *state == IntakeRunState::Completed {
                    let unresolved: i64 = self
                        .tx
                        .query_row(
                            "SELECT COUNT(*) FROM intake_candidates WHERE run_id=?1 \
                             AND status NOT IN ('accepted', 'replayed', 'rejected', \
                             'capacity_deferred')",
                            params![run_id],
                            |r| r.get(0),
                        )
                        .map_err(map_err)?;
                    if unresolved > 0 {
                        return Err(StoreError::ConstraintConflict {
                            detail: format!(
                                "intake run {run_id} cannot complete with {unresolved} unresolved or interrupted candidates"
                            ),
                        });
                    }
                }
                self.exec(
                    "UPDATE intake_runs SET state=?2, detail_code=?3, \
                     finished_unix_seconds=?4 \
                     WHERE run_id=?1 AND state=?5",
                    params![
                        run_id,
                        tag(state),
                        detail_code,
                        finished_unix_seconds,
                        tag(expected_state),
                    ],
                )?;
            }
            Effect::UpdateIntakeCandidate {
                candidate,
                expected_status,
            } => {
                // Identity fields are immutable: the UPDATE names only
                // the status and decision-link columns. The closed
                // transition table plus the expected-status guard mean
                // a stale or terminal writer conflicts instead of
                // regressing the row
                // (`docs/intake-identity-storage.md`).
                let current: Option<(String, String)> = self
                    .tx
                    .query_row(
                        "SELECT status, replay_key FROM intake_candidates \
                         WHERE run_id=?1 AND ordinal=?2",
                        params![candidate.run_id, i64::from(candidate.ordinal)],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(map_err)?;
                let Some((current, replay_key)) = current else {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake candidate {}/{} is absent",
                            candidate.run_id, candidate.ordinal
                        ),
                    });
                };
                let current_status: IntakeCandidateStatus = untag(&current).map_err(map_err)?;
                if current_status != *expected_status {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake candidate {}/{} is {current}, not the expected status",
                            candidate.run_id, candidate.ordinal
                        ),
                    });
                }
                use IntakeCandidateStatus as S;
                let legal = matches!(
                    (current_status, candidate.status),
                    (S::Pending, S::Accepted)
                        | (S::Pending, S::Replayed)
                        | (S::Pending, S::Rejected)
                        | (S::Pending, S::CapacityDeferred)
                        | (S::Pending, S::Unknown)
                        | (S::Pending, S::Interrupted)
                        | (S::Unknown, S::Accepted)
                        | (S::Unknown, S::Replayed)
                        | (S::Unknown, S::Rejected)
                        | (S::Unknown, S::Interrupted)
                );
                if !legal {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake candidate {}/{} cannot move {current} -> {}",
                            candidate.run_id,
                            candidate.ordinal,
                            tag(&candidate.status)
                        ),
                    });
                }
                // The spec's Links column is enforced, not just
                // documented: unresolved statuses keep every decision
                // link NULL (a deferred candidate must never look
                // decided), a rejection never creates Work, and the
                // decided statuses carry the command identity their
                // outcome came from. The linked ledger row itself must
                // agree: a status can never contradict the saved
                // decision it points at, and `committed_seq` is that
                // decision's `committed_at`, not an arbitrary value.
                // A replay must mirror the saved decision: when that
                // decision accepted and created a Work, the candidate
                // links the original Work; when it rejected, the
                // candidate links no Work. The linked ledger row is
                // the decision stored under *this candidate's* replay
                // key, not merely some command with the same shape — an
                // accepted decision from another proposal's key cannot
                // back this candidate.
                let saved_decision = match candidate.command_id.as_ref() {
                    Some(id) => self.command_decision_for_replay(id, &replay_key)?,
                    None => None,
                };
                let links_clear = candidate.command_id.is_none()
                    && candidate.work_id.is_none()
                    && candidate.committed_seq.is_none();
                let links_ok = match candidate.status {
                    IntakeCandidateStatus::Pending
                    | IntakeCandidateStatus::CapacityDeferred
                    | IntakeCandidateStatus::Unknown
                    | IntakeCandidateStatus::Interrupted => links_clear,
                    IntakeCandidateStatus::Rejected => match &saved_decision {
                        Some(CommandDecision::Rejected {
                            reason,
                            committed_at,
                            ..
                        }) => {
                            candidate.command_id.is_some()
                                && candidate.work_id.is_none()
                                && candidate.committed_seq == Some(*committed_at)
                                && candidate.status_reason.as_deref() == Some(tag(reason).as_str())
                        }
                        _ => false,
                    },
                    IntakeCandidateStatus::Accepted => match &saved_decision {
                        Some(CommandDecision::Accepted {
                            work_id,
                            committed_at,
                            ..
                        }) => {
                            candidate.command_id.is_some()
                                && work_id.is_some()
                                && candidate.work_id == *work_id
                                && candidate.committed_seq == Some(*committed_at)
                                && candidate.status_reason.is_none()
                        }
                        _ => false,
                    },
                    IntakeCandidateStatus::Replayed => match &saved_decision {
                        Some(CommandDecision::Accepted {
                            work_id,
                            committed_at,
                            ..
                        }) => {
                            candidate.command_id.is_some()
                                && work_id.is_some()
                                && candidate.work_id == *work_id
                                && candidate.committed_seq == Some(*committed_at)
                                && candidate.status_reason.is_none()
                        }
                        Some(CommandDecision::Rejected {
                            reason,
                            committed_at,
                            ..
                        }) => {
                            candidate.command_id.is_some()
                                && candidate.work_id.is_none()
                                && candidate.committed_seq == Some(*committed_at)
                                && candidate.status_reason.as_deref() == Some(tag(reason).as_str())
                        }
                        // An absent ledger row is refused here as well as
                        // by the command_id foreign key.
                        None => false,
                    },
                };
                if !links_ok {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "intake candidate {}/{} status {} carries an inconsistent decision-link set",
                            candidate.run_id,
                            candidate.ordinal,
                            tag(&candidate.status)
                        ),
                    });
                }
                // `replayed` reports the saved decision's outcome, so
                // the Work link is whatever that decision recorded:
                // an accepted decision that created Work must surface
                // the same `work_id` here — the link can never be
                // silently dropped — and a rejected decision never
                // carries one. The command ledger is the authority.
                if candidate.status == IntakeCandidateStatus::Replayed {
                    let command_id = candidate
                        .command_id
                        .as_ref()
                        .expect("links_ok guarantees a replayed command_id");
                    match self.command_decision(command_id)? {
                        Some(CommandDecision::Accepted { work_id, .. }) => {
                            if candidate.work_id != work_id {
                                return Err(StoreError::ConstraintConflict {
                                    detail: format!(
                                        "intake candidate {}/{} replayed link does not \
                                         match the saved decision's work",
                                        candidate.run_id, candidate.ordinal
                                    ),
                                });
                            }
                        }
                        Some(CommandDecision::Rejected { .. }) => {
                            if candidate.work_id.is_some() {
                                return Err(StoreError::ConstraintConflict {
                                    detail: format!(
                                        "intake candidate {}/{} replayed a rejection \
                                         but carries a work link",
                                        candidate.run_id, candidate.ordinal
                                    ),
                                });
                            }
                        }
                        None => {
                            return Err(StoreError::ConstraintConflict {
                                detail: format!(
                                    "intake candidate {}/{} replays an absent command \
                                     decision",
                                    candidate.run_id, candidate.ordinal
                                ),
                            });
                        }
                    }
                }
                self.exec(
                    "UPDATE intake_candidates SET status=?3, \
                     status_reason=?4, command_id=?5, work_id=?6, \
                     committed_seq=?7 \
                     WHERE run_id=?1 AND ordinal=?2 AND status=?8",
                    params![
                        candidate.run_id,
                        i64::from(candidate.ordinal),
                        tag(&candidate.status),
                        candidate.status_reason,
                        candidate.command_id.as_ref().map(|id| id.0.clone()),
                        candidate.work_id.as_ref().map(|id| id.0.clone()),
                        candidate.committed_seq,
                        tag(expected_status),
                    ],
                )?;
            }
            Effect::RecordDeferral {
                execution_id,
                reason,
                diagnostic,
            } => {
                self.exec(
                    "INSERT INTO deferrals (execution_id, reason, diagnostic, \
                     since_seq) VALUES (?1,?2,?3,?4) \
                     ON CONFLICT(execution_id) DO UPDATE SET \
                     reason=?2, diagnostic=?3",
                    params![execution_id.0, reason, diagnostic, self.last_seq()?],
                )?;
            }
            Effect::ClearDeferral { execution_id } => {
                self.exec(
                    "DELETE FROM deferrals WHERE execution_id=?1",
                    params![execution_id.0],
                )?;
            }
            Effect::InsertAttentionAck {
                work_id,
                attention_id,
                actor_id,
            } => {
                // Acknowledgement is a no-op upsert: a repeat of the same
                // (work, attention, actor) key is idempotent, never a
                // constraint conflict.
                self.exec(
                    "INSERT OR IGNORE INTO attention_acks (work_id, attention_id, \
                     actor_id, seq) VALUES (?1,?2,?3,?4)",
                    params![work_id.0, attention_id, actor_id, self.last_seq()?],
                )?;
            }
            Effect::MarkRequestConsumed { request_id } => {
                self.exec(
                    "UPDATE invocation_requests SET consumed=1 WHERE request_id=?1",
                    params![request_id],
                )?;
            }
            Effect::UpsertHandoff {
                handoff,
                expected_seq,
            } => {
                // Compare-and-set on recorded_seq: a concurrent
                // supervisor's write first lands here, and the loser's
                // write becomes a conflict instead of regressing the
                // row it read before doing its git work.
                let n = self.exec(
                    "INSERT INTO artifact_handoffs (artifact_id, invocation_id, \
                     request_id, work_id, execution_id, base, produced, \
                     manifest_digest, state, invalid_reason, attempt, \
                     recorded_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
                     ON CONFLICT(artifact_id) DO UPDATE SET base=?6, produced=?7, \
                     manifest_digest=?8, state=?9, invalid_reason=?10, \
                     attempt=?11, recorded_seq=?12 \
                     WHERE artifact_handoffs.recorded_seq=?13",
                    params![
                        handoff.artifact_id,
                        handoff.invocation_id.0,
                        handoff.request_id,
                        handoff.work_id.0,
                        handoff.execution_id.0,
                        handoff.base,
                        handoff.produced,
                        handoff.manifest_digest,
                        tag(&handoff.state),
                        handoff.invalid_reason,
                        handoff.attempt,
                        handoff.recorded_seq,
                        expected_seq,
                    ],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!(
                            "artifact_handoffs {} advanced concurrently",
                            handoff.artifact_id
                        ),
                    });
                }
            }
            Effect::InsertVerificationResult { result } => {
                // One row per (artifact, requirement): a retried
                // inconclusive run replaces its previous record so the
                // latest result is the evidence of record.
                self.exec(
                    "INSERT INTO verification_results (id, artifact_id, \
                     requirement, argv, produced, environment_digest, result, \
                     output, recorded_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
                     ON CONFLICT (artifact_id, requirement) DO UPDATE SET \
                     id=excluded.id, argv=excluded.argv, produced=excluded.produced, \
                     environment_digest=excluded.environment_digest, \
                     result=excluded.result, output=excluded.output, \
                     recorded_seq=excluded.recorded_seq",
                    params![
                        result.id,
                        result.artifact_id,
                        result.requirement,
                        json(&result.argv)?,
                        result.produced,
                        result.environment_digest,
                        tag(&result.result),
                        result.output,
                        result.recorded_seq,
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn save_decision(
        &mut self,
        origin: &Origin,
        actor: &Actor,
        idempotency_key: &str,
        payload_digest: &str,
        canonical_payload: &[u8],
        decision: &CommandDecision,
    ) -> Result<(), StoreError> {
        let command_id = match decision {
            CommandDecision::Accepted { command_id, .. } => command_id.0.clone(),
            CommandDecision::Rejected { command_id, .. } => command_id.0.clone(),
        };
        self.exec(
            "INSERT INTO commands (origin, actor_kind, actor_id, \
             idempotency_key, payload_digest, canonical_payload, command_id, \
             decision, seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                origin.0,
                tag(&actor.kind),
                actor.external_id,
                idempotency_key,
                payload_digest,
                canonical_payload,
                command_id,
                json(decision)?,
                self.last_seq()?,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_tables_cover_the_declared_schema() {
        let declared: std::collections::BTreeSet<String> = SCHEMA
            .lines()
            .filter_map(|l| l.strip_prefix("CREATE TABLE IF NOT EXISTS "))
            .map(|l| l.split('(').next().unwrap().trim().to_string())
            .collect();
        let expected: std::collections::BTreeSet<&str> = EXPECTED_TABLES.iter().copied().collect();
        assert_eq!(
            declared,
            expected.into_iter().map(String::from).collect(),
            "EXPECTED_TABLES drifted from schema.sql"
        );
    }
}
