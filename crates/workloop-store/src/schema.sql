-- workloop v3 store: current-state tables enforcing structural
-- invariants, plus an append-only event_log carrying EventSequence.
-- `PRAGMA user_version = 3` marks the format; a stamped older file with
-- the complete recognized schema upgrades additively at open, while an
-- unversioned slice-0 database and a newer version are refused.

CREATE TABLE IF NOT EXISTS store_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

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
    binding TEXT NOT NULL,
    spec INTEGER NOT NULL,
    approved_spec INTEGER NOT NULL,
    source_gate TEXT NOT NULL,
    policy TEXT NOT NULL,
    created_seq INTEGER NOT NULL
);

-- Immutable spec snapshots; existing rows are never mutated.
CREATE TABLE IF NOT EXISTS specs (
    work_id TEXT NOT NULL REFERENCES works(id),
    revision INTEGER NOT NULL,
    payload TEXT NOT NULL,
    digest TEXT NOT NULL,
    approved_by TEXT NOT NULL,
    recorded_seq INTEGER NOT NULL,
    PRIMARY KEY (work_id, revision)
);

-- Immutable source membership recorded at acceptance.
CREATE TABLE IF NOT EXISTS work_issue_refs (
    work_id TEXT NOT NULL REFERENCES works(id),
    issue_key TEXT NOT NULL,
    issue TEXT NOT NULL,
    PRIMARY KEY (work_id, issue_key)
);

-- Active claims; a paused or terminal Work holds none.
CREATE TABLE IF NOT EXISTS issue_claims (
    issue_key TEXT PRIMARY KEY,
    issue TEXT NOT NULL,
    work_id TEXT NOT NULL REFERENCES works(id)
);

-- Durable Work-wide usage accounting.
CREATE TABLE IF NOT EXISTS work_usage (
    work_id TEXT PRIMARY KEY REFERENCES works(id),
    fix_invocations_used INTEGER NOT NULL DEFAULT 0,
    transport_retries_used INTEGER NOT NULL DEFAULT 0,
    equivalent_failure_streak INTEGER NOT NULL DEFAULT 0,
    equivalent_signature TEXT,
    automatic_seconds_used INTEGER NOT NULL DEFAULT 0,
    first_dispatch_unix_seconds INTEGER,
    last_progress_unix_seconds INTEGER,
    human_wait_seconds_total INTEGER NOT NULL DEFAULT 0
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
    repair_prerequisites TEXT NOT NULL DEFAULT '[]',
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
    paused_since_unix_seconds INTEGER,
    next_role TEXT,
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
    -- Write-capable descendants recorded while the leader was alive;
    -- every recorded identity must inspect stopped before an exit fact.
    descendants TEXT NOT NULL DEFAULT '[]',
    required_artifacts TEXT NOT NULL,
    evidence_refs TEXT NOT NULL
);

-- At most one unresolved writer per Execution.
CREATE UNIQUE INDEX IF NOT EXISTS one_unresolved_writer
    ON invocations (execution_id)
    WHERE state IN ('prepared', 'running', 'unknown');

-- `consumed` flips to 1 when the store records the Invocation for this
-- request (Effect::MarkRequestConsumed); `pending_requests` reads it.
CREATE TABLE IF NOT EXISTS invocation_requests (
    request_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    execution_id TEXT NOT NULL REFERENCES executions(id),
    role TEXT NOT NULL,
    spec_revision INTEGER NOT NULL,
    payload TEXT NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0
);

-- Durable Command decisions; the canonical payload bytes are stored
-- verbatim so replay compares bytes, and the digest detects a reused
-- key carrying a different payload.
CREATE TABLE IF NOT EXISTS commands (
    origin TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    canonical_payload BLOB NOT NULL,
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
    delivery_scope TEXT,
    delivery_host TEXT,
    delivery_source TEXT,
    delivery_event_id TEXT,
    subject_work_id TEXT,
    subject_execution_id TEXT,
    fact TEXT NOT NULL,
    observed_unix_seconds INTEGER NOT NULL
);

-- Dedupe keyed on delivery identity; rows without one never dedupe.
CREATE UNIQUE INDEX IF NOT EXISTS observations_dedupe
    ON observations (delivery_scope, delivery_host, delivery_source,
                     delivery_event_id)
    WHERE delivery_event_id IS NOT NULL;

-- Recorded host write intents; `pending`/`unknown` are outstanding
-- effects a terminal state must not silently drain.
CREATE TABLE IF NOT EXISTS host_action_intents (
    action_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    role TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    status TEXT NOT NULL,
    authenticated_actor TEXT,
    effects TEXT NOT NULL DEFAULT '[]',
    created_seq INTEGER NOT NULL
);

-- Immutable review rounds; a changed occurrence set installs a new row.
CREATE TABLE IF NOT EXISTS review_rounds (
    id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL REFERENCES works(id),
    target TEXT NOT NULL,
    source TEXT NOT NULL,
    source_revision TEXT,
    completeness TEXT NOT NULL,
    set_digest TEXT NOT NULL,
    generation INTEGER NOT NULL,
    seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS finding_occurrences (
    id TEXT PRIMARY KEY,
    round_id TEXT NOT NULL REFERENCES review_rounds(id),
    source_key TEXT NOT NULL,
    content_digest TEXT,
    severity TEXT,
    position TEXT,
    thread_id TEXT
);

-- One immutable exact-set judgment per sealed round.
CREATE TABLE IF NOT EXISTS round_judgments (
    id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    round_id TEXT NOT NULL UNIQUE REFERENCES review_rounds(id),
    set_digest TEXT NOT NULL,
    decisions TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS stop_intents (
    invocation_id TEXT PRIMARY KEY,
    identities TEXT NOT NULL,
    recorded_seq INTEGER NOT NULL
);

-- Durable artifact handoff (worker-protocol): export_started → bundle →
-- quarantine import → ref → acknowledged. `state` drives crash recovery;
-- `attempt` counts recovery attempts against the artifact-recovery bound.
CREATE TABLE IF NOT EXISTS artifact_handoffs (
    artifact_id TEXT PRIMARY KEY,
    invocation_id TEXT NOT NULL REFERENCES invocations(id),
    request_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    base TEXT NOT NULL,
    produced TEXT,
    manifest_digest TEXT,
    state TEXT NOT NULL,
    invalid_reason TEXT,
    attempt INTEGER NOT NULL,
    recorded_seq INTEGER NOT NULL
);

-- One bounded result per declared verification argv on a handed-off
-- artifact; each argv runs on an independent clean worktree of the
-- exact produced commit.
CREATE TABLE IF NOT EXISTS verification_results (
    id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES artifact_handoffs(artifact_id),
    requirement TEXT NOT NULL,
    argv TEXT NOT NULL,
    produced TEXT NOT NULL,
    environment_digest TEXT NOT NULL,
    result TEXT NOT NULL,
    output TEXT NOT NULL,
    recorded_seq INTEGER NOT NULL,
    UNIQUE (artifact_id, requirement)
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

-- Immutable failure/recovery decision records.
CREATE TABLE IF NOT EXISTS recovery_decisions (
    id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    execution_id TEXT,
    classification TEXT NOT NULL,
    signature TEXT,
    prerequisites TEXT NOT NULL,
    detail TEXT NOT NULL,
    usage_after TEXT NOT NULL,
    seq INTEGER NOT NULL
);

-- Single-flight run lease: at most one loop driver per installation.
-- The recorded owner identity (pid + opaque start-time verifier) lets a
-- contender re-inspect the holder: a live owner is never displaced, a
-- provably gone owner's lease is reclaimed, and an unreadable inspection
-- fails closed. Ordinary Commands and Queries never touch this row.
CREATE TABLE IF NOT EXISTS run_lease (
    lease_key TEXT PRIMARY KEY,
    owner_pid INTEGER NOT NULL,
    owner_started_at TEXT NOT NULL,
    owner_nonce TEXT NOT NULL,
    acquired_unix_seconds INTEGER NOT NULL
);

-- Durable recovery intents: (pending | consumed | retired), scheduled
-- in the deciding transaction and fired exactly once by the loop's
-- recovery-scheduling phase. `generation` pins retirement to the
-- evidence generation that invalidated the intent.
CREATE TABLE IF NOT EXISTS recovery_intents (
    intent_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    execution_id TEXT,
    kind TEXT NOT NULL,
    due_at_unix_seconds INTEGER NOT NULL,
    generation INTEGER NOT NULL DEFAULT 1,
    evidence_basis TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'consumed', 'retired')),
    retirement_reason TEXT,
    scheduled_seq INTEGER NOT NULL
);

-- Managed body catalog: durable metadata rows only; the body itself is
-- the file at `path`. `reclaiming` -> `reclaimed` is the two-phase
-- deletion protocol; `eligible_after_unix_seconds` is NULL while a body
-- is unattributed and is never sweep-eligible. `pin_state` is the
-- catalogue's explicit protective-dependency summary; any value other
-- than `none` is a pin. `registered_unix_seconds` is the body's
-- `created_at` for age eligibility.
CREATE TABLE IF NOT EXISTS body_catalog (
    body_id TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    registered_unix_seconds INTEGER NOT NULL,
    eligible_after_unix_seconds INTEGER,
    pinned INTEGER NOT NULL DEFAULT 0,
    pin_state TEXT NOT NULL DEFAULT 'none'
        CHECK (pin_state IN
               ('none', 'active', 'paused', 'recovering', 'unknown-effect')),
    state TEXT NOT NULL DEFAULT 'present'
        CHECK (state IN ('present', 'reclaiming', 'reclaimed')),
    reclaim_reason TEXT,
    delete_failures INTEGER NOT NULL DEFAULT 0,
    reclaimed_unix_seconds INTEGER
);

-- Protective-dependency pin rows: one row per live dependency, so a
-- body stays pinned while ANY row exists and discharging one
-- dependency deletes only its own row. `body_catalog.pinned` is the
-- materialized EXISTS of this table, updated in the same transaction
-- as every ledger write.
CREATE TABLE IF NOT EXISTS body_pins (
    body_id TEXT NOT NULL REFERENCES body_catalog(body_id),
    dependency TEXT NOT NULL,
    PRIMARY KEY (body_id, dependency)
);

-- The durable WorkerRequest export for one Invocation (U4.2): written
-- before any process is launched; `state` flips to 'bound' in the same
-- commit that records the running Invocation's identity. An 'exported'
-- row whose Invocation never launched is the crash window
-- reconciliation tolerates — never a stop proof, never a release.
CREATE TABLE IF NOT EXISTS worker_request_exports (
    invocation_id TEXT PRIMARY KEY REFERENCES invocations(id),
    request_id TEXT NOT NULL UNIQUE,
    profile_id TEXT NOT NULL,
    workspace_root TEXT NOT NULL,
    digest TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'exported'
        CHECK (state IN ('exported', 'bound')),
    process_identity TEXT
);
