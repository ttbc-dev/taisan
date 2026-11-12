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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimedBody {
    pub body_id: String,
    pub bytes: u64,
    /// `age_expired` (deleted this pass) or `missing` (already absent).
    pub reason: String,
}

/// One body the pass declined, with the recorded reason.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        match self.mark_reclaiming_if_eligible(body_id, settings, now_unix_seconds)? {
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
                    report.held.push(HeldBody {
                        body_id: candidate.body_id.clone(),
                        reason: "pinned".into(),
                    });
                    continue;
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
                    self.finish_reclaim(&candidate.body_id, &reason, now_unix_seconds)?;
                    report.bytes_reclaimed += candidate.bytes;
                    report.reclaimed.push(ReclaimedBody {
                        body_id: candidate.body_id.clone(),
                        bytes: candidate.bytes,
                        reason,
                    });
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
        now_unix_seconds: i64,
    ) -> Result<MarkOutcome, StoreError> {
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
        let present_bytes: i64 = tx
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM body_catalog \
                 WHERE state='present' AND category=?1",
                params![category],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let present_bytes = present_bytes.max(0) as u64;
        let over_max_bytes = limits
            .max_bytes
            .is_some_and(|max_bytes| present_bytes > max_bytes);
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

    fn finish_reclaim(
        &self,
        body_id: &str,
        reason: &str,
        now_unix_seconds: i64,
    ) -> Result<(), StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute(
            "UPDATE body_catalog SET state='reclaimed', reclaim_reason=?2, \
             delete_failures=0, reclaimed_unix_seconds=?3 WHERE body_id=?1 \
             AND state='reclaiming'",
            params![body_id, reason, now_unix_seconds],
        )
        .map_err(map_err)?;
        tx.commit().map_err(internal)?;
        Ok(())
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
fn is_protected(managed: &Path, protected: &[PathBuf]) -> bool {
    protected.iter().any(|p| {
        if p == managed {
            return true;
        }
        match (std::fs::canonicalize(p), std::fs::canonicalize(managed)) {
            (Ok(a), Ok(b)) => a == b,
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
