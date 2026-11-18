-- workloop v9 store: current-state tables enforcing structural
-- invariants, plus an append-only event_log carrying EventSequence.
-- `PRAGMA user_version = 9` marks the format; a stamped older file with
-- the complete recognized schema upgrades additively at open, while an
-- unversioned slice-0 database and a newer version are refused.
-- v7 added work_usage.last_progress_basis; v8 added the intake run and
-- candidate tables specified in docs/intake-identity-storage.md; v9
-- added the intake_settings admission record (#213).

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
    human_wait_seconds_total INTEGER NOT NULL DEFAULT 0,
    last_progress_basis TEXT
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
    -- The Worker adapter binding captured before the first launch;
    -- null until the supervisor selects the adapter for this request.
    adapter_binding TEXT,
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
-- evidence generation that invalidated the intent; the retirement
-- records that generation and may never predate the row's own.
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
    retirement_generation INTEGER,
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

-- Two-phase bounded reclaim plans for explicit `retention cleanup`.
-- Phase 1 computes the eligible set and persists it here (bounded by
-- max_bodies/max_bytes); phase 2 executes it. An interrupted phase 2
-- resumes from the pending entries; already-reclaimed bodies are
-- skipped. The plan and every entry's outcome are durable metadata
-- that survives the bodies they name.
CREATE TABLE IF NOT EXISTS reclaim_plans (
    plan_id TEXT PRIMARY KEY,
    created_unix_seconds INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'completed')),
    body_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    completed_unix_seconds INTEGER
);

-- One row per planned body. `reason` is the eligibility reason that
-- authorized planning; `outcome` is NULL while pending, then one of
-- 'reclaimed' | 'unavailable' | 'held' | 'delete_failed'.
CREATE TABLE IF NOT EXISTS reclaim_plan_entries (
    plan_id TEXT NOT NULL REFERENCES reclaim_plans(plan_id),
    body_id TEXT NOT NULL REFERENCES body_catalog(body_id),
    category TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    reason TEXT NOT NULL,
    outcome TEXT
        CHECK (outcome IS NULL OR outcome IN
               ('reclaimed', 'unavailable', 'held', 'delete_failed')),
    outcome_reason TEXT,
    outcome_unix_seconds INTEGER,
    PRIMARY KEY (plan_id, body_id)
);

-- Durable intake records (docs/intake-identity-storage.md). One row
-- per invocation attempt — including attempts that never launch the
-- adapter (`skipped`) or fail before a validated response (`failed`).
-- Identity and ordering live in columns; the request bytes and digest
-- keep the full exclusion snapshot auditable after the adapter is gone.
CREATE TABLE IF NOT EXISTS intake_runs (
    run_id TEXT PRIMARY KEY,
    trigger TEXT NOT NULL CHECK (trigger IN
        ('manual', 'scheduled_startup', 'scheduled_interval',
         'scheduled_reenable')),
    selector_id TEXT NOT NULL,
    selector_version INTEGER NOT NULL CHECK (selector_version > 0),
    policy_revision INTEGER NOT NULL CHECK (policy_revision >= 0),
    settings_revision TEXT,
    state TEXT NOT NULL CHECK (state IN
        ('open', 'admitting', 'completed', 'failed', 'interrupted',
         'skipped')),
    detail_code TEXT,
    request_payload BLOB NOT NULL,
    request_digest TEXT NOT NULL,
    owner_pid INTEGER NOT NULL,
    owner_started_at TEXT NOT NULL,
    owner_nonce TEXT NOT NULL,
    created_unix_seconds INTEGER NOT NULL,
    finished_unix_seconds INTEGER,
    recorded_seq INTEGER NOT NULL
);

-- At most one live invocation per installation, structurally
-- enforced in addition to the single-flight intake lease.
CREATE UNIQUE INDEX IF NOT EXISTS one_active_intake_run
    ON intake_runs ((CASE WHEN state IN ('open', 'admitting') THEN 1 END));

-- One row per validated candidate in the adapter's returned order.
-- Identity fields are immutable after insert; status and the
-- command/Work decision links are the only mutable surface. A
-- capacity_deferred/unknown/pending/interrupted row never carries a
-- command or Work link.
CREATE TABLE IF NOT EXISTS intake_candidates (
    run_id TEXT NOT NULL REFERENCES intake_runs(run_id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    proposal_key TEXT NOT NULL,
    replay_key TEXT NOT NULL,
    payload BLOB NOT NULL,
    payload_digest TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN
        ('pending', 'accepted', 'replayed', 'rejected',
         'capacity_deferred', 'unknown', 'interrupted')),
    status_reason TEXT,
    command_id TEXT REFERENCES commands(command_id),
    work_id TEXT REFERENCES works(id),
    committed_seq INTEGER,
    recorded_seq INTEGER NOT NULL,
    PRIMARY KEY (run_id, ordinal),
    UNIQUE (run_id, proposal_key)
);
CREATE INDEX IF NOT EXISTS intake_candidates_proposal
    ON intake_candidates (proposal_key);
CREATE INDEX IF NOT EXISTS intake_candidates_replay
    ON intake_candidates (replay_key);
CREATE INDEX IF NOT EXISTS intake_candidates_command
    ON intake_candidates (command_id) WHERE command_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS intake_candidates_unresolved
    ON intake_candidates (status) WHERE status IN ('pending', 'unknown');

-- The active installation intake-settings record (#213): at most one
-- row, written only through the revision-guarded SetIntakeSettings
-- effect so a stale reload writer can never overwrite a newer active
-- limit. `settings_revision` is the profile loader's content digest —
-- opaque provenance; `pending_queue_limit` is the admission bound
-- every submit transaction enforces. An absent row resolves to the
-- documented default.
CREATE TABLE IF NOT EXISTS intake_settings (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    settings_revision TEXT NOT NULL CHECK (length(settings_revision) > 0),
    pending_queue_limit INTEGER NOT NULL CHECK (pending_queue_limit > 0),
    recorded_seq INTEGER NOT NULL
);
