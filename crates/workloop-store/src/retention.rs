//! Bounded retention sweep: the runtime tooling that reclaims managed
//! bodies listed in `body_catalog`. Core never performs filesystem I/O —
//! this pass executes deletions only, under the approved bounds
//! (cadence, `max_bodies`, `max_bytes`, `delete_error_limit`) and only
//! for rows the store already marks unattributed-free, unpinned and
//! age-eligible. Durable metadata rows are never deleted; a deleted body
//! leaves its catalog row behind with the reclaim reason, so evidence
//! references resolve `unavailable` rather than vanishing.

use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use workloop_core::store::StoreError;

use crate::sqlite::{SqliteStore, internal, map_err};

/// Bounds for one sweep pass. `0` on any axis is invalid configuration,
/// never "no limit" and never "reclaim everything".
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
}

impl Default for SweepParams {
    fn default() -> Self {
        Self {
            interval_seconds: 3600,
            max_bodies: 256,
            max_bytes: 1 << 30,
            delete_error_limit: 3,
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
             state) VALUES (?1,?2,?3,?4,?5,?6,?7,'present')",
            params![
                body.body_id,
                body.category,
                body.path,
                body.bytes as i64,
                body.registered_unix_seconds,
                body.eligible_after_unix_seconds,
                i64::from(body.pinned)
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
        tx.commit().map_err(internal)?;
        Ok(())
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
