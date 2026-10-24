//! The SQLite store: current-state tables plus an append-only `event_log`.
//! Structural invariants live in constraints and partial unique indexes;
//! transition legality stays in `work`. One connection behind a mutex
//! serializes transactions within a process; across processes (every CLI
//! invocation is one) SQLite's file locking serializes writers, and the busy
//! timeout makes the loser wait for the lock instead of failing immediately —
//! the constraint layer is the belt, not a race window.

use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Mutex;
use std::time::Duration;

use workloop_core::command::{CommandDecision, CommandId};
use workloop_core::domain::*;
use workloop_core::observation::Observation;
use workloop_core::store::{Deferral, Effect, PrActionIntent, ReadView, Store, StoreError, TxView};

const SCHEMA: &str = include_str!("schema.sql");

/// How long a writer waits on the file lock before `SQLITE_BUSY` surfaces.
/// Connection-local (applies inside `transact` and during `init` itself) and
/// set explicitly so the guarantee does not depend on the binding's default.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SqliteStore {
    conn: Mutex<Connection>,
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(internal)?;
        conn.execute_batch(SCHEMA).map_err(internal)?;
        // Seed the fixed slot rows once; claiming is an UPDATE inside the
        // caller's transaction.
        for slot in 0..capacity {
            conn.execute(
                "INSERT OR IGNORE INTO capacity_slots (slot_id) VALUES (?1)",
                params![i64::from(slot)],
            )
            .map_err(internal)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn internal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Internal {
        detail: e.to_string(),
    }
}

/// Map a rusqlite failure to the store's conflict vocabulary.
fn map_err(e: rusqlite::Error) -> StoreError {
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

struct Tx<'t> {
    tx: rusqlite::Transaction<'t>,
}

impl Store for SqliteStore {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut guard = self.conn.lock().map_err(|_| StoreError::Internal {
            detail: "store lock poisoned".into(),
        })?;
        let tx = guard.transaction().map_err(internal)?;
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
        outcome: opt_de_row(row.get::<_, Option<String>>(8)?)?,
        enqueued_seq: row.get(9)?,
        allowed_effects: de_row(&row.get::<_, String>(10)?)?,
        input: de_row(&row.get::<_, String>(11)?)?,
        budget: de_row(&row.get::<_, String>(12)?)?,
        invocations_used: row.get(13)?,
        review_rounds_used: row.get(14)?,
        started_seq: row.get(15)?,
        started_unix_seconds: row.get(16)?,
        recovery_attempts_used: row.get(17)?,
        recovery_deadline_unix_seconds: row.get(18)?,
        uncertain_since_unix_seconds: row.get(19)?,
    })
}

const EXECUTION_COLS: &str = "id, work_id, ordinal, state, waiting_reason, \
    desired_action, terminal_code, pause_reason, outcome, enqueued_seq, \
    allowed_effects, input, budget, invocations_used, review_rounds_used, \
    started_seq, started_unix_seconds, recovery_attempts_used, \
    recovery_deadline_unix_seconds, uncertain_since_unix_seconds";

const INVOCATION_COLS: &str = "id, request_id, execution_id, role, state, \
    process_identity, required_artifacts, evidence_refs";

fn invocation_from_row(row: &rusqlite::Row<'_>) -> Result<Invocation, rusqlite::Error> {
    Ok(Invocation {
        id: InvocationId(row.get::<_, String>(0)?),
        request_id: row.get(1)?,
        execution_id: ExecutionId(row.get::<_, String>(2)?),
        role: untag(&row.get::<_, String>(3)?)?,
        state: untag(&row.get::<_, String>(4)?)?,
        process_identity: opt_de_row(row.get::<_, Option<String>>(5)?)?,
        required_artifacts: de_row(&row.get::<_, String>(6)?)?,
        evidence_refs: de_row(&row.get::<_, String>(7)?)?,
    })
}

fn request_from_row(row: &rusqlite::Row<'_>) -> Result<InvocationRequest, rusqlite::Error> {
    Ok(InvocationRequest {
        request_id: row.get(0)?,
        work_id: WorkId(row.get::<_, String>(1)?),
        execution_id: ExecutionId(row.get::<_, String>(2)?),
        role: untag(&row.get::<_, String>(3)?)?,
        payload: de_row(&row.get::<_, String>(4)?)?,
    })
}

fn observation_from_row(row: &rusqlite::Row<'_>) -> Result<Observation, rusqlite::Error> {
    Ok(Observation {
        source_event_id: row.get(0)?,
        subject_work_id: row.get::<_, Option<String>>(1)?.map(WorkId),
        subject_execution_id: row.get::<_, Option<String>>(2)?.map(ExecutionId),
        fact: de_row(&row.get::<_, String>(3)?)?,
        observed_unix_seconds: row.get(4)?,
    })
}

fn write_execution(tx: &Tx<'_>, e: &Execution) -> Result<(), StoreError> {
    tx.exec(
        "INSERT INTO executions (id, work_id, ordinal, state, waiting_reason, \
         desired_action, terminal_code, pause_reason, outcome, enqueued_seq, \
         allowed_effects, input, budget, invocations_used, review_rounds_used, \
         started_seq, started_unix_seconds, recovery_attempts_used, \
         recovery_deadline_unix_seconds, uncertain_since_unix_seconds) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
        params![
            e.id.0,
            e.work_id.0,
            e.ordinal,
            tag(&e.state),
            e.waiting_reason.as_ref().map(tag),
            e.desired_action.as_ref().map(tag),
            e.terminal_code,
            e.pause_reason,
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
         terminal_code=?7, pause_reason=?8, outcome=?9, enqueued_seq=?10, \
         allowed_effects=?11, input=?12, budget=?13, invocations_used=?14, \
         review_rounds_used=?15, started_seq=?16, started_unix_seconds=?17, \
         recovery_attempts_used=?18, recovery_deadline_unix_seconds=?19, \
         uncertain_since_unix_seconds=?20 \
         WHERE id=?1 AND work_id=?2 AND ordinal=?3 AND state=?21",
        params![
            e.id.0,
            e.work_id.0,
            e.ordinal,
            tag(&e.state),
            e.waiting_reason.as_ref().map(tag),
            e.desired_action.as_ref().map(tag),
            e.terminal_code,
            e.pause_reason,
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

impl ReadView for Tx<'_> {
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
            "SELECT id, version, revision, intent, issue_refs, constraints_, \
             verification_argv, created_seq FROM works WHERE id=?1",
            params![work_id.0],
            |row| {
                Ok(Work {
                    id: WorkId(row.get::<_, String>(0)?),
                    version: row.get::<_, u64>(1)?,
                    revision: row.get(2)?,
                    intent: row.get(3)?,
                    issue_refs: de_row(&row.get::<_, String>(4)?)?,
                    constraints: de_row(&row.get::<_, String>(5)?)?,
                    verification_argv: de_row(&row.get::<_, String>(6)?)?,
                    created_seq: row.get(7)?,
                })
            },
        )
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
        let n: i64 = self
            .tx
            .query_row("SELECT COUNT(*) FROM capacity_slots", [], |r| r.get(0))
            .map_err(map_err)?;
        Ok(n as u32)
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
            "SELECT request_id, work_id, execution_id, role, payload \
             FROM invocation_requests WHERE consumed=0 ORDER BY rowid",
            [],
            |row| {
                Ok(InvocationRequest {
                    request_id: row.get(0)?,
                    work_id: WorkId(row.get::<_, String>(1)?),
                    execution_id: ExecutionId(row.get::<_, String>(2)?),
                    role: untag(&row.get::<_, String>(3)?)?,
                    payload: de_row(&row.get::<_, String>(4)?)?,
                })
            },
        )
    }

    fn request(&self, request_id: &str) -> Result<Option<InvocationRequest>, StoreError> {
        self.query_opt(
            "SELECT request_id, work_id, execution_id, role, payload \
             FROM invocation_requests WHERE request_id=?1",
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
            "SELECT id, version, revision, intent, issue_refs, constraints_, \
             verification_argv, created_seq FROM works ORDER BY created_seq",
            [],
            |row| {
                Ok(Work {
                    id: WorkId(row.get::<_, String>(0)?),
                    version: row.get::<_, u64>(1)?,
                    revision: row.get(2)?,
                    intent: row.get(3)?,
                    issue_refs: de_row(&row.get::<_, String>(4)?)?,
                    constraints: de_row(&row.get::<_, String>(5)?)?,
                    verification_argv: de_row(&row.get::<_, String>(6)?)?,
                    created_seq: row.get(7)?,
                })
            },
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
            "SELECT source_event_id, subject_work_id, subject_execution_id, \
             fact, observed_unix_seconds FROM observations \
             WHERE subject_execution_id=?1 ORDER BY id",
            params![execution_id.0],
            observation_from_row,
        )
    }

    fn observations_for_work(&self, work_id: &WorkId) -> Result<Vec<Observation>, StoreError> {
        self.query_all(
            "SELECT source_event_id, subject_work_id, subject_execution_id, \
             fact, observed_unix_seconds FROM observations \
             WHERE subject_work_id=?1 ORDER BY id",
            params![work_id.0],
            observation_from_row,
        )
    }

    fn action_intents(&self) -> Result<Vec<PrActionIntent>, StoreError> {
        self.query_all(
            "SELECT action_id, work_id, repository, action, status \
             FROM pr_action_intents ORDER BY rowid",
            [],
            |row| {
                Ok(PrActionIntent {
                    action_id: row.get(0)?,
                    work_id: WorkId(row.get::<_, String>(1)?),
                    repository: row.get(2)?,
                    action: de_row(&row.get::<_, String>(3)?)?,
                    status: untag(&row.get::<_, String>(4)?)?,
                })
            },
        )
    }

    fn judgments(
        &self,
        work_id: &WorkId,
    ) -> Result<Vec<(String, Vec<workloop_core::command::FindingDecision>)>, StoreError> {
        self.query_all(
            "SELECT expected_pr_head, decisions FROM judgments \
             WHERE work_id=?1 ORDER BY seq",
            params![work_id.0],
            |row| Ok((row.get(0)?, de_row(&row.get::<_, String>(1)?)?)),
        )
    }

    fn last_seq(&self) -> Result<EventSequence, StoreError> {
        let s: Option<i64> = self
            .tx
            .query_row("SELECT MAX(seq) FROM event_log", [], |r| r.get(0))
            .map_err(map_err)?;
        Ok(s.unwrap_or(0))
    }
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
            Effect::InsertWork { work } => {
                self.exec(
                    "INSERT INTO works (id, version, revision, intent, issue_refs, \
                     constraints_, verification_argv, created_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        work.id.0,
                        work.version,
                        work.revision,
                        work.intent,
                        json(&work.issue_refs)?,
                        json(&work.constraints)?,
                        json(&work.verification_argv)?,
                        work.created_seq,
                    ],
                )?;
            }
            Effect::BumpWorkVersion {
                work_id,
                expected,
                next,
            } => {
                let n = self.exec(
                    "UPDATE works SET version=?1 WHERE id=?2 AND version=?3",
                    params![next, work_id.0, expected],
                )?;
                if n == 0 {
                    let current: u64 = self
                        .tx
                        .query_row(
                            "SELECT version FROM works WHERE id=?1",
                            params![work_id.0],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(map_err)?
                        .unwrap_or(0);
                    return Err(StoreError::VersionConflict {
                        work_id: work_id.clone(),
                        current_version: current,
                    });
                }
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
                let n = self.exec(
                    "UPDATE capacity_slots SET execution_id=?1 \
                     WHERE slot_id = (SELECT slot_id FROM capacity_slots \
                                      WHERE execution_id IS NULL LIMIT 1)",
                    params![execution_id.0],
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
                // Plain INSERT: a live row conflicts on the PK; `work`
                // deletes an expired claim (by its own holder) first.
                self.exec(
                    "INSERT INTO resource_reservations \
                     (resource_key, resource, holder, expires_at_unix_seconds) \
                     VALUES (?1,?2,?3,?4)",
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
                for key in resource_keys {
                    self.exec(
                        "DELETE FROM resource_reservations \
                         WHERE resource_key=?1 AND holder=?2",
                        params![key, json(holder)?],
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
                    "INSERT INTO invocation_requests \
                     (request_id, work_id, execution_id, role, payload) \
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        request.request_id,
                        request.work_id.0,
                        request.execution_id.0,
                        tag(&request.role),
                        json(&request.payload)?,
                    ],
                )?;
            }
            Effect::InsertInvocation { invocation } => {
                self.exec(
                    "INSERT INTO invocations \
                     (id, request_id, execution_id, role, state, \
                      process_identity, required_artifacts, evidence_refs) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        invocation.id.0,
                        invocation.request_id,
                        invocation.execution_id.0,
                        tag(&invocation.role),
                        tag(&invocation.state),
                        invocation.process_identity.as_ref().map(json).transpose()?,
                        json(&invocation.required_artifacts)?,
                        json(&invocation.evidence_refs)?,
                    ],
                )?;
            }
            Effect::UpdateInvocation {
                invocation,
                expected_state,
            } => {
                let n = self.exec(
                    "UPDATE invocations SET state=?2, process_identity=?3, \
                     required_artifacts=?4, evidence_refs=?5 \
                     WHERE id=?1 AND state=?6",
                    params![
                        invocation.id.0,
                        tag(&invocation.state),
                        invocation.process_identity.as_ref().map(json).transpose()?,
                        json(&invocation.required_artifacts)?,
                        json(&invocation.evidence_refs)?,
                        tag(expected_state),
                    ],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("invocation {} not in expected state", invocation.id.0),
                    });
                }
            }
            Effect::InsertObservation { observation } => {
                self.exec(
                    "INSERT INTO observations \
                     (source_event_id, subject_work_id, subject_execution_id, \
                      fact, observed_unix_seconds) \
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        observation.source_event_id,
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
                    "INSERT INTO stop_intents (invocation_id, identities, recorded_seq) \
                     VALUES (?1,?2,?3)",
                    params![
                        intent.invocation_id.0,
                        json(&intent.identities)?,
                        intent.recorded_seq,
                    ],
                )?;
            }
            Effect::InsertPrActionIntent { intent } => {
                self.exec(
                    "INSERT INTO pr_action_intents \
                     (action_id, work_id, repository, action, status) \
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        intent.action_id,
                        intent.work_id.0,
                        intent.repository,
                        json(&intent.action)?,
                        tag(&intent.status),
                    ],
                )?;
            }
            Effect::MarkRequestConsumed { request_id } => {
                let n = self.exec(
                    "UPDATE invocation_requests SET consumed=1 \
                     WHERE request_id=?1 AND consumed=0",
                    params![request_id],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("request {request_id} already consumed or missing"),
                    });
                }
            }
            Effect::UpdateActionIntent { action_id, status } => {
                let n = self.exec(
                    "UPDATE pr_action_intents SET status=?2 WHERE action_id=?1",
                    params![action_id, tag(status)],
                )?;
                if n == 0 {
                    return Err(StoreError::ConstraintConflict {
                        detail: format!("unknown action_id {action_id}"),
                    });
                }
            }
            Effect::RecordDeferral {
                execution_id,
                reason,
                diagnostic,
            } => {
                let seq = self.alloc_seq()?;
                self.exec(
                    "INSERT OR REPLACE INTO deferrals \
                     (execution_id, reason, diagnostic, since_seq) \
                     VALUES (?1,?2,?3, \
                        COALESCE((SELECT since_seq FROM deferrals WHERE execution_id=?1), ?4))",
                    params![execution_id.0, reason, diagnostic, seq],
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
                let seq = self.alloc_seq()?;
                self.exec(
                    "INSERT OR REPLACE INTO attention_acks \
                     (work_id, attention_id, actor_id, seq) VALUES (?1,?2,?3,?4)",
                    params![work_id.0, attention_id, actor_id, seq],
                )?;
            }
            Effect::InsertJudgment {
                work_id,
                expected_pr_head,
                decisions,
            } => {
                let seq = self.alloc_seq()?;
                self.exec(
                    "INSERT INTO judgments (work_id, expected_pr_head, decisions, seq) \
                     VALUES (?1,?2,?3,?4)",
                    params![work_id.0, expected_pr_head, json(decisions)?, seq],
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
        decision: &CommandDecision,
    ) -> Result<(), StoreError> {
        self.exec(
            "INSERT INTO commands \
             (origin, actor_kind, actor_id, idempotency_key, payload_digest, \
              command_id, decision, seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                origin.0,
                tag(&actor.kind),
                actor.external_id,
                idempotency_key,
                payload_digest,
                decision.command_id().0.clone(),
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
    fn open_sets_a_busy_timeout() {
        let store = SqliteStore::in_memory(1).unwrap();
        let guard = store.conn.lock().unwrap();
        let ms: i64 = guard
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, i64::try_from(BUSY_TIMEOUT.as_millis()).unwrap());
    }
}
