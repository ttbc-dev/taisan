//! Bounded retention sweep: the runtime tooling that reclaims managed
//! bodies listed in `body_catalog`. Core never performs filesystem I/O —
//! this pass executes deletions only, under the approved bounds
//! (cadence, `max_bodies`, `max_bytes`, `delete_error_limit`) and only
//! for rows the store already marks unattributed-free, unpinned and
//! age-eligible. Durable metadata rows are never deleted; a deleted body
//! leaves its catalog row behind with the reclaim reason, so evidence
//! references resolve `unavailable` rather than vanishing.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

use workloop_core::store::StoreError;

use crate::sqlite::{SqliteStore, internal, map_err};

/// Bounds and exclusions for one sweep pass. `0` on any bound axis is
/// invalid configuration, never "no limit" and never "reclaim
/// everything".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepParams {
    /// Minimum seconds between completed passes.
    pub interval_seconds: i64,
    /// Maximum bodies reclaimed in one pass.
    pub max_bodies: usize,
    /// Maximum bytes reclaimed in one pass.
    pub max_bytes: u64,
    /// Consecutive failed delete attempts on one body before it is no
    /// longer attempted by the automatic sweep.
    pub delete_error_limit: usize,
    /// The installation's own files — the live store, its SQLite
    /// sidecars and the config. Containment under the state root does
    /// not authorize deleting them: a catalog row pointing at one of
    /// these is held, never reclaimed.
    pub protected: Vec<PathBuf>,
}

impl Default for SweepParams {
    fn default() -> Self {
        Self {
            interval_seconds: 3600,
            max_bodies: 256,
            max_bytes: 1 << 30,
            delete_error_limit: 3,
            protected: Vec::new(),
        }
    }
}

/// One body the pass reclaimed (or found already gone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReclaimedBody {
    pub body_id: String,
    pub bytes: u64,
    /// `age_expired` (deleted this pass) or `missing` (already absent).
    pub reason: String,
}

/// One body the pass declined, with the recorded reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeldBody {
    pub body_id: String,
    pub reason: String,
}

/// The outcome of one sweep pass. `ran == false` with `defer_reason`
/// set means the cadence gate held the pass; nothing was touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub ran: bool,
    pub defer_reason: Option<String>,
    pub reclaimed: Vec<ReclaimedBody>,
    pub failed: Vec<HeldBody>,
    pub held: Vec<HeldBody>,
    pub bytes_reclaimed: u64,
    /// Eligible bodies left untouched because a pass bound stopped the
    /// sweep; the next pass continues with them.
    pub eligible_remaining: usize,
}

impl SweepReport {
    fn cadence_deferred() -> Self {
        Self {
            ran: false,
            defer_reason: Some("cadence".into()),
            ..Self::default()
        }
    }
}

struct Candidate {
    body_id: String,
    path: String,
    bytes: u64,
    state: String,
}

/// One managed body as its producer registers it: durable metadata in
/// `body_catalog`, the body itself at `path`. `eligible_after_unix_seconds`
/// is `NULL` while the body is unattributed — never sweep-eligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogBody {
    pub body_id: String,
    pub category: String,
    pub path: String,
    pub bytes: u64,
    pub registered_unix_seconds: i64,
    pub eligible_after_unix_seconds: Option<i64>,
    pub pinned: bool,
}

/// Protective-dependency state recorded in the body catalogue. Any
/// value other than [`PinState::None`] is a pin: the body is retained
/// regardless of age or byte pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum PinState {
    #[default]
    None,
    Active,
    Paused,
    Recovering,
    UnknownEffect,
}

impl PinState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Recovering => "recovering",
            Self::UnknownEffect => "unknown-effect",
        }
    }

    pub fn is_pin(self) -> bool {
        !matches!(self, Self::None)
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "none" => Ok(Self::None),
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "recovering" => Ok(Self::Recovering),
            "unknown-effect" => Ok(Self::UnknownEffect),
            other => Err(StoreError::Internal {
                detail: format!("unknown catalogue pin_state {other:?}"),
            }),
        }
    }
}

impl std::fmt::Display for PinState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The two owner-approved limit axes for one retention category. `None`
/// means `unlimited` on that axis: the other axis still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CategoryLimits {
    pub max_age: Option<i64>,
    pub max_bytes: Option<u64>,
}

impl CategoryLimits {
    pub const fn new(max_age: Option<i64>, max_bytes: Option<u64>) -> Self {
        Self { max_age, max_bytes }
    }

    pub const fn unlimited() -> Self {
        Self {
            max_age: None,
            max_bytes: None,
        }
    }

    /// The documented default for one of the closed retention
    /// categories. An omitted configuration key never means "no
    /// limit": only `archive` is documented as unlimited. `None` means
    /// the name is outside the closed category set.
    pub fn documented_default(category: &str) -> Option<Self> {
        let limits = match category {
            "workspace" => Self::new(Some(2_592_000), Some(21_474_836_480)),
            "worker_output" => Self::new(Some(2_592_000), Some(10_737_418_240)),
            "cas" => Self::new(Some(7_776_000), Some(42_949_672_960)),
            "handoff_objects" => Self::new(Some(7_776_000), Some(10_737_418_240)),
            "archive" => Self::unlimited(),
            _ => return None,
        };
        Some(limits)
    }
}

/// Per-category limits keyed by the catalogue's `category`. A category
/// absent from the map resolves to its documented default; a category
/// outside the closed set is a `StoreError` rather than silent
/// `unlimited` retention.
pub type CategorySettings = BTreeMap<String, CategoryLimits>;

/// Resolve the authoritative limits for a catalogue category: an
/// explicit setting wins, an omitted documented category applies its
/// default, and an unknown name fails closed.
fn effective_limits(
    settings: &CategorySettings,
    category: &str,
) -> Result<CategoryLimits, StoreError> {
    if let Some(limits) = settings.get(category) {
        return Ok(*limits);
    }
    CategoryLimits::documented_default(category).ok_or_else(|| StoreError::ConstraintConflict {
        detail: format!(
            "unknown retention category {category:?}; supply its limits or register under a documented category"
        ),
    })
}

/// One body as its producer registers it. `created_at` is the age
/// baseline; `path` is the body itself, while the catalogue row is the
/// durable metadata that survives reclamation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedBody {
    pub body_id: String,
    pub category: String,
    pub size_bytes: u64,
    pub created_at: i64,
    pub pin_state: PinState,
    pub path: String,
}

/// The five catalogue fields as persisted, independent of body
/// availability. Reclamation changes the row's availability state, not
/// this identity/provenance record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueRecord {
    pub body_id: String,
    pub category: String,
    pub size_bytes: u64,
    pub created_at: i64,
    pub pin_state: PinState,
}

/// One present body's eligibility under the authoritative catalogue
/// state and the supplied category settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyEligibility {
    pub body_id: String,
    pub category: String,
    pub size_bytes: u64,
    pub created_at: i64,
    pub pin_state: PinState,
    pub age_expired: bool,
    pub category_over_max_bytes: bool,
    pub eligible: bool,
    /// `age_expired`, `size_pressure`, `pinned`, or `within_limits`.
    pub reason: String,
}

/// One category's accounting. `held` names every protected body in a
/// category that is over its byte cap, so over-budget protected data is
/// explicitly reported rather than silently retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryEligibility {
    pub category: String,
    pub max_age: Option<i64>,
    pub max_bytes: Option<u64>,
    pub present_bytes: u64,
    pub protected_bytes: u64,
    pub over_max_bytes: bool,
    pub protected_over_limit: bool,
    pub held: Vec<HeldBody>,
}

/// Read-only eligibility computation over `body_catalog`. It mutates
/// nothing and is the store-authoritative input to any later pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EligibilityReport {
    pub entries: Vec<BodyEligibility>,
    pub categories: Vec<CategoryEligibility>,
}

impl EligibilityReport {
    pub fn entry(&self, body_id: &str) -> Option<&BodyEligibility> {
        self.entries.iter().find(|entry| entry.body_id == body_id)
    }

    pub fn is_eligible(&self, body_id: &str) -> bool {
        self.entry(body_id).is_some_and(|entry| entry.eligible)
    }

    pub fn category(&self, category: &str) -> Option<&CategoryEligibility> {
        self.categories
            .iter()
            .find(|reported| reported.category == category)
    }

    pub fn protected_over_limit(&self, category: &str) -> bool {
        self.category(category)
            .is_some_and(|reported| reported.protected_over_limit)
    }
}

/// The result of reclaiming one catalogue body. `reclaimed == false`
/// means the body was retained, with `reason` naming why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueReclaim {
    pub body_id: String,
    pub reclaimed: bool,
    pub reason: String,
}

/// Outcome of the marking transaction: either the row moved to
/// `reclaiming` under a recomputed eligibility decision, or it is held
/// (pinned / within limits).
enum MarkOutcome {
    Marked { reason: &'static str },
    Pinned,
    WithinLimits,
}

/// Marker dependency used by [`SqliteStore::set_pin_state`] so the
/// catalogue's explicit pin state participates in the same ledger as
/// lifecycle-created protective dependencies.
const PIN_STATE_DEPENDENCY: &str = "__pin_state__";

impl SqliteStore {
    /// Register a managed body (producer-side tooling). Durable
    /// metadata only — registration never touches the filesystem.
    /// A registration-time pin is recorded as a `body_pins` row under
    /// the unnamed `registered` dependency; lifecycle sites that know
    /// their obligation should pin with their own dependency id via
    /// [`Self::set_body_pin`] instead.
    pub fn register_body(&self, body: &CatalogBody) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "INSERT INTO body_catalog (body_id, category, path, bytes, \
             registered_unix_seconds, eligible_after_unix_seconds, pinned, \
             pin_state, state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'present')",
            params![
                body.body_id,
                body.category,
                body.path,
                body.bytes as i64,
                body.registered_unix_seconds,
                body.eligible_after_unix_seconds,
                i64::from(body.pinned),
                if body.pinned { "active" } else { "none" }
            ],
        )
        .map_err(map_err)?;
        if body.pinned {
            tx.execute(
                "INSERT OR IGNORE INTO body_pins (body_id, dependency) \
                 VALUES (?1, 'registered')",
                params![body.body_id],
            )
            .map_err(map_err)?;
        }
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Extend a registered body's eligibility horizon when `until` is
    /// later than the recorded one. Re-observation of still-needed
    /// material refreshes the horizon; it never shortens one, never
    /// applies to a body that already left `present` (reclaimed is
    /// terminal), and never turns an unlimited (`NULL`) horizon into a
    /// bounded one.
    pub fn extend_body_eligibility(
        &self,
        body_id: &str,
        until_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "UPDATE body_catalog SET eligible_after_unix_seconds=?2 \
             WHERE body_id=?1 AND state='present' \
             AND eligible_after_unix_seconds IS NOT NULL \
             AND eligible_after_unix_seconds < ?2",
            params![body_id, until_unix_seconds],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Set or clear ONE protective dependency pin on a registered body.
    /// Pin bookkeeping is the `body_pins` ledger: each live dependency
    /// is its own row, `pinned` is the materialized EXISTS of those
    /// rows, and discharging one dependency deletes only its own row —
    /// another dependency's protection survives. Pin *creation* is
    /// refused once the body leaves `present` (`reclaiming`,
    /// `reclaimed`): the requester sees the body as unavailable and
    /// follows ordinary missing-evidence rules. That refusal closes the
    /// race between eligibility marking and deletion — a pin can never
    /// land on a body the sweep has already claimed, so no later pass
    /// can silently override it. Clearing a dependency that does not
    /// exist is an idempotent no-op; clearing stays allowed in any
    /// state: it is bookkeeping that can only ever make a body *less*
    /// protected.
    pub fn set_body_pin(
        &self,
        body_id: &str,
        dependency: &str,
        pinned: bool,
    ) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // Distinguish "never registered" from "registered but no
        // longer present" before touching the ledger, so the caller
        // follows the right rule.
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        let Some(state) = state else {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is not registered"),
            });
        };
        if pinned && state != "present" {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is unavailable ({state}); pin creation is refused"),
            });
        }
        if pinned {
            tx.execute(
                "INSERT OR IGNORE INTO body_pins (body_id, dependency) VALUES (?1, ?2)",
                params![body_id, dependency],
            )
            .map_err(map_err)?;
        } else {
            tx.execute(
                "DELETE FROM body_pins WHERE body_id=?1 AND dependency=?2",
                params![body_id, dependency],
            )
            .map_err(map_err)?;
        }
        tx.execute(
            "UPDATE body_catalog SET pinned = EXISTS \
             (SELECT 1 FROM body_pins WHERE body_id=?1) WHERE body_id=?1",
            params![body_id],
        )
        .map_err(map_err)?;
        tx.execute(
            "UPDATE body_catalog SET pin_state = CASE \
             WHEN pinned=1 AND pin_state='none' THEN 'active' \
             WHEN pinned=0 AND pin_state='active' THEN 'none' \
             ELSE pin_state END WHERE body_id=?1",
            params![body_id],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Register one retained body with the catalogue's explicit five
    /// fields. Registration is durable metadata only; it never touches
    /// the filesystem. A non-`none` pin state also inserts the marker
    /// dependency into the same protective ledger lifecycle sites use.
    pub fn register_retained_body(&self, body: &RetainedBody) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "INSERT INTO body_catalog (body_id, category, path, bytes, \
             registered_unix_seconds, eligible_after_unix_seconds, pinned, \
             pin_state, state) VALUES (?1,?2,?3,?4,?5,NULL,?6,?7,'present')",
            params![
                body.body_id,
                body.category,
                body.path,
                body.size_bytes as i64,
                body.created_at,
                i64::from(body.pin_state.is_pin()),
                body.pin_state.as_str()
            ],
        )
        .map_err(map_err)?;
        if body.pin_state.is_pin() {
            tx.execute(
                "INSERT OR IGNORE INTO body_pins (body_id, dependency) \
                 VALUES (?1, ?2)",
                params![body.body_id, PIN_STATE_DEPENDENCY],
            )
            .map_err(map_err)?;
        }
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Read the five persisted catalogue fields for one body, including
    /// after reclamation. `None` means the body was never registered.
    pub fn catalogue_record(&self, body_id: &str) -> Result<Option<CatalogueRecord>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let row: Option<(String, String, i64, i64, String)> = guard
            .query_row(
                "SELECT body_id, category, bytes, registered_unix_seconds, \
                 pin_state FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(map_err)?;
        row.map(|(body_id, category, bytes, created_at, pin_state)| {
            Ok(CatalogueRecord {
                body_id,
                category,
                size_bytes: bytes.max(0) as u64,
                created_at,
                pin_state: PinState::parse(&pin_state)?,
            })
        })
        .transpose()
    }

    /// The full catalogue row the dispatch path resolves bodies
    /// through: identity, category, stored path, byte size, lifecycle
    /// state, and pin flag in one read. `None` means never registered.
    pub fn catalogue_entry(
        &self,
        body_id: &str,
    ) -> Result<Option<workloop_core::store::BodyCatalogEntry>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let row: Option<(String, String, String, i64, String, i64)> = guard
            .query_row(
                "SELECT body_id, category, path, bytes, state, pinned \
                 FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(map_err)?;
        Ok(row.map(|(body_id, category, path, bytes, state, pinned)| {
            workloop_core::store::BodyCatalogEntry {
                body_id,
                category,
                path,
                bytes: bytes.max(0) as u64,
                state,
                pinned: pinned != 0,
            }
        }))
    }

    /// The catalogue row for one `(category, path)` pair — the lookup
    /// retained bodies with deterministic paths (issue snapshots)
    /// resolve through. `None` means nothing registered at that
    /// location.
    pub fn catalogue_entry_at(
        &self,
        category: &str,
        path: &str,
    ) -> Result<Option<workloop_core::store::BodyCatalogEntry>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let row: Option<(String, String, String, i64, String, i64)> = guard
            .query_row(
                "SELECT body_id, category, path, bytes, state, pinned \
                 FROM body_catalog WHERE category=?1 AND path=?2",
                params![category, path],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(map_err)?;
        Ok(row.map(|(body_id, category, path, bytes, state, pinned)| {
            workloop_core::store::BodyCatalogEntry {
                body_id,
                category,
                path,
                bytes: bytes.max(0) as u64,
                state,
                pinned: pinned != 0,
            }
        }))
    }

    /// Set the catalogue's explicit protective-dependency state. Pin
    /// creation is refused once the body leaves `present`, exactly as
    /// lifecycle dependency pins are; `none` removes only the catalogue
    /// marker and leaves any other protective dependency intact.
    pub fn set_pin_state(&self, body_id: &str, pin_state: PinState) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        let Some(state) = state else {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is not registered"),
            });
        };
        if pin_state.is_pin() && state != "present" {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is unavailable ({state}); pin creation is refused"),
            });
        }
        tx.execute(
            "DELETE FROM body_pins WHERE body_id=?1 AND dependency=?2",
            params![body_id, PIN_STATE_DEPENDENCY],
        )
        .map_err(map_err)?;
        if pin_state.is_pin() {
            tx.execute(
                "INSERT OR IGNORE INTO body_pins (body_id, dependency) VALUES (?1, ?2)",
                params![body_id, PIN_STATE_DEPENDENCY],
            )
            .map_err(map_err)?;
        }
        let remaining: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM body_pins WHERE body_id=?1)",
                params![body_id],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let effective = if pin_state.is_pin() {
            pin_state
        } else if remaining {
            PinState::Active
        } else {
            PinState::None
        };
        tx.execute(
            "UPDATE body_catalog SET pinned=?2, pin_state=?3 WHERE body_id=?1",
            params![body_id, i64::from(remaining), effective.as_str()],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Compute per-body eligibility from authoritative catalogue rows
    /// and the supplied per-category settings. Present bytes (pinned or
    /// not) count toward a category's `max_bytes`; the rule is exactly
    /// `pin_state = none AND (age > max_age OR category over max_bytes)`.
    /// Protected bodies in an over-budget category are reported in that
    /// category's `held` list and `protected_over_limit` flag.
    pub fn catalogue_eligibility(
        &self,
        settings: &CategorySettings,
        now_unix_seconds: i64,
    ) -> Result<EligibilityReport, StoreError> {
        let rows: Vec<(String, String, i64, i64, String, i64)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare(
                    "SELECT body_id, category, bytes, registered_unix_seconds, \
                     pin_state, pinned FROM body_catalog WHERE state='present' \
                     ORDER BY registered_unix_seconds, body_id",
                )
                .map_err(map_err)?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?
        };

        let mut present_bytes: BTreeMap<String, u64> = BTreeMap::new();
        let mut protected_bytes: BTreeMap<String, u64> = BTreeMap::new();
        let mut observed: Vec<(String, String, u64, i64, PinState)> =
            Vec::with_capacity(rows.len());
        for (body_id, category, bytes, created_at, stored, pinned) in rows {
            let size_bytes = bytes.max(0) as u64;
            let stored = PinState::parse(&stored)?;
            let pin_state = if stored.is_pin() {
                stored
            } else if pinned != 0 {
                PinState::Active
            } else {
                PinState::None
            };
            let total = present_bytes.entry(category.clone()).or_default();
            *total = total.saturating_add(size_bytes);
            if pin_state.is_pin() {
                let total = protected_bytes.entry(category.clone()).or_default();
                *total = total.saturating_add(size_bytes);
            }
            observed.push((body_id, category, size_bytes, created_at, pin_state));
        }

        let mut entries = Vec::with_capacity(observed.len());
        for (body_id, category, size_bytes, created_at, pin_state) in observed {
            let limits = effective_limits(settings, &category)?;
            let age = now_unix_seconds.saturating_sub(created_at);
            let age_expired = limits.max_age.is_some_and(|max_age| age > max_age);
            let category_over_max_bytes = limits
                .max_bytes
                .is_some_and(|max_bytes| present_bytes[&category] > max_bytes);
            let pinned = pin_state.is_pin();
            let eligible = !pinned && (age_expired || category_over_max_bytes);
            let reason = if eligible {
                if age_expired {
                    "age_expired"
                } else {
                    "size_pressure"
                }
            } else if pinned {
                "pinned"
            } else {
                "within_limits"
            };
            entries.push(BodyEligibility {
                body_id,
                category,
                size_bytes,
                created_at,
                pin_state,
                age_expired,
                category_over_max_bytes,
                eligible,
                reason: reason.into(),
            });
        }

        let mut categories = Vec::with_capacity(present_bytes.len());
        for (category, total_bytes) in &present_bytes {
            let limits = effective_limits(settings, category)?;
            let protected = protected_bytes.get(category).copied().unwrap_or(0);
            let over_max_bytes = limits.max_bytes.is_some_and(|max| *total_bytes > max);
            let mut held = Vec::new();
            if over_max_bytes {
                for entry in &entries {
                    if entry.category == *category && entry.pin_state.is_pin() {
                        held.push(HeldBody {
                            body_id: entry.body_id.clone(),
                            reason: "protected_over_limit".into(),
                        });
                    }
                }
            }
            categories.push(CategoryEligibility {
                category: category.clone(),
                max_age: limits.max_age,
                max_bytes: limits.max_bytes,
                present_bytes: *total_bytes,
                protected_bytes: protected,
                over_max_bytes,
                protected_over_limit: limits.max_bytes.is_some_and(|max| protected > max),
                held,
            });
        }

        Ok(EligibilityReport {
            entries,
            categories,
        })
    }

    /// Stat a category's present bodies under `root` and return the
    /// verified byte total — bodies whose paths cannot be stat'ed are
    /// excluded. The stat snapshot is taken before any transaction;
    /// concurrent reclamation only shrinks the verified total
    /// (conservative direction for a deletion decision).
    fn verified_present_bytes(&self, category: &str, root: &Path) -> Result<u64, StoreError> {
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        let rows: Vec<(String, i64)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare(
                    "SELECT path, bytes FROM body_catalog \
                     WHERE state='present' AND category=?1",
                )
                .map_err(map_err)?;
            stmt.query_map(params![category], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?
        };
        let mut total: u64 = 0;
        for (path, bytes) in rows {
            let stat_ok = match managed_path(&root_abs, &path) {
                Ok(managed) => std::fs::symlink_metadata(&managed).is_ok(),
                Err(_) => false,
            };
            if stat_ok {
                total = total.saturating_add(bytes.max(0) as u64);
            }
        }
        Ok(total)
    }

    /// Reclaim one eligible body through the same two-phase protocol as
    /// the bounded sweep. Eligibility is recomputed from authoritative
    /// catalogue state inside the marking transaction, so a caller can
    /// never delete a body the policy retains. Installation files named
    /// by `protected` are held before any marking. The catalogue row and
    /// its five fields are never deleted: only the body's availability
    /// state changes.
    pub fn reclaim_catalogue_body(
        &self,
        body_id: &str,
        settings: &CategorySettings,
        root: &Path,
        protected: &[PathBuf],
        now_unix_seconds: i64,
    ) -> Result<CatalogueReclaim, StoreError> {
        let found: Option<(String, String)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT state, path FROM body_catalog WHERE body_id=?1",
                    params![body_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(map_err)?
        };
        let Some((state, path)) = found else {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is not registered"),
            });
        };
        if state != "present" {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is unavailable ({state})"),
            });
        }
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        let managed = match managed_path(&root_abs, &path) {
            Ok(managed) => managed,
            Err(reason) => {
                return Ok(CatalogueReclaim {
                    body_id: body_id.into(),
                    reclaimed: false,
                    reason,
                });
            }
        };
        if is_protected(&managed, protected) {
            return Ok(CatalogueReclaim {
                body_id: body_id.into(),
                reclaimed: false,
                reason: "protected_path".into(),
            });
        }
        match self.mark_reclaiming_if_eligible(body_id, settings, root, now_unix_seconds)? {
            MarkOutcome::Pinned => Ok(CatalogueReclaim {
                body_id: body_id.into(),
                reclaimed: false,
                reason: "pinned".into(),
            }),
            MarkOutcome::WithinLimits => Ok(CatalogueReclaim {
                body_id: body_id.into(),
                reclaimed: false,
                reason: "within_limits".into(),
            }),
            MarkOutcome::Marked { reason } => match delete_managed(&managed) {
                Ok(delete_reason) => {
                    // Keep `missing` when the body was already gone;
                    // otherwise record the eligibility reason that
                    // authorized the deletion.
                    let recorded = if delete_reason == "missing" {
                        delete_reason.as_str()
                    } else {
                        reason
                    };
                    self.finish_reclaim(body_id, recorded, now_unix_seconds)?;
                    Ok(CatalogueReclaim {
                        body_id: body_id.into(),
                        reclaimed: true,
                        reason: recorded.into(),
                    })
                }
                Err(detail) => {
                    let failures = self.record_delete_failure(body_id)?;
                    Ok(CatalogueReclaim {
                        body_id: body_id.into(),
                        reclaimed: false,
                        reason: format!("delete_failed({failures}): {detail}"),
                    })
                }
            },
        }
    }

    /// Read one catalog row's state (verification queries and tests).
    pub fn body_state(
        &self,
        body_id: &str,
    ) -> Result<Option<(String, Option<String>, i64)>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let row: Option<(String, Option<String>, i64)> = guard
            .query_row(
                "SELECT state, reclaim_reason, delete_failures FROM body_catalog \
                 WHERE body_id=?1",
                params![body_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(map_err)?;
        Ok(row)
    }

    /// Run one bounded retention pass. The store supplies eligibility
    /// and pin state; this pass only deletes already-eligible bodies.
    /// Reads happen in short transactions, deletions happen outside any
    /// store transaction, and each reclaim commits `reclaiming` →
    /// `reclaimed` so an interrupted pass resumes idempotently.
    pub fn retention_sweep(
        &self,
        params: &SweepParams,
        root: &Path,
        now_unix_seconds: i64,
    ) -> Result<SweepReport, StoreError> {
        if params.interval_seconds <= 0
            || params.max_bodies == 0
            || params.max_bytes == 0
            || params.delete_error_limit == 0
        {
            return Err(StoreError::Internal {
                detail: "retention sweep bounds must all be positive".into(),
            });
        }

        // Phase A: read cadence + candidates (no fs work under a lock).
        let (last_sweep_at, candidates, pinned_eligible, erroring) = {
            let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(internal)?;
            let last: Option<String> = tx
                .query_row(
                    "SELECT value FROM store_meta WHERE key='retention_last_sweep_at'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?;
            let last_sweep_at = last.and_then(|s| s.parse::<i64>().ok());
            let mut candidates = Vec::new();
            for state in ["reclaiming", "present"] {
                let mut stmt = tx
                    .prepare(
                        "SELECT body_id, path, bytes, delete_failures \
                         FROM body_catalog WHERE state=?1 AND \
                         ((?1 = 'reclaiming') OR \
                          (pinned=0 AND eligible_after_unix_seconds IS NOT NULL \
                           AND eligible_after_unix_seconds <= ?2)) \
                         ORDER BY registered_unix_seconds, body_id",
                    )
                    .map_err(map_err)?;
                let rows: Vec<(String, String, i64, i64)> = stmt
                    .query_map(params![state, now_unix_seconds], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .map_err(map_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_err)?;
                drop(stmt);
                for (body_id, path, bytes, failures) in rows {
                    if failures >= params.delete_error_limit as i64 {
                        continue; // reported once via the erroring list
                    }
                    candidates.push(Candidate {
                        body_id,
                        path,
                        bytes: bytes.max(0) as u64,
                        state: state.to_string(),
                    });
                }
            }
            let mut stmt = tx
                .prepare(
                    "SELECT body_id FROM body_catalog WHERE state='present' \
                     AND pinned=1 AND eligible_after_unix_seconds IS NOT NULL \
                     AND eligible_after_unix_seconds <= ?1 \
                     ORDER BY registered_unix_seconds, body_id",
                )
                .map_err(map_err)?;
            let pinned: Vec<String> = stmt
                .query_map(params![now_unix_seconds], |r| r.get(0))
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            drop(stmt);
            let mut stmt = tx
                .prepare(
                    "SELECT body_id FROM body_catalog WHERE state != 'reclaimed' \
                     AND delete_failures >= ?1 ORDER BY body_id",
                )
                .map_err(map_err)?;
            let erroring: Vec<String> = stmt
                .query_map(params![params.delete_error_limit as i64], |r| r.get(0))
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            drop(stmt);
            drop(tx);
            (last_sweep_at, candidates, pinned, erroring)
        };

        if let Some(last) = last_sweep_at
            && now_unix_seconds.saturating_sub(last) < params.interval_seconds
        {
            return Ok(SweepReport::cadence_deferred());
        }

        let mut report = SweepReport {
            ran: true,
            ..SweepReport::default()
        };
        for body_id in erroring {
            report.held.push(HeldBody {
                body_id,
                reason: "delete_error_limit".into(),
            });
        }
        for body_id in pinned_eligible {
            report.held.push(HeldBody {
                body_id,
                reason: "pinned".into(),
            });
        }

        // Phase B: reclaim oldest-first until a pass bound stops it.
        // The body bound counts deletion attempts, not successes: a
        // failed delete has already paid the pass's cost, so it must
        // advance the bound instead of letting a pass walk the whole
        // candidate set when every attempt fails.
        let mut attempted_bodies: usize = 0;
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        for (index, candidate) in candidates.iter().enumerate() {
            if attempted_bodies >= params.max_bodies {
                if candidate.state == "present" {
                    report.eligible_remaining += 1;
                }
                continue;
            }
            if report.bytes_reclaimed.saturating_add(candidate.bytes) > params.max_bytes {
                if candidate.bytes > params.max_bytes {
                    // A body larger than the whole pass bound can
                    // never fit any pass, so stopping here would
                    // starve every later body forever (each pass would
                    // trip on this same oldest candidate). Hold it
                    // with its reason and keep the pass moving over
                    // bodies that can fit; the operator's repair path
                    // (raise the bound, or `retention cleanup` with an
                    // override) reclaims it.
                    report.held.push(HeldBody {
                        body_id: candidate.body_id.clone(),
                        reason: "exceeds_max_bytes".into(),
                    });
                    if candidate.state == "present" {
                        report.eligible_remaining += 1;
                    }
                    continue;
                }
                // Deterministic oldest-first: the bound stops the pass;
                // later (smaller) bodies wait for the next pass rather
                // than reordering around this one. This candidate fits
                // a fresh pass, so the next pass continues with it.
                report.eligible_remaining += candidates[index..]
                    .iter()
                    .filter(|c| c.state == "present")
                    .count();
                break;
            }
            let managed = match managed_path(&root_abs, &candidate.path) {
                Ok(p) => p,
                Err(reason) => {
                    report.held.push(HeldBody {
                        body_id: candidate.body_id.clone(),
                        reason,
                    });
                    continue;
                }
            };

            // The installation's own files are never reclaimable, even
            // when a catalog row points at them: deleting the live
            // store or the config would destroy the very catalog this
            // sweep reads. Held before the `reclaiming` mark, so the
            // row keeps its durable `present` state and the pass moves
            // on to the next candidate.
            if is_protected(&managed, &params.protected) {
                report.held.push(HeldBody {
                    body_id: candidate.body_id.clone(),
                    reason: "protected_path".into(),
                });
                continue;
            }

            // Two-phase: mark `reclaiming` (re-checking the pin inside
            // the same transaction) before touching the filesystem.
            if candidate.state == "present" {
                let marked = self.mark_reclaiming(&candidate.body_id)?;
                if !marked {
                    // A false return also means the row left `present`
                    // (a concurrent pass moved it). Re-read instead of
                    // reporting a false `pinned`: a row that is now
                    // `reclaiming` resumes through the same delete path
                    // below; a reclaimed row is skipped.
                    match self.row_state(&candidate.body_id)? {
                        Some(current) if current == "present" => {
                            report.held.push(HeldBody {
                                body_id: candidate.body_id.clone(),
                                reason: "pinned".into(),
                            });
                            continue;
                        }
                        Some(current) if current == "reclaiming" => {
                            // A concurrent pass marked the row; finish
                            // it here under the same pin re-check an
                            // interrupted pass requires, instead of
                            // recording a false `pinned` hold.
                            if self.row_pinned(&candidate.body_id)? {
                                report.held.push(HeldBody {
                                    body_id: candidate.body_id.clone(),
                                    reason: "pinned".into(),
                                });
                                continue;
                            }
                        }
                        Some(_) => {
                            report.held.push(HeldBody {
                                body_id: candidate.body_id.clone(),
                                reason: "reclaimed_by_concurrent_pass".into(),
                            });
                            continue;
                        }
                        None => {
                            report.held.push(HeldBody {
                                body_id: candidate.body_id.clone(),
                                reason: "not_registered".into(),
                            });
                            continue;
                        }
                    }
                }
            } else {
                // Resumed `reclaiming` row: re-check the pin before the
                // retry, exactly as an interrupted pass requires.
                let pinned_now = self.row_pinned(&candidate.body_id)?;
                if pinned_now {
                    report.held.push(HeldBody {
                        body_id: candidate.body_id.clone(),
                        reason: "pinned".into(),
                    });
                    continue;
                }
            }

            attempted_bodies += 1;
            match delete_managed(&managed) {
                Ok(reason) => {
                    let transitioned =
                        self.finish_reclaim(&candidate.body_id, &reason, now_unix_seconds)?;
                    if transitioned == 1 {
                        report.bytes_reclaimed += candidate.bytes;
                        report.reclaimed.push(ReclaimedBody {
                            body_id: candidate.body_id.clone(),
                            bytes: candidate.bytes,
                            reason,
                        });
                    } else {
                        // A concurrent maintenance pass completed this
                        // body first; this pass never re-deletes, never
                        // re-counts bytes.
                        report.held.push(HeldBody {
                            body_id: candidate.body_id.clone(),
                            reason: "already_reclaimed".into(),
                        });
                    }
                }
                Err(detail) => {
                    let failures = self.record_delete_failure(&candidate.body_id)?;
                    report.failed.push(HeldBody {
                        body_id: candidate.body_id.clone(),
                        reason: format!("delete_failed({failures}): {detail}"),
                    });
                }
            }
        }

        // Phase C: stamp the completed pass so the cadence gate counts
        // from completion, not from pass start.
        self.stamp_last_sweep(now_unix_seconds)?;
        Ok(report)
    }

    fn stamp_last_sweep(&self, now_unix_seconds: i64) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "INSERT INTO store_meta (key, value) VALUES ('retention_last_sweep_at', ?1) \
             ON CONFLICT(key) DO UPDATE SET value=?1",
            params![now_unix_seconds.to_string()],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Two-phase marker for explicit catalogue reclamation: inside one
    /// immediate transaction it reads the authoritative row, refuses a
    /// pin, recomputes age and category-byte eligibility from present
    /// rows, and only then moves `present` to `reclaiming`. A policy
    /// that retains the body leaves it `present`.
    fn mark_reclaiming_if_eligible(
        &self,
        body_id: &str,
        settings: &CategorySettings,
        root: &Path,
        now_unix_seconds: i64,
    ) -> Result<MarkOutcome, StoreError> {
        // Capacity pressure is decided on verified bytes: a category
        // whose catalog total exceeds the cap is stat'ed before the
        // transaction, so ghost bytes can never authorize a deletion.
        // The stat snapshot is taken outside the transaction;
        // concurrent reclamation only shrinks the verified total.
        // Age eligibility is decided per body and needs no snapshot.
        let pre: Option<(String, i64)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT category, registered_unix_seconds FROM body_catalog \
                     WHERE body_id=?1",
                    params![body_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(map_err)?
        };
        let verified_over: Option<bool> = match &pre {
            Some((category, _)) => {
                let limits = effective_limits(settings, category)?;
                match limits.max_bytes {
                    Some(max_bytes) => {
                        let catalog_bytes: i64 = {
                            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                                detail: "store lock poisoned".into(),
                            })?;
                            guard
                                .query_row(
                                    "SELECT COALESCE(SUM(bytes), 0) FROM body_catalog \
                                     WHERE state='present' AND category=?1",
                                    params![category],
                                    |r| r.get(0),
                                )
                                .map_err(map_err)?
                        };
                        if catalog_bytes.max(0) as u64 > max_bytes {
                            Some(self.verified_present_bytes(category, root)? > max_bytes)
                        } else {
                            Some(false)
                        }
                    }
                    None => Some(false),
                }
            }
            None => None,
        };
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let row: Option<(String, String, i64, i64, String, i64)> = tx
            .query_row(
                "SELECT state, category, bytes, registered_unix_seconds, \
                 pin_state, pinned FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(map_err)?;
        let Some((state, category, _bytes, created_at, stored, pinned)) = row else {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is not registered"),
            });
        };
        if state != "present" {
            return Err(StoreError::ConstraintConflict {
                detail: format!("body {body_id} is unavailable ({state})"),
            });
        }
        let stored = PinState::parse(&stored)?;
        if stored.is_pin() || pinned != 0 {
            return Ok(MarkOutcome::Pinned);
        }
        let limits = effective_limits(settings, &category)?;
        let age = now_unix_seconds.saturating_sub(created_at);
        let age_expired = limits.max_age.is_some_and(|max_age| age > max_age);
        // Ghost bytes never authorize a reclaim: over-cap must hold on
        // the verified (stat'able) total as well as on the catalog.
        let over_max_bytes = verified_over.unwrap_or(false);
        if !(age_expired || over_max_bytes) {
            return Ok(MarkOutcome::WithinLimits);
        }
        let updated = tx
            .execute(
                "UPDATE body_catalog SET state='reclaiming' \
                 WHERE body_id=?1 AND state='present' AND pinned=0",
                params![body_id],
            )
            .map_err(map_err)?;
        if updated != 1 {
            // The read above established `present` and unpinned inside
            // this same immediate transaction on the single locked
            // connection, so no other writer can interleave. A miss is
            // an internal inconsistency, never a legitimate pin, and
            // must not be reported as one.
            return Err(StoreError::Internal {
                detail: format!("body {body_id} changed while being marked for reclamation"),
            });
        }
        tx.commit().map_err(internal)?;
        Ok(MarkOutcome::Marked {
            reason: if age_expired {
                "age_expired"
            } else {
                "size_pressure"
            },
        })
    }

    fn mark_reclaiming(&self, body_id: &str) -> Result<bool, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let updated = tx
            .execute(
                "UPDATE body_catalog SET state='reclaiming' \
                 WHERE body_id=?1 AND state='present' AND pinned=0",
                params![body_id],
            )
            .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(updated == 1)
    }

    fn row_pinned(&self, body_id: &str) -> Result<bool, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let pinned: i64 = guard
            .query_row(
                "SELECT pinned FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .unwrap_or(1); // a vanished row is never deleted again
        Ok(pinned != 0)
    }

    fn row_state(&self, body_id: &str) -> Result<Option<String>, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        guard
            .query_row(
                "SELECT state FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)
    }

    /// Crash-safe reclaim record: the catalogue row's `reclaiming →
    /// reclaimed` transition and the plan entry's outcome are written
    /// in one transaction, so a crash between them can never strand a
    /// pending entry for a body this run already reclaimed (the resume
    /// path would otherwise misrecord the plan's own deletion as
    /// `unavailable`).
    fn finish_reclaim_and_record(
        &self,
        plan_id: &str,
        body_id: &str,
        reason: &str,
        now_unix_seconds: i64,
    ) -> Result<usize, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let transitioned = tx
            .execute(
                "UPDATE body_catalog SET state='reclaimed', reclaim_reason=?2, \
             delete_failures=0, reclaimed_unix_seconds=?3 WHERE body_id=?1 \
             AND state='reclaiming'",
                params![body_id, reason, now_unix_seconds],
            )
            .map_err(map_err)?;
        if transitioned == 1 {
            tx.execute(
                "UPDATE reclaim_plan_entries SET outcome='reclaimed', \
                 outcome_reason=?2, outcome_unix_seconds=?3 \
                 WHERE plan_id=?1 AND body_id=?4 AND outcome IS NULL",
                params![plan_id, reason, now_unix_seconds, body_id],
            )
            .map_err(map_err)?;
        }
        tx.commit().map_err(internal)?;
        Ok(transitioned)
    }

    fn finish_reclaim(
        &self,
        body_id: &str,
        reason: &str,
        now_unix_seconds: i64,
    ) -> Result<usize, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let transitioned = tx
            .execute(
                "UPDATE body_catalog SET state='reclaimed', reclaim_reason=?2, \
             delete_failures=0, reclaimed_unix_seconds=?3 WHERE body_id=?1 \
             AND state='reclaiming'",
                params![body_id, reason, now_unix_seconds],
            )
            .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(transitioned)
    }

    fn record_delete_failure(&self, body_id: &str) -> Result<i64, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "UPDATE body_catalog SET delete_failures = delete_failures + 1 \
             WHERE body_id=?1",
            params![body_id],
        )
        .map_err(map_err)?;
        let failures: i64 = tx
            .query_row(
                "SELECT delete_failures FROM body_catalog WHERE body_id=?1",
                params![body_id],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(failures)
    }
}

/// Map a recorded catalog path onto a managed absolute path. Relative
/// paths resolve against the installation root; `..`, foreign roots and
/// the root itself are refused before any filesystem cost.
fn managed_path(root_abs: &Path, recorded: &str) -> Result<PathBuf, String> {
    let raw = Path::new(recorded);
    if recorded.is_empty() {
        return Err("empty_path".into());
    }
    if raw.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("unsafe_path".into());
    }
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root_abs.join(raw)
    };
    if candidate == root_abs {
        return Err("outside_root".into());
    }
    if !candidate.starts_with(root_abs) {
        return Err("outside_root".into());
    }
    if let Ok(real) = std::fs::canonicalize(&candidate) {
        let real_root = std::fs::canonicalize(root_abs).unwrap_or_else(|_| root_abs.to_path_buf());
        if real == real_root || !real.starts_with(&real_root) {
            return Err("outside_root".into());
        }
    }
    Ok(candidate)
}

/// Whether a managed path names one of the installation's protected
/// files. Lexical equality first, then canonical paths, so a recorded
/// spelling (or a symlinked directory) cannot reach one of them by a
/// different string. A protected file that does not exist still
/// matches lexically: it must never be committed as `reclaimed`
/// either.
///
/// When the file itself cannot be canonicalized — the `-journal`
/// sidecar is routinely absent — the last resort is the canonical
/// parent directory plus the file name. `std::path::absolute` keeps
/// `..` and never resolves symlinks, so a protected entry's spelling
/// can differ from the root-joined catalogue form while naming the
/// same location; an entry whose parent resolves to the same
/// directory identifies the same file, whether or not it exists yet.
fn is_protected(managed: &Path, protected: &[PathBuf]) -> bool {
    let canonical_location = |path: &Path| -> Option<PathBuf> {
        let name = path.file_name()?;
        let parent = path.parent()?;
        Some(std::fs::canonicalize(parent).ok()?.join(name))
    };
    // `managed` is fixed for the whole scan, so resolve its canonical
    // forms once instead of once per protected entry.
    let managed_canonical = std::fs::canonicalize(managed).ok();
    let managed_location = canonical_location(managed);
    protected.iter().any(|p| {
        if p == managed {
            return true;
        }
        if let (Ok(p_canonical), Some(managed_canonical)) =
            (std::fs::canonicalize(p), managed_canonical.as_ref())
            && p_canonical == *managed_canonical
        {
            return true;
        }
        match (canonical_location(p), managed_location.as_ref()) {
            (Some(p_location), Some(managed_location)) => p_location == *managed_location,
            _ => false,
        }
    })
}

/// Delete one managed body. Symlinks and special files are refused —
/// only regular files and directories a producer registered are
/// removable by the automatic sweep.
fn delete_managed(path: &Path) -> Result<String, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("missing".into()),
        Err(e) => return Err(e.to_string()),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Err("symlink_refused".into());
    }
    let outcome = if file_type.is_dir() {
        std::fs::remove_dir_all(path)
    } else if file_type.is_file() {
        std::fs::remove_file(path)
    } else {
        return Err("special_file_refused".into());
    };
    match outcome {
        Ok(()) => Ok("age_expired".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("missing".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Bounds for one explicitly computed reclaim plan (phase 1). `0` on
/// either axis is invalid configuration, never "no bound".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanBounds {
    pub max_bodies: usize,
    pub max_bytes: u64,
}

/// One planned body with its execution outcome. `outcome` is `None`
/// while the entry is pending; phase 2 fills it with one of
/// `reclaimed`, `unavailable`, `held` or `delete_failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReclaimPlanEntry {
    pub body_id: String,
    pub category: String,
    pub size_bytes: u64,
    pub reason: String,
    pub outcome: Option<String>,
    pub outcome_reason: Option<String>,
}

/// A persisted two-phase reclaim plan. `status` is `open` until every
/// entry carries an outcome, then `completed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReclaimPlan {
    pub plan_id: String,
    pub created_unix_seconds: i64,
    pub status: String,
    pub entries: Vec<ReclaimPlanEntry>,
    pub total_bytes: u64,
    /// Bodies that are eligible but can never fit a pass bound
    /// (larger than `max_bytes`); reported, never planned.
    pub skipped: Vec<HeldBody>,
    /// Pinned bodies that would otherwise be eligible; reported,
    /// never planned — "what pins prevented", per the retention policy.
    pub held: Vec<HeldBody>,
}

/// The result of executing (or resuming) an open plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ReclaimExecution {
    pub plan_id: String,
    pub reclaimed: Vec<ReclaimedBody>,
    /// Planned bodies whose catalogue row was no longer `present`
    /// (already reclaimed by another pass) or whose registration vanished;
    /// reported, never retried, never an error.
    pub unavailable: Vec<HeldBody>,
    /// Planned bodies the pass refused (pin re-appeared, protected path,
    /// unsafe path); retained, never deleted.
    pub held: Vec<HeldBody>,
    /// Delete attempts that failed and remain retryable (or hit the
    /// error limit); the row stays `reclaiming` for the next resume.
    pub failed: Vec<HeldBody>,
    pub bytes_reclaimed: u64,
    pub completed: bool,
    /// Entries whose outcome was already recorded when the resume began;
    /// their bodies were not touched again.
    pub already_recorded: usize,
}

/// One category's storage-pressure accounting for `retention report`.
/// Byte totals never double-count: each body's bytes land in exactly
/// one of `present`, `reclaiming` or `reclaimed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CategoryReport {
    pub category: String,
    pub max_age_seconds: Option<i64>,
    pub max_bytes: Option<u64>,
    pub present_count: u64,
    pub present_bytes: u64,
    pub pinned_count: u64,
    pub pinned_bytes: u64,
    pub eligible_count: u64,
    pub eligible_bytes: u64,
    pub reclaiming_count: u64,
    pub reclaiming_bytes: u64,
    pub reclaimed_count: u64,
    pub reclaimed_bytes: u64,
    pub reclaimed_by_reason: BTreeMap<String, u64>,
    /// Cumulative bytes reclaimed, by reason.
    pub reclaimed_bytes_by_reason: BTreeMap<String, u64>,
    /// Age of the oldest `present` body in the category, against
    /// `max_age_seconds`. `None` when the category holds nothing.
    pub oldest_present_age_seconds: Option<i64>,
    /// Bodies whose backing path could not be `stat`ed (missing,
    /// permission denied, unreadable). Reported as `unknown`, never
    /// counted as present usage or fed into the pressure computation.
    /// The failed regions are named (bounded).
    pub unknown_count: u64,
    pub unknown_bytes: u64,
    pub unknown_bodies: Vec<String>,
    pub unknown_bodies_truncated: bool,
    pub over_max_bytes: bool,
    pub protected_over_limit: bool,
    /// `none`, `over_limit` (reclaimable excess exists) or
    /// `protected_over_limit` (pinned bodies alone exceed the cap).
    pub pressure: String,
}

/// One reclaimed body, reported as explicit unavailable evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UnavailableBody {
    pub body_id: String,
    pub category: String,
    pub size_bytes: u64,
    pub reclaim_reason: Option<String>,
    pub reclaimed_unix_seconds: Option<i64>,
}

/// Read-only storage-pressure report over `body_catalog`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionReport {
    pub generated_unix_seconds: i64,
    pub categories: Vec<CategoryReport>,
    pub unavailable: Vec<UnavailableBody>,
    /// The unavailable list is bounded; this flags truncation.
    pub unavailable_truncated: bool,
    /// The filesystem scan underpinning `unknown_*` is bounded; this
    /// flags that not every present body was stat'ed.
    pub unknown_scan_truncated: bool,
    /// Managed rows pointing at the installation's protected files
    /// (the store, its sidecars, the config). Never reclaimable; held
    /// and named here — bounded.
    pub held: Vec<HeldBody>,
}

/// The selected, bounded set both phase-1 entry points compute.
struct PlannedSet {
    entries: Vec<BodyEligibility>,
    skipped: Vec<HeldBody>,
    total_bytes: u64,
    held: Vec<HeldBody>,
}

const UNAVAILABLE_REPORT_BOUND: usize = 100;
const UNKNOWN_SCAN_BOUND: usize = 1000;

fn reclaim_plan_id(now_unix_seconds: i64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("plan-{now_unix_seconds}-{nanos:x}-{}", std::process::id())
}

impl SqliteStore {
    /// Read the open plan, if any. An interrupted `cleanup --execute`
    /// leaves exactly one; both the dry-run and the resume path read it.
    pub fn reclaim_plan_open(&self) -> Result<Option<ReclaimPlan>, StoreError> {
        let plan_id: Option<String> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT plan_id FROM reclaim_plans WHERE status='open' ",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?
        };
        match plan_id {
            Some(id) => Ok(Some(self.load_reclaim_plan(&id)?)),
            None => Ok(None),
        }
    }

    fn load_reclaim_plan(&self, plan_id: &str) -> Result<ReclaimPlan, StoreError> {
        let guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let (created, status, total_bytes): (i64, String, i64) = guard
            .query_row(
                "SELECT created_unix_seconds, status, total_bytes \
                 FROM reclaim_plans WHERE plan_id=?1",
                params![plan_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(map_err)?;
        let mut stmt = guard
            .prepare(
                "SELECT body_id, category, bytes, reason, outcome, outcome_reason \
                 FROM reclaim_plan_entries WHERE plan_id=?1 \
                 ORDER BY body_id",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![plan_id], |r| {
                Ok(ReclaimPlanEntry {
                    body_id: r.get(0)?,
                    category: r.get(1)?,
                    size_bytes: r.get::<_, i64>(2)?.max(0) as u64,
                    reason: r.get(3)?,
                    outcome: r.get(4)?,
                    outcome_reason: r.get(5)?,
                })
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let (skipped, held): (Vec<HeldBody>, Vec<HeldBody>) = {
            let phase1: Option<String> = guard
                .query_row(
                    "SELECT value FROM store_meta WHERE key=?1",
                    params![format!("reclaim_plan_phase1:{plan_id}")],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?;
            match phase1
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            {
                Some(v) => (
                    serde_json::from_value(v["skipped"].clone()).unwrap_or_default(),
                    serde_json::from_value(v["held"].clone()).unwrap_or_default(),
                ),
                None => (Vec::new(), Vec::new()),
            }
        };
        Ok(ReclaimPlan {
            plan_id: plan_id.to_string(),
            created_unix_seconds: created,
            status,
            entries: rows,
            total_bytes: total_bytes.max(0) as u64,
            skipped,
            held,
        })
    }

    /// The eligible, bounded selection both phase-1 entry points
    /// compute before deciding whether to persist it.
    fn collect_plan(
        &self,
        settings: &CategorySettings,
        bounds: &PlanBounds,
        root: &Path,
        protected: &[PathBuf],
        now_unix_seconds: i64,
    ) -> Result<PlannedSet, StoreError> {
        let eligibility = self.catalogue_eligibility(settings, now_unix_seconds)?;
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        // Veto size_pressure-only verdicts driven by ghost bytes: a
        // category over its byte cap by the catalogue total but under
        // it on the verified (stat'able) total has no pressure, and
        // its young unpinned bodies must not be planned for deletion.
        let mut entries = eligibility.entries;
        for category in eligibility
            .categories
            .iter()
            .filter(|c| c.over_max_bytes)
            .map(|c| c.category.clone())
        {
            let Some(max_bytes) = effective_limits(settings, &category)?.max_bytes else {
                continue;
            };
            if self.verified_present_bytes(&category, root)? <= max_bytes {
                for entry in entries.iter_mut().filter(|e| {
                    e.category == category && !e.age_expired && e.category_over_max_bytes
                }) {
                    entry.category_over_max_bytes = false;
                    entry.eligible = false;
                    entry.reason = "within_limits".into();
                }
            }
        }
        let paths: BTreeMap<String, String> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare("SELECT body_id, path FROM body_catalog WHERE state='present'")
                .map_err(map_err)?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(map_err)?
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map_err(map_err)?
        };
        let mut planned: Vec<BodyEligibility> = Vec::new();
        let mut skipped: Vec<HeldBody> = Vec::new();
        let mut total_bytes: u64 = 0;
        // Pinned bodies that would otherwise be eligible: what pins
        // prevented. `entries` from catalogue_eligibility are ordered
        // by registered_unix_seconds, body_id — oldest first.
        let held: Vec<HeldBody> = entries
            .iter()
            .filter(|e| {
                !e.eligible && e.pin_state.is_pin() && (e.age_expired || e.category_over_max_bytes)
            })
            .map(|e| HeldBody {
                body_id: e.body_id.clone(),
                reason: "pinned".into(),
            })
            .collect();
        for entry in entries.into_iter().filter(|e| e.eligible) {
            if planned.len() >= bounds.max_bodies {
                break;
            }
            if entry.size_bytes > bounds.max_bytes {
                skipped.push(HeldBody {
                    body_id: entry.body_id.clone(),
                    reason: "exceeds_max_bytes".into(),
                });
                continue;
            }
            if total_bytes.saturating_add(entry.size_bytes) > bounds.max_bytes {
                break;
            }
            let Some(path) = paths.get(&entry.body_id) else {
                continue;
            };
            let managed = match managed_path(&root_abs, path) {
                Ok(managed) => managed,
                Err(reason) => {
                    skipped.push(HeldBody {
                        body_id: entry.body_id.clone(),
                        reason,
                    });
                    continue;
                }
            };
            if is_protected(&managed, protected) {
                skipped.push(HeldBody {
                    body_id: entry.body_id.clone(),
                    reason: "protected_path".into(),
                });
                continue;
            }
            total_bytes += entry.size_bytes;
            planned.push(entry);
        }
        Ok(PlannedSet {
            entries: planned,
            skipped,
            total_bytes,
            held,
        })
    }

    /// Dry-run phase 1: the bounded eligible set as an unsaved preview.
    /// Nothing is persisted and no body is marked or deleted; when an
    /// open plan exists it is returned instead — nothing new is
    /// computed or written in that case either.
    pub fn reclaim_plan_candidates(
        &self,
        settings: &CategorySettings,
        bounds: &PlanBounds,
        root: &Path,
        protected: &[PathBuf],
        now_unix_seconds: i64,
    ) -> Result<ReclaimPlan, StoreError> {
        if bounds.max_bodies == 0 || bounds.max_bytes == 0 {
            return Err(StoreError::Internal {
                detail: "reclaim plan bounds must be positive".into(),
            });
        }
        if let Some(open) = self.reclaim_plan_open()? {
            return Ok(open);
        }
        let PlannedSet {
            entries: planned,
            skipped,
            total_bytes,
            held,
        } = self.collect_plan(settings, bounds, root, protected, now_unix_seconds)?;
        Ok(ReclaimPlan {
            plan_id: "preview".into(),
            created_unix_seconds: now_unix_seconds,
            status: "preview".into(),
            entries: planned
                .iter()
                .map(|e| ReclaimPlanEntry {
                    body_id: e.body_id.clone(),
                    category: e.category.clone(),
                    size_bytes: e.size_bytes,
                    reason: e.reason.clone(),
                    outcome: None,
                    outcome_reason: None,
                })
                .collect(),
            total_bytes,
            skipped,
            held,
        })
    }

    /// Read one plan by id, including completed ones. `None` means the
    /// plan id is unknown. Verification/evidence queries only — the
    /// resume path reads through [`Self::reclaim_plan_open`].
    pub fn reclaim_plan(&self, plan_id: &str) -> Result<Option<ReclaimPlan>, StoreError> {
        let found: Option<String> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT plan_id FROM reclaim_plans WHERE plan_id=?1",
                    params![plan_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?
        };
        match found {
            Some(id) => Ok(Some(self.load_reclaim_plan(&id)?)),
            None => Ok(None),
        }
    }

    /// Phase 1: compute the bounded eligible set and persist it as an
    /// open plan. Deterministic oldest-first by registration time, then
    /// body id. A plan never exceeds `bounds`. Bodies whose catalogue
    /// path fails containment or names a protected installation file are
    /// held out of the plan; bodies larger than the whole byte bound
    /// are reported as skipped, never planned. If an open plan already
    /// exists it is returned unchanged — resume semantics, never a
    /// second concurrent plan.
    pub fn reclaim_plan_create(
        &self,
        settings: &CategorySettings,
        bounds: &PlanBounds,
        root: &Path,
        protected: &[PathBuf],
        now_unix_seconds: i64,
    ) -> Result<ReclaimPlan, StoreError> {
        if bounds.max_bodies == 0 || bounds.max_bytes == 0 {
            return Err(StoreError::Internal {
                detail: "reclaim plan bounds must be positive".into(),
            });
        }
        if let Some(open) = self.reclaim_plan_open()? {
            return Ok(open);
        }
        let PlannedSet {
            entries: planned,
            skipped,
            total_bytes,
            held,
        } = self.collect_plan(settings, bounds, root, protected, now_unix_seconds)?;
        let plan_id = reclaim_plan_id(now_unix_seconds);
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT plan_id FROM reclaim_plans WHERE status='open'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        if let Some(id) = existing {
            // Another process won the race: return its plan exactly as
            // persisted (phase-1 evidence included), never a mixture
            // of its entries with this process's fresh selection.
            drop(tx);
            drop(guard);
            return self.load_reclaim_plan(&id);
        }
        tx.execute(
            "INSERT INTO reclaim_plans (plan_id, created_unix_seconds, status, \
             body_count, total_bytes) VALUES (?1, ?2, 'open', ?3, ?4)",
            params![
                plan_id,
                now_unix_seconds,
                planned.len() as i64,
                total_bytes as i64
            ],
        )
        .map_err(map_err)?;
        for entry in &planned {
            tx.execute(
                "INSERT INTO reclaim_plan_entries (plan_id, body_id, category, \
                 bytes, reason) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    plan_id,
                    entry.body_id,
                    entry.category,
                    entry.size_bytes as i64,
                    entry.reason
                ],
            )
            .map_err(map_err)?;
        }
        // Phase-1 held/skipped evidence travels with the plan row, so
        // an interrupted run resumes with the same operator-facing
        // context it was created with.
        let phase1 = serde_json::json!({ "skipped": skipped, "held": held }).to_string();
        tx.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, ?2)",
            params![format!("reclaim_plan_phase1:{plan_id}"), phase1],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(ReclaimPlan {
            plan_id,
            created_unix_seconds: now_unix_seconds,
            status: "open".into(),
            entries: planned
                .iter()
                .map(|e| ReclaimPlanEntry {
                    body_id: e.body_id.clone(),
                    category: e.category.clone(),
                    size_bytes: e.size_bytes,
                    reason: e.reason.clone(),
                    outcome: None,
                    outcome_reason: None,
                })
                .collect(),
            total_bytes,
            skipped,
            held,
        })
    }

    /// Phase 2 for one planned entry: re-verify, reclaim, record. Used
    /// by [`Self::reclaim_plan_execute`] and directly by tests to drive
    /// an interrupted run one body at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn reclaim_plan_entry(
        &self,
        plan_id: &str,
        body_id: &str,
        settings: &CategorySettings,
        root: &Path,
        protected: &[PathBuf],
        delete_error_limit: usize,
        now_unix_seconds: i64,
    ) -> Result<ReclaimExecution, StoreError> {
        if delete_error_limit == 0 {
            return Err(StoreError::Internal {
                detail: "delete_error_limit must be positive".into(),
            });
        }
        let mut execution = ReclaimExecution {
            plan_id: plan_id.to_string(),
            ..ReclaimExecution::default()
        };
        // The entry must belong to this plan and still be pending —
        // the public entry point is not a general delete primitive.
        let pending: Option<Option<String>> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT outcome FROM reclaim_plan_entries \
                     WHERE plan_id=?1 AND body_id=?2",
                    params![plan_id, body_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?
        };
        match pending {
            None => {
                return Err(StoreError::ConstraintConflict {
                    detail: format!("body {body_id} is not an entry of plan {plan_id}"),
                });
            }
            Some(Some(_)) => {
                execution.already_recorded = 1;
                return Ok(execution);
            }
            Some(None) => {}
        }
        self.execute_entry(
            plan_id,
            body_id,
            settings,
            root,
            protected,
            delete_error_limit,
            now_unix_seconds,
            &mut execution,
        )?;
        self.maybe_complete_plan(plan_id, now_unix_seconds, &mut execution)?;
        Ok(execution)
    }

    /// Phase 2: execute the open plan. Already-recorded entries are
    /// skipped untouched; `present` entries are re-checked (pin, path,
    /// protection) inside the marking transaction before any filesystem
    /// work; `reclaiming` rows from an interrupted run finish through
    /// the same delete-and-record path. When every entry carries an
    /// outcome the plan is stamped `completed`. Returns `None` when no
    /// open plan exists.
    pub fn reclaim_plan_execute(
        &self,
        settings: &CategorySettings,
        root: &Path,
        protected: &[PathBuf],
        delete_error_limit: usize,
        now_unix_seconds: i64,
    ) -> Result<Option<ReclaimExecution>, StoreError> {
        if delete_error_limit == 0 {
            return Err(StoreError::Internal {
                detail: "delete_error_limit must be positive".into(),
            });
        }
        let Some(plan) = self.reclaim_plan_open()? else {
            return Ok(None);
        };
        let mut execution = ReclaimExecution {
            plan_id: plan.plan_id.clone(),
            ..ReclaimExecution::default()
        };
        for entry in &plan.entries {
            if entry.outcome.is_some() {
                execution.already_recorded += 1;
                continue;
            }
            // Re-read the entry's outcome: a concurrent maintenance
            // writer may have recorded it since this plan was read.
            // The loser of that race is already_recorded, never a
            // second reclaim.
            let still_pending: Option<Option<String>> = {
                let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                    detail: "store lock poisoned".into(),
                })?;
                guard
                    .query_row(
                        "SELECT outcome FROM reclaim_plan_entries \
                         WHERE plan_id=?1 AND body_id=?2",
                        params![plan.plan_id, entry.body_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(map_err)?
            };
            match still_pending {
                Some(Some(_)) => {
                    execution.already_recorded += 1;
                    continue;
                }
                Some(None) => {}
                None => {
                    return Err(StoreError::Internal {
                        detail: format!(
                            "plan entry {} vanished from plan {}",
                            entry.body_id, plan.plan_id
                        ),
                    });
                }
            }
            self.execute_entry(
                &plan.plan_id,
                &entry.body_id,
                settings,
                root,
                protected,
                delete_error_limit,
                now_unix_seconds,
                &mut execution,
            )?;
        }
        self.maybe_complete_plan(&plan.plan_id, now_unix_seconds, &mut execution)?;
        Ok(Some(execution))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_entry(
        &self,
        plan_id: &str,
        body_id: &str,
        settings: &CategorySettings,
        root: &Path,
        protected: &[PathBuf],
        delete_error_limit: usize,
        now_unix_seconds: i64,
        execution: &mut ReclaimExecution,
    ) -> Result<(), StoreError> {
        // The eligibility reason that authorized planning, used as the
        // recorded reclaim reason when the delete itself succeeds.
        let entry_reason: Option<String> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT reason FROM reclaim_plan_entries \
                     WHERE plan_id=?1 AND body_id=?2",
                    params![plan_id, body_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?
        };
        let row: Option<(String, String, i64)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT state, path, bytes FROM body_catalog WHERE body_id=?1",
                    params![body_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(map_err)?
        };
        let Some((state, path, bytes)) = row else {
            // An entry can never outlive its catalogue row (FK), but
            // treat a vanished row as unavailable evidence, never an
            // error.
            self.record_entry_outcome(
                plan_id,
                body_id,
                "unavailable",
                "not_registered",
                now_unix_seconds,
            )?;
            execution.unavailable.push(HeldBody {
                body_id: body_id.into(),
                reason: "not_registered".into(),
            });
            return Ok(());
        };
        if state == "reclaimed" {
            self.record_entry_outcome(
                plan_id,
                body_id,
                "unavailable",
                "already_reclaimed",
                now_unix_seconds,
            )?;
            execution.unavailable.push(HeldBody {
                body_id: body_id.into(),
                reason: "already_reclaimed".into(),
            });
            return Ok(());
        }
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        let managed = match managed_path(&root_abs, &path) {
            Ok(managed) => managed,
            Err(reason) => {
                self.record_entry_outcome(plan_id, body_id, "held", &reason, now_unix_seconds)?;
                execution.held.push(HeldBody {
                    body_id: body_id.into(),
                    reason,
                });
                return Ok(());
            }
        };
        if is_protected(&managed, protected) {
            self.record_entry_outcome(
                plan_id,
                body_id,
                "held",
                "protected_path",
                now_unix_seconds,
            )?;
            execution.held.push(HeldBody {
                body_id: body_id.into(),
                reason: "protected_path".into(),
            });
            return Ok(());
        }
        let bytes = bytes.max(0) as u64;
        if state == "present" {
            // Pin AND age/byte eligibility are recomputed inside the
            // marking transaction — a `size_pressure` entry whose
            // category fell back under the cap while this plan ran is
            // no longer eligible and stays `present`. A pin that
            // appeared after planning holds the body here.
            match self.mark_reclaiming_if_eligible(body_id, settings, root, now_unix_seconds) {
                Ok(MarkOutcome::Marked { reason }) => {
                    let _ = reason;
                    self.delete_and_record(
                        plan_id,
                        body_id,
                        bytes,
                        &managed,
                        entry_reason.as_deref(),
                        delete_error_limit,
                        now_unix_seconds,
                        execution,
                    )?;
                }
                Ok(MarkOutcome::Pinned) => {
                    self.record_entry_outcome(
                        plan_id,
                        body_id,
                        "held",
                        "pinned",
                        now_unix_seconds,
                    )?;
                    execution.held.push(HeldBody {
                        body_id: body_id.into(),
                        reason: "pinned".into(),
                    });
                }
                Ok(MarkOutcome::WithinLimits) => {
                    self.record_entry_outcome(
                        plan_id,
                        body_id,
                        "held",
                        "within_limits",
                        now_unix_seconds,
                    )?;
                    execution.held.push(HeldBody {
                        body_id: body_id.into(),
                        reason: "within_limits".into(),
                    });
                }
                // The row left `present` between the earlier read and
                // the marking transaction (a concurrent sweep, or the
                // second of two concurrent cleanup invocations):
                // re-read instead of aborting the pass or recording a
                // false hold.
                Err(StoreError::ConstraintConflict { .. }) => match self.row_state(body_id)? {
                    Some(current) if current == "reclaiming" => {
                        self.delete_and_record_with_pin_recheck(
                            plan_id,
                            body_id,
                            bytes,
                            &managed,
                            entry_reason.as_deref(),
                            delete_error_limit,
                            now_unix_seconds,
                            execution,
                        )?;
                    }
                    Some(_) => {
                        self.record_entry_outcome(
                            plan_id,
                            body_id,
                            "unavailable",
                            "already_reclaimed",
                            now_unix_seconds,
                        )?;
                        execution.unavailable.push(HeldBody {
                            body_id: body_id.into(),
                            reason: "already_reclaimed".into(),
                        });
                    }
                    None => {
                        self.record_entry_outcome(
                            plan_id,
                            body_id,
                            "unavailable",
                            "not_registered",
                            now_unix_seconds,
                        )?;
                        execution.unavailable.push(HeldBody {
                            body_id: body_id.into(),
                            reason: "not_registered".into(),
                        });
                    }
                },
                Err(e) => return Err(e),
            }
            return Ok(());
        }
        // `reclaiming` row — an interrupted run left it for us, or
        // a concurrent pass marked it without deleting yet:
        // re-check the pin before retrying the delete, exactly as
        // the sweep's resume path does.
        self.delete_and_record_with_pin_recheck(
            plan_id,
            body_id,
            bytes,
            &managed,
            entry_reason.as_deref(),
            delete_error_limit,
            now_unix_seconds,
            execution,
        )
    }

    /// Pin re-check shared by the interrupted-run and lost-race resume
    /// paths, then the delete-and-record step.
    #[allow(clippy::too_many_arguments)]
    fn delete_and_record_with_pin_recheck(
        &self,
        plan_id: &str,
        body_id: &str,
        bytes: u64,
        managed: &Path,
        entry_reason: Option<&str>,
        delete_error_limit: usize,
        now_unix_seconds: i64,
        execution: &mut ReclaimExecution,
    ) -> Result<(), StoreError> {
        if self.row_pinned(body_id)? {
            self.record_entry_outcome(plan_id, body_id, "held", "pinned", now_unix_seconds)?;
            execution.held.push(HeldBody {
                body_id: body_id.into(),
                reason: "pinned".into(),
            });
            return Ok(());
        }
        self.delete_and_record(
            plan_id,
            body_id,
            bytes,
            managed,
            entry_reason,
            delete_error_limit,
            now_unix_seconds,
            execution,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn delete_and_record(
        &self,
        plan_id: &str,
        body_id: &str,
        bytes: u64,
        managed: &Path,
        entry_reason: Option<&str>,
        delete_error_limit: usize,
        now_unix_seconds: i64,
        execution: &mut ReclaimExecution,
    ) -> Result<(), StoreError> {
        match delete_managed(managed) {
            Ok(reason) => {
                // Keep `missing` when the body was already gone;
                // otherwise record the planning reason.
                let recorded = if reason == "missing" {
                    "missing"
                } else {
                    entry_reason.unwrap_or("age_expired")
                };
                let transitioned =
                    self.finish_reclaim_and_record(plan_id, body_id, recorded, now_unix_seconds)?;
                if transitioned == 1 {
                    execution.bytes_reclaimed += bytes;
                    execution.reclaimed.push(ReclaimedBody {
                        body_id: body_id.into(),
                        bytes,
                        reason: recorded.into(),
                    });
                } else {
                    // A concurrent maintenance writer completed this
                    // body first: record it as already_recorded (the
                    // outcome write is first-write-wins) and never
                    // double-count its bytes.
                    execution.already_recorded += 1;
                    self.record_entry_outcome(
                        plan_id,
                        body_id,
                        "unavailable",
                        "already_reclaimed",
                        now_unix_seconds,
                    )?;
                    execution.unavailable.push(HeldBody {
                        body_id: body_id.into(),
                        reason: "already_reclaimed".into(),
                    });
                }
            }
            Err(detail) => {
                let failures = self.record_delete_failure(body_id)?;
                if failures >= delete_error_limit as i64 {
                    let outcome_reason = format!("delete_failed({failures}): {detail}");
                    self.record_entry_outcome(
                        plan_id,
                        body_id,
                        "delete_failed",
                        &outcome_reason,
                        now_unix_seconds,
                    )?;
                }
                execution.failed.push(HeldBody {
                    body_id: body_id.into(),
                    reason: format!("delete_failed({failures}): {detail}"),
                });
            }
        }
        Ok(())
    }

    fn record_entry_outcome(
        &self,
        plan_id: &str,
        body_id: &str,
        outcome: &str,
        reason: &str,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // First recorded outcome wins: a concurrent cleanup or sweep
        // may have recorded the entry already, and that record must not
        // be overwritten here.
        tx.execute(
            "UPDATE reclaim_plan_entries SET outcome=?3, outcome_reason=?4, \
             outcome_unix_seconds=?5 WHERE plan_id=?1 AND body_id=?2 \
             AND outcome IS NULL",
            params![plan_id, body_id, outcome, reason, now_unix_seconds],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
    }

    fn maybe_complete_plan(
        &self,
        plan_id: &str,
        now_unix_seconds: i64,
        execution: &mut ReclaimExecution,
    ) -> Result<(), StoreError> {
        let pending: i64 = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            guard
                .query_row(
                    "SELECT COUNT(*) FROM reclaim_plan_entries \
                     WHERE plan_id=?1 AND outcome IS NULL",
                    params![plan_id],
                    |r| r.get(0),
                )
                .map_err(map_err)?
        };
        if pending > 0 {
            return Ok(());
        }
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "UPDATE reclaim_plans SET status='completed', completed_unix_seconds=?2 \
             WHERE plan_id=?1 AND status='open'",
            params![plan_id, now_unix_seconds],
        )
        .map_err(map_err)?;
        tx.execute(
            "DELETE FROM store_meta WHERE key=?1",
            params![format!("reclaim_plan_phase1:{plan_id}")],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        execution.completed = true;
        Ok(())
    }

    /// Read-only storage-pressure report: per-category usage against
    /// the configured limits, reclaim status, and the bounded
    /// unavailable body list. Mutates nothing. Body paths are resolved
    /// against `root` exactly as the reclaim path resolves them —
    /// relative paths under the installation state root, absolute paths
    /// contained in it; anything that cannot be stat'ed is reported as
    /// `unknown`, never as present usage.
    pub fn retention_report(
        &self,
        settings: &CategorySettings,
        root: &Path,
        protected: &[PathBuf],
        now_unix_seconds: i64,
    ) -> Result<RetentionReport, StoreError> {
        let eligibility = self.catalogue_eligibility(settings, now_unix_seconds)?;
        let counts: BTreeMap<(String, String), (u64, u64)> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare(
                    "SELECT category, state, COUNT(*), COALESCE(SUM(bytes), 0) \
                     FROM body_catalog GROUP BY category, state",
                )
                .map_err(map_err)?;
            stmt.query_map([], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                    (
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)?.max(0) as u64,
                    ),
                ))
            })
            .map_err(map_err)?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(map_err)?
        };
        let reasons: BTreeMap<String, Vec<(String, u64, u64)>> = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare(
                    "SELECT category, COALESCE(reclaim_reason, 'unrecorded'), COUNT(*), \
                     COALESCE(SUM(bytes), 0) \
                     FROM body_catalog WHERE state='reclaimed' \
                     GROUP BY category, COALESCE(reclaim_reason, 'unrecorded')",
                )
                .map_err(map_err)?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)?.max(0) as u64,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?
            .into_iter()
            .fold(
                BTreeMap::new(),
                |mut acc, (category, reason, count, bytes)| {
                    acc.entry(category)
                        .or_default()
                        .push((reason, count, bytes));
                    acc
                },
            )
        };
        // Filesystem scan behind the `unknown` accounting: at most
        // UNKNOWN_SCAN_BOUND present bodies are stat'ed; a row whose
        // path cannot be read contributes `unknown`, never present
        // bytes. One over the bound flips the truncated flag.
        // (unknown count, unknown bytes, unknown pinned bytes,
        // unknown ids, verified pinned count, verified oldest)
        type UnknownTotals = BTreeMap<String, (u64, u64, u64, Vec<String>, u64, i64)>;
        let root_abs = std::path::absolute(root).map_err(|e| StoreError::Internal {
            detail: format!("retention root {}: {e}", root.display()),
        })?;
        type ScanOutcome = (
            UnknownTotals,
            bool,
            Vec<HeldBody>,
            Vec<String>,
            BTreeMap<String, (u64, u64)>,
        );
        let (unknown, unknown_scan_truncated, held_protected, protected_excluded, protected_stats): ScanOutcome = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let mut stmt = guard
                .prepare(
                    "SELECT body_id, category, path, bytes, pin_state, pinned, \
                     registered_unix_seconds FROM body_catalog WHERE state='present' \
                     ORDER BY registered_unix_seconds, body_id LIMIT ?1",
                )
                .map_err(map_err)?;
            let rows: Vec<(String, String, String, i64, String, i64, i64)> = stmt
                .query_map(params![(UNKNOWN_SCAN_BOUND + 1) as i64], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            let truncated = rows.len() > UNKNOWN_SCAN_BOUND;
            let mut acc: UnknownTotals = BTreeMap::new();
            let mut verified: BTreeMap<String, (u64, i64)> = BTreeMap::new();
            let mut held_protected: Vec<HeldBody> = Vec::new();
            let mut protected_excluded: Vec<String> = Vec::new();
            let mut protected_stats: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for (entry_body_id, category, path, bytes, pin_state, pinned, created_at) in
                rows.into_iter().take(UNKNOWN_SCAN_BOUND)
            {
                // Protected installation files: never counted, never
                // deleted — held and named.
                if let Ok(managed) = managed_path(&root_abs, &path)
                    && is_protected(&managed, protected)
                {
                    if held_protected.len() < UNKNOWN_SCAN_BOUND {
                        held_protected.push(HeldBody {
                            body_id: entry_body_id.clone(),
                            reason: "protected_path".into(),
                        });
                    }
                    let pstat = protected_stats.entry(category.clone()).or_insert((0, 0));
                    pstat.0 += 1;
                    pstat.1 += bytes.max(0) as u64;
                    protected_excluded.push(entry_body_id.clone());
                    continue;
                }
                let stat_ok = match managed_path(&root_abs, &path) {
                    Ok(managed) => std::fs::symlink_metadata(&managed).is_ok(),
                    Err(_) => false,
                };
                let is_pin = PinState::parse(&pin_state)
                    .map(|p| p.is_pin())
                    .unwrap_or(false)
                    || pinned != 0;
                if stat_ok {
                    let v = verified.entry(category.clone()).or_insert((0, i64::MAX));
                    if is_pin {
                        v.0 += 1;
                    }
                    v.1 = v.1.min(created_at);
                    continue;
                }
                let entry = acc.entry(category).or_insert((0, 0, 0, Vec::new(), 0, i64::MAX));
                entry.0 += 1;
                entry.1 += bytes.max(0) as u64;
                entry.3.push(entry_body_id);
                if is_pin {
                    entry.2 += bytes.max(0) as u64;
                }
            }
            // Fold the verified aggregates into the unknown entries so
            // pinned_count and oldest age come from the same measured
            // (stat'able) set; a category present only as ghosts
            // reports no verified pins and no oldest age.
            for (category, (pinned_count, created)) in verified {
                let entry = acc
                    .entry(category)
                    .or_insert((0, 0, 0, Vec::new(), 0, i64::MAX));
                entry.4 = pinned_count;
                entry.5 = created;
            }
            (acc, truncated, held_protected, protected_excluded, protected_stats)
        };
        let mut names: Vec<String> = eligibility
            .categories
            .iter()
            .map(|c| c.category.clone())
            .collect();
        for (category, _) in counts.keys() {
            if !names.contains(category) {
                names.push(category.clone());
            }
        }
        names.sort();
        names.dedup();
        let mut categories: Vec<CategoryReport> = Vec::with_capacity(names.len());
        for category in names {
            let reported = eligibility.category(&category);
            let limits = reported.map(|r| (r.max_age, r.max_bytes)).or_else(|| {
                CategoryLimits::documented_default(&category).map(|l| (l.max_age, l.max_bytes))
            });
            let (present_count, present_bytes) = counts
                .get(&(category.clone(), "present".to_string()))
                .copied()
                .unwrap_or((0, 0));
            let (present_count, present_bytes) = {
                let (pcount, pbytes) = protected_stats.get(&category).copied().unwrap_or((0, 0));
                (
                    present_count.saturating_sub(pcount),
                    present_bytes.saturating_sub(pbytes),
                )
            };
            let (reclaiming_count, reclaiming_bytes) = counts
                .get(&(category.clone(), "reclaiming".to_string()))
                .copied()
                .unwrap_or((0, 0));
            let (reclaimed_count, reclaimed_bytes) = counts
                .get(&(category.clone(), "reclaimed".to_string()))
                .copied()
                .unwrap_or((0, 0));
            let (
                unknown_count,
                unknown_bytes,
                unknown_pinned_bytes,
                unknown_ids,
                pinned_count_measured,
                oldest_measured,
            ) = match unknown.get(&category) {
                Some((c, b, p, ids, vc, vold)) => (*c, *b, *p, ids.as_slice(), *vc, *vold),
                None => (0, 0, 0, &[][..], 0, i64::MAX),
            };
            let pinned_count = pinned_count_measured;
            // Pressure is computed on the verifiable remainder only:
            // bodies whose path cannot be stat'ed never count toward
            // present usage or pinned-byte caps.
            let verified_present_bytes = present_bytes.saturating_sub(unknown_bytes);
            let verified_protected_bytes = reported
                .map(|r| r.protected_bytes)
                .unwrap_or(0)
                .saturating_sub(unknown_pinned_bytes);
            let max_bytes = limits.and_then(|l| l.1);
            let over_max_bytes = max_bytes.is_some_and(|max| verified_present_bytes > max);
            let protected_over_limit = max_bytes.is_some_and(|max| verified_protected_bytes > max);
            let eligible: Vec<&BodyEligibility> = eligibility
                .entries
                .iter()
                .filter(|e| {
                    e.category == category
                        && e.eligible
                        && !unknown_ids.iter().any(|id| id == &e.body_id)
                        && !protected_excluded.iter().any(|id| id == &e.body_id)
                        && (e.age_expired || (e.category_over_max_bytes && over_max_bytes))
                })
                .collect();
            let eligible_count = eligible.len() as u64;
            let eligible_bytes = eligible.iter().map(|e| e.size_bytes).sum();
            let pressure = if protected_over_limit {
                "protected_over_limit"
            } else if eligible_count > 0 {
                "over_limit"
            } else {
                "none"
            };
            let default_limits = CategoryLimits::unlimited();
            categories.push(CategoryReport {
                category: category.clone(),
                max_age_seconds: limits.map_or(default_limits.max_age, |l| l.0),
                max_bytes,
                present_count: present_count.saturating_sub(unknown_count),
                present_bytes: verified_present_bytes,
                pinned_count,
                pinned_bytes: verified_protected_bytes,
                eligible_count,
                eligible_bytes,
                reclaiming_count,
                reclaiming_bytes,
                reclaimed_count,
                reclaimed_bytes,
                reclaimed_by_reason: reasons
                    .get(&category)
                    .map(|v| v.iter().map(|(r, c, _)| (r.clone(), *c)).collect())
                    .unwrap_or_default(),
                reclaimed_bytes_by_reason: reasons
                    .get(&category)
                    .map(|v| v.iter().map(|(r, _, b)| (r.clone(), *b)).collect())
                    .unwrap_or_default(),
                unknown_bodies: unknown_ids.iter().take(16).cloned().collect(),
                unknown_bodies_truncated: unknown_ids.len() > 16,
                oldest_present_age_seconds: if oldest_measured == i64::MAX {
                    None
                } else {
                    Some(now_unix_seconds.saturating_sub(oldest_measured))
                },
                unknown_count,
                unknown_bytes,
                over_max_bytes,
                protected_over_limit,
                pressure: pressure.into(),
            });
        }
        categories.sort_by(|a, b| a.category.cmp(&b.category));
        let (unavailable, unavailable_truncated) = {
            let guard = self.conn.lock().map_err(|_| StoreError::Internal {
                detail: "store lock poisoned".into(),
            })?;
            let total: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM body_catalog WHERE state='reclaimed'",
                    [],
                    |r| r.get(0),
                )
                .map_err(map_err)?;
            let mut stmt = guard
                .prepare(
                    "SELECT body_id, category, bytes, reclaim_reason, \
                     reclaimed_unix_seconds FROM body_catalog \
                     WHERE state='reclaimed' \
                     ORDER BY reclaimed_unix_seconds, body_id LIMIT ?1",
                )
                .map_err(map_err)?;
            let rows = stmt
                .query_map(params![UNAVAILABLE_REPORT_BOUND as i64], |r| {
                    Ok(UnavailableBody {
                        body_id: r.get(0)?,
                        category: r.get(1)?,
                        size_bytes: r.get::<_, i64>(2)?.max(0) as u64,
                        reclaim_reason: r.get(3)?,
                        reclaimed_unix_seconds: r.get(4)?,
                    })
                })
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            (rows, total as usize > UNAVAILABLE_REPORT_BOUND)
        };
        Ok(RetentionReport {
            generated_unix_seconds: now_unix_seconds,
            categories,
            unavailable,
            unavailable_truncated,
            unknown_scan_truncated,
            held: held_protected,
        })
    }
}

/// The dispatch-path view of the body catalogue: the same durable rows
/// the retention sweep owns, exposed through the core port so
/// `workloop-exec` never names this crate's concrete types.
impl workloop_core::store::BodyCatalog for SqliteStore {
    fn body_entry(
        &self,
        body_id: &str,
    ) -> Result<Option<workloop_core::store::BodyCatalogEntry>, StoreError> {
        self.catalogue_entry(body_id)
    }

    fn body_entry_at(
        &self,
        category: &str,
        path: &str,
    ) -> Result<Option<workloop_core::store::BodyCatalogEntry>, StoreError> {
        self.catalogue_entry_at(category, path)
    }

    fn register_body(&self, body: &workloop_core::store::NewCatalogBody) -> Result<(), StoreError> {
        // Content-addressed identity: re-registering the same body is
        // a replay, conflicting metadata under the same id is refused.
        if let Some(existing) = self.catalogue_entry(&body.body_id)? {
            let same = existing.category == body.category
                && existing.path == body.path
                && existing.bytes == body.bytes;
            if !same {
                return Err(StoreError::ConstraintConflict {
                    detail: format!(
                        "body {} already registered with different metadata",
                        body.body_id
                    ),
                });
            }
            // Re-observation of still-needed material extends the
            // eligibility horizon — never shortens it.
            if let Some(until) = body.eligible_after_unix_seconds {
                self.extend_body_eligibility(&body.body_id, until)?;
            }
            return Ok(());
        }
        self.register_body(&CatalogBody {
            body_id: body.body_id.clone(),
            category: body.category.clone(),
            path: body.path.clone(),
            bytes: body.bytes,
            registered_unix_seconds: body.registered_unix_seconds,
            eligible_after_unix_seconds: body.eligible_after_unix_seconds,
            pinned: body.pinned,
        })
    }

    fn set_body_pin(
        &self,
        body_id: &str,
        dependency: &str,
        pinned: bool,
    ) -> Result<(), StoreError> {
        self.set_body_pin(body_id, dependency, pinned)
    }
}
