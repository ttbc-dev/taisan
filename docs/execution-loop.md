# Execution loop, CLI contract, and skill workflow

Status: owner-approved target design from the #55 review. This document changes no runtime behavior, installs no skill, and claims no live evidence. The [installation configuration](installation-config.md) owns schema and credentials; [recovery policy](recovery-policy.md) owns limits and classification; [worker protocol](worker-protocol.md) owns Invocation/artifact semantics; [retention policy](retention-policy.md) owns body reclamation; [evidence gates](evidence-gates.md) owns assessment. This document specifies how the CLI drives them: the loop, the wire contract, recovery scheduling, and the agent-facing skill surface.

## Owner decisions

| Decision | Choice | Rejected / deferred |
| --- | --- | --- |
| Loop command | `taisan run` drives the installation loop; `taisan run --once` runs one pass; `--work <id>` scopes a pass. One verb, one code path (Laravel `queue:work --once` precedent) | Separate `tick` verb — jargon, drift risk between two code paths |
| Output contract | Always-JSON envelope on stdout for every result including errors; human diagnostics on stderr | `--json` flag (two contracts to keep honest); per-subcommand formats |
| Exit codes | `0` ok / `1` domain rejected / `2` usage / `3` config / `4` store-internal / `5` source unavailable / `6` unknown | sysexits 64+; 0/1/2 minimalism that cannot express unknown |
| Skill packaging | Agent-agnostic `skills/taisan/SKILL.md` following the cross-agent skill convention; installable via `npx skills add`; direct CLI remains the fallback | Per-agent install recipes; Claude-Code-only placement |
| Console/projection/MCP requirements (~25) | Re-modelled: mechanisms stay deferred/absent; retained obligations (read-only deterministic projection, fail-closed, closed error vocabulary, untrusted-input sanitization) move onto the CLI Query contract. TUI is not built; MCP stays optional | Building any console/projection service |
| Loop defaults | interval 30 s; tick deadline 120 s; per-source refresh timeout 30 s; outbox ≤ 8 effects/tick; reconciliation ≤ 16 items/tick; dispatch bounded by capacity only | Hourly scheduler cadence — wrong scale for one installation |
| Recovery scheduling | `recovery_intents` table: durable `(due_at, generation, kind, evidence_basis, state)` persisted in the same transaction as the deciding decision | In-memory timers or config-derived requeues that cannot survive restart |
| No-progress | Attention item `no_progress` at `recovery.no_progress_seconds` (1800 s); auto-clears on material progress; never a stop or writer replacement | Any automatic stop from absent progress — absence of progress never proves death |
| Attention delivery | Query only. At-least-once means "remains visible and unacked until acknowledged", not push delivery | Notice ledger, push channels, MCP delivery |

## Loop model

`taisan run` is a foreground process that repeats one bounded pass over the installation's active Work until SIGINT/SIGTERM. It is not a daemon contract — no systemd unit, cron entry, or supervision tree is specified; unattended scheduling is out of scope. `--once` executes exactly one pass and exits; `--work <id>` restricts the pass to one Work for diagnosis. `run --interval <s>` overrides `loop.interval_seconds` for that process.

- **Run lock.** At most one loop holds the installation's run lease (a store-locked row in the existing capacity machinery) at a time; a second `run` fails closed with `lock.held`. Ordinary Commands and Queries never take the run lock — they work against the store directly.
- **Crash safety.** Every durable effect is a store transaction; a crash mid-pass leaves no half-applied phase. Restart re-enters through phase order and reconciles recorded identities — a pass is resumable, never resumed blindly.
- **Shutdown.** SIGINT/SIGTERM finishes the current phase boundary and exits. Worker processes are independent of the loop: shutdown never kills, suspends, or invalidates a live Invocation; reconciliation on the next pass re-attaches by recorded process identity.
- **Per-Work isolation.** A failure processing one Work inside a phase is recorded against that Work and the phase continues for others. A phase-level infrastructure failure (store unreachable, adapter constructor failure) aborts the pass and reports the failure — the next tick retries from phase order.
- **Tick deadline.** A pass exceeding `loop.tick_deadline_seconds` (120 s) stops starting new work in the current phase, persists progress, and reports `tick.deadline` per unfinished item — remaining items are handled next pass, never silently dropped.

### Tick phases, in order

| # | Phase | Bounded by | Driver |
| --- | --- | --- | --- |
| 1 | Source refresh: read declared sources for active Works (issue/PR head, named checks), ingest deduplicated Observations | per-source `loop.source_refresh_timeout_seconds` (30 s) | host adapter (read only) |
| 2 | Dispatch admission: `listQueuedExecutions` in enqueue order → `work` rechecks version/eligibility/capacity/head/ownership/recovery budget → atomic `queued→ready` + capacity reserve + InvocationRequest → `execution` persists Invocation and launches via supervisor | capacity; no extra per-tick cap | Core judges; supervisor launches |
| 3 | In-flight supervision: poll recorded process identities, collect exits, read WorkerReports, run mechanical verification against the handed-off commit, ingest results; execute pending `stop_intents`; detect escaped/zombie descendants | per-Invocation worker bounds | supervisor |
| 4 | Host outbox: apply pending `pr_action_intents` (push/PR create/comment) and reconcile unknown effects by read-back against original binding | ≤ 8 items/tick | host adapter (write) |
| 5 | Timed sweep: evaluate per-Invocation bounds, `recovery.automatic_time_limit_seconds`, `recovery.effect_reconcile_seconds` windows, and no-progress detection → Core records stop intents, deadline transitions, or Attention; re-assess `waiting` Executions whose recorded evidence may have completed without a triggering observation | clock-derived | Core |
| 6 | Recovery scheduling: consume `recovery_intents` with `due_at ≤ now` (requeue after revalidation); suppress equivalent failures; retire obsolete intents | ≤ 16 items/tick | Core |
| 7 | Retention sweep: periodic pass per [retention policy](retention-policy.md) (3600 s cadence, ≤ 256 bodies / 1 GiB) | retention limits | runtime tooling |
| 8 | Attention derivation: recompute the current Attention set (uncertainty duration, unanswered judgments, escalation counters, `protected_over_limit`) | derived | Core |

Completion assessment is level-triggered, not only edge-triggered on observation ingest: phase 5 re-runs the completion assessment for every `waiting` Execution that survives the protective bound checks, inside that Execution's own transaction. Re-assessment re-derives every gate — current diff target, latest check conclusions, sealed review rounds and judgments, invariant assessment plus its completed check Invocation, Invocation/artifact state, and applied stage intents — from durable records; it cannot succeed on stale, partial, or superseded-target evidence, and it never acts on cached or claimed state. Protective bounds keep precedence: an Execution the sweep stops or fails is never re-assessed in the same pass. Only the loop invokes it — `run`, or a scoped `--work` diagnostic pass limited to that Work; no Command triggers re-assessment. The cost bound is one assessment read-set per nonterminal `waiting` Execution per pass.

Commands are loop-independent: `submit`, `retry`, `resume`, `cancel`, `judge-findings`, `release-judgment`, `acknowledge`, `check`, and `retention` transact directly against the store and succeed whether or not `run` is live. The loop only progresses already-accepted Work. If the loop is down, Work holds in its recorded state and resumes progress on the next pass — a stopped loop is backpressure, not failure.

## Intake scheduling

Status: normative contract for #211 (v0.2). This section specifies the selector intake scheduler hosted by `taisan run`, its configuration reload, the one-shot intake CLI surface, and the installation-wide pending-queue admission policy. It changes no runtime behavior by itself — the scheduler and CLI land in later v0.2 units — but the admission rule it names is enforced inside the submit transaction: every `submit`, manual or selector-originated, passes the same capacity check described under [queue admission](#queue-admission).

### Scheduler model

`taisan run` hosts one intake scheduler next to the eight-phase execution loop. The two cadences are independent:

- The execution loop repeats bounded passes on `loop.interval_seconds` (30 s default).
- The intake scheduler evaluates intake state on a fixed 60-second settings tick — separate from the pass cadence, never blocking a pass, and never itself a pass phase.

The scheduler is internal to `run`: there is no separate intake daemon, cron entry, or second run lock. Single-flight is durable, not a process-local flag: an intake invocation holds the installation intake lease and at most one `intake_runs` row may be `open`/`admitting` at a time ([intake identity and storage](intake-identity-storage.md)).

On every settings tick the scheduler:

1. Re-reads and re-validates `config.toml` through the versioned profile loader.
2. Computes the intake settings revision — a digest of the complete validated `[intake]` content.
3. Applies the reload transition table below.
4. Evaluates whether intake is due and, when due and no run is active, records an `open` intake run and launches the configured selector argv. The invocation is nonblocking: the pass continues, the adapter runs under its configured timeout, and the run's candidates admit through the normal submit path across later ticks.

A due time that arrives while a run is active is skipped, not queued: there is no catch-up burst, no replacement fetch inside the same run, and no invocation retry. After a launch or a skip, `next_due` advances by whole intervals until it lies in the future, preserving the interval phase across downtime and restarts.

### Reload transitions

| Event | Effect on scheduling and admission |
| --- | --- |
| Process start with intake enabled | Immediately due — the startup transition |
| Disabled → enabled on a settings tick | Immediately due |
| Enabled → disabled | The pending due is cancelled; an in-flight run is unaffected and completes normally |
| `intake.interval_minutes` changed | `next_due` resets to the reload tick plus the new interval |
| Selector binding changed (argv, adapter id, selector version, policy revision) | `next_due` preserved — adapter identity never moves the schedule |
| Intake content otherwise unchanged | Identical revision; `next_due` untouched — identical content never resets the schedule |
| Settings tick finds the config invalid | New intake is suppressed for that tick only, the error is reported, the last valid settings stay active for admission, and the next tick retries the reload; existing Work is never stopped |

Restart recovery is derived, not stored: on start the scheduler reads the durable intake records — a live `open`/`admitting` run resumes through the lease and owner-evidence path, and with no active run plus intake enabled the startup rule makes intake immediately due. `next_due` itself is process-local; the schedule re-anchors deterministically from the startup rule rather than replaying missed slots.

An accepted Work's saved profile, repository/credential binding, usage budgets, and unresolved effects are immutable across reload: only Work admitted after a settings revision takes effect binds the new revision, and `intake_runs.settings_revision` records that provenance. The pending-queue limit is the deliberate exception that proves the rule — it is an installation-wide admission policy read from the active settings record inside every submit transaction, so a lowered limit applies to the next submit without touching existing Work.

### Queue admission

The pending-queue limit is installation-wide and enforced inside the authoritative submit transaction — the same `handle_command` path manual `submit` and per-candidate selector submits share. It is not advisory: the read-only preflight may report free positions, but only the transaction decides.

- The pending count is the number of distinct Works whose active Execution is `queued`. Executing Work — `ready`, `running`, `waiting`, `stopping`, `uncertain` — and `paused` Work are excluded; a paused Work holds no queue position, though its issue claims are retained through the pause exactly as [identity storage](identity-storage.md) requires.
- A grouped proposal consumes exactly one position: one candidate produces one Work with one queued Execution.
- At `pending == limit` a submit is rejected `capacity_exhausted` with the occupancy evidence, and no partial Work, Execution, claims, usage, or spec rows commit — and no decision is saved under the caller's key either: the refusal is transient, so an identical resubmit is decided fresh against then-current capacity. For an intake candidate the coordinator maps this to `capacity_deferred` — deferrable, never a saved rejection ([intake identity and storage](intake-identity-storage.md)).
- The limit applies to manual `submit` and Selector intake alike. `retry` and `resume` are recovery of already-accepted Work and are never gated — a full queue cannot strand them.
- Replaying a decided submit returns the stored decision even when the queue has since filled; the idempotency lookup precedes the capacity check.
- Lowering the limit below current occupancy refuses new submissions only; existing Work drains naturally and is never cancelled.
- With no active settings record the limit resolves to the documented default of 10 Work — the same constant the profile loader defaults to — and every caller observes the single active revision; stale callers can neither supply nor overwrite it.

### One-shot intake CLI

| Command | Kind | Contract |
| --- | --- | --- |
| `taisan intake` | driver | Runs one intake invocation: acquires the intake lease, launches the configured argv once, validates the response, admits candidates through the normal submit path, and reports the recorded run outcome |
| `taisan intake-runs` | query | Lists durable intake runs newest-first (bounded) with trigger, state, detail code, and candidate counts |
| `taisan intake-detail --run <id>` | query | One run plus its candidates in ordinal order: status, bounded reason, and command/work decision links |

`intake` shares the scheduler's single-flight lease: while a scheduled run is active the verb is refused `intake_active` — status `rejected`, exit 1 — and never launches a second adapter or waits indefinitely. Automatic intake being disabled does not disable the verb; it requires only a configured selector binding. Per-candidate outcomes are data inside the run record, mirroring the `propose` batch envelope: each candidate reports `accepted`, `replayed`, `rejected`, `capacity_deferred`, or `unknown` with its replay key so `command-result` can recover any decision. A lost response resolves through the recorded run and the command ledger — `unknown` at exit 6 when the fate cannot be determined, never a fresh run identity.

`run --once` performs exactly one pass of the whole driver — one settings-tick evaluation plus the eight execution phases — and may launch a due intake without waiting for it, since the invocation is nonblocking. `run --work <id>` is a diagnostic scope: it restricts the execution phases to one Work and does not evaluate intake scheduling. SIGINT/SIGTERM ends the scheduler and finishes the current phase boundary; an in-flight intake invocation is not killed by the loop — its durable run record and lease ownership carry it through restart recovery exactly like worker invocations.

## CLI contract

### Envelope

Every command prints exactly one JSON object on stdout. `schema_version` is a string (`"1"`) required in every envelope; a consumer must refuse a newer version rather than parse optimistically.

```json
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd_01J8EXAMPLE",
    "kind": "submit",
    "actor": "github:you",
    "idempotency_key": "cli-abc123"
  },
  "work_id": "w_01J8...",
  "execution_id": "e_01J8...",
  "result": { "queue_position": 1 },
  "reason": null,
  "evidence_refs": []
}
```

`status` ∈ `accepted | rejected | unknown | error`:

- `accepted` — the Command or Query completed and its durable result is recorded. Acceptance is never Work completion.
- `rejected` — a domain refusal (`idempotency_conflict`, `version_conflict`, `resource_conflict`, `policy_limit`, `invariant`, `unsupported`, `not_found` for the requested identity). The envelope carries `reason.code`, a bounded message, and retained evidence references; the rejection itself is recorded.
- `unknown` — the mutation's fate cannot be determined (lost response, uncertain stop). Recovery is `command-result` by the original `(origin, actor, idempotency_key)` or an identical replay. An unknown result never justifies a fresh key or a rewritten payload. A batch mutation (`propose`) reports `unknown` under the interrupted proposal's own submit identity, not the batch run's query identity.
- `error` — input/usage, configuration, store, or source failure. Nothing was recorded as accepted; the class is in `reason.code`. A multi-proposal batch is the documented exception to the second sentence: `propose` is not atomic, so an `error` can end a run after earlier proposals committed. There, `error` means the *batch run* stopped and nothing was recorded for the interrupted proposal — the envelope's `evidence_refs` (bounded) name the recorded decisions' replay keys, and re-running the batch replays them instead of admitting them again. A consumer must never read a partial batch's `error` as "no decision exists"; `unknown` stays reserved for a fate that cannot be determined either way.

Queries use the same envelope with `status` ∈ `accepted | error` (`unknown` cannot occur — queries mutate nothing); a not-found item is `accepted` with `result: null`, distinct from an unreadable store (`error` / exit 4) or unavailable source (`error` / exit 5).

### Exit codes

| code | class | Examples |
| --- | --- | --- |
| 0 | ok | accepted Command; any completed Query including not-found |
| 1 | rejected | `idempotency_conflict`, `version_conflict`, `resource_conflict`, `invariant`, `policy_limit` |
| 2 | usage | unknown subcommand/flag, unparseable argument (clap default) |
| 3 | config | absent/malformed/refused config per [installation config](installation-config.md) |
| 4 | store | SQLite errors, schema/user_version refusal, internal invariant violation |
| 5 | unavailable | declared source unreachable where the operation requires it |
| 6 | unknown | mutation fate undetermined; `reason` names the unresolved identity |

Two combinations sit across the table's boundaries deliberately: a `source_unavailable` **rejection** keeps `status: "rejected"` (the refusal is recorded) but exits `5` — the failure class wins over the rejection class so a caller can distinguish an unavailable declared source from a domain refusal without scraping prose. A Query refused by policy (`Forbidden`) is `status: "error"` with `reason.code: forbidden` at exit `1` — a refusal, distinct from an unreadable store.

The `run` driver's envelope is its terminal result: a bounded run (`--ticks N`) emits exactly one envelope when it finishes — kind `run`, carrying the final work-list. Per-pass progress is a stderr diagnostic line. An unbounded run produces no result until it ends, so stdout carries nothing until then; a consumer must not read it as a stream of per-tick envelopes.

Process exit zero alone never proves a Command was accepted or Work completed — the structured `status` and command identity are authoritative. Diagnostics never corrupt stdout: stderr carries human prose only.

### Identities and preconditions for mutation

Before mutation the caller retains `(origin, actor, idempotency_key)`, the exact payload, and displayed preconditions (`expected_work_version`, `expected_execution_id`, `expected_head`, Attention version). Core enforces all preconditions at acceptance; a precondition failure is `rejected` with the current recorded value, never silently retargeted. Recovery of a lost response is `command-result --key <idempotency_key>` or byte-identical replay; a caller must not mint a new key or edit the payload to recover.

## Command surface

| Command | Kind | Loop dependency |
| --- | --- | --- |
| `submit` | mutation | none — queues durably |
| `retry`, `resume`, `cancel`, `acknowledge`, `judge-findings`, `release-judgment` | mutation | none — Core judges eligibility; the loop executes consequences |
| `init`, `check`, `check --probe` | setup | none — per [installation config](installation-config.md) |
| `work-list`, `work-detail`, `attention`, `capacity`, `command-result`, `retention report` | query | none |
| `retention cleanup` | mutation | none — bounded two-phase pass per retention policy |
| `run`, `run --once`, `run --work <id>` | driver | the loop itself, plus the intake scheduler's settings tick |
| `intake` | driver | one invocation under the scheduler's single-flight lease |
| `intake-runs`, `intake-detail` | query | none — durable intake records |

`cancel` acceptance records a stop intent; it is never stop proof — the Execution settles through the uncertain-stop path until writer safety is established. `acknowledge` applies to the current Attention version only and releases nothing.

## Failure-scenario matrix

Each row names trigger/detection evidence, the durable transition, ownership treatment, the permitted automatic action and its bounds, restart behavior, Attention escalation, and the planned fault-injection proof. Requirement IDs reference [loop requirements](loop-requirements.md).

| # | Scenario | Detection evidence | Durable transition and ownership | Automatic action and bounds | Restart behavior | Attention | Proof |
| --- | --- | --- | --- | --- | --- | --- | --- |
| F1 | Duplicate or manual dispatch | Same `request_id` or nonterminal Execution exists; `UNIQUE` constraint | Rejected; original queue position and InvocationRequest retained | None — replay returns recorded decision | Same on restart; no re-derivation | none | LP-10: concurrent and replayed dispatch yield one Invocation (R163, R167, R10) |
| F2 | Lost launch response | InvocationRequest persisted, no process identity | `unknown` process evidence; Execution stays owned | `execution` inspects recorded identity before any restart; ≤ worker start retries | Re-entry reconciles the recorded request — never a second writer | `process_uncertain` after threshold | LP-11: crash after request persist / after spawn; no duplicate writer (R11, R82, R170) |
| F3 | Uncertain stop | Stop intent issued; contradictory or absent inspection | `stop_intents` unresolved; slot and reservations retained | Descendant snapshot inspection; escalate after `recovery.effect_reconcile_seconds` | Unresolved intent survives restart; inspection resumes | `stop_uncertain` | LP-12: zombie, escaped child, PID reuse (R15, R95, R178) |
| F4 | Orphan/escaped writer | Recorded parent exited; descendant snapshot shows survivor | Ownership retained; no replacement | Kill/reap owned descendants by recorded identity; unknown survivor ⇒ hold | Survivor state re-derived from recorded snapshot, never assumed dead | `writer_unproven` | LP-13 (R15, R82, R95) |
| F5 | Missing/corrupt artifact | Handoff unacknowledged, bundle invalid, digest mismatch | `artifact_handoffs` stays non-`acknowledged`; Invocation result `invalid` | Bounded artifact recovery within worker bound; then failure classification | Unacknowledged state pins the out dir (retention) | on bound exhaustion | LP-14: corrupt bundle, deleted out dir (R17, R189, R255) |
| F6 | Publish-before-collect loss | `pr_action_intents` pending; response lost | Intent retained with original actor/payload/binding | Read-back reconcile against recorded intent within `effect_reconcile_seconds`; unknown stays unknown — never blind replay | Reconciliation resumes; intent generation unchanged | `effect_unknown` at window expiry | LP-15: lost response after write may have occurred (R30, R82, R119, R316) |
| F7 | Partial publication | Push receipt recorded, PR-create lost | Partial receipts preserved; unresolved intent retained | Reconcile the failed effect only; no combined-success claim | Same | same as F6 | LP-15 (R30, R316) |
| F8 | Interrupted finding disposition | `judge_findings` response lost, or same-head finding growth | Command result unknown / `version`/`coverage` conflict | `command-result` recovery or re-inspection; never silent rebind | Same | none until retried | LP-16: finding added between inspect and submit (R27, R124–R126) |
| F9 | Stale head | Observation shows head ≠ assessed head | Old-head evidence retained as history; current head unchanged | Fresh read → new assessment under current head; budgets unchanged | Same | none | LP-17 (R82, R165, R172) |
| F10 | Unavailable source | Source refresh fails per-source timeout | Work holds; `unavailable` ≠ empty findings | No dispatch/publication dependent on the source; retry next tick within refresh bounds | Same | `source_unavailable` after configured threshold | LP-18 (R36, R231, R234) |
| F11 | Stalled execution | No material progress for `recovery.no_progress_seconds` | Progress clock unchanged; state unchanged | Emit `no_progress` Attention only — never stop | Clock is durable; survives restart | `no_progress` | LP-19: fake liveness (log volume, new commit) does not reset (R39, R100) |
| F12 | Repeated equivalent failure | Same `(stage, check id, category, normalized diagnostics)` under Work identity | `recovery_usage_entries` accumulate; at `recovery.equivalent_failure_limit` automatic retries hold | Requeue below limit with persisted backoff; at limit → human handoff | Counters durable; restart cannot re-arm | `equivalent_failure` | LP-20: identical then non-equivalent failures (R35, R37, R119, R137) |
| F13 | Terminal pending intents | Terminal remote/local evidence invalidates a pending retry/requeue | `recovery_intents` → `retired` with reason + evidence generation, idempotent | Retirement is durable; restart cannot resurrect | Retired intent never fires after restart | none | LP-21: invalidate between scheduling and firing (R43, R87, R179) |
| F14 | Prolonged uncertainty | Uncertainty duration exceeds configured Attention threshold | Uncertainty retained; ownership held | Attention escalation; no automatic settlement | Duration is durable | `prolonged_uncertain` | LP-22 (R36, R100, R210) |
| F15 | Deadline / budget exhaustion | `automatic_time_limit_seconds` or Invocation bound reached | `policy_limit` decision; stop intent issued | Stop recorded writers → reconcile unknown effects → bounded requeue only if budget remains; else human handoff with retained WIP | Budget ledger durable; no reset across Executions | on exhaustion | LP-23: exhaustion mid-phase, crash before/after stop (R40, R119, R164, R238) |

A transition to `failed` with a manual retry command does not satisfy automatic recovery: rows F12/F13/F15 require the persisted `recovery_intents` path below.

## Bounded automatic recovery

### Progress detection

Observable material progress per Work: a new accepted artifact, a previously failing required check becoming valid and successful, or an unknown effect acquiring a verified result — the [recovery policy](recovery-policy.md#failure-and-progress-classification) definition applied by the loop. The persisted progress timestamp is the last material-progress commit for the Work; it rearms `recovery.no_progress_seconds` and nothing else. Missing or stale progress signal behavior: an absent timestamp (never-progressed Work) uses the first dispatch commit time; a corrupted signal is `unknown`, surfaced, and never treated as progress or as death.

At threshold the loop emits an Attention item `no_progress` carrying Work, Execution, last progress timestamp, and elapsed. It clears automatically when material progress is recorded. Absence of progress never proves death, never authorizes a stop, and never authorizes a replacement writer. Cross-task doctor diagnosis is deferred; its Phase-1 input is the bounded read-only `anomalies` census ([module contracts](module-contracts.md#type-sketch)), which inventories every stuck lane state from durable records without mutating anything.

### Timeout recovery sequence

On a deadline/budget limit the sequence is ordered and each step is durable:

1. **Stop intent**: `stop_intents` row with descendant snapshot, recorded before signaling.
2. **Proven stop**: supervisor inspection confirms every recorded writer identity stopped. Uncertain stop retains ownership — see F3/F4.
3. **Effect reconciliation**: unresolved host effects reconcile against original intent/binding within `effect_reconcile_seconds`; expiry requests human action.
4. **Requeue decision**: only if recovery budget remains and the failure is not permanent — a `recovery_intents` row `(kind=requeue, due_at=now+backoff, generation, evidence_basis)` in the same transaction as the decision.
5. **Human handoff**: permanent refusal (`provider_refused`), exhausted budget, or `equivalent_failure_limit` → `failed`/Attention with retained WIP, receipts, and stop evidence. A `retry` Command remains available but is never an automatic path.

Same versus new Execution identity: a requeue re-enters the *same paused/failed-eligible* Execution through the queue (same `execution_id`, retained budgets and WIP claims); an operator `retry` Command creates a *new* Execution under the immutable specification. Neither path resets consumed budgets.

### Persisted scheduling and retirement

`recovery_intents(intent_id, work_id, execution_id, kind, due_at, generation, evidence_basis, state, retirement_reason)` with `state` ∈ `pending | consumed | retired`. Scheduling writes the row in the deciding transaction — a crash before scheduling leaves no intent (safe: next evaluation re-derives it), a crash after scheduling fires exactly once (`consumed` before dispatch). Concurrent manual `retry`/`resume` version-conflicts or consumes the same intent idempotently. Terminal evidence (merged PR, deleted branch, terminal Work) retires the intent with `retirement_reason` and the evidence generation; retirement is idempotent and a restart cannot resurrect an obsolete intent or reset its bounds.

Equivalent-failure suppression: the failure classifier `(stage, stable_check_id, error_category, normalized_diagnostics)` from recovery policy. A requeue whose new failure records an identical classifier key under the same Work increments the equivalent counter; at the limit, automatic requeues hold and escalate. A suppressed intent is recorded as suppressed — it is not silently consumed.

## Skill workflow

Deliverable: `skills/taisan/SKILL.md` — one agent-agnostic skill file following the cross-agent skill convention (YAML frontmatter `name`/`description`, then instructions), installable via `npx skills add shimpeiws/taisan`. It names no agent-specific paths; placement follows each agent's own convention. Direct CLI use is always available.

The skill collects intent/issues/constraints/verification, invokes `taisan`, and explains the recorded result. It never maintains a queue, launches workers, invents recovery policy, or rewrites preconditions to make a rejection pass. Conversation closure is not cancellation: recorded Work and execution ownership remain inspectable after the agent session ends.

Every [agent-interface](agent-interface.md#workflow-and-authority) row maps onto the contract above: `submit` → accepted-means-queued; `work-list`/`work-detail`/`attention`/`capacity` → recorded state with unavailable sources visible; `retry`/`resume`/`cancel`/`acknowledge` → observed-version interventions with eligibility rejection surfaced; `judge-findings`/`release-judgment` → exact head/finding-set coverage, no conversational approval; `command-result`/replay → unknown-result recovery without fresh keys.

Eligible vs ineligible interventions are a Core judgment surfaced through exit code 1 + `reason.code`; the skill translates them faithfully. S1 mapping: each [S1 row](v0.1.md#acceptance-scenarios) that an operator performs is invocable through either direct CLI or the skill with identical semantics.

## Evidence classes

For every record the loop touches: **absent** (required evidence missing — hold, never infer), **valid** (exact schema + identity + revision + criterion match — the only class that satisfies), **invalid** (malformed/stale/mismatched — retained with reason, never counted), **unavailable** (body reclaimed or source unreadable — surfaced distinctly from absent). A lost-response Command is `unknown`, not `invalid`. No evidence class permits silent success, silent rewrite, or fresh-key retry.

## Planned verification suite

Named contract tests; all are future tests, no live claim:

| Case | Covers |
| --- | --- |
| LP-01 | Envelope literal: accepted/rejected/error/unknown for each Command class; schema_version refusal |
| LP-02 | Exit code vocabulary 0–6 against staged faults (parse, config, store, source, unknown) |
| LP-03 | `run` lock: second loop refused; `run --once` equivalent to one pass; `--work` scoping |
| LP-04 | Tick ordering: phase 1→8; per-Work isolation; phase-level abort; tick deadline continuation |
| LP-05 | Shutdown: SIGINT mid-phase completes boundary; workers survive; restart reconciles |
| LP-06 | Lost-response recovery: `command-result` and identical replay; fresh key rejected as new Command |
| LP-07 | Queries: not-found vs unreadable store vs unavailable source are distinct statuses/exits |
| LP-08 | Skill mapping: every agent-interface row through both direct CLI and skill-equivalent invocation |
| LP-09 | Conversation closure: session end leaves Work inspectable; no inferred cancellation |
| LP-10..LP-23 | Failure matrix F1–F15 fault injection |
| LP-24 | No-progress: material progress definition, warning at threshold, auto-clear, never-stop |
| LP-25 | `recovery_intents` lifecycle: persisted schedule, crash before/after, concurrent manual retry, versioned retirement, no resurrection |
| LP-26 | Equivalent-failure suppression: classifier key matching, limit hold, suppressed intent recording |
| LP-27 | Timeout recovery sequence ordering: intent → proven stop → reconcile → requeue/handoff |
| LP-28 | Sanitization: untrusted fields in Observations/envelopes stripped; closed error vocabulary |

## Six design aspects

| Aspect | Application |
| --- | --- |
| State transitions | Tick phases drive recorded transitions only; `queued→ready` + capacity + request is one transaction; recovery intents have a closed pending/consumed/retired lifecycle |
| Failure semantics | Every failure row specifies detection evidence, permitted action, bounds, and escalation; `unknown` is a first-class outcome, never collapsed into success or empty |
| Trust boundaries | CLI drives I/O/timing; Core judges supplied facts; supervisor/host adapters execute effects; skills carry no execution authority; untrusted Observation text is sanitized at ingest |
| Identity invariants | Work/Execution/Invocation/request/intent/Command identities are durable; requeue keeps the Execution, retry mints a new one, intent generation prevents resurrection |
| Information preservation | Original binding, receipts, WIP, failure classifiers, and retirement reasons are retained; `unknown` and `unavailable` remain distinguishable |
| Resource bounds | Per-tick phase bounds, tick deadline, capacity-derived dispatch, persisted backoff, equivalent-failure limits, and the Work-wide time budget compose; none replenish another |

Open questions: the concrete jitter policy for `recovery.retry.backoff_seconds` (scheduled in the [installation config](installation-config.md) reference); whether a `--phase` filter joins `--work` on `run` (defaults to no — phases are not independently safe to skip).

## Implementation-plan mapping

| Unit | Coverage |
| --- | --- |
| U5 CLI execution-loop composition | Loop model, tick phases, failure matrix, bounded recovery, persisted scheduling — this document's loop and recovery sections |
| U7 CLI results, skills, walkthrough | Envelope/exit contract, Command surface, skill workflow, agent-interface mapping |
| U1 Core/store contracts | `recovery_intents` joins the target table list; the loop supplies facts, Core decides |
| U2/U3/U4 | Phase 1/4 drive host adapters per [installation config](installation-config.md); phase 2/3 drive supervisor/Worker per [worker protocol](worker-protocol.md) |

This document authorizes no implementation; the [delivery units](implementation-plan.md#dependent-delivery-units) and issue #56's gate remain authoritative.
