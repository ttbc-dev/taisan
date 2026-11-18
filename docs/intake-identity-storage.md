# Intake identity, persistence, and restart recovery

Status: normative design for #206, implemented together with the #207 store changes as store format v8 against runtime revision `a175376`; the coordinator (#203), recovery (#208), CLI verbs and projections (#209–#210), scheduler (#215), `run` integration (#204), bounded exchange (#201), and the packaged reference Selector (#214) are implemented against this contract. This document specifies the durable records, states, and recovery rules for v0.2 intake. The [selector protocol](selector-protocol.md) (#200) remains authoritative for the wire contract, and [identity and admission](identity-storage.md) remains authoritative for command replay, overlap, and Work identity. Evidence distinctions follow the same rule as everywhere else: fixture results and owner-run live results are never conflated.

## Usage and authority

An intake invocation is one bounded operation that asks the configured Selector for proposals and admits each validated candidate through the ordinary `submit` Command path. The invocation is owned by exactly one process at a time — the `taisan run` driver for scheduled intake or a one-shot CLI process for manual intake — under an installation-wide single-flight lease. The owner writes durable intake records around the invocation so that a crash at any commit boundary leaves enough evidence to reconcile without duplicate admission.

The Selector never writes intake records and holds no store handle. The coordinator — the only writer — records what it requested, what the adapter returned after whole-response validation, and what each ordinary submit decided. Manual `submit` is unaffected: it uses the same command ledger but creates no intake run.

## Identities

| Identity | Form | Equality and lifetime |
| --- | --- | --- |
| Intake run | `run_id`, an install-scoped opaque ID allocated like other store IDs | One per invocation attempt, including attempts that never launch the adapter (`skipped`) or fail before a validated response (`failed`). Never reused |
| Candidate | `(run_id, ordinal)` plus `proposal_key` | `ordinal` is the candidate's position in the validated response, preserved forever. `proposal_key` is the proposal-v1 semantic digest from the selector protocol; it is globally unique per semantic proposal because the selector ID is inside the digested payload |
| Submit-command identity | `replay_key` = `proposal/<selector_id>:<proposal_key>` under `(origin=selector, actor=installation actor)` | Identical to the existing proposal replay rule: the same semantic proposal always maps to the same command-ledger key, across ticks, response loss, candidate reordering, restart, and configuration reload |
| Work identity | `work_id` on the accepted decision | Set only from a durable `CommandDecision`; a replayed decision links the original Work, never a new one |

Trigger vocabulary is closed: `manual` (one-shot CLI), `scheduled_startup` (automatic intake enabled at driver start), `scheduled_interval` (a periodic due tick), `scheduled_reenable` (disabled→enabled transition invoking immediately). #211 maps scheduler events onto these values; adding a trigger is a format change, not a config change.

`selector_id`, `selector_version`, and `policy_revision` record the adapter identity triple from the protocol request. `settings_revision` is an opaque caller-supplied revision of the installation intake settings (#212 owns its content); it is audit provenance only and never participates in proposal or replay identity. Runs recorded before settings exist carry `NULL`.

## Record shapes

SQL columns carry identity, ordering, joins, and state; versioned payloads carry the request and proposal bytes. This mirrors the existing command ledger: canonical bytes plus a digest, never a re-interpretation.

```sql
CREATE TABLE intake_runs (
    run_id TEXT PRIMARY KEY,
    trigger TEXT NOT NULL CHECK (trigger IN
        ('manual', 'scheduled_startup', 'scheduled_interval', 'scheduled_reenable')),
    selector_id TEXT NOT NULL,
    selector_version INTEGER NOT NULL CHECK (selector_version > 0),
    policy_revision INTEGER NOT NULL CHECK (policy_revision >= 0),
    settings_revision TEXT,
    state TEXT NOT NULL CHECK (state IN
        ('open', 'admitting', 'completed', 'failed', 'interrupted', 'skipped')),
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

-- At most one live invocation per installation, structurally enforced
-- in addition to the single-flight lease.
CREATE UNIQUE INDEX one_active_intake_run
    ON intake_runs ((CASE WHEN state IN ('open', 'admitting') THEN 1 END));

CREATE TABLE intake_candidates (
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
CREATE INDEX intake_candidates_proposal ON intake_candidates (proposal_key);
CREATE INDEX intake_candidates_replay ON intake_candidates (replay_key);
CREATE INDEX intake_candidates_command
    ON intake_candidates (command_id) WHERE command_id IS NOT NULL;
CREATE INDEX intake_candidates_unresolved
    ON intake_candidates (status) WHERE status IN ('pending', 'unknown');
```

`request_payload` is the canonical JSON of everything the coordinator asked for: trigger context, adapter triple, settings revision, repositories, the complete exclusion snapshot, `max_candidates`, task defaults, verification argv, required permissions, and the deadline. `request_digest` is SHA-256 over those bytes with an explicit version tag. `payload`/`payload_digest` on a candidate hold the canonical proposal-v1 semantic object whose digest is `proposal_key`; storing it lets recovery verify identity without re-running selection and lets audit read what was proposed after the adapter is gone.

`owner_pid`/`owner_started_at`/`owner_nonce` copy the lease owner identity under which the run was created — provenance for "who held this invocation", distinct from the lease row that enforces exclusivity. `recorded_seq` is the event-log position at insert; `committed_seq` on a candidate is the deciding Command's `committed_at`, anchoring the link into ledger order.

## States and transitions

Run states:

| From | To | Meaning |
| --- | --- | --- |
| — | `open` | Invocation owned; adapter call in flight or response being validated. Zero candidates may exist |
| — | `skipped` | Recorded without adapter launch (e.g. preflight found zero free pending positions). Terminal at insert |
| `open` | `admitting` | A whole-response-validated candidate batch (possibly empty) committed atomically with this transition |
| `open` | `failed` | Invocation-level failure before a validated batch: transport, deadline, contract violation, adapter failure. `detail_code` carries the bounded protocol/launcher code |
| `open`, `admitting` | `interrupted` | The owning invocation cannot continue: recovery proved the owner gone, or the live owner itself aborted — a propagating store fault (`store_error`) or admission halted with candidates still unresolved (`unresolved_candidates`). Candidates stay for ledger reconciliation |
| `admitting` | `completed` | Every candidate reached a resolved terminal status (`accepted`, `replayed`, `rejected`, or `capacity_deferred`) — an `interrupted` candidate keeps the run from completing |
| terminal | — | `completed`, `failed`, `interrupted`, `skipped` never regress |

Every transition into a terminal run state records `finished_unix_seconds`; `detail_code` carries the bounded diagnostic for `failed`, `interrupted`, and `skipped`. `open` and `admitting` runs keep both NULL — a live invocation has no finish time and no failure code, and a run recovered to `completed` carries none either. The store enforces both halves: an insert or transition carrying an inconsistent field combination conflicts rather than being normalized, and `admitting` → `completed` additionally requires every linked candidate to have reached a terminal status.

The `detail_code` vocabulary is closed and bounded — never payload text. Skipped preflight refusals use `queue_full`, `repositories_bound` (the repository list is empty or beyond the wire's 1–64 bound), and `exclusions_overflow`. A failed invocation records one launcher code: `exchange_launch`, `exchange_exit`, `exchange_deadline`, `exchange_stdout_overflow`, `exchange_stderr_overflow`, or `exchange_unknown` for the bounded exchange; `malformed_response` or `contract_<rule>` when the response fails whole-response validation; `adapter_<code>` when the adapter itself returned a failure response. Interrupted runs use `owner_lost` / `no_conforming_response` from the recovery sweep and `store_error` / `unresolved_candidates` from a live owner.

An empty successful scan is a `completed` run with zero candidates — "the selector ran and found nothing" is durable evidence, distinct from `skipped` (never ran) and `failed` (ran, failed).

Candidate statuses:

| Status | Meaning | Links |
| --- | --- | --- |
| `pending` | Validated, not yet decided in this run | none |
| `accepted` | This run's submit freshly accepted; new Work created | `command_id`, `work_id`, `committed_seq` |
| `replayed` | Submit returned the saved decision for this replay identity — the proposal was admitted by an earlier run or path | `command_id`, `work_id` (when the saved decision accepted), `committed_seq` of the original decision |
| `rejected` | Ordinary final rejection (overlap, ineligible, invalid, unsupported capability, …) with `status_reason` = rejection code | `command_id`, `committed_seq`; `work_id` stays NULL — a rejection never creates Work |
| `capacity_deferred` | Admission was refused for capacity before any decision was recorded; deferrable, not final | none — see below |
| `unknown` | A submit's outcome is undetermined (store fault, lost response); reconcile via the saved command identity | none until reconciled |
| `interrupted` | The run ended before this candidate's outcome was resolved — never attempted, or attempted with an undetermined outcome that reconciliation proved had no saved decision | none |

`capacity_deferred` is terminal *as a stored status*: the candidate row never transitions again, and run completion counts it among the terminal outcomes. "Not final" refers only to the command ledger — no decision was saved under the replay key, so a later run re-proposing the same semantic proposal submits fresh. Deferral is never retried by mutating this row. When capacity is found exhausted — at preflight or inside a submit transaction — the refused candidate *and every candidate after it in the original order* is marked `capacity_deferred`: the unattempted remainder is deferred with it, never left `pending`.

Allowed transitions: `pending` → `accepted`, `replayed`, `rejected`, `capacity_deferred`, `interrupted`, or `unknown`; `unknown` → `accepted`, `replayed`, `rejected`, or `interrupted` (reconciliation either finds the saved decision and fixes the terminal status, or proves none exists and interrupts the candidate — it never returns a candidate to `pending`, because an interrupted run has no admission pass left to re-drive it). `accepted`, `replayed`, `rejected`, `capacity_deferred`, and `interrupted` are the terminal candidate statuses — none has an outgoing transition. Every transition is guarded by the expected prior status inside the store transaction; a stale writer conflicts instead of regressing a row.

The Links column is enforced by the store, not just documented: unresolved statuses (`pending`, `capacity_deferred`, `unknown`, `interrupted`) must keep `command_id`/`work_id`/`committed_seq` all NULL; `rejected` requires `command_id` and `committed_seq` and forbids `work_id`; `accepted` requires all three; `replayed` requires `command_id` and `committed_seq` with `work_id` present only when the saved decision created Work. The store also verifies the linked ledger row belongs to the candidate's replay identity and agrees with the status: the command must be saved under that `replay_key` or under `<replay_key>#<digest>` where the suffix is exactly that command's stored payload digest (a key that merely shares the prefix is not this identity), `accepted` must reference an accepted decision whose Work is the linked `work_id` and carries no reason, `rejected` must reference a rejected decision, `replayed` follows the same accepted/rejected shape, and a rejection-backed `rejected` or `replayed` row's `status_reason` must equal that decision's rejection code. Every decided row's `committed_seq` must equal that decision's `committed_at` — an arbitrary sequence, a borrowed decision, a reason unrelated to the linked rejection, or a status contradicting its decision is a conflict, not a faithful record.

Insert rules mirror the tables: a run is recorded only as `open` or `skipped`; a candidate batch is recorded by the single `AdmitIntakeBatch` effect — all-`pending`, link-free, each candidate naming the batch's run — which persists the batch and moves the run `open` → `admitting` atomically. No effect can append candidates to a run that already left `open`, and no effect can insert a candidate in a decided status or with decision links — those are transition-table violations, not inserts.

## Commit boundaries and crash matrix

The invocation has five durable boundaries. Each names what is recorded, what may be lost, and the recovery action.

| # | Boundary | Durable state after commit | Crash before commit | Crash after commit |
| --- | --- | --- | --- | --- |
| B0 | Lease acquisition, then run insert (`open`) | Lease row + `open` run with owner identity | Lease held by dead owner → reclaimed via process evidence; no run → nothing to recover | Invocation proceeds normally. A `skipped` record (preflight found zero free pending positions) is written under the same held lease — it is an invocation attempt like any other |
| B1 | Selector output persisted: candidate batch + run → `admitting`, one transaction | All validated candidates `pending`, original order, run `admitting` | Run stays `open`, zero candidates → recovery marks `interrupted` (`no_conforming_response`); no candidates are invented | Admission proceeds per candidate |
| B2 | Per-candidate submit commit (inside `handle_command`) | Command decision in `commands` under the replay key; Work/claims if accepted | Candidate still `pending`; ledger may or may not hold the decision → recovery consults `stored_decision` by replay key before any resend | Candidate update (B3) may not have landed |
| B3 | Candidate status/link update | Candidate records the decision outcome and links | As B2 — the candidate row is the only pending evidence; the ledger is authoritative | Next candidate proceeds |
| B4 | Run → `completed` | Run terminal; all candidates resolved (none `interrupted`) | Run stays `admitting` with all resolved candidates → recovery closes it `completed` | Done |

Rules that fall out of the matrix:

- **Committed admissions survive later failure.** Once B2 commits, the Work and its decision are durable regardless of what happens to the run row or the adapter.
- **At most one Work per request identity.** Every candidate submit travels the ordinary command path under its deterministic replay key; replay returns the saved decision instead of creating Work. This holds across ticks, response loss, order changes, and restart because the key never depends on run-local state.
- **A recovery cursor never acknowledges or discards an uncommitted candidate.** There is no adapter cursor in this design: candidate rows are the recovery state. The adapter's `provenance.snapshot_cursor` remains audit metadata and is never used to skip or confirm admission.
- **Unknown outcomes reconcile by saved command identity.** A candidate in `unknown` is resolved by looking up the command ledger under its `replay_key` (origin `selector`, installation actor): a found decision fixes the candidate's terminal status and links; no decision interrupts the candidate — the run has no admission pass left to re-drive it, and `unknown` never transitions back to `pending`. Reconciliation never resends blindly and never treats "no row found" as proof of non-commit without the lookup.
- **Interruption before output persistence invents nothing.** If the owner dies while the run is `open`, recovery marks the run `interrupted` and leaves zero candidates; a fresh scheduled selection may run only after ownership is proven safe.

## Replay identity and finished sources

The same semantic proposal — same selector ID/version, policy revision, source set with revisions and digests, task fields, and required permissions — always produces the same `proposal_key`, hence the same `replay_key`, hence the saved decision. Request IDs, deadlines, candidate rank, fetch timestamps, cursors, display numbers, poll IDs, and mutable labels are excluded by the protocol and stay excluded here. Configuration reloads do not mint proposal identity: `settings_revision` is recorded on the run for audit but is not part of the proposal digest, and an unchanged selector/policy over an unchanged source yields the same key and the same replayed decision.

A `policy_revision` change is different: it is inside the proposal identity, so a semantic policy change deliberately produces new keys — that is the protocol's rule, not an intake invention.

A previously finished source is not special-cased. After safe terminal release the Issue claim is gone, so ordinary validation admits a new submission; a *new request identity* (changed source content producing a new proposal key, or an operator's fresh command key) goes through the normal submit path with no terminal-source approval gate. The intake layer keeps no finished-Issue blacklist and never rotates replay keys. Non-terminal source owners — `queued`, `ready`, `running`, `waiting`, `stopping`, `paused`, and `uncertain` — still exclude a new Work through the existing `issue_claims` overlap rule, which retains claims until safe terminal settlement — unchanged.

## Capacity deferral is not rejection

`capacity_deferred` records that admission was refused for capacity **before any command decision existed**. Two cases produce it:

1. Preflight: the pending-queue position count under the active settings revision was zero, so no submit was attempted for this or later candidates in the batch.
2. Mid-batch: the authoritative submit transaction found the queue full. The unattempted remainder is marked deferred.

A deferred candidate has `command_id`/`work_id`/`committed_seq` all NULL and — critically — **no saved decision under its replay key**. This is what makes deferral recoverable: a later run re-proposes the same semantic proposal, the replay lookup finds nothing, and the submit is decided fresh against then-current capacity. If capacity refusal were persisted as a `Rejected` decision, every future identical proposal would replay that refusal forever.

`rejected` is the opposite: a durable, final decision recorded in the command ledger and linked on the candidate. Ordinary rejections (source overlap, ineligible, invalid request, unsupported capability) are final for that proposal identity — replaying them returns the same rejection, which is correct because the refusal was about facts, not transient capacity.

The pending-queue limit itself, its atomic enforcement inside submit, and the preflight view are #213's scope; this document fixes only the record semantics that make deferral distinguishable from rejection. As implemented, the submit transaction answers a full queue with `CapacityExhausted` and persists no decision under the caller's key — the refusal is transient, so the replay lookup finds nothing and the next identical proposal is decided fresh against then-current capacity, for manual and selector submits alike.

## Recovery

Recovery (#208) consumes these records; this section fixes the contract it relies on.

- A run in `open` or `admitting` whose owner is proven gone is marked `interrupted` with a bounded recovery-origin `detail_code` (`owner_lost`, `no_conforming_response`). Unknown liveness retains exclusion — the run is not touched. Live-owner closes use the separate codes `store_error` / `unresolved_candidates` (next bullet).
- A live owner closes its own run `interrupted` rather than leaving it `admitting` — best-effort, not guaranteed: when admission halts with `pending`/`unknown` candidates, or a propagating store fault ends the invocation, the owner attempts the guarded close (`unresolved_candidates` / `store_error`) and runs the same ledger reconciliation sweep recovery uses — the next invocation is normally not wedged behind the active-run guard, and no candidate is silently acknowledged. If the store fault persists through the abort write itself, the run stays non-terminal until the next recovery sweep proves the owner state — a documented residual risk, never a silent acceptance. `admitting` therefore only ever belongs to a live in-flight invocation or a dead/abandoned owner awaiting the sweep.
- Candidates still `pending` or `unknown` under an interrupted run are reconciled against the command ledger by `replay_key` before any further admission: found decisions fix terminal status and links; absent decisions mark the candidate `interrupted` — never silently acknowledged as accepted. An interrupted candidate is terminal; a later run re-proposing the same semantic proposal creates a fresh candidate and submits fresh, because no decision exists under its replay key.
- A run left `admitting` whose candidates are all resolved (an `interrupted` candidate is terminal but not resolved, so it forces `interrupted`) is closed `completed`.
- A `skipped` or terminal run is never reopened; adapter swaps, configuration reloads, and restarts cannot rewrite run provenance or relabel a new candidate as a replay.

## Store upgrade, formats, and retention

Intake storage arrives as store format v8: two new tables and their indexes, no column changes to existing tables. Opening a v7 store installs the intake schema inside the same bootstrap transaction that stamps `user_version = 8`; a crash mid-upgrade rolls the whole transaction back and the next open retries idempotently (`CREATE … IF NOT EXISTS`). Existing Works, commands, claims, specs, and evidence are not read, rewritten, or re-keyed. A file stamped v8 that lacks the intake tables, a foreign stamped file, an unversioned slice-0 file, and a newer format are all refused at open exactly as before.

Store format v9 (#213) adds one more table and nothing else: the `intake_settings` singleton holding `settings_revision`, `pending_queue_limit`, and `recorded_seq`. Opening a v8 store installs it inside the same idempotent upgrade transaction and stamps `user_version = 9`; a v8 file lacking the v8 intake tables, a foreign stamped file, and a newer format are refused exactly as before. The record has exactly one writer — the revision-guarded settings effect: the first write requires an explicit absent-revision expectation, every later write must name the revision it observed, and a stale expectation is a constraint conflict, so a reload writer holding an older snapshot can never overwrite a newer active limit.

Store format v10 (#139) adds one nullable column, `recovery_intents.retirement_generation`, and no tables. Opening a v9 store adds it inside the same idempotent upgrade transaction and stamps `user_version = 10`; the column records the evidence generation a retirement was decided from, so a stale retirement decision cannot overwrite a newer intent's ownership.

Retention treats intake rows like the commands ledger: they are replay-critical identity and recovery evidence, never body-catalog entries, and no cleanup path deletes them. `request_payload` and candidate `payload` follow the same rule — they are retained because recovery and audit need them; if a future retention limit cannot preserve that, the tradeoff surfaces to the owner rather than silently deleting.

## Query results

Read-only queries (#210) project these records without mutating them. The shapes below are the storage contract; envelope/CLI mapping is #211's.

- **Run list** — bounded, newest or oldest first per the query contract. Each item: `run_id`, `trigger`, `selector_id`, `selector_version`, `policy_revision`, `settings_revision`, `state`, `detail_code`, `created_unix_seconds`, `finished_unix_seconds`, per-status candidate counts.
- **Run detail** — the run record plus every candidate in `ordinal` order: `ordinal`, `proposal_key`, `replay_key`, `status`, `status_reason`, `command_id`, `work_id`, `committed_seq`, and the candidate payload digest. Candidate order is the adapter's returned order, preserved by `ordinal`, never resorted.
- **Proposal lookup** — by `proposal_key`, returns every candidate row across runs (a re-proposed semantic proposal appears once per run that validated it), each with its run and decision links.
- **Command lookup** — by `command_id`, returns every candidate that linked the decision, oldest first (a replayed proposal links the same saved decision, so the original recorder leads); this recovers the original outcome after a lost response.
- **Status lookup** — candidates in `pending`/`unknown` across runs, the recovery sweep's work list.

Every result distinguishes `accepted`, `replayed`, `rejected`, `capacity_deferred`, `interrupted`, and `unknown` explicitly; a partial batch never reads as wholly accepted because per-candidate status is the unit of truth.

## Literal examples

Values are illustrative, not fetched identities or executed evidence.

| Situation | Required record |
| --- | --- |
| Manual one-shot, adapter returns 2 candidates, both accepted | run `manual`/`completed`; candidates ordinal 0,1 `accepted` with distinct `work_id`s and `command_id`s |
| Scheduled tick re-proposes an already-admitted semantic proposal | new run `scheduled_interval`; candidate `replayed` linking the original `command_id`/`work_id`; no second Work |
| Queue full at preflight | run `skipped`/`queue_full`, zero candidates, no adapter launch recorded |
| Exclusion set beyond the wire bound at preflight | run `skipped`/`exclusions_overflow`, zero candidates, no adapter launch recorded — exclusions are never truncated to fit |
| Repository list empty or beyond the 1–64 wire bound at preflight | run `skipped`/`repositories_bound`, zero candidates, no adapter launch recorded — the request is never built and scope is never broadened to fit |
| Store fault after the run record commits | run `interrupted` — `store_error` when the fault propagated out of the invocation, `unresolved_candidates` when a submit absorbed it and admission halted (best-effort either way); persisted candidates reconcile through the ledger — a live owner normally leaves no non-terminal run behind; a fault that also defeats the abort write leaves the close to the next recovery sweep |
| Adapter exits nonzero | run `failed`/`exchange_exit`, zero candidates |
| Owner dies after batch persisted, before first submit | run `admitting` + all candidates `pending` → recovery marks run `interrupted`, candidates reconciled via ledger |
| Submit commits, candidate update crashes | candidate `pending`, ledger holds decision → recovery links it `accepted`/`replayed`/`rejected` |
| Capacity refusal mid-batch | attempted candidates decided; remainder `capacity_deferred` with NULL links and no saved decisions |
| Same Issue re-proposed after its Work safely terminated, new content | new `proposal_key` → fresh submit → ordinary validation, no terminal-source gate |
