# Development-orchestration cutover plan

Status: planning document for #75. This document organizes the migration
backlog from the existing installation to taisan: it maps each tracked
responsibility to the delivery unit whose acceptance evidence covers it,
specifies the ordered cutover and rollback runbooks, and records the
disposition of each retained post-v0.1 candidate. **It authorizes
nothing.** Per-unit implementation start, production cutover, and live
tests each require the explicit owner authorization recorded in
[design closure](design-closure.md#implementation-start-gate). It claims
no implemented migration and no live verification.

## Scope and boundaries

- Reuse the approved delivery-unit contracts. U1 (#66) owns Work
  identity, deduplication, finding/decision binding, and artifact
  integrity; U2 (#67) owns authentication, permissions, configuration,
  and retention; U3 (#68, reusing #17) owns remote effects and
  repository/PR/head binding; U4 (#69) owns Worker start evidence and
  artifact handoff; U5 (#70) owns retry/resume, capacity, terminal
  reconciliation, and Attention; U6 (#71) owns admission; U7 (#72) owns
  CLI/skills; U8 (#73) owns live acceptance. The migration does not
  recreate the legacy card database, CLI verbs, or filesystem layout.
- Do not migrate legacy records into executable new-format records.
  Unfinished legacy work is drained with verified stop evidence, or an
  explicit handoff decision is recorded while preserving artifacts; the
  new installation then starts with a new-format store and new Work.
  Legacy history is retained for read-only reference.
- Independent review discovery, assessment evidence, and
  execution-adapter defects remain with their providers. Nothing here
  introduces competing authority over taisan's final Outcome, repair
  dispatch, or capacity.
- Dispatcher, hook, and CLI defects specific to the legacy platform
  remain compatibility fixes for that installation. Their
  implementations are not ported into taisan.
- The existing per-unit implementation-start and live-test gates are
  preserved. This plan does not create implementation issues, change
  any unit's scope, or declare migration complete from fixture results.

## Responsibility and acceptance-evidence map

Each tracked responsibility resolves to existing acceptance evidence or
to a recorded deferral. No requirement in #75 was found uncovered; no
unit body needed revision and no duplicate implementation issue is
proposed.

| #75 responsibility | Owning unit | Acceptance evidence that covers it |
| --- | --- | --- |
| Work identity, deduplication, finding/decision binding, artifact integrity | U1 (#66) | "same-head growth, missing/wrong effect facts, invalid/corrupt artifacts ... identity collisions/rename"; #24 finding coverage |
| Authentication, permissions, configuration, retention | U2 (#67) | "denied/missing/expired credentials visible; no secrets in argv/log/export; worker cannot use inherited GitHub writes"; retention pins for active/paused/recovering/unknown-effect bodies |
| Remote effects, repository/PR/head binding | U3 (#68, reusing #17) | "separate guarded fast-forward push, PR create/update, comments, reviews, replies/thread actions and ready effects"; lost response, duplicate events, stale head; "unknown never authorizes blind replay" |
| Worker start evidence, artifact handoff | U4 (#69) | "duplicate/lost start, unknown liveness, stop proof, missing/invalid artifacts, stale revision and denied writes"; "no running-Invocation migration" |
| Retry/resume, capacity, terminal reconciliation, Attention | U5 (#70) | "durable bounded automatic recovery across Executions, stop/reconcile/requeue, persisted backoff and terminal retry-intent retirement"; restart at ready/spawn/stop/effect boundaries |
| Admission | U6 (#71) | "same snapshot/policy yields same identity; duplicates and stale sources handled; unauthorized sources refused" |
| CLI / skills | U7 (#72) | agent-interface matrix through real CLI and installed skill; "malformed/config/store errors cannot appear accepted" |
| Live acceptance | U8 (#73) | S1 success/Attention/recovery variants "with exact revisions/config, ... remote PR/head/check/review receipts and stop evidence"; "never merge/deploy as part of acceptance" |
| No executable migration of legacy records; read-only archive and fresh store | U1 (#66) | "archive/new-store cutover, new-format record integrity"; owner-approved archive/new-store decision in [identity-storage](identity-storage.md) |
| Drain/handoff procedure, archive operation, enablement order | This document | [Cutover runbook](#cutover-runbook); operational procedure, not unit scope |
| Provider independence for review discovery, assessment evidence, adapter defects | U1 (#66), U4 (#69), U5 (#70) | [Authority separation](#authority-separation); "Workers never publish or drive outer loops"; immutable failure/recovery decisions and Outcome evidence references in Core |
| Legacy-platform dispatcher/hook/CLI defects | Out of scope | Compatibility fixes for that installation; nothing is ported |

## Cutover runbook

The runbook orders the cutover so that at no point can both systems
accept Work, and at no point does a replacement writer start while a
legacy writer is unproven. Steps execute in order; a step is not
complete until its recorded evidence exists.

### Preconditions

- Every unit required for the cutover scope has delivered its
  acceptance evidence, and the owner has authorized the live-test gate
  for the cutover scenario. Until then this runbook is specified, not
  executable.
- The operator records the cutover inventory ledger: one durable,
  human-readable record capturing every decision and its evidence. The
  ledger lives outside both systems' executable state.

### Step 1 — Stop legacy admission

Disable every entry point that can admit new work in the existing
installation: the ops repo's scheduler intake and dispatch lanes, the
milestone/enqueue commands, hooks that auto-submit, and
selector-driven intake. Manual submissions stop as well. Evidence: each
entry point is recorded disabled in the ledger, and a probe submission
is not admitted. This is an operation on the legacy installation, not a
code change in taisan.

### Step 2 — Inventory writers and remote effects

Enumerate and record in the ledger, before touching anything:

- Runs in non-terminal states, including parked or queued items.
- Active or unknown writers: any worker process whose liveness or stop
  is unproven. Unknown is never treated as stopped.
- Unresolved remote effects: open pull requests produced by workers,
  in-flight comment/review/thread effects, pending retries or backoff,
  and any remote write whose result is unknown.
- Write-capable resources held: managed workspaces, managed branches,
  and PR heads owned by legacy work.

The output is one ledger row per in-flight item naming its identity,
state, writers, remote effects, and the disposition chosen in step 3.

### Step 3 — Drain or record an explicit handoff

For each inventoried item, choose exactly one disposition and record
its evidence:

- **Drain**: let the legacy item run to a terminal state under the
  legacy system's own completion rules.
- **Verified stop**: stop the item; the ledger retains stop evidence —
  the writer stopped or is proven unable to write — and every remote
  effect is reconciled to a terminal receipt or recorded unknown.
- **Explicit handoff**: a human records that the item is abandoned in
  the legacy installation and will be re-expressed as new Work in
  taisan. Artifacts (workspace, branch, PR reference, evidence) are
  preserved and referenced by the new Work's intent. Handoff never
  converts a legacy record, replay key, or executable state into the
  new-format store; it is a re-submission decision, not a data
  migration.

Guard: an issue still owned by an unfinished legacy item must not be
submitted to taisan until that item is terminal or its handoff decision
releases ownership. This is the duplicate-Work guard for the cutover.

### Step 4 — Retain the read-only archive

Freeze the legacy card database, run history, state store — including
the unversioned legacy `workloop.db` — output directories, and evidence
bodies as read-only reference. Record the archive location and cutoff
point in the ledger. Per the owner-approved decision in
[identity-storage](identity-storage.md), the new installation
initializes a fresh new-format store with a new StoreId; legacy history
is preserved, never replayed.

### Step 5 — Verify new-system startup

After live-test authorization, verify:

- The new-format store initializes with a new StoreId.
- A dedicated probe Work — one item chosen not to overlap any
  inventoried legacy item — submitted through the new admission path is
  accepted, queued, and dispatchable. The probe exercises admission in
  verification scope only; general operator admission stays off until
  step 6.
- The legacy archive is readable and not writable.
- Artifacts awaiting handoff are preserved and untouched; each handed-
  off item is referenced by the new Work's intent, never by a legacy
  key or record.
- No duplicate writer exists: no legacy writer holds or can write to a
  `(repository, branch)` resource that a new-format Work may reserve;
  the new store's reservations start clean.

These checks are specified now and executed only under the live-test
gate; fixture results do not satisfy them.

### Step 6 — Enable new admission

With startup verification recorded, enable general admission in the
new installation: the `taisan run` execution loop, direct CLI
submission, and the installed agent skill per
[execution-loop](execution-loop.md) and
[agent-interface](agent-interface.md). Legacy admission stays off
permanently; the legacy installation remains only for read-only
reference and compatibility fixes.

## Recovery-behavior verification map

Issue #75 requires retry budgets, durable retry intents, backoff,
terminal intent retirement, capacity retention under unknown liveness,
and preservation of explicit holds to be verified through U5/U8
evidence. The mapping is:

| Required behavior | Governing contract | Evidence that covers it |
| --- | --- | --- |
| Retry budgets | [recovery-policy](recovery-policy.md) configurable defaults (`recovery.max_fix_invocations`, `recovery.automatic_time_limit_seconds`, per-Execution bounds) | U5: "durable bounded automatic recovery across Executions"; execution-loop LP-20–LP-27 fault injection; U8 recovery variant |
| Durable retry intents | [execution-loop](execution-loop.md) `recovery_intents` `(pending/consumed/retired)` persisted in the deciding transaction | U5 "persisted backoff and terminal retry-intent retirement"; LP-25 lifecycle rows |
| Backoff | `recovery.transport.backoff_seconds` with persisted schedule | U5 persisted backoff across restart; LP-20 equivalent-failure requeue |
| Terminal intent retirement | Terminal evidence retires the intent with reason + generation; restart cannot resurrect | U5 terminal retry-intent retirement; F13/LP-21 invalidation between scheduling and firing |
| Capacity retention under unknown liveness | [recovery-policy](recovery-policy.md) human-waiting/capacity matrix: unproven writer retains slot and reservations | U4 stop proof; U5 safe reconciliation; slice-0 `uncertain`-holds-capacity contract rows |
| Preservation of explicit holds | Paused Work retains write-resource reservations while releasing the compute slot | v0.1 acceptance row "Paused Work keeps WIP"; U1 reservations; U5 requeue-without-reset |

## Post-v0.1 candidate dispositions

Each retained candidate's disposition is recorded below. Every
candidate is deferred for v0.1; adoption requires its own scope,
acceptance criteria, and owner authorization. None becomes an implicit
v0.1 requirement.

| Candidate | Disposition | Existing record |
| --- | --- | --- |
| Fair automatic selection across repositories; suppression of exhausted work | Deferred | [design-closure](design-closure.md) deferrals: "unattended scheduling" and "rich Selector grouping/triage"; the audit omits scheduler auto-intake; U6 stays a thin single-issue proposal |
| Reassessment after issue changes | Deferred | [identity-storage](identity-storage.md): the dispatch-time authoritative read and same-Work specification revisions are v0.1; the configured auto mode that regenerates invariants is explicitly not required for v0.1 |
| Partial policy-based finding judgments | Deferred | [evidence-requirements](evidence-requirements.md): "policy-based autonomous judgment remains outside the reference profile"; operator exact-set judgment carries authority in v0.1 |
| Cross-task anomaly diagnosis | Deferred | [design-closure](design-closure.md) deferral "cross-task doctor diagnosis"; U5 scope states "cross-task doctor audits excluded" |
| Proposal-assisted TUI | Deferred | #47 TUI deferral; [design-closure](design-closure.md) deferral registry "TUI delivery" |
| MCP | Deferred | [v0.1](v0.1.md): "MCP is optional future work"; [design-closure](design-closure.md) deferral registry |

## Authority separation

Independent review discovery, assessment evidence, and
execution-adapter defects remain with their providers; they do not
acquire authority over taisan's final Outcome, repair dispatch, or
capacity.

- **Independent review discovery** produces findings and head-bound
  Observations. A review source's verdict is evidence input; it cannot
  dispose of a finding episode or mark a head reviewed for a different
  head. Operator `judge_findings`/`release_judgment` — or a separately
  authorized declared policy — carries judgment authority.
- **Assessment evidence** from a Worker is a claim. Per
  [worker-protocol](worker-protocol.md) and the audit's retained
  boundary, a worker report is evidence, never an Outcome; Workers
  never publish or drive outer loops (U4).
- **Execution-adapter defects** are repaired in the adapter that owns
  the behavior — worker runtime, host adapter, supervisor — and are
  verified through that adapter's shared contract suite. Core owns the
  immutable failure/recovery decision recorded from that evidence.
- **Core authority** (U1) is single: it records the Outcome, decides
  repair dispatch within durable budgets, and accounts for capacity.
  U5 wires the loop; U4/U3 supply process and remote evidence.

The required evidence handoff is verified through the units: U1
normalized evidence, required stage effect set, and per-finding
disposition; U4 durable request/artifact export and exact commit/check
binding; U5 loop composition and unknown-effect reconciliation.

## Rollback runbook

If a cutover fails, returning to the legacy installation is a distinct
procedure with its own evidence — it is not "keep running both" and it
is not implied by pausing the new system.

1. **Stop new-system writers.** Cancel or drain in-flight Executions.
   Required evidence: verified stop proof (or write-prevention proof)
   for every writer the new installation started — the same
   supervisor-stop contract U4/U5 enforce. An unproven writer blocks
   rollback; it is never assumed dead.
2. **Reconcile state.** Every outstanding host effect reaches a
   terminal receipt or is recorded unknown; capacity and reservations
   are reconciled; Attention is current, not silently cleared.
3. **Stop the new loop.** Halt `taisan run` and any scheduled
   invocation so no new admission or dispatch can occur.
4. **Re-enable legacy admission.** Restore the entry points disabled
   in cutover step 1.
5. **Preserve the new system's state as its own read-only archive.**
   New-format records are not converted back into legacy cards. Work
   that was in flight under taisan follows the same drain/handoff
   procedure in reverse: an explicit human decision, preserved
   artifacts, and no record conversion.

Starting taisan alone never authorizes legacy retirement. The legacy
installation remains operable — and its read-only archive retained —
until the owner records an explicit retirement decision with stop
evidence for any remaining legacy writers.

## Acceptance-criteria coverage

| #75 acceptance criterion | Where satisfied |
| --- | --- |
| Map acceptance evidence to U1–U8; integrate uncovered requirements | [Responsibility and acceptance-evidence map](#responsibility-and-acceptance-evidence-map); no uncovered requirement found, so no unit body changed |
| Cutover runbook in the required order | [Cutover runbook](#cutover-runbook) steps 1–6 |
| Verify startup: new-format store, new Work, read-only legacy, preserved handoff artifacts | [Step 5](#step-5--verify-new-system-startup); execution gated on live-test authorization |
| Verify retry budgets, durable intents, backoff, terminal retirement, capacity under unknown liveness, explicit holds via U5/U8 | [Recovery-behavior verification map](#recovery-behavior-verification-map) |
| Record adoption/deferral/rejection per post-v0.1 candidate | [Post-v0.1 candidate dispositions](#post-v01-candidate-dispositions) |
| Separate provider responsibilities from Core's final authority; evidence handoff via U1/U4/U5 | [Authority separation](#authority-separation) |
| Document rollback with verified stop evidence and reconciliation first | [Rollback runbook](#rollback-runbook) |
