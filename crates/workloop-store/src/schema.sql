-- workloop slice-0 store: current-state tables enforcing structural
-- invariants, plus an append-only event_log carrying EventSequence.

CREATE TABLE IF NOT EXISTS id_counters (
    prefix TEXT PRIMARY KEY,
    next INTEGER NOT NULL
);

-- Append-only; seq is the global EventSequence (queue order, snapshot
-- as_of, resume position). alloc_seq inserts a 'tick' row; each applied
-- effect appends a row too, so positions interleave but stay monotonic.
CREATE TABLE IF NOT EXISTS event_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS works (
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    intent TEXT NOT NULL,
    issue_refs TEXT NOT NULL,
    constraints_ TEXT NOT NULL,
    verification_argv TEXT NOT NULL,
    created_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS executions (
    id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL REFERENCES works(id),
    ordinal INTEGER NOT NULL,
    state TEXT NOT NULL,
    waiting_reason TEXT,
    desired_action TEXT,
    terminal_code TEXT,
    pause_reason TEXT,
    outcome TEXT,
    enqueued_seq INTEGER NOT NULL,
    allowed_effects TEXT NOT NULL,
    input TEXT NOT NULL,
    budget TEXT NOT NULL,
    invocations_used INTEGER NOT NULL DEFAULT 0,
    review_rounds_used INTEGER NOT NULL DEFAULT 0,
    started_seq INTEGER,
    started_unix_seconds INTEGER,
    recovery_attempts_used INTEGER NOT NULL DEFAULT 0,
    recovery_deadline_unix_seconds INTEGER,
    uncertain_since_unix_seconds INTEGER,
    UNIQUE (work_id, ordinal)
);

-- At most one nonterminal Execution per Work.
CREATE UNIQUE INDEX IF NOT EXISTS one_nonterminal_per_work
    ON executions (work_id)
    WHERE state NOT IN ('succeeded', 'failed', 'cancelled');

CREATE TABLE IF NOT EXISTS invocations (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    execution_id TEXT NOT NULL REFERENCES executions(id),
    role TEXT NOT NULL,
    state TEXT NOT NULL,
    process_identity TEXT,
    required_artifacts TEXT NOT NULL,
    evidence_refs TEXT NOT NULL
);

-- At most one unresolved writer per Execution.
CREATE UNIQUE INDEX IF NOT EXISTS one_unresolved_writer
    ON invocations (execution_id)
    WHERE state IN ('prepared', 'running', 'unknown');

-- `consumed` flips to 1 when the store records the Invocation for this
-- request (Effect::InsertInvocation); `pending_requests` reads it.
CREATE TABLE IF NOT EXISTS invocation_requests (
    request_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    execution_id TEXT NOT NULL REFERENCES executions(id),
    role TEXT NOT NULL,
    payload TEXT NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS commands (
    origin TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    command_id TEXT NOT NULL UNIQUE,
    decision TEXT NOT NULL,
    seq INTEGER NOT NULL,
    UNIQUE (origin, actor_kind, actor_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS resource_reservations (
    resource_key TEXT PRIMARY KEY,
    resource TEXT NOT NULL,
    holder TEXT NOT NULL,
    expires_at_unix_seconds INTEGER
);

-- Fixed N rows seeded at open; a holder claims a free row atomically.
CREATE TABLE IF NOT EXISTS capacity_slots (
    slot_id INTEGER PRIMARY KEY,
    execution_id TEXT UNIQUE
);

CREATE TABLE IF NOT EXISTS observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_event_id TEXT,
    subject_work_id TEXT,
    subject_execution_id TEXT,
    fact TEXT NOT NULL,
    observed_unix_seconds INTEGER NOT NULL
);

-- Dedupe keyed on delivery identity; rows without one never dedupe.
CREATE UNIQUE INDEX IF NOT EXISTS observations_dedupe
    ON observations (source_event_id)
    WHERE source_event_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS pr_action_intents (
    action_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    repository TEXT NOT NULL,
    action TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS stop_intents (
    invocation_id TEXT PRIMARY KEY,
    identities TEXT NOT NULL,
    recorded_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deferrals (
    execution_id TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    diagnostic TEXT NOT NULL,
    since_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attention_acks (
    work_id TEXT NOT NULL,
    attention_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (work_id, attention_id, actor_id)
);

CREATE TABLE IF NOT EXISTS judgments (
    work_id TEXT NOT NULL,
    expected_pr_head TEXT NOT NULL,
    decisions TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (work_id, expected_pr_head)
);
