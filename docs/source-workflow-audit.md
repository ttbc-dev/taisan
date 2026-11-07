# Source Workflow Audit

This document audits the original personal development workflow that
workloop v0.1 distills into an open-source project (issue #33). It records
which existing operational behavior belongs in v0.1 — classified as
**retain**, **re-model**, or **omit** — and what evidence backs each
decision. It does not authorize implementation; it ends at scope definition
and implementation planning.

The four source repositories are private. Publication-facing documents
reference them by role, not by name (see `AGENTS.md`). This audit record
is the owner-controlled exception: it retains issue numbers, revision
SHAs, and source-internal module names as owner-verifiable evidence
locators so each mapping can be re-checked against the sources:

| Role name used here | What it is |
| --- | --- |
| **the ops repo** | the personal agent-operations repository hosting the Kanban extensions, the dispatch lanes, the scheduler, reconciliation and attention jobs, and the operator console |
| **the worker repo** | the autonomous coding worker for issue-driven development |
| **the selector repo** | the harness-agnostic issue-selection tool |
| **the infra repo** | the remote-execution infrastructure running bounded worker jobs in isolated containers |

These locators identify nothing without access to the sources.
`agent-workloop` is itself private today; if it is ever published, this
document needs a disclosure review first — and so does the git history,
which retains the identifiers removed from current files; publication
must either rewrite/export history or explicitly accept that exposure.
Publication-facing documents
carry only a minimal origin statement (see `docs/provenance.md`); the
name- and identifier-level detail removed there — source-system names,
private issue/PR metadata, the reference-verification table, and the
document-audit corrections list — is preserved in this repository's git
history.

## Inspected revisions and scope

Audit performed 2026-11-05. Inspection was source reads at the recorded
revisions plus authenticated tracker queries — no live host, no installed
copies on the host, and no running processes were inspected.

| Repository | Inspected revision | Scope inspected |
| --- | --- | --- |
| the ops repo | `6371cf99` (`origin/main`) | all orchestration scripts and libraries, design and operations docs, skills, installer scripts |
| the worker repo | `66daef53` (`origin/main`) | contract types, the devflow pipeline modules (ship/converge/propose/verify/resume/claim), the runtime adapter, checks runner, worker-contract and supervision docs |
| the selector repo | `f89c35bb` (`origin/main`) | the pure core (selection, grouping, scoring, policy, deps/graph), CLI and MCP entrypoints, tracker adapters, policy files, bindings |
| the infra repo | `3522605a` (`origin/main`) | the coordinator CLI (Rust), host/worker/incus scripts, execution and host docs, offline test suite |

Notes on inspection fidelity:

- The local checkout of the worker repo was 231 commits behind
  `origin/main`; all claims below were verified against `origin/main`
  via `git show`, not the stale working tree.
- The local checkouts of the selector and infra repos were not on `main`
  (one ahead of `origin/main`, one on a pinned lane branch); claims were
  likewise verified against `origin/main`.
- Symbol-level locators were collected during inspection. File paths,
  function names, and line numbers are not reproduced; module, command,
  and component names are retained as owner-verifiable locators under
  the audit-record exception in `AGENTS.md`.
- The ops repo's converge state-transitions design document is itself a
  design audit pinned to earlier revisions (`839094bb` ops / `e6c16686`
  worker); where this document cites it as evidence, the underlying
  claims were re-verified at this audit's own recorded revisions.
- Unverified/inaccessible areas: live host configuration and installed
  script state; messaging-facing runtime behavior; any actual production
  run output; whether merged fixes (#447) behave correctly in production.
  These are called out again in [Evidence classification](#evidence-classification).

Issue statuses cited below were re-queried on 2026-11-05 via `gh`; they
differ from the 2026-10-22/23-era statuses in earlier research notes
(notably #389 and #382 are now CLOSED, and #445/#446/#447 are new).

## Behavior map

Each table row is one operational behavior, with its trigger, the actor
that holds decision authority, what state it persists, which external
facts it depends on, its failure/recovery path, and source evidence.
The classification that follows each table assigns retain / re-model /
omit with rationale, the destination workloop responsibility or invariant,
and verification gaps. "taisan responsibility" references the contracts
in `docs/lifecycle.md`, `docs/module-contracts.md`, and
`docs/adapter-boundaries.md`.

### 1. Selection — which issues can start

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Ready-issue selection | caller invocation (CLI, MCP, scheduler tick) | a pure function of snapshot + policy | none in the selector (snapshot and result are caller-owned) | tracker snapshot via read-only CLI | pure; staleness is the caller's problem | selector repo selection module: status precedence `excluded`>`unresolved`>`in_progress`>`blocked`>`ready`; `sha256:` fingerprint; selection-invariant failure maps to a distinct exit code |
| Grouping | inside selection | deterministic bucket key = milestone + ordered label set | none | issue labels/milestone in snapshot | oversized buckets split into deterministic chunks | selector repo grouping module: unlabeled issues get a unique per-issue key so they never share a group; `max_size` split |
| Per-repo policy scoping | config load | per-repo policy file | policy file (repo config) | none | unknown keys rejected at parse | selector repo policy module + policy files (`exclusion_labels`, `require_labels`, `max_group_size: 3` justified by a lane audit of the 3h deadline vs 4+ issue groups) |
| Scheduler candidate intake | cron tick (every 30 min) | ops-repo scheduler picks first ready group; never ranks/splits/re-filters | run state, parked selectors, run history | selector output | triage errors → park with diagnostic | ops repo scheduler library (run lock, state file, parked registry, run-history append); the ops repo's scheduler design doc |

**Classification — Selection**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Pure snapshot+policy selection with stable fingerprint | **retain** (semantics) | the selection-is-a-pure-function boundary is what made the ops repo's scheduler and the CLI agree by construction; workloop keeps the identical boundary | Work Selector port in `adapter-boundaries.md` — Candidates carry stable proposal identity | no implementation yet; contract tests needed for determinism and stable identity | port specified but unimplemented; snapshot schema for workloop undefined |
| Grouping by milestone+label-set, group as atomic batch | **re-model** | grouping is selection detail, not a core invariant; the boundary is: oversized *candidate buckets* are split deterministically at selection time (`max_size`), and each emitted group is then atomic for execution — never split mid-run. Keep the latter; key derivation and `max_size` stay selector-internal | Work Selector output shape (group `execution_order`) | fixture selector for contract tests | real selector deferred; group identity vs `work_id` mapping is issue #34 |
| Per-repo policy / allowlist | **re-model** | the need (scope which repos/issues are eligible) is generic; the file format and label namespaces are personal config | installation/adapter config | `workloop-config` fields | none beyond config schema |
| Scheduler auto-intake of selected groups | **omit** | automated issue discovery is a personal operations feature, not lifecycle semantics; submission into workloop is via `submit` | none — out of scope per `architecture.md` | n/a | if a selector adapter is later added, it must go through `submit`, never around admission |

### 2. Admission — turning a request into tracked work

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Enqueue request → card | milestone CLI enqueue / scheduler intake | enqueue command + kanban library | request file (atomic write), Kanban card | repo allowlist, issue data | dedup onto existing card; bounded re-key, `error_kind: terminal_card` | ops repo milestone CLI `enqueue` command; kanban library create/save-request helpers (temp-file + rename writes) |
| Content-hash idempotency | every enqueue | default idempotency key = content hash excluding the selection snapshot and post-create bookkeeping | idempotency key → request path | none | re-run dedups rather than duplicating; #185 (fingerprint break on bookkeeping update) fixed by excluding bookkeeping from the hash | ops repo kanban library: default idempotency-key + request-fingerprint functions; ops issue #185 CLOSED 2026-10-05 |
| Terminal-card dedup guard | dispatch scan | scheduler / dispatch | card status | card status | dedup onto a terminal card is never started | ops repo kanban library terminal-status set (`done`/`blocked`/`archived`); ops AGENTS.md dedup notes (#163) |
| Issue triage before enqueue | plan/start/enqueue | triage via selector; ambiguity filtering | triage diagnostic files | issue body | empty-triage (retry later) vs permission-triage (deterministic) errors distinguished | ops repo milestone CLI plan/triage commands |

**Classification — Admission**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Stable request identity + content-hash dedup excluding bookkeeping | **retain** | #185 shows the invariant: identity must not depend on mutable bookkeeping | `submit` Command: identity + payload digest + result stored in one commit; same key/different payload rejected | `workloop-core` `work` decision logic; store tests | covered by core contracts; replay-while-bookkeeping-changed test should exist |
| Terminal dedup never restarts | **retain** | a terminal card must not revive — continuation is a new identity | terminal `Execution` states; retry creates a *new* Execution | lifecycle.md retry/resume semantics; store tests | — |
| Card-as-queue-entry storage model | **omit** | external board as store is the source of card/run disagreement; replaced by the single authoritative store | SQLite store (current-state + append-only `event_log`) | `workloop-store` | — |
| Triage/ambiguity filtering upstream of admission | **re-model** | workloop accepts what `submit` is given; ambiguity screening is selector-side policy, not a core gate | selector/binding concern | none | — |

### 3. Queueing — what runs next

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Claim card for dispatch | dispatch tick | card claim with TTL | card claim record | card assignee/status | claim expiry ≠ process death (#266) | ops repo kanban library claim/heartbeat helpers; ops issue #266 CLOSED 2026-10-16 |
| Equivalent-active exclusion | scheduler tick, per candidate | scheduler scans active cards/runs for same repo+issue overlap | scan outcome informs enqueue/skip | all lanes' cards and run records | scan failure → `unknown` hold, not "free" (three-way distinction from #307) | ops repo scheduler library equivalent-active scan; ops issues #300, #307, #346 |
| Parked selectors | scheduler tick on triage failure | park-selector helpers | parked registry file | — | parked ≠ blocked; re-evaluated per tick | ops repo scheduler library parked-registry functions |
| Global/per-repo capacity | every dispatch decision | scheduler config per-repo limit | incidents/state | live process counts per lane | lane-blind counting overcounted other lanes (#346) | ops repo scheduler library per-repo-limit function; ops issue #346 CLOSED 2026-10-21 |

**Classification — Queueing**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Bounded scan that skips an unstartable head and advances independent work | **retain** | #300: head-of-line terminal-dedup starved 7 ready groups at 0/2 capacity — FIFO order must not imply head-of-line blocking | dispatcher picks "oldest eligible Execution" in finite scan; `queued` persists with diagnostic | `workloop-cli` dispatch; contract test: blocked head doesn't starve followers | none — expressible in current contract |
| Unknown scan result → hold, never "free" | **retain** | #307: conflating "verified busy" / "scan failed" / "enqueue unknown" caused wrong human actions | `uncertain`/`waiting` semantics; capacity reservation rationale queryable | lifecycle.md `uncertain`; Attention rules | — |
| Capacity as claim on a card | **re-model** | #266: claim TTL expiry is not death; #346: capacity must be an authoritative reservation with explainable accounting | capacity slots reserved atomically with the dispatch transition; `ready/running/waiting/uncertain/stopping` hold capacity | lifecycle.md capacity section; `workloop-cli` budget sweep | per-repo/multi-host capacity unresolved |
| Parked selector registry, card claim TTLs | **omit** | mechanisms for an external board the new store makes unnecessary | n/a | n/a | — |

### 4. Dispatch — starting work

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Lane dispatch execution | claimed card | dispatch command → lane by `executor:` config (hosted-agent / worker-local / worker-remote) | lane run record, launch attempts | lane config, card binding | structured stop-action classifies; retry → pending-retry + next-attempt stamp | ops repo milestone CLI dispatch/launch/settle commands; the ops repo's worker-lane doc |
| Remote job launch | remote-lane dispatch | remote library builds the job contract, pushes the job file, starts the container run | remote state record; idempotency key `(job_id, attempt, base_revision)` | remote host admission | `uncertain` ack → reconcile, never drop or re-key | ops repo remote library: job-build / remote-state / idempotency-key / launch functions; ops repo remote wait-outcome handling (lost-ack is not failure) |
| Admission slot contention | remote launch | host-side admission (distinct exit code = conflict) | pending-since episode budget accumulates | remote slot state | admission conflict → bounded deferred retry, reason code only (no remote text on card) | ops repo remote library admission-status; coordinator CLI exit-code table (ops issue #460) |
| Duplicate dispatch guard | first writing stage of worker ship | worker-side claim: same-dir lock file (O_EXCL, pid takeover) + `agent:running` issue label pinned to the `labeled` event id | claim record under ship dir; GitHub issue label | issue event log | second run refuses; crash between steps resumes idempotently | worker repo claim module (ordering: claim record → label → pin) |

**Classification — Dispatch**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Dispatch = atomic (transition + reservation + saved request) | **retain** | the atomicity is what the ops repo approximates across card/claim/launch | dispatcher transition `queued→ready/running` reserves slot and saves request in one commit | lifecycle.md dispatch section; store tests | — |
| Lost acknowledgement is not failure | **retain** | remote `uncertain` handling: identity persisted, next tick reconciles | `uncertain` Invocation state; reconcile, never re-key | lifecycle.md; `workloop-exec` stop proof | — |
| Two-layer duplicate guard (local lock + remote label) | **re-model** | the *requirement* is single-writer ownership of a workspace/branch; the mechanism (label+lock) is GitHub-and-host specific | `SafeOwnership`/writer fencing before start | module-contracts ownership; `workloop-exec` | covered for supported entry points and TTL external claims (lifecycle.md:108-112); only contract-bypassing processes remain out of scope (see incident audit) |
| Remote slot admission, lane executor matrix | **omit** | single-host v0.1; the executor dimension is config, not lifecycle | n/a | n/a | revived only if remote execution enters scope |

### 5. Execution — the worker attempt

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Bounded worker run | ship/converge stage dispatch | worker runs a job contract: caller-owned job id, immutable validated base revision, timeout, limits, declared checks, issue snapshot | run dir + status record + evidence manifest | job file; model runtime | status ∈ completed/failed/timed_out/cancelled/blocked + termination reason | worker repo worker-contract doc; contract types + validation (job hash) |
| Plan/execute split | job with planner runtime | separate planner phase; read-only tool surface enforced by audit | plan artifact ref, phase accounting | none | write-tool call in plan phase fails the run | worker repo runtime adapter: pinned planner agent + code-owned read-only MCP tool list; plan contract module |
| Container supervisor (remote) | coordinator launch | entrypoint script: non-root drop, detached session, setup hooks, atomic status/pid/times records | run dir; state file | container filesystem | worker exit without terminal record → `uncertain` (never unlocks retries) | infra repo worker supervisor script (terminal-state writer, signal trap); infra execution lifecycle doc §1 |
| Managed model runtime | every run | the runtime adapter supervises a pinned agent runtime (managed version gate) | — | agent HTTP API | provider refusal classification; step-limit is post-hoc best-effort | worker repo supervision doc + runtime adapter/normalize modules |
| Process stop | timeout/judgment halt | stop routine: verified pid identity + start-stamp gate, process-group SIGTERM→grace→SIGKILL, stop proof = zero live group members | durable stop-intent marker via pre-signal callback | `ps` output | `unverified` (no signal delivered) vs `unknown` (signal delivered, outcome unproven) → safety hold | ops repo lane library stop routine + tri-state pid-liveness; ops issue #337; ops milestone CLI stop-intent stamping (see incidents for the `OSError` swallow) |

**Classification — Execution**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Bounded run contract (immutable base, timeouts, budgets) | **retain** | resource bounds were the load-bearing safety shape | Invocation request: input, budgets, limits | lifecycle.md Invocation; `workloop-config` worker argv | — |
| Worker report is evidence, not outcome | **retain** | structured stop action over exit code; `uncertain` is first-class | `WorkerReport.status` never settles; core decides Outcome | module-contracts Worker port; fixture worker tests | — |
| Stop semantics: identity gates, group-wide stop proof, unverified/unknown distinction | **retain** | #337's fixes encode exactly the right contract | `ProcessControlPort` stop proof (descendant snapshots, PID-reuse safety, reaping) | `workloop-exec` implements this | — |
| Plan/execute split with read-only planner | **re-model** | a useful hardening, but the agent/runtime boundary is worker-internal | worker implementation detail | fixture worker | not required by port contract |
| Pinned agent-runtime version, supervision specifics, container details | **omit** | adapter/runtime internals | n/a | n/a | — |

### 6. Verification — did the attempt produce a sound result

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Declared checks run | after implementation, before propose | worker executes declared checks serially; bounded parallel groups (cap 2, write-surface independence documented) | per-check record (cmd, exit, timing, timed-out flag) | none | failing required check → run fails | worker repo contract doc checks/policy section; checks runner; infra repo repo-setup-check mechanism |
| Verify stage + remediation | verify post-run | verify module produces a verification record bound to exact patch bytes | verification record | patch + base | code-class failure seeds bounded remediation (max 2 attempts); environment/human classes halt | worker repo verify + verify-remediation modules (worker issue #140) |
| High-risk review gate | before propose | risk assessment over the run's self-authored impact manifest — failure/permission semantics, trust boundaries, identity invariants | cleared-review manifest | manifest | missing reviewer *stops* the pipeline (never silently ships) | worker repo review-gate module (worker issue #115) |
| Settle classification | run end | structured stop action (done/retry/human/fix_env) preferred over exit code | last-result on run record | record file | missing/corrupt classification is never success | ops repo lane library result classifier; ops repo worker-lane doc "terminal judgment" |

**Classification — Verification**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Verification record bound to the exact patch bytes | **retain** | the honesty rule — PR only from a passing record bound to this run's patch | readiness/verification evidence refs bound to `work_id`+head | module-contracts evidence refs; fixture worker tests | — |
| Structured terminal classification over exit codes | **retain** | worker prose is a claim; classification is typed evidence | `WorkerReport` typed status; never accept prose success | Worker port | — |
| High-risk manifest → independent review gate | **re-model** | the *invariant* (declared risk requires review before publication) is good; the manifest-assessment mechanism is worker-internal | optional adapter-level gate; not core v0.1 | none | whether v0.1 needs a declared-risk gate is open (issue #35 area) |
| Bounded verify remediation | **re-model** | remediation is just another Invocation under budget — no special mechanism needed | Invocation budget + retry semantics | lifecycle.md | count transient-failure vs review-round budget consumption — explicit v0.1 simplification allowed |

### 7. Publication — making the result visible

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Worker `propose` | verified run | the pipeline's PR-creation write path: branch, commit, push, PR creation | propose record with bounded refusal codes | git remote, GitHub API | refusal codes are the only persisted detail (sanitized export can't leak); `publish_failed` covers partial publish | worker repo propose module (worker issues #41/#317): never merges, never force-pushes, marker + provenance on every body. Note: propose creates the PR; converge (below) performs its own push/reply mutations — they are distinct write paths inside one tool, not one shared mutation funnel |
| Worker `converge` fix publication | converge round with fixes | converge pushes fix commits to head branch, posts marked replies with fix SHA | findings ledger, converge record | PR head, review threads | head-signal marks bound to head SHA | worker repo converge + head-signal modules |
| Remote-lane publication | remote ship/converge settle | coordinator publish op + job-contract bounded publish stage (#447) | run record publication status, pushed head | remote container, GitHub | publish-before-collect ordering (#389); fail-closed adoption check | ops repo remote library publish op; ops milestone CLI pushed-head adoption check; ops issues #389 CLOSED, #445/#447 |
| Unknown write reconciliation | after any GitHub mutation | action-id/PR-URL reconciliation | proposal/publication records | GitHub reads | adopt matching existing effect; refuse ambiguous | worker repo propose reconciliation; infra repo publication-validation script; coordinator CLI publish-conflict exit code (second-different-PR) |

**Classification — Publication**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Single publication owner with persisted intent | **retain** | #389: double publication owner → duplicate-PR risk; #445: verdict without push evidence. In the source, propose and converge are *two* mutation paths inside one tool; workloop's invariant is stricter — every GitHub mutation from any stage funnels through persisted `PrActionIntent` | core-owned `PrActionIntent` outbox is the only mutation entry; unknown responses reconciled by action ID | module-contracts PrActionIntent; `workloop-github` fixture | real adapter is issue #17 |
| Verdict requires publication evidence (fail-closed adoption) | **retain** | #445: `fixed` while head == pre-run head must block, never settle | stage contract: `converge` outcome = fix commits *published* to PR head | lifecycle.md head-bound completion; contract test | — |
| Marked replies, no merge/force-push, provenance markers | **retain** | machine marker + bounded effects are the trust contract | adapter mutation surface | fixture adapter records intents | — |
| Bounded refusal codes as the only persisted failure detail | **re-model** | sanitize-by-code is a good idea; exact code set is implementation detail | failure codes on Command results | lifecycle.md failure classes | — |
| Job-contract embedded publish instructions (remote lane) | **omit** | executor-internal workaround for a stage-boundary bug; the invariant (publication is part of the stage) is what transfers | stage contract via port | n/a | — |

### 8. Review / fix — convergence on a PR head

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Review-bot path | `pull_request_review` webhook on managed PR | resume webhook filter records resume events; dispatch consumes | resume-events ledger, debounce | webhook, managed-PR binding | at-least-once events; replay-safe | ops repo automation inventory + resume-events drain in the milestone CLI |
| Lane shadow review | shadow event (webhook or lane-emitted) | shadow webhook filter + debounced dispatch | shadow-events ledger | PR author/sender, lane binding | bot-author drop; lane-bound exemption post-#447 | ops repo shadow filter + debounce scripts; ops repo shadow-review doc; ops issues #234, #323, #446, #447 |
| Converge rounds | review findings on a head | converge loop: classify findings (severity × disposition), one batched bounded fix run per round, round cap | findings ledger (cross-invocation), converge record (per-invocation) | PR checks, review surfaces | `converged`/`awaiting-human`/`capped-*`/`halted`/`budget-exhausted`; security/data/high-severity → human halt | worker repo converge module (status set, findings ledger, out-dir-scoped lock — not repo-wide) |
| Judgment capture/release | operator decision on findings | judge/release commands → judgment wait/decision/release records; carryover across generations | judgment-wait files, judgments handoff | PR head at judgment vs release | head move stales answers — never silently rebind; replay of lost response goes to owner ledger | ops repo lane library judgment functions (record wait / record judgment / mark head-change / release wait); ops milestone CLI judge/release/autojudge commands |
| Console judgment UI | operator action | console builds structured action requests; action adapter delegates to judge/release/scheduler reenqueue | projection cache only (snapshot dir, atomic) | live head, finding IDs | preflight + schema-checked response; exact replay bypasses preflight | ops repo console action module + console doc |
| Finding disposition evidence | settle/reconciliation | per-finding disposition ledger — cross-attempt proof a finding was fixed | disposition ledger | commits on PR | later explicit halt supersedes older fix evidence | ops repo lane library disposition/reconcile functions |

**Classification — Review / fix**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Head-bound judgment: stale answers never silently rebind | **retain** | head is the unit of review truth | `judge_findings`/`release_judgment` with expected head + version; stale head = explicit rejection | module-contracts judgment commands; contract tests | — |
| Bounded converge loop with severity-routed human halt | **retain** | the review/fix loop is the product; the bound + human-halt boundary is the safety shape | bounded fix rounds → new Invocations; Attention for human-decision halt | lifecycle.md review/fix states | convergence stall detection remains open (incidents §) |
| Judgment waits/decisions as durable ledgers surviving card replacement | **retain** | decisions outlive attempts and cards | Work-level decision/evidence records keyed to head | lifecycle.md evidence | — |
| Finding disposition ledger across attempts | **retain** | proof a *finding* was fixed must outlive the run that fixed it | evidence refs on Execution/Invocation | store event log | — |
| Operator console over versioned read contracts + typed actions | **re-model** | same shape (reads projections, submits Commands, never reclassifies) but workloop TUI is first-party, not a foreign-board adapter | `workloop-tui` over Command/Query | existing crate | — |
| Webhook routes, debounce timers, bot-author filters | **omit** | transport of a specific deployment; workloop ingests Observations through the GitHub adapter instead | GitHub adapter Observations | fixture adapter | reviewer-orthogonality lesson retained via #21 invariants |
| Out-dir-scoped converge lock | **omit** | superseded by single-writer ownership | ownership/claim | n/a | — |

### 9. Recovery — returning from failure

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Reconcile sweep | cron every 5 min | reconcile pass advances card-owned runs; settles card/run disagreement | run/card state | process liveness, run records | tri-state liveness; `uncertain` holds | ops repo reconcile CLI; milestone CLI terminal-settle entry point |
| Blocker re-evaluation | independent periodic tick (#358) | blocker-reeval library — classifies from structured evidence only, recovers only proven-transient causes | evaluation/next-check/attempt records | issue state, timers | fails closed on unknown; never auto-recovers human/spec/manual blockers; deterministic retry key | ops repo blocker-reeval library + design doc |
| Remote recollect/discard/quarantine | operator action | remote recollect / discard / quarantine commands | resets push-check-failure counter; quarantine fences unverifiable runs | container/evidence | container+evidence retained until proven; coordinator collect permanent-refusal exit (settle, stop retrying) | ops repo milestone CLI remote recollect/discard/quarantine commands; infra repo coordinator CLI exit-code table; ops issue #377 |
| Resume from retained workspace | operator `submit --resume` | resume resolution binds a resume chain by job hash (task identity), adopts completed leaf | ship dir workspaces | evidence files | unreadable/mismatched binding → error, never half-trusted | worker repo resume module (worker issue #105); bound-runs module (dead-end runs count toward dispatch budget, worker issues #139/#124/#221) |
| Timeout replan attribution | reconcile detecting timeout | checkpoint marker/PID/invocation-stamp attribution to a prior run; unreadable → possibly-live → refuse successor | checkpoint/ship-dir evidence | filesystem, PID | kill-then-final-flush can mimic foreign writer — attribution, not freshness | ops repo milestone CLI checkpoint-attribution heuristic; ops issue #381 CLOSED 2026-10-23 |
| Retention/pruning | cron | run-reclaim, round-prune, orphan-reaper jobs | — | state dirs | aggressive cleanup destroyed required evidence | ops repo prune/reclaim/reaper libraries + disk-reclaim doc |

**Classification — Recovery**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Reconciliation reads facts, never invents them | **retain** | every recovery path reconciles persisted evidence + live facts; tri-state liveness | store + supervisor reconcile loop; `uncertain` holds capacity | lifecycle.md; `workloop-cli` run loop | — |
| Idempotent lifecycle ops keyed on stable identities | **retain** | the coordinator CLI's repeated-call reconciliation is the right shape | all Commands idempotent by key+digest | store command tests | — |
| Bounded, recorded recovery with permanent-refusal class | **retain** | #377: recovery retry needs durable attempts, budgets, escalation — `max_invocations` alone doesn't bound collect | bounded durable recovery (waiting reason / paused(repair) / failed(evidence_unavailable)); Attention escalation | lifecycle.md; contract tests | artifact-disposition state still unmodeled in current contract (open) |
| Deterministic blocker re-evaluation from structured evidence | **re-model** | the *requirement* (resume only when provable conditions changed; never auto-expand permissions, #215) transfers; the periodic-job mechanism doesn't | `paused` continuation conditions + resume validation | lifecycle.md resume contract | recording "what must change to resume" is a required field — currently implicit |
| Attribution heuristic for foreign writers (checkpoint markers) | **re-model** | the mechanism is ship-dir-specific and omitted; the requirement — a writer whose provenance can't be proven is treated as live — transfers into ownership evidence | `SafeOwnership` evidence | module-contracts | covered by the reservation store + TTL external claims for supported entry points (incident audit) |
| Prune/reclaim scripts | **omit** | they destroyed evidence the workloop requires; retention policy is a deliberate non-port | append-only `event_log` preserves history | store | retention/export safety is decisions.md Q4 — open |

### 10. Attention — surfacing what needs a human

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Attention projection | status/attention read | attention builder — reconciles incidents, parked selectors, blocked cards, judgment waits; `unknown` reason/source/id is surfaced, not hidden | projection only (no authority) | board, requests, runs, incidents | bounded tracker lookups; incomplete evidence → `unknown` item | ops repo status-projection library: attention builder (versioned attention schema), blocked-attention helper reading ship/converge records for fresh cause |
| Stale-attention cleanup | scheduled job (#295) | attention-cleanup library — archives only when every linked issue/PR is closed/merged, or a provably-newer duplicate covers all identity | archive ledger (auditable) | issue/PR state | fails closed on uncertainty | ops repo attention-cleanup library (covering/active-reason functions) + cleanup doc |
| Change-aware notice + acknowledge | notify tick (#309) | hourly attention runner; acknowledge marks seen without releasing any gate | notice state | — | repeated identical blocker → notify once | ops repo attention runner; ops issue #309 CLOSED |

**Classification — Attention**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Attention = actionable-now decisions, derived not stored | **retain** | the projection has no authority; uncertainty is displayed | workloop Attention: paused judgment, confirmed failure with eligible retry, prolonged uncertain | lifecycle.md Attention rules; TUI/CLI queries | "prolonged" threshold and acknowledge-visibility semantics undecided |
| Unknown-surfaced-over-hidden | **retain** | an incomplete-evidence item is shown as unknown, never silently dropped | projection exposes provenance/uncertainty | query layer | — |
| Archive/cleanup job with fail-closed covering checks | **omit** | needed only because Attention was persisted state; derived Attention makes stale items impossible by construction | n/a | n/a | if historical terminal evidence ever counts as non-actionable needs an explicit rule |
| Change-aware notification + external message delivery | **omit** | delivery mechanism is personal infra | n/a | n/a | acknowledgment-as-seen (not gate release) retained as semantic |

### 11. Environment setup — making a host able to run work

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Installer scripts | operator run | per-feature installers — copy scripts/skills into the host's ops home; idempotent, mostly no uninstall | installed copies, manifest | host fs | `--check` detects drift | ops repo install scripts + deploy conventions doc (code vs installed state are separate) |
| Packaged component install | operator | console installer — manifest + per-file SHA-256, atomic promote, rollback slot, disable/rollback commands | console-owned install root | backend/schema compatibility | missing/incompatible backend → hard fail, no degraded view | ops repo console install script + console doc + package tests |
| Worker host bootstrap | one-time host setup | host bootstrap + restricted-SSH helper; image build pins worker artifact sha256 | host state, image | container runtime, SSH | documented runbook | infra repo host bootstrap/helper/ssh-transport scripts + host docs; worker-image build script + component manifest |
| Credential handling | runtime | worker credential module; remote-lane credential files + redaction + no-secrets-on-argv check | credential refs, never values in job | local auth files | secrets stay out of argv/`ps` | worker repo credentials module; ops repo remote library credential/redact helpers |

**Classification — Environment setup**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Declarative installation config | **re-model** | the requirement — actor identity, worker argv, capacity, workspace root, allowed effects — is generic; file layout is personal | `workloop-config` | existing crate | — |
| Secrets never in argv/logs/evidence | **retain** | credential refs not values; redaction is structural | secrets policy on adapters; `credentials_ref` opaque | Worker port input contract | — |
| Ops deploy model (repo→host copy, skill wiring, manifest/rollback packaging) | **omit** | personal host ops | n/a | n/a | — |

## Incident audit

Six failure classes the issue names explicitly. "Incident" = a recorded
past failure; "residual defect" = behavior that can still fail today.
CLOSED status alone is not proof of non-recurrence — it means a fix was
deployed; each class states what the fix was and what remains.

### Card / run disagreement

- **Incidents**: ops #337 (CLOSED 2026-10-21 — timed-out ship runs left
  cards `running`), ops #343 (CLOSED 2026-10-21 — human-judgment halt left
  converge run/card `running`), ops #266 (CLOSED 2026-10-16 — claim-TTL
  reclaim marked live successful ships `gave_up`/`blocked` while the
  process succeeded).
- **Fixes deployed**: reconcile sweep with a terminal-settle entry point;
  durable stop-intent marker before signal; tri-state pid liveness
  (unknown ≠ dead); verified pid identity + start stamp before kill;
  heartbeat/claim semantics clarified.
- **Residual defect**: the structural cause persists in the ops repo —
  authority is split across card + request + run record + live process,
  so disagreement is reconciled rather than impossible. In workloop this
  is re-modeled: one authoritative store, `Work`/`Execution`/`Invocation`
  separation, `uncertain` as first-class. **Disposition: re-model.**
- **Remaining source defect found in this audit**: the durable stop-intent
  marker write swallows `OSError` inside the pre-signal callback of the
  ops repo's timeout path — the signal is still delivered when the marker
  write fails. The documented contract says refusal must not leave a
  marker, but nothing says a marker failure must abort the signal —
  a crash-consistent write-order violation that remains live at the
  recorded revision.

### Duplicate dispatch

- **Incidents**: ops #185 (CLOSED — retry bookkeeping broke the request
  fingerprint), ops #307 (CLOSED — three-way ambiguity caused wrong human
  action), ops #381 (CLOSED — lane-blind equivalent-active scan collided
  with a manual resume writing to the same ship dir).
- **Fixes deployed**: fingerprint excludes bookkeeping; three-way
  exclusion classification; checkpoint attribution (marker + PID +
  invocation stamp + updated-at) with unreadable→possibly-live→refuse;
  worker-side `agent:running` label + same-dir lock claim.
- **Residual defect**: the manual-writer boundary — a foreign process
  (unmanaged CLI, manual salvage) writing to a managed workspace is only
  defended by heuristic attribution, and only on the paths that check it.
  taisan's contract now covers this: cross-Work exclusion is enforced
  by resource reservations keyed on workspace + `(repository, branch)`
  (lifecycle.md:108-112, decisions.md), and supported external writers
  register TTL-bounded claims via `claim_resources`; what remains out of
  scope is arbitrary external processes that bypass the contract
  entirely. **Disposition: re-model — the requirement transfers as
  reservations + external claims; the heuristic mechanism is omitted.**

### Uncertain stops

- **Incidents**: ops #337, #266 (both CLOSED — see above).
- **Fixes deployed**: the stop contract — verified start stamp +
  group-member count as stop proof, `unverified` vs `unknown`; remote
  supervisor `uncertain` state on missing terminal record; stop-intent
  marker ordering (inside the pre-signal gate).
- **Residual defect**: the `OSError` swallow above — a stop-intent
  persistence failure does not abort the signal. Also, `unknown` after a
  delivered signal relies on the next sweep reading group death "as
  ours"; there is no independent deadline for how long `stopping` may
  hold before escalation. **Disposition: retain the contract, fix the
  swallow in re-modeling (persist stop intent as part of the `stopping`
  commit *before* signaling — structurally guaranteed by the single
  store).**

### Missing PR publication

- **Incidents**: ops #445 (CLOSED 2026-10-28 — remote converge reported
  `review-fixed`, never pushed; container destroyed with the commits;
  real loss on two worker-repo PRs), ops #389 (CLOSED 2026-10-23 —
  collect-before-publish doubled the publication owner).
- **Fixes deployed**: ops PR #447 (MERGED) — bounded publish stage in the
  remote job contract (non-force push to the same head ref, marked
  replies, `fixed` only after push); fail-closed adoption check covering
  *all* produced-commit completes; `push-uncertain`/`push-failed`
  blocking classes; recollect resets the check-failure counter.
- **Residual defect**: none observed in source at `6371cf99`; live-
  operation verification is absent (see Evidence classification). The
  generalized defect class — a stage whose definition varies by executor
  — is the subject of agent-workloop #21. **Disposition: retain the
  invariant (publication evidence required for success classification),
  re-model as executor-independent stage contract.**

### Stale-head reviews

- **Incidents**: ops #446 (CLOSED 2026-10-28 — the webhook filter dropped
  bot-authored PRs; remote-lane PRs structurally never received shadow
  review; `reviewer: shadow|both` silently degraded to the other
  reviewer), ops #234 (CLOSED — the original shadow route), worker #246
  (CLOSED — skip PRs already merged at review start), ops #382 (CLOSED —
  decoupled reviewer firing from executor).
- **Fixes deployed**: lane-bound exemption (managed-PR / pushed-head
  ledgers, not the author-controlled PR body marker, fail-closed on
  unreadable ledger); the lane emits `origin="lane"` shadow events on
  settle against the *live* head; head-signal markers bind replies to
  head SHA; pending answers stale on head change.
- **Residual defect**: the reviewer dimension is now deliverable on
  remote lanes, but only because the lane synthesizes the event the
  webhook would have sent — a second, config-dependent firing path.
  Judgments bound to a stale head are still rejected correctly.
  **Disposition: retain head-bound judgment and reviewer-orthogonality
  invariants (#21); omit the webhook/debounce mechanism.**

### Stalled convergence

- **Incidents**: ops #377 (CLOSED 2026-10-22 — terminal remote failure
  known, collect retried forever, card `running`), ops #326 (OPEN —
  converge 30-min stall vs 120-min deadline vs pending-retry expiry vs
  settle-lodging proposal), ops #379 (OPEN — same failure kind 4× ≈30
  min each), ops #278/#279 (OPEN — bounded recovery-first parking
  proposals).
- **Fixes deployed**: bounded collect with a check-failure budget (max
  3), `push-uncertain`/`push-failed` blocking classes, container +
  evidence retention, remote recollect; the coordinator CLI's permanent
  collect-refusal exit code.
- **Residual defects**: **stall detection, repeated-identical-failure
  detection, and bounded requeue after timeout are all still OPEN
  proposals in the ops repo** — this is the largest class that is *not*
  fixed at the recorded revision. taisan's re-model must include:
  distinguish "no progress" (stall — notify only) from "deadline
  exceeded" (stop intent → proven group stop → bounded requeue);
  repeated identical failure kinds surface to Attention; recovery loops
  never run unbounded. **Disposition: retain as open requirements; the
  specific thresholds (30/120 min) are policy values, not invariants.**

## Design evidence issues

| Item | Status (2026-11-05) | What it contributes | Contract conflicts / missing evidence |
| --- | --- | --- | --- |
| `agent-workloop` issue #21 | OPEN | The cross-cutting invariant discovered by ops #445/#446: **stage contracts are executor-independent** — `converge` means "bounded fix commits published to the PR head, marked replies, draft→ready exactly once" regardless of where the stage runs; **success classification requires publication evidence**; reviewer dimension is orthogonal to executor | Its invariants are the audit's own re-model targets; no conflict. Evidence: the issue body records two verified runs on worker-repo PRs |
| ops repo issue #445 | CLOSED 2026-10-28 | Missing-publication incident; exact reproduction (run id, card id, 4 unresolved threads, head still at pre-run SHA) | Conflict it exposed: the remote job contract ended at "make the bounded fix + report result" — no publication stage; resolved by PR #447 |
| ops repo issue #446 | CLOSED 2026-10-28 | Silent-reviewer incident; evidence: 97 shadow events all human-authored, zero for lane PRs | Exposed that one matrix dimension (reviewer) was silently overridden by another (executor) — the orthogonality invariant |
| ops repo PR #447 | MERGED | The fix: bounded publish stage inside the job contract + fail-closed adoption check + lane-bound webhook exemption + lane-emitted shadow events | Contract change: the converge-fixed verdict now means "fix pushed to head", not "fix produced"; verification of live post-merge behavior is absent — the code paths were read, not exercised |

## Required behavior vs personal-environment mechanisms

The audit deliberately separates *what must hold* from *how this
deployment does it*:

- **Required (retain/re-model)**: stable request identity; dedup that
  excludes mutable bookkeeping; terminal dedup; admission ≠ dispatch;
  atomic transition+reservation+request; lost-ack ≠ failure; bounded
  runs; worker reports as evidence; stop proof over process groups;
  verification bound to patch bytes; single publication owner with
  persisted intent; verdicts requiring publication evidence; head-bound
  judgment; decisions surviving card/attempt replacement; bounded
  recorded recovery; tri-state liveness; Attention as derived
  actionable-now decisions; uncertainty surfaced; secrets never in
  argv/logs.
- **Personal-environment (omit)**: the external Kanban
  board/card/request/run-record triangulation; claim TTLs and
  heartbeats; parked-selector registry; the lane executor matrix;
  external message delivery and cron cadence; webhook filters and
  debounce; the repo→host installer deploy model; the pinned agent
  runtime; container runtime and restricted-SSH transport; the
  prune/reclaim jobs; the out-dir converge lock. Two list-adjacent items
  are mechanisms only, not requirements: per-repo policy file *format*
  (the scoping requirement is re-modeled, §1) and checkpoint-attribution
  *heuristics* (the foreign-writer requirement is re-modeled, §9).

## Design aspects

Per the issue, no concrete rule is settled here — each aspect records
applicability, rationale, and open questions.

### State transitions

- **Applicable.** The card/request/run-record triangulation and its
  disagreement class are the direct ancestors of the
  Work/Execution/Invocation split and the single authoritative store.
- **Rationale.** The audit shows the source system *reconciles*
  disagreement continuously (reconcile sweep, terminal-settle entry
  point, judgment-wait ledgers); workloop instead makes most
  disagreement unrepresentable — which is exactly why the port is a
  re-model.
- **Open questions.** Whether artifact-collection pending becomes a
  `waiting` reason or a `paused(repair)` substate. (Foreign-writer
  handling at queued admission is now settled: a conflicting external
  claim leaves the Execution `queued` with a retryable diagnostic —
  lifecycle.md:110-112.)

### Failure semantics

- **Applicable.** Tri-state liveness, `unverified` vs `unknown` stop
  results, `uncertain` remote acks, permanent vs transient collect
  refusal (a distinct exit code), structured stop action over exit code.
- **Rationale.** These distinctions each exist because conflating them
  caused a specific incident (#266, #307, #337, #377).
- **Already settled in the contract.** The swallowed stop-intent
  `OSError` shows "persist intent before signal" was asserted but not
  enforced in the source; workloop's contract now requires it —
  lifecycle.md:162 and decisions.md require durable stop intent before
  signaling with no signal when persistence fails, and `workloop-exec`
  implements that ordering. Invocation budgeting is likewise decided:
  lifecycle.md:149 counts every durably recorded Invocation attempt,
  including a definite `not_started` result.

### Trust boundaries

- **Applicable.** Worker reports are claims; GitHub mutations only via
  persisted intent; judgments need explicit release; remote-adapter text
  never reaches card-facing strings unchecked (bounded reason codes).
- **Rationale.** Each boundary exists because authority confusion caused
  incidents (#389 double publication; #445 verdict-without-effect;
  card-facing strings carrying remote detail).
- **Open questions.** Credential/permission scope per adapter (issue
  #35); whether a declared-high-risk manifest gate like the worker's
  review gate belongs in v0.1 scope.

### Identity invariants

- **Applicable.** the remote lane's idempotency key built from
  `(job_id, attempt, base_revision)`; job-hash binding of runs to tasks; request
  fingerprint excluding bookkeeping; claim pinned to the tracker's
  `labeled` event id; plan keys binding head+findings+decisions; run id
  vs coordinator vs worker ids deliberately distinct.
- **Rationale.** taisan's `work_id`/`execution_id`/`invocation_id`
  exist precisely because issue numbers, card IDs, and PIDs were not
  stable identities.
- **Open questions.** How Work identity relates to multi-issue groups
  and external card IDs (issue #34); whether finding IDs are
  review-surface identities or durable defect identities.

### Information preservation

- **Applicable.** The findings ledger surviving invocations, judgment
  ledgers surviving card replacement, disposition ledgers surviving
  attempts, retention failures (prune jobs) destroying required
  evidence, sanitized export via bounded refusal codes.
- **Rationale.** The append-only `event_log`, evidence refs, and
  preserved-WIP-on-failure are direct responses.
- **Open questions.** Retention vs export safety (decisions.md Q4);
  artifact digest/retention minimums; whether an "evidence unavailable"
  terminal class is needed.

### Resource bounds

- **Applicable.** Global + per-repo capacity, claim TTLs, dispatch
  budgets (dead-end runs count), converge round caps, bounded verify
  remediation, the push-check-failure budget, job step/cost limits, the
  ship wall-clock deadline vs `max_group_size` (capacity-derived policy
  bound).
- **Rationale.** v0.1 defaults are re-modeled from these previously
  implicit operational limits.
- **Open questions.** Per-host vs per-repo vs global capacity scope;
  whether recovery retry budgets are distinct from invocation budgets
  (this audit says they must be: collect retry is not an invocation);
  "prolonged uncertain" thresholds.

## Evidence classification

Every claim above rests on evidence classified as **valid** (verified at
the recorded revision), **valid-after-verification** (verified against a
different recorded revision — noted inline), **absent** (could not be
checked), or **invalid** (does not support the claim). Nothing absent or
invalid is presented as established fact.

| Evidence | Classification | Notes |
| --- | --- | --- |
| Issue/PR states and titles (tracker-queried 2026-11-05) | **valid** | All statuses re-queried for this audit |
| Ops repo source at `6371cf99` | **valid** | Verified by direct source reads (this audit) |
| Worker repo source at `66daef53`, selector at `f89c35bb`, infra at `3522605a` | **valid** | Read via `git show`/`ls-tree` on `origin/main` |
| Prior internal research notes on the ops repo's lifecycle (intake/review-scheduler/worker-recovery) | **valid-after-verification** | Used as pointers; every claim carried into this document was re-verified at this audit's revisions; their 2026-10-23 issue states were refreshed |
| Ops repo design docs (converge state transitions, kanban, scheduler, blocker-reeval, worker-lane, shadow-review) | **valid** as design documentation — not proof of deployed behavior | The converge doc pins earlier revisions; re-verified against this audit's revisions |
| Ops repo automation-inventory doc (cron/webhook listing, 2026-10-17) | **valid-after-verification** for 2026-10-17 only; current host state not re-checked | Treated as a recorded claim, not current fact |
| Live host state, installed copies, runtime process behavior, message delivery | **absent** | Not inspected; flagged wherever a claim would have depended on it |
| Whether merged fix PR #447 works in production | **absent** | Code paths verified; operational outcome unverifiable from here |
| Infra repo's own pinned-commit claims about the worker repo (interface doc) | **valid** as that repo's recorded claims | Independent re-verification against this audit's worker revision where the audit depended on it |
| Prior audit's line-level locators | **valid-after-verification** | Symbols re-verified to exist at the recorded revisions; exact positions drifted (and are deliberately not reproduced here) |
| Web-search-derived issue history (from prior research notes) | **invalid** | Earlier notes found search could not surface the private repos; this audit used authenticated `gh` only |

## Extraction catalog

<!-- extraction-catalog:v1:start -->
```json
{
 "sources": [
  {
   "role": "ops",
   "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
   "repository_path": "/Users/shin/hermes-setup"
  },
  {
   "role": "worker",
   "revision": "c8c3e2b44080d678c162131d53b790b66f20072f",
   "repository_path": "/Users/shin/src/github.com/shimpeiws/coding-worker"
  },
  {
   "role": "selector",
   "revision": "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c",
   "repository_path": "/Users/shin/src/github.com/shimpeiws/devflow"
  },
  {
   "role": "infra",
   "revision": "74303040aea74e6cfc931234be198e557bc696d5",
   "repository_path": "/Users/shin/src/github.com/shimpeiws/coding-worker-infra"
  },
  {
   "role": "target",
   "revision": "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90",
   "repository_path": "/private/tmp/taisan-issue-51"
  }
 ],
 "files": [
  {
   "id": "ops-F00001",
   "role": "ops",
   "path": ".agents/skills/lane-doctor/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "8a16a3f37deb110f0e3ed53bab6dc6d4aeeb7970",
   "size": 10622,
   "review_evidence": {
    "inspected_evidence": [
     ".agents/skills/lane-doctor/SKILL.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00002",
   "role": "ops",
   "path": ".agents/skills/lane-doctor/agents/openai.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "5676003d4641f568eb621b0512a872883b7cc2dc",
   "size": 255
  },
  {
   "id": "ops-F00003",
   "role": "ops",
   "path": ".github/workflows/python-ci.yml",
   "mode": "100644",
   "kind": "blob",
   "blob": "f44f74194d0d3812b9d4b7cb64fc07f8d2b17d40",
   "size": 7491
  },
  {
   "id": "ops-F00004",
   "role": "ops",
   "path": ".gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "0336508bf949805fadd01a33eb80bb8db4918a0e",
   "size": 1038
  },
  {
   "id": "ops-F00005",
   "role": "ops",
   "path": ".ignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "615e581e9799f0b9c7d8047439211fc5f19d3329",
   "size": 180
  },
  {
   "id": "ops-F00006",
   "role": "ops",
   "path": "AGENTS.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "07ae3f6ee8d0d3982e7cb7fdb6b53f18eba2a43b",
   "size": 22840
  },
  {
   "id": "ops-F00007",
   "role": "ops",
   "path": "README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a819ab2826c2523c87c8dcada500c238d003e605",
   "size": 32329
  },
  {
   "id": "ops-F00008",
   "role": "ops",
   "path": "config/blocker-reeval.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "88786e7cf21e5f8b305dc841d6cfc9c9a5f0b3f6",
   "size": 1508,
   "review_evidence": {
    "inspected_evidence": [
     "config/blocker-reeval.yaml.example:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00009",
   "role": "ops",
   "path": "config/cloudflared-config.yml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "ac8c45a7f4a18192b7f69d9671cfe8937e648270",
   "size": 279
  },
  {
   "id": "ops-F00010",
   "role": "ops",
   "path": "config/dependabot-policy.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4c939010e4911959321573d8451926b91409d12",
   "size": 823
  },
  {
   "id": "ops-F00011",
   "role": "ops",
   "path": "config/devflow-scheduler.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9faafe7c318dcc88e90ba6c09db20bfd97b03fd",
   "size": 3374
  },
  {
   "id": "ops-F00012",
   "role": "ops",
   "path": "config/devin-kanban.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "a6363180609f1f5b3ddc5086ec1357cfae22c187",
   "size": 17754
  },
  {
   "id": "ops-F00013",
   "role": "ops",
   "path": "config/disk-reclaim.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "1e98a124d81555fe83399c9044ba0776a3f1d0f0",
   "size": 1327,
   "review_evidence": {
    "inspected_evidence": [
     "config/disk-reclaim.yaml.example:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00014",
   "role": "ops",
   "path": "config/hermes-config.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "575e3c1f7f8f52a94d88f13a82cd3cf8f2b44dee",
   "size": 648
  },
  {
   "id": "ops-F00015",
   "role": "ops",
   "path": "config/issue-refinement.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "34e42e744ad7f7aca156a2f85543d152bef61bc7",
   "size": 1439
  },
  {
   "id": "ops-F00016",
   "role": "ops",
   "path": "config/kanban-attention-cleanup.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "6eb18d2d3f017e6dfb534ecac32c247beeca0fa4",
   "size": 544,
   "review_evidence": {
    "inspected_evidence": [
     "config/kanban-attention-cleanup.yaml.example:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00017",
   "role": "ops",
   "path": "config/local-agent-events.json.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "f787f2a46e35d7c9420d9987707f1a2562cd8c78",
   "size": 224
  },
  {
   "id": "ops-F00018",
   "role": "ops",
   "path": "config/policies/hermes-setup.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "5bbb1c2f3566a8040430908ac1ef57b4f3ac3745",
   "size": 1162
  },
  {
   "id": "ops-F00019",
   "role": "ops",
   "path": "config/repo-maintenance.yaml.example",
   "mode": "100644",
   "kind": "blob",
   "blob": "81bd0b2815f2bdb954505183eedbad0de1a58350",
   "size": 1211
  },
  {
   "id": "ops-F00020",
   "role": "ops",
   "path": "docs/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2f1911de3a17e374335461bff387cb36647a7103",
   "size": 584
  },
  {
   "id": "ops-F00021",
   "role": "ops",
   "path": "docs/agent-access.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "528bb7165798d0c656462c7c834b385cd0bdd090",
   "size": 8265
  },
  {
   "id": "ops-F00022",
   "role": "ops",
   "path": "docs/auto-sync.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "ded564cff0d2bd597ec7356a2d9542e8a46dcac7",
   "size": 3173,
   "review_evidence": {
    "inspected_evidence": [
     "docs/auto-sync.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00023",
   "role": "ops",
   "path": "docs/automation-inventory.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9404796b06dca6dcce5ebe68b74c97e1acd8511",
   "size": 14256
  },
  {
   "id": "ops-F00024",
   "role": "ops",
   "path": "docs/blocker-reeval.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3b83f8a5b745176013446eebfe1d87b4f7f03499",
   "size": 8572,
   "review_evidence": {
    "inspected_evidence": [
     "docs/blocker-reeval.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00025",
   "role": "ops",
   "path": "docs/coding-worker.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "44306247d1eb7aeaf359e35c46a04167fb72d2ab",
   "size": 162200
  },
  {
   "id": "ops-F00026",
   "role": "ops",
   "path": "docs/cw-github-token-refresh.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a38048e3f4f1ebf6d6245ebd3241e707450b4dc",
   "size": 8028,
   "review_evidence": {
    "inspected_evidence": [
     "docs/cw-github-token-refresh.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00027",
   "role": "ops",
   "path": "docs/cw-orphan-reaper.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "043549ec35c3cad663c519c21c24325700bc2a13",
   "size": 7419,
   "review_evidence": {
    "inspected_evidence": [
     "docs/cw-orphan-reaper.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00028",
   "role": "ops",
   "path": "docs/cw-round-prune.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1627d092c260df247d5bdfb5359e4ab7edb2b3ec",
   "size": 11061,
   "review_evidence": {
    "inspected_evidence": [
     "docs/cw-round-prune.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00029",
   "role": "ops",
   "path": "docs/design/converge-state-transitions.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "eef2e98a9bd5dba2f2f696273d543729acfe6d1b",
   "size": 50983
  },
  {
   "id": "ops-F00030",
   "role": "ops",
   "path": "docs/design/software-factory-tracing-and-cost-accounting.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f470486e4e1ac7b171750b6726cd721b580ca555",
   "size": 51616
  },
  {
   "id": "ops-F00031",
   "role": "ops",
   "path": "docs/devflow-scheduler.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "93de7967303e91e6cb2e746ac6e78060976c49d6",
   "size": 68981
  },
  {
   "id": "ops-F00032",
   "role": "ops",
   "path": "docs/devin-kanban.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1b8b7c0937cedcf5f7c74c2a5acb5cd29cc60f81",
   "size": 66967
  },
  {
   "id": "ops-F00033",
   "role": "ops",
   "path": "docs/devin-milestone.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "de130fe53564fbffbccf6b0c7372e6fbe4d4fd36",
   "size": 80913
  },
  {
   "id": "ops-F00034",
   "role": "ops",
   "path": "docs/disk-reclaim.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ce38ba06e9234132d2faff7083973c5057a83aa",
   "size": 24424
  },
  {
   "id": "ops-F00035",
   "role": "ops",
   "path": "docs/github-subscription.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "67de8f0fe64e6457f13ecc5e90432b546abb56f7",
   "size": 6505
  },
  {
   "id": "ops-F00036",
   "role": "ops",
   "path": "docs/herdr-agents.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2627e08ba2cba9fcb794acceaa0fb9c4e4f813b4",
   "size": 6443
  },
  {
   "id": "ops-F00037",
   "role": "ops",
   "path": "docs/issue-refinement.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "35c8e821a1d456c3f4a354f3937952f5d9acc9d3",
   "size": 17571,
   "review_evidence": {
    "inspected_evidence": [
     "docs/issue-refinement.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00038",
   "role": "ops",
   "path": "docs/kanban-attention-cleanup.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5d40a61429bef08bc9ec835f2a0dc5e292e74218",
   "size": 10454,
   "review_evidence": {
    "inspected_evidence": [
     "docs/kanban-attention-cleanup.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00039",
   "role": "ops",
   "path": "docs/kanban-dev-action.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb6d0944eb81ed6997bf5b496ef037eddbb496e1",
   "size": 12220
  },
  {
   "id": "ops-F00040",
   "role": "ops",
   "path": "docs/kanban-dev-console.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d08ac0b1996e929f3e01cbed4745f5a851318af8",
   "size": 29374
  },
  {
   "id": "ops-F00041",
   "role": "ops",
   "path": "docs/kanban-dev-status-model.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2789b050d199637d6118dbfb07ff8504717f3ac8",
   "size": 17749
  },
  {
   "id": "ops-F00042",
   "role": "ops",
   "path": "docs/kanban-dev-status-rust-adapter.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "33280c81c4f0804facfe536ca2f011179c014e18",
   "size": 17301
  },
  {
   "id": "ops-F00043",
   "role": "ops",
   "path": "docs/kanban-dev-status-rust-cli.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "8efa5e5b5df50e7c6ad058b09536e2838affb303",
   "size": 6994
  },
  {
   "id": "ops-F00044",
   "role": "ops",
   "path": "docs/kanban-dev-status-rust-mcp.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "01f4bb1f80909dbd947da1a8c80a367202e77b12",
   "size": 8861
  },
  {
   "id": "ops-F00045",
   "role": "ops",
   "path": "docs/kanban-dev-status.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9b3a903c650f9ad447eedfb1e7968d6e14304c1",
   "size": 46201
  },
  {
   "id": "ops-F00046",
   "role": "ops",
   "path": "docs/kanban-development-console-v0.1.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6eb810dacebd6850bc720bc4149a2d61d3a850f1",
   "size": 17605
  },
  {
   "id": "ops-F00047",
   "role": "ops",
   "path": "docs/kanban-unstick.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "eb3670ccad9480ee60322e80d8e2012580cc4fdd",
   "size": 6760,
   "review_evidence": {
    "inspected_evidence": [
     "docs/kanban-unstick.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00048",
   "role": "ops",
   "path": "docs/local-agent-events.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "527b2f86fb2404a28eb0d550ffa343acfded75d0",
   "size": 28279
  },
  {
   "id": "ops-F00049",
   "role": "ops",
   "path": "docs/mdub-cron.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7878f4db25d5882458e12715a6d43c3d26ccc430",
   "size": 2112,
   "review_evidence": {
    "inspected_evidence": [
     "docs/mdub-cron.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00050",
   "role": "ops",
   "path": "docs/memory-incident-2026-10-28.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "520c98c3f6c3218c8ff79fc284145ddec7577449",
   "size": 5159,
   "review_evidence": {
    "inspected_evidence": [
     "docs/memory-incident-2026-10-28.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00051",
   "role": "ops",
   "path": "docs/memory-monitor.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "da54a9e6c53e70ea00b4eba6e08211601422d73e",
   "size": 6602,
   "review_evidence": {
    "inspected_evidence": [
     "docs/memory-monitor.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00052",
   "role": "ops",
   "path": "docs/merged-pr-reclaim.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e4760f1adec30deab968f5a39fce95ce4c9b1214",
   "size": 4158,
   "review_evidence": {
    "inspected_evidence": [
     "docs/merged-pr-reclaim.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00053",
   "role": "ops",
   "path": "docs/opencode-go.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "fed14384ec52178caef11631faebb3560beecc9e",
   "size": 5067,
   "review_evidence": {
    "inspected_evidence": [
     "docs/opencode-go.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00054",
   "role": "ops",
   "path": "docs/opencode-watch.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "add002c54800578ac35e7f82563c8cdd44567cb5",
   "size": 7224,
   "review_evidence": {
    "inspected_evidence": [
     "docs/opencode-watch.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00055",
   "role": "ops",
   "path": "docs/recovery-first-park-advance.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3259eaddae585674ce85f8d2dd962c75c4502452",
   "size": 21681,
   "review_evidence": {
    "inspected_evidence": [
     "docs/recovery-first-park-advance.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00056",
   "role": "ops",
   "path": "docs/repo-maintenance.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3904de7e7aec799b19a34185900b4b1a0238cd58",
   "size": 14843,
   "review_evidence": {
    "inspected_evidence": [
     "docs/repo-maintenance.md:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00057",
   "role": "ops",
   "path": "docs/shadow-review.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c9d04307c916713d44220dabc510570478489360",
   "size": 33595
  },
  {
   "id": "ops-F00058",
   "role": "ops",
   "path": "docs/triage-graft-artifact-analysis-2026-10-16.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "cf09244f4c9d753a649ff376f360c674ec03dfe7",
   "size": 15877
  },
  {
   "id": "ops-F00059",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation-manifest-2026-10-16.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "03f2f4a613ac0644a11d36cdd6c1233bd4ce4e86",
   "size": 2153760
  },
  {
   "id": "ops-F00060",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation-pilot-2026-10-16.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f15705e2c447aa1a682dc28f98ea28cb29e16ef",
   "size": 6443
  },
  {
   "id": "ops-F00061",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation-pilot-plan-2026-10-16.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ac0a8e534b3a83cceb09214f212daf1eec11432d",
   "size": 8348
  },
  {
   "id": "ops-F00062",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation-reclassified-2026-10-16.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "af51a20d47b429ad49ba5d247b7c927d053e6fa9",
   "size": 1326843
  },
  {
   "id": "ops-F00063",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation-report-2026-10-16.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4850e94073d0a42e30e88cf24b6ddcc1feeb5531",
   "size": 8473
  },
  {
   "id": "ops-F00064",
   "role": "ops",
   "path": "docs/triage-worktree-reclamation.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e473f4adc707d095969f034568c9e01ae46801ab",
   "size": 7416
  },
  {
   "id": "ops-F00065",
   "role": "ops",
   "path": "docs/webhook-operations.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5b190a6b86637e897567ee80b77dca14945890d5",
   "size": 6414
  },
  {
   "id": "ops-F00066",
   "role": "ops",
   "path": "docs/yuurei-e2e-notify.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f25a5084fd13bc0b219a84e5a3d0f8775298892",
   "size": 1527
  },
  {
   "id": "ops-F00067",
   "role": "ops",
   "path": "launchagents/ws.shimpei.memory-monitor.plist",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d00b1ed88e25e7cde49044e7ef8012a493309cc",
   "size": 689
  },
  {
   "id": "ops-F00068",
   "role": "ops",
   "path": "opencode/plugins/hermes-local-events.js",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9f01d759efdba989e53f796b46dd1b3c04c2588",
   "size": 7471
  },
  {
   "id": "ops-F00069",
   "role": "ops",
   "path": "scripts/blocker-reeval-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a4ea14d4ac626094d1995b6e6eaaf7bfc8b524f",
   "size": 1363,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/blocker-reeval-run.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00070",
   "role": "ops",
   "path": "scripts/blocker-reeval.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0a9a9cf8ae4215f33ce4033a6e0d90591666f7db",
   "size": 2694,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/blocker-reeval.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00071",
   "role": "ops",
   "path": "scripts/blocker_reeval_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d731baca174a7dbf19a34a8218821fd876c832c1",
   "size": 68825
  },
  {
   "id": "ops-F00072",
   "role": "ops",
   "path": "scripts/coding-worker-attention-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "9296dbecdf63f1e50bb2cd686fc6aedef39abe3c",
   "size": 26786
  },
  {
   "id": "ops-F00073",
   "role": "ops",
   "path": "scripts/coding-worker-resume-only.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "6159f47a07407c5380b411b0047ace9bc35a493d",
   "size": 7752
  },
  {
   "id": "ops-F00074",
   "role": "ops",
   "path": "scripts/coding_worker_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "09dba672f7f8055a63935ddc00f5d51d37011fe1",
   "size": 330745
  },
  {
   "id": "ops-F00075",
   "role": "ops",
   "path": "scripts/coding_worker_remote_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "842a1682a66dbf72396472a1016a046612d0e030",
   "size": 77659,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/coding_worker_remote_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00076",
   "role": "ops",
   "path": "scripts/cw-github-token-refresh.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "eb47df3092ceaf60bfa69250d2a8636805296f8e",
   "size": 22223,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-github-token-refresh.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00077",
   "role": "ops",
   "path": "scripts/cw-orphan-reaper-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0775a1092cbf98edd4830b768f40b821038a9612",
   "size": 1633,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-orphan-reaper-run.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00078",
   "role": "ops",
   "path": "scripts/cw-orphan-reaper.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "c496239eb72f58bfda655770152217c7d098a501",
   "size": 4790,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-orphan-reaper.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00079",
   "role": "ops",
   "path": "scripts/cw-round-prune.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "8d22b0676c01346d86968207c2f6cf9645fac95e",
   "size": 2487,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-round-prune.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00080",
   "role": "ops",
   "path": "scripts/cw-run-reclaim-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "47fcbe14d1d11dd5521db9411a472613747a98ee",
   "size": 3994,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-run-reclaim-run.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00081",
   "role": "ops",
   "path": "scripts/cw-run-reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "65f1c1262033c87124e13e2c93ae9a4e8b121d25",
   "size": 2577,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw-run-reclaim.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00082",
   "role": "ops",
   "path": "scripts/cw_orphan_reaper_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ac8849f01a2d1f30c7a21e504ffa3f1a7ab2cf3f",
   "size": 26141,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw_orphan_reaper_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00083",
   "role": "ops",
   "path": "scripts/cw_round_prune_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a50e6fb359fb99577c13ae3f65963380ec3235c2",
   "size": 28254,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw_round_prune_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00084",
   "role": "ops",
   "path": "scripts/cw_run_reclaim_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ae3056501f74c48ad646bba0592aaac4b75debb2",
   "size": 54219,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/cw_run_reclaim_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00085",
   "role": "ops",
   "path": "scripts/dependabot-pr-only.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "08a69ca81bb807a7cce14ca12c6f80e7628b813e",
   "size": 617
  },
  {
   "id": "ops-F00086",
   "role": "ops",
   "path": "scripts/devflow-scheduler-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "9bfac429f1ebb9389420282ed46fc875fb2bc163",
   "size": 1369
  },
  {
   "id": "ops-F00087",
   "role": "ops",
   "path": "scripts/devflow-scheduler.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0d7dee774b97867c7b2670ddf7b62d8dd0bd98fe",
   "size": 12987
  },
  {
   "id": "ops-F00088",
   "role": "ops",
   "path": "scripts/devflow_exclusion_comment_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "fcf5185d6f7fe236b0a83088cebb005886f43cb1",
   "size": 38057
  },
  {
   "id": "ops-F00089",
   "role": "ops",
   "path": "scripts/devflow_notice_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "664f7fee8704cc71bf4daa911df95fe57eabf846",
   "size": 22387
  },
  {
   "id": "ops-F00090",
   "role": "ops",
   "path": "scripts/devflow_remediation_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "752e5bdae71dcd5436e72dc759c685745f60bd0b",
   "size": 75471
  },
  {
   "id": "ops-F00091",
   "role": "ops",
   "path": "scripts/devflow_scheduler_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "41adb1986a9260ef239e3b5974d27f2728f1e3cf",
   "size": 247953
  },
  {
   "id": "ops-F00092",
   "role": "ops",
   "path": "scripts/devin-hooks.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "e89685ffe19a8c1bbe50773a2c20db33885ae4ba",
   "size": 9955
  },
  {
   "id": "ops-F00093",
   "role": "ops",
   "path": "scripts/devin-milestone-reconcile.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "805add174241eab24b15de751c81432344f52a94",
   "size": 11401
  },
  {
   "id": "ops-F00094",
   "role": "ops",
   "path": "scripts/devin-milestone.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "9f7e59c45b80936868987b59ec647781b29ca835",
   "size": 944879
  },
  {
   "id": "ops-F00095",
   "role": "ops",
   "path": "scripts/devin_dangerous_guard.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "ff2cd5f0cc20f7a9260d8f946b80bc135b76f479",
   "size": 29377
  },
  {
   "id": "ops-F00096",
   "role": "ops",
   "path": "scripts/devin_kanban_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2ca8f03f0e74471bc3ecbba09e78dfe1d2d76fd",
   "size": 44074
  },
  {
   "id": "ops-F00097",
   "role": "ops",
   "path": "scripts/devin_local_event_hook.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "cbcbcff659e84a365aa06510fa710be9ecc8245f",
   "size": 6293
  },
  {
   "id": "ops-F00098",
   "role": "ops",
   "path": "scripts/devin_milestone_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "cd68a07e8837e3f163120c396c66167103c10dc6",
   "size": 445997
  },
  {
   "id": "ops-F00099",
   "role": "ops",
   "path": "scripts/disk-reclaim-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d2219e75997b6138b056c96538fcaf3dbea2ac2d",
   "size": 1641
  },
  {
   "id": "ops-F00100",
   "role": "ops",
   "path": "scripts/disk-reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "389c8673ce30fa2e63b05ba764dd7b51025df83c",
   "size": 7306
  },
  {
   "id": "ops-F00101",
   "role": "ops",
   "path": "scripts/disk_reclaim_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "328dbdc7acdc93ff8f96bc64799ea3633222c9e9",
   "size": 35767
  },
  {
   "id": "ops-F00102",
   "role": "ops",
   "path": "scripts/github-ci-failure-only.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "ee92b0741bc4dc649a039b6185e0be5a6d9c672d",
   "size": 695
  },
  {
   "id": "ops-F00103",
   "role": "ops",
   "path": "scripts/github-subscription-manage.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "4aa151741bf13635f1274cb6bea59125ff5c38ba",
   "size": 11059
  },
  {
   "id": "ops-F00104",
   "role": "ops",
   "path": "scripts/github-subscription-monitor.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "5ca1ef05ff40492f526efae63bcd1b757de523f6",
   "size": 3258
  },
  {
   "id": "ops-F00105",
   "role": "ops",
   "path": "scripts/github-subscription-opencode-monitor.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "f52bf9b0d63972abd50bdc456b85e96a9b5502b8",
   "size": 1074
  },
  {
   "id": "ops-F00106",
   "role": "ops",
   "path": "scripts/github_subscription_lib.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "fccd27690c7cfbe122c9d3ce212f6d133cccbbe4",
   "size": 21341
  },
  {
   "id": "ops-F00107",
   "role": "ops",
   "path": "scripts/herdr-agents.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a7f58188414457fde4e6b637d8ed80c407f7549f",
   "size": 7745
  },
  {
   "id": "ops-F00108",
   "role": "ops",
   "path": "scripts/herdr_agents_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "5f43874b5e0b10684c89bbe9bd4293db6123bf84",
   "size": 11134
  },
  {
   "id": "ops-F00109",
   "role": "ops",
   "path": "scripts/hermes-setup-sync.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f0ec661388717ecdf3f9f7a116ca6bc21cadb01a",
   "size": 7471,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/hermes-setup-sync.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00110",
   "role": "ops",
   "path": "scripts/hermes_bridge_lib.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "9c4628e778bd0da224f5f79f1b51e57d7a99cbf3",
   "size": 11495
  },
  {
   "id": "ops-F00111",
   "role": "ops",
   "path": "scripts/hermes_mcp_bridge.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "e10329e6467aec1ba5c0d16ced921aca21f3fc6c",
   "size": 9917
  },
  {
   "id": "ops-F00112",
   "role": "ops",
   "path": "scripts/install-agent-access.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "c401b859a3ac92da7972b70bfd48bb946dfac17e",
   "size": 4316
  },
  {
   "id": "ops-F00113",
   "role": "ops",
   "path": "scripts/install-blocker-reeval.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "8d23be69d958a66d1eb2cb71b827f370db0d6e76",
   "size": 6599,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-blocker-reeval.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00114",
   "role": "ops",
   "path": "scripts/install-coding-worker.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "fbe8ba01ea78991fd0cdd0e074ada564e8cb4385",
   "size": 14202,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-coding-worker.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00115",
   "role": "ops",
   "path": "scripts/install-cw-github-token-refresh.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "f0b94caaa6b14ab74135bcd85d108c89d5c348bb",
   "size": 4694,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-cw-github-token-refresh.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00116",
   "role": "ops",
   "path": "scripts/install-cw-orphan-reaper.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8d4e474102866841efb068f4a72c36eb9e2bb0c",
   "size": 2565,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-cw-orphan-reaper.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00117",
   "role": "ops",
   "path": "scripts/install-devflow-scheduler.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "d5d5ac627380ccb9e05a6decf007600521a01bdc",
   "size": 5244,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-devflow-scheduler.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00118",
   "role": "ops",
   "path": "scripts/install-devin-hooks.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "770178459ead7d7e8590e698979a96b46a978308",
   "size": 1736
  },
  {
   "id": "ops-F00119",
   "role": "ops",
   "path": "scripts/install-devin-kanban.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "a3456ebe790698e6659711172993603e6633c96a",
   "size": 8282,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-devin-kanban.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00120",
   "role": "ops",
   "path": "scripts/install-devin-milestone.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "09345fb6f2f8f60f3e143664af4f735560e2bee6",
   "size": 9431,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-devin-milestone.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00121",
   "role": "ops",
   "path": "scripts/install-devin-repo-hooks.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "27a7feda9d8ea3c8ac42d27c4621653b5ddb546f",
   "size": 4843
  },
  {
   "id": "ops-F00122",
   "role": "ops",
   "path": "scripts/install-disk-reclaim.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "15e3f8b416de2ffaf8ce11f0eb0654b5b9f33a01",
   "size": 4158,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-disk-reclaim.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00123",
   "role": "ops",
   "path": "scripts/install-github-subscription.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "6f3d915c6fffa64d1b431ff6b073127c226fe895",
   "size": 1754
  },
  {
   "id": "ops-F00124",
   "role": "ops",
   "path": "scripts/install-herdr-agents.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "68e1217e3c470dc26884beaccb809719a2f627f2",
   "size": 848
  },
  {
   "id": "ops-F00125",
   "role": "ops",
   "path": "scripts/install-hermes-setup-sync.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "620fc08205210e835f071f16259c0a66018d8709",
   "size": 1825,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-hermes-setup-sync.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00126",
   "role": "ops",
   "path": "scripts/install-issue-refinement.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "bd497c549f7585c5126243c147830b50f1fab65f",
   "size": 3397
  },
  {
   "id": "ops-F00127",
   "role": "ops",
   "path": "scripts/install-kanban-attention-cleanup.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "91116123c3d07e63976ea8b82dfbed3a40820277",
   "size": 4045
  },
  {
   "id": "ops-F00128",
   "role": "ops",
   "path": "scripts/install-kanban-dev-console.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "a313b7776dd51b06db46d546793e21686239ec14",
   "size": 15559
  },
  {
   "id": "ops-F00129",
   "role": "ops",
   "path": "scripts/install-kanban-dev-status-rust.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "c8d17277790815b91fcfdb1af4a0842698a2df5b",
   "size": 9028
  },
  {
   "id": "ops-F00130",
   "role": "ops",
   "path": "scripts/install-kanban-dev-status.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "7f1a6646bbcdeb2f2a95e8e6720388f15f490365",
   "size": 5831
  },
  {
   "id": "ops-F00131",
   "role": "ops",
   "path": "scripts/install-kanban-unstick.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2713106204fc1e1703fd2ab31d373ef97a35fe41",
   "size": 4427,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/install-kanban-unstick.sh:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00132",
   "role": "ops",
   "path": "scripts/install-local-agent-events.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "89471052c7a4a7d9cebc256631414407e8f70b35",
   "size": 3325
  },
  {
   "id": "ops-F00133",
   "role": "ops",
   "path": "scripts/install-mdub-cron.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "c60143f4a2e4292096f67fb5e53cbeaa22ba6638",
   "size": 1177
  },
  {
   "id": "ops-F00134",
   "role": "ops",
   "path": "scripts/install-memory-monitor.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2bc837672d4f5552279cad0edafa156cbce535cb",
   "size": 1727
  },
  {
   "id": "ops-F00135",
   "role": "ops",
   "path": "scripts/install-opencode-plugin.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "8066a74a32f8c4c64589eb4b0d18d16569a1a283",
   "size": 4548
  },
  {
   "id": "ops-F00136",
   "role": "ops",
   "path": "scripts/install-opencode-watch.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "93f482868c63bdb0d1cec6a86be6671f4ee1d37a",
   "size": 5244
  },
  {
   "id": "ops-F00137",
   "role": "ops",
   "path": "scripts/install-repo-maintenance.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "5d2fbb392a32b15bacd3e2f768ba53b468e83b35",
   "size": 2208
  },
  {
   "id": "ops-F00138",
   "role": "ops",
   "path": "scripts/install-shadow-review.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "695fe750b2134710b910a6f2910649d946a63e56",
   "size": 4938
  },
  {
   "id": "ops-F00139",
   "role": "ops",
   "path": "scripts/install-triage-worktree-reclaim.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "742e278b12b1b37988f8cbc589c2f3591e043931",
   "size": 2037
  },
  {
   "id": "ops-F00140",
   "role": "ops",
   "path": "scripts/install-yuurei-e2e-notify.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2f22f8013d458485e30d9efdb3e739b5cd12cb6b",
   "size": 957
  },
  {
   "id": "ops-F00141",
   "role": "ops",
   "path": "scripts/issue-refinement-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4af16128874919f3526a2dffcfecd4a5d4b6c69",
   "size": 2343
  },
  {
   "id": "ops-F00142",
   "role": "ops",
   "path": "scripts/issue-refinement.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2747c4e6e9e0343fd217ce83822614d7fdbdc44",
   "size": 3720
  },
  {
   "id": "ops-F00143",
   "role": "ops",
   "path": "scripts/issue_refinement_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ad21e4c90446670e7a3a0040898ffe46e3a4675",
   "size": 78680
  },
  {
   "id": "ops-F00144",
   "role": "ops",
   "path": "scripts/kanban-attention-cleanup-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a0ada8c7dd42c3dcad598849bcf07e3111d4b074",
   "size": 964,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/kanban-attention-cleanup-run.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00145",
   "role": "ops",
   "path": "scripts/kanban-attention-cleanup.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "27ff860e45173c03b8fc7e918a891ecf26c8e5d4",
   "size": 2015,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/kanban-attention-cleanup.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00146",
   "role": "ops",
   "path": "scripts/kanban-dev-action.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f0848d9d09e36ecb9796ee30ca1889ca384da31",
   "size": 3067
  },
  {
   "id": "ops-F00147",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/Cargo.lock",
   "mode": "100644",
   "kind": "blob",
   "blob": "407a4a3b857001897702312f544d80ca70d7478b",
   "size": 17626
  },
  {
   "id": "ops-F00148",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "108537af7b99814a06b90a6645b558a8ebf0583b",
   "size": 486
  },
  {
   "id": "ops-F00149",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/action.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "fa747c0fcf7abe39f2d81424885378067126c228",
   "size": 77107
  },
  {
   "id": "ops-F00150",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/app.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "04452f8e9ba65b35bb97692cf21d4eab0566e78a",
   "size": 55217
  },
  {
   "id": "ops-F00151",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "fd45dbcbc4d1e37352436c9073b82ecff49769fe",
   "size": 9098
  },
  {
   "id": "ops-F00152",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/links.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "1fba3fe05de66c2c75b126b3bad7c4b5aaea8db4",
   "size": 3066
  },
  {
   "id": "ops-F00153",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/main.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d8aef5758280619695ef1bdc6cb842bafc17119",
   "size": 7110
  },
  {
   "id": "ops-F00154",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/render.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "130af40742b625bf34930e03373684e2f9b7b55b",
   "size": 4477
  },
  {
   "id": "ops-F00155",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/screens.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c6452ef9f68c0ae1102512b702b0af4d3dd19f10",
   "size": 33193
  },
  {
   "id": "ops-F00156",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/snapshot_store.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "a66ceb92410fdd2a9091511ddf71e3fad90c830e",
   "size": 25602
  },
  {
   "id": "ops-F00157",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/src/worker.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "466308ab9b17db0eba0a2b17e584617e616bca46",
   "size": 83851
  },
  {
   "id": "ops-F00158",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/conflict_unlinked/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a1f61a489b15fb1579b1b978dfeff17f0334b3d",
   "size": 394
  },
  {
   "id": "ops-F00159",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/conflict_unlinked/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "22c9b52169597cb10f7ae6543d57d33ff9c5941d",
   "size": 1925
  },
  {
   "id": "ops-F00160",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/conflict_unlinked/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "47b6ea6cda2ca10bd54f4a4ed0a715391eb1f79c",
   "size": 2159
  },
  {
   "id": "ops-F00161",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/conflict_unlinked/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fc66f5ccf6a6bd545cf38984dd51c795ab558792",
   "size": 1494
  },
  {
   "id": "ops-F00162",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_open/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c010f0b519709577a4fc156c76497b2a826a03d",
   "size": 983
  },
  {
   "id": "ops-F00163",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_open/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "161b6b1a7cb728ced08b4fdee44a518f94614a5e",
   "size": 2006
  },
  {
   "id": "ops-F00164",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_open/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d8c748575aa6727c62ab498c42ef38e1b3fcbe13",
   "size": 2428
  },
  {
   "id": "ops-F00165",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_open/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79cfdf0dced34a56690e0ac903066020a4ced4ff",
   "size": 1277
  },
  {
   "id": "ops-F00166",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_released/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c010f0b519709577a4fc156c76497b2a826a03d",
   "size": 983
  },
  {
   "id": "ops-F00167",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_released/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8766f7138a9f898dc708d4fa0881bc6f894ad450",
   "size": 2308
  },
  {
   "id": "ops-F00168",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_released/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d26ba6b82028d3fc06b698586251547e36559754",
   "size": 1851
  },
  {
   "id": "ops-F00169",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_released/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79cfdf0dced34a56690e0ac903066020a4ced4ff",
   "size": 1277
  },
  {
   "id": "ops-F00170",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_resolved/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c010f0b519709577a4fc156c76497b2a826a03d",
   "size": 983
  },
  {
   "id": "ops-F00171",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_resolved/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b786db03fd1d7add68396a7bdbfc6e7ba593293f",
   "size": 2181
  },
  {
   "id": "ops-F00172",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_resolved/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5e1922e0420e20c9bd2aabfb1e761a154b881bbc",
   "size": 2428
  },
  {
   "id": "ops-F00173",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_judgment_wait_resolved/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79cfdf0dced34a56690e0ac903066020a4ced4ff",
   "size": 1277
  },
  {
   "id": "ops-F00174",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_queue/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a1f61a489b15fb1579b1b978dfeff17f0334b3d",
   "size": 394
  },
  {
   "id": "ops-F00175",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_queue/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9c71dc46397920a3366fd0dba569900ce4c8812c",
   "size": 589
  },
  {
   "id": "ops-F00176",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_queue/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f2edfab335c62d99d285d4730247bbaedf9b9907",
   "size": 857
  },
  {
   "id": "ops-F00177",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/empty_queue/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cd84a71916fd771449eaf9c62f6f3af49c92a660",
   "size": 1277
  },
  {
   "id": "ops-F00178",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/ghost_card/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1148cb71f860c08a7097b029038857fe4e404049",
   "size": 989
  },
  {
   "id": "ops-F00179",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/ghost_card/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9e6bbc31fffe57a1ab3345be6e61b7bce9e4b608",
   "size": 1702
  },
  {
   "id": "ops-F00180",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/ghost_card/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "38b1c9a72e2a7c74bdfe1c04b2aba3d289f8fca3",
   "size": 1936
  },
  {
   "id": "ops-F00181",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/ghost_card/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79cfdf0dced34a56690e0ac903066020a4ced4ff",
   "size": 1277
  },
  {
   "id": "ops-F00182",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "fbfd5ed636f208f30dd9195bbe0cfd47f5316bc2",
   "size": 3403
  },
  {
   "id": "ops-F00183",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "f55783888d3f7babfa07e4905383180a226966f9",
   "size": 1492
  },
  {
   "id": "ops-F00184",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f770a8d475a8cb3d13f364e8857378831c4fe2",
   "size": 3369
  },
  {
   "id": "ops-F00185",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ffd1ff79b7a345aec1ff4e9c0640316857855b7",
   "size": 1456
  },
  {
   "id": "ops-F00186",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "74cd95fd2ffb6874edda09ba365b621ae0b2b608",
   "size": 3731
  },
  {
   "id": "ops-F00187",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9035f3b35ec7e2f97c40bd36bdf6a3a099d3cec3",
   "size": 1658
  },
  {
   "id": "ops-F00188",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e23611a3849a2ef464d1ce9b97c6164a3c50f25b",
   "size": 3331
  },
  {
   "id": "ops-F00189",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_conflict_unlinked_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00190",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "53fc9d2d472e143ebe3fceee32a395a9d00f3a86",
   "size": 3359
  },
  {
   "id": "ops-F00191",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "dbb8c855b92cd1bdf3e7e59aa25f29b61c65d432",
   "size": 1448
  },
  {
   "id": "ops-F00192",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "63de00bc2bff5bbed7dc4584c40a14077d0f295a",
   "size": 3369
  },
  {
   "id": "ops-F00193",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "7429c76779b15fd6fe0a0426f3c5d603e2b41e36",
   "size": 1456
  },
  {
   "id": "ops-F00194",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a53b026409f82b123cfc7d21f4de0d04c6722571",
   "size": 3705
  },
  {
   "id": "ops-F00195",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1210df81ac3c4ff8c1a04651a6f490c1b8374b1e",
   "size": 1632
  },
  {
   "id": "ops-F00196",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e495eef8a9138606f0e6b280afcdb77902a3ee61",
   "size": 3331
  },
  {
   "id": "ops-F00197",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_open_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00198",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "53fc9d2d472e143ebe3fceee32a395a9d00f3a86",
   "size": 3359
  },
  {
   "id": "ops-F00199",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "dbb8c855b92cd1bdf3e7e59aa25f29b61c65d432",
   "size": 1448
  },
  {
   "id": "ops-F00200",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "facd9e4aceefeb9d52fdddde4ce4e31d767e76f2",
   "size": 3367
  },
  {
   "id": "ops-F00201",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "aff807ad0df7b6099b4c59b13c33cc2fb8b8b404",
   "size": 1454
  },
  {
   "id": "ops-F00202",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a53b026409f82b123cfc7d21f4de0d04c6722571",
   "size": 3705
  },
  {
   "id": "ops-F00203",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1210df81ac3c4ff8c1a04651a6f490c1b8374b1e",
   "size": 1632
  },
  {
   "id": "ops-F00204",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e495eef8a9138606f0e6b280afcdb77902a3ee61",
   "size": 3331
  },
  {
   "id": "ops-F00205",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_released_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00206",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "53fc9d2d472e143ebe3fceee32a395a9d00f3a86",
   "size": 3359
  },
  {
   "id": "ops-F00207",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "dbb8c855b92cd1bdf3e7e59aa25f29b61c65d432",
   "size": 1448
  },
  {
   "id": "ops-F00208",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "5f7e9c8cb8cff52d8322cf919e22e8619c9bb2e4",
   "size": 3385
  },
  {
   "id": "ops-F00209",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "02f4d41c1847f0f2b8c0d4cf8b6f0f7774deac27",
   "size": 1472
  },
  {
   "id": "ops-F00210",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a53b026409f82b123cfc7d21f4de0d04c6722571",
   "size": 3705
  },
  {
   "id": "ops-F00211",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1210df81ac3c4ff8c1a04651a6f490c1b8374b1e",
   "size": 1632
  },
  {
   "id": "ops-F00212",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e495eef8a9138606f0e6b280afcdb77902a3ee61",
   "size": 3331
  },
  {
   "id": "ops-F00213",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_judgment_wait_resolved_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00214",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "3ca7657932b84e08576b960c9f4f7907f3e8c0b2",
   "size": 3381
  },
  {
   "id": "ops-F00215",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "31c67de4c70dfe94263c3bdd2323bcf6e38c6c4f",
   "size": 1470
  },
  {
   "id": "ops-F00216",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2d2ccd17368e9f95fe42e93075182ed8fab7e9a",
   "size": 3379
  },
  {
   "id": "ops-F00217",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "969629ae2e931fe1324942b8d7a2d96eb40e4158",
   "size": 1466
  },
  {
   "id": "ops-F00218",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "3ab0065edf222d1fdc6dfcee0e2e63392c1d7afb",
   "size": 3731
  },
  {
   "id": "ops-F00219",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9035f3b35ec7e2f97c40bd36bdf6a3a099d3cec3",
   "size": 1658
  },
  {
   "id": "ops-F00220",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "cc993ecd272abf6e78c0ba62b734fff3db678a51",
   "size": 3343
  },
  {
   "id": "ops-F00221",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_empty_queue_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "fda249134f3e3f48a56ec58409823140c06a94d7",
   "size": 1434
  },
  {
   "id": "ops-F00222",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "23193d32f297f150fee577bd606eb98a2635998e",
   "size": 3359
  },
  {
   "id": "ops-F00223",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "54c529b54360b58676064421f205bdc3b06553e0",
   "size": 1448
  },
  {
   "id": "ops-F00224",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f770a8d475a8cb3d13f364e8857378831c4fe2",
   "size": 3369
  },
  {
   "id": "ops-F00225",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ffd1ff79b7a345aec1ff4e9c0640316857855b7",
   "size": 1456
  },
  {
   "id": "ops-F00226",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "ad96d7f40a96894b23530bbd0822ae33e96be51d",
   "size": 3705
  },
  {
   "id": "ops-F00227",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a1f9a5c06cff964e1dcf2eae6335ef13a469d2b2",
   "size": 1632
  },
  {
   "id": "ops-F00228",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1a2b76d5f53533712d1b77978eb6cca6b8ea787f",
   "size": 3331
  },
  {
   "id": "ops-F00229",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_ghost_card_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00230",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "53fc9d2d472e143ebe3fceee32a395a9d00f3a86",
   "size": 3359
  },
  {
   "id": "ops-F00231",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "dbb8c855b92cd1bdf3e7e59aa25f29b61c65d432",
   "size": 1448
  },
  {
   "id": "ops-F00232",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "487920acdea787dfb7ecb6536e42cfc1c184de2e",
   "size": 3369
  },
  {
   "id": "ops-F00233",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "921f56c6f384cd9548635ea01f678d5c0603cdaf",
   "size": 1456
  },
  {
   "id": "ops-F00234",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a53b026409f82b123cfc7d21f4de0d04c6722571",
   "size": 3705
  },
  {
   "id": "ops-F00235",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1210df81ac3c4ff8c1a04651a6f490c1b8374b1e",
   "size": 1632
  },
  {
   "id": "ops-F00236",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e495eef8a9138606f0e6b280afcdb77902a3ee61",
   "size": 3331
  },
  {
   "id": "ops-F00237",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_judgment_wait_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "550d562c42091bc2ff5b32942f804b43c0212f85",
   "size": 1422
  },
  {
   "id": "ops-F00238",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "bf6931668093329adaba73c6581c0205dddeeb58",
   "size": 3359
  },
  {
   "id": "ops-F00239",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a5871f3495343a0cc52e95ddee7f7be1debc2f4d",
   "size": 1448
  },
  {
   "id": "ops-F00240",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f770a8d475a8cb3d13f364e8857378831c4fe2",
   "size": 3369
  },
  {
   "id": "ops-F00241",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ffd1ff79b7a345aec1ff4e9c0640316857855b7",
   "size": 1456
  },
  {
   "id": "ops-F00242",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "7044c0ba8770dc4c801f991e801181f7a5546423",
   "size": 3731
  },
  {
   "id": "ops-F00243",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9035f3b35ec7e2f97c40bd36bdf6a3a099d3cec3",
   "size": 1658
  },
  {
   "id": "ops-F00244",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "37c284ce84bc7f504ed9813c0aee333f3b84e221",
   "size": 3331
  },
  {
   "id": "ops-F00245",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_multiple_attempts_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e1660ffade88b678c11968dc8c20731620186819",
   "size": 1422
  },
  {
   "id": "ops-F00246",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "2cd025d0db44847f763bba22ab1277f913a02af0",
   "size": 3359
  },
  {
   "id": "ops-F00247",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "00e10ff5ea53908416b991b26808bf7358c3ed37",
   "size": 1448
  },
  {
   "id": "ops-F00248",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f770a8d475a8cb3d13f364e8857378831c4fe2",
   "size": 3369
  },
  {
   "id": "ops-F00249",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ffd1ff79b7a345aec1ff4e9c0640316857855b7",
   "size": 1456
  },
  {
   "id": "ops-F00250",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "25ef6c5c5c2c2889bee584fc1879c1676168a29f",
   "size": 3705
  },
  {
   "id": "ops-F00251",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1210df81ac3c4ff8c1a04651a6f490c1b8374b1e",
   "size": 1632
  },
  {
   "id": "ops-F00252",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e11a8ac32f62bc45f168e84341a475064780b836",
   "size": 3331
  },
  {
   "id": "ops-F00253",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_normal_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a98a093cc42ed1b2eb2394f00504f03b8e93b37",
   "size": 1422
  },
  {
   "id": "ops-F00254",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "15fdccb183c514458f3d34d73eb9013814235075",
   "size": 3453
  },
  {
   "id": "ops-F00255",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "7fcd68495db39973c581ef51d9089ab18d28dec8",
   "size": 1542
  },
  {
   "id": "ops-F00256",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "25da862b5790b5d2293c47323b4e199384a8e3c5",
   "size": 3463
  },
  {
   "id": "ops-F00257",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d8737f084ddabdcf34e71d57cda7f9cf765e7fbb",
   "size": 1550
  },
  {
   "id": "ops-F00258",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d83ee7d9f384b03dd60c7ab4b20f49df5e954df3",
   "size": 3843
  },
  {
   "id": "ops-F00259",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "5bc3570576be6df400dc70c7567ad2fae06c0a50",
   "size": 1752
  },
  {
   "id": "ops-F00260",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "434617f7b803b0c46b603d0a93923463f827313d",
   "size": 3425
  },
  {
   "id": "ops-F00261",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_partial_sources_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0e68d31d742083706d0876f04032463425e38b76",
   "size": 1516
  },
  {
   "id": "ops-F00262",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_detail_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9fa00a1f8342c4c7de66ce125f655c5a078486fc",
   "size": 3359
  },
  {
   "id": "ops-F00263",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_detail_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "ac5d760fcdd0491e75d008fd075567f4c23aede1",
   "size": 1448
  },
  {
   "id": "ops-F00264",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_judgment_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f770a8d475a8cb3d13f364e8857378831c4fe2",
   "size": 3369
  },
  {
   "id": "ops-F00265",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_judgment_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ffd1ff79b7a345aec1ff4e9c0640316857855b7",
   "size": 1456
  },
  {
   "id": "ops-F00266",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_overview_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "a3d799b2c9abfbeac4cab830c8389de9561ea47a",
   "size": 3731
  },
  {
   "id": "ops-F00267",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_overview_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9035f3b35ec7e2f97c40bd36bdf6a3a099d3cec3",
   "size": 1658
  },
  {
   "id": "ops-F00268",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_work_100x30.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "f56acf794b129060a3b63dad14c55fd5be79471d",
   "size": 3331
  },
  {
   "id": "ops-F00269",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/golden_recent_outcomes_work_60x20.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "948d0985bffe173349d3d52412fe671a3bece7a7",
   "size": 1422
  },
  {
   "id": "ops-F00270",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/judgment_wait/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c010f0b519709577a4fc156c76497b2a826a03d",
   "size": 983
  },
  {
   "id": "ops-F00271",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/judgment_wait/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9dac534a636734f8900ef601eed9a1c7f663f8c0",
   "size": 2515
  },
  {
   "id": "ops-F00272",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/judgment_wait/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cffd04d7244fe97fad247d285158badb94bd0724",
   "size": 2346
  },
  {
   "id": "ops-F00273",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/judgment_wait/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79cfdf0dced34a56690e0ac903066020a4ced4ff",
   "size": 1277
  },
  {
   "id": "ops-F00274",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/multiple_attempts/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a1f61a489b15fb1579b1b978dfeff17f0334b3d",
   "size": 394
  },
  {
   "id": "ops-F00275",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/multiple_attempts/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f2834ecbd33446e5cf22d8208d4a5ea7c683756",
   "size": 2642
  },
  {
   "id": "ops-F00276",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/multiple_attempts/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "feccf07d83f7591a202dff29025a9096dc022fcb",
   "size": 1856
  },
  {
   "id": "ops-F00277",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/multiple_attempts/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "df7e5140bef18722d88a65fa8942be70444ed65e",
   "size": 1277
  },
  {
   "id": "ops-F00278",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/normal/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c010f0b519709577a4fc156c76497b2a826a03d",
   "size": 983
  },
  {
   "id": "ops-F00279",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/normal/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e153058e6a30af49c2c5eae0a8d56ecacc27aa08",
   "size": 2056
  },
  {
   "id": "ops-F00280",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/normal/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7a2160420a89a375ada96ea10779fa2be556a2fb",
   "size": 1886
  },
  {
   "id": "ops-F00281",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/normal/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "df7e5140bef18722d88a65fa8942be70444ed65e",
   "size": 1277
  },
  {
   "id": "ops-F00282",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/partial_sources/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e94849d415b32f51b8f3cd7112adaa6ce22ead92",
   "size": 393
  },
  {
   "id": "ops-F00283",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/partial_sources/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "10c255b0ef8d5eeaae6c82b85f8d63f0726ec3f5",
   "size": 1637
  },
  {
   "id": "ops-F00284",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/partial_sources/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4af3cb5ed98264e7b48ca5263adaedc1a8260722",
   "size": 1906
  },
  {
   "id": "ops-F00285",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/partial_sources/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "62d38742335cc49ee0df775bbaf4135af26c6ab5",
   "size": 1298
  },
  {
   "id": "ops-F00286",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/recent_outcomes/actions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a1f61a489b15fb1579b1b978dfeff17f0334b3d",
   "size": 394
  },
  {
   "id": "ops-F00287",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/recent_outcomes/detail.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "02517838f0ab55c3d17cfa6b1c7e03882bd32067",
   "size": 1687
  },
  {
   "id": "ops-F00288",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/recent_outcomes/status.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "254c2be374fea68ea109f7ec35090208986ff22d",
   "size": 1880
  },
  {
   "id": "ops-F00289",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/fixtures/recent_outcomes/summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "231fdbbbd229c451f15e6f3a77f5f428d98124cf",
   "size": 1697
  },
  {
   "id": "ops-F00290",
   "role": "ops",
   "path": "scripts/kanban-dev-console-rust/tests/golden.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "71fb168faf036f56925e4b7905c82ec786b24d74",
   "size": 3052
  },
  {
   "id": "ops-F00291",
   "role": "ops",
   "path": "scripts/kanban-dev-console.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "b7481e96818808467219c9ee44b6d4bf9bd6d748",
   "size": 8290
  },
  {
   "id": "ops-F00292",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/Cargo.lock",
   "mode": "100644",
   "kind": "blob",
   "blob": "299e51144aa6b462acbdf7f385c68ac6fd9a3773",
   "size": 6667
  },
  {
   "id": "ops-F00293",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "d15a74ece02a123a6e00b8dca33ea6dd0f75a8ef",
   "size": 561
  },
  {
   "id": "ops-F00294",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/bin/kanban-dev-status-mcp.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "644fe89e7f1b166ad7838eb306a4bb50f4c9d721",
   "size": 1762
  },
  {
   "id": "ops-F00295",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/bin/kanban-dev-status.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "5935e50e934af6a254910397acadb2beddd75402",
   "size": 11239
  },
  {
   "id": "ops-F00296",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/config.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "14025a9f5175314f4c315489ff2d862d87361070",
   "size": 5894
  },
  {
   "id": "ops-F00297",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/error.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "96230129a7e12d587703e4fa7b519dd7016bc5a3",
   "size": 6799
  },
  {
   "id": "ops-F00298",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "6349b4cf1a1b88756f6686b104bf9c0381addeaa",
   "size": 7582
  },
  {
   "id": "ops-F00299",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/mcp.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "f400b9bfce8bd44bdd0ea97257bfa5b33358ab3e",
   "size": 39425
  },
  {
   "id": "ops-F00300",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/process.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "5dc07dbec9de731f56fe5f28cde1c6dc18f8f15d",
   "size": 16227
  },
  {
   "id": "ops-F00301",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/src/schema.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "223d4b6abbdac306ef6d9916740bd5b8db777cbf",
   "size": 13298
  },
  {
   "id": "ops-F00302",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/cli.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "495d0157d97cd6b04b78088a8f8ab4588ef2ac87",
   "size": 18613
  },
  {
   "id": "ops-F00303",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/actions_ok.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a0bd9d8e7f60cea59122ddfffafc8f72368becad",
   "size": 17738
  },
  {
   "id": "ops-F00304",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/attention_all.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0b6752f6d8afd218a5d35099aa579169cda0b5c5",
   "size": 897
  },
  {
   "id": "ops-F00305",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/attention_current.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4aec484538bb2ad4280b2b3d8316445cd6b6c0d4",
   "size": 598
  },
  {
   "id": "ops-F00306",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/attention_history.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "eb541502984a5a4608905c4f152bac2a44d4800d",
   "size": 586
  },
  {
   "id": "ops-F00307",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/attention_unknown_fields.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9cf084be38e255124d744ef47d7fc9d6bed05007",
   "size": 704
  },
  {
   "id": "ops-F00308",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/detail_not_found.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1d9f60ec6c767096db55da40c6f5e0ec1ca2c0de",
   "size": 2517
  },
  {
   "id": "ops-F00309",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/detail_ok.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a37d43e4c329babc13ed054e8e0dcd8c1aaeb3ea",
   "size": 3082
  },
  {
   "id": "ops-F00310",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/human_attention.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "b422a29790925dbd85544f1e902222105ee55f9b",
   "size": 104
  },
  {
   "id": "ops-F00311",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/human_status.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "96a1eed00174f55183d1b9a38ad1470ec6f61176",
   "size": 142
  },
  {
   "id": "ops-F00312",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/malformed.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "87b0d274cafff68ce470083ec50562b79b3a7794",
   "size": 17
  },
  {
   "id": "ops-F00313",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/status_ok.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cf3fcf6f5f4d89ff603107d8bb393c4346e8025c",
   "size": 690
  },
  {
   "id": "ops-F00314",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/status_partial.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4fb24524d2efc76a145945f34b9da92aa326f4b9",
   "size": 539
  },
  {
   "id": "ops-F00315",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/status_sources_unreadable.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f4cc0740bb90d05d27ea0dfca6b4bfc1012b3978",
   "size": 371
  },
  {
   "id": "ops-F00316",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/summary_bad_version.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d21fe94a9a6e6d0ddc0e62eb89f0ff60ea908758",
   "size": 8872
  },
  {
   "id": "ops-F00317",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/summary_ok.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "147f2fa43c971c3fd0f0e7830cf0fa8cd1f7140e",
   "size": 8872
  },
  {
   "id": "ops-F00318",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/summary_unknown_fields.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9bb978e64ced9538f976ab84079fcf726e0c40de",
   "size": 8912
  },
  {
   "id": "ops-F00319",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/wrong_schema.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c571b69baa76df2d9da0879ba326342e52dad50f",
   "size": 228
  },
  {
   "id": "ops-F00320",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/fixtures/wrong_types.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9fd437eab217a509643422fb8a11320d376a5424",
   "size": 228
  },
  {
   "id": "ops-F00321",
   "role": "ops",
   "path": "scripts/kanban-dev-status-rust/tests/mcp.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "ee51aad97c0d1e119792ae66b98553d94633353b",
   "size": 31465
  },
  {
   "id": "ops-F00322",
   "role": "ops",
   "path": "scripts/kanban-dev-status.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "abbb787329e3380d2ad29045b0943f313b01aa7b",
   "size": 9204
  },
  {
   "id": "ops-F00323",
   "role": "ops",
   "path": "scripts/kanban-unstick-run.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "ea75e00b698617cb1c4f8aef647ba724b598b934",
   "size": 1509,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/kanban-unstick-run.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00324",
   "role": "ops",
   "path": "scripts/kanban-unstick.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "12aa6476fe1ff09e12bc5cb5fcd5d9179122d776",
   "size": 4415,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/kanban-unstick.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00325",
   "role": "ops",
   "path": "scripts/kanban_attention_cleanup_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "76ed908bf05bf2091dd117d4f056459b64718236",
   "size": 40047
  },
  {
   "id": "ops-F00326",
   "role": "ops",
   "path": "scripts/kanban_dev_action_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ecf63667b5cc6d1d3dc5c0dce7be16ec0d8addbe",
   "size": 50102
  },
  {
   "id": "ops-F00327",
   "role": "ops",
   "path": "scripts/kanban_dev_console_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f735e69af142c57c226d9543f38247f569742093",
   "size": 45895
  },
  {
   "id": "ops-F00328",
   "role": "ops",
   "path": "scripts/kanban_dev_status_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "dcec59726de0c6b6c2a2c8c882a7c3ad667d79a4",
   "size": 259402
  },
  {
   "id": "ops-F00329",
   "role": "ops",
   "path": "scripts/kanban_unstick_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f86d3262ecb66d6d286a313878bf0d593510e1f",
   "size": 29199,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/kanban_unstick_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00330",
   "role": "ops",
   "path": "scripts/local-agent-events.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "ac7fb77dde10d4a3557f9972d8ba37777a44e0be",
   "size": 15200
  },
  {
   "id": "ops-F00331",
   "role": "ops",
   "path": "scripts/local_agent_events_lib.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "5b8d9535f36046b88bb13837949900e2295a33b9",
   "size": 50245
  },
  {
   "id": "ops-F00332",
   "role": "ops",
   "path": "scripts/memory-monitor.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f30237b004779917da159e5f9277d545805137d",
   "size": 13207
  },
  {
   "id": "ops-F00333",
   "role": "ops",
   "path": "scripts/merged-pr-reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f1b0c9bf161e672b31e5219f22c319c6b5082791",
   "size": 2727,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/merged-pr-reclaim.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00334",
   "role": "ops",
   "path": "scripts/merged_pr_reclaim_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ac8998597c63719a1b0ae35624c7ff6a6dd353ac",
   "size": 23607,
   "review_evidence": {
    "inspected_evidence": [
     "scripts/merged_pr_reclaim_lib.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00335",
   "role": "ops",
   "path": "scripts/repo-maintenance-run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "74a854c682ba001b4022c9898672c2ffb0aa5f33",
   "size": 1474
  },
  {
   "id": "ops-F00336",
   "role": "ops",
   "path": "scripts/repo-maintenance.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "50fbb167d1f52c170f930bef1d3d195446af4eba",
   "size": 11650
  },
  {
   "id": "ops-F00337",
   "role": "ops",
   "path": "scripts/repo_maintenance_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e39e846508747b9c6bbfc48f8cea8635ec9ebbd1",
   "size": 96147
  },
  {
   "id": "ops-F00338",
   "role": "ops",
   "path": "scripts/session_ledger_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "df29a9d3866be9ef512d8286a88d1f25a26d2887",
   "size": 82764
  },
  {
   "id": "ops-F00339",
   "role": "ops",
   "path": "scripts/shadow-debounce-dispatch.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "90d959dfc5fab3b0d045ef44a63b42a75b3c83de",
   "size": 2627
  },
  {
   "id": "ops-F00340",
   "role": "ops",
   "path": "scripts/shadow-review-only.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "33ee85a59ca7ded7b91e56a7823f943b854f02a1",
   "size": 5910
  },
  {
   "id": "ops-F00341",
   "role": "ops",
   "path": "scripts/triage-worktree-reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "923e54f171a047e66c071ec15b30f10dc90e49cd",
   "size": 6336
  },
  {
   "id": "ops-F00342",
   "role": "ops",
   "path": "scripts/triage_worktree_reclaim_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "c81d5a564d255d71a22ba3001b5654abb1163f59",
   "size": 31361
  },
  {
   "id": "ops-F00343",
   "role": "ops",
   "path": "scripts/verify-webhook.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2a22b910f1a2b5185676a61e0b56d51b9960b7c2",
   "size": 2810
  },
  {
   "id": "ops-F00344",
   "role": "ops",
   "path": "scripts/yuurei-nightly-e2e-only.py",
   "mode": "100755",
   "kind": "blob",
   "blob": "7a27d0e3a1b51b82befd72a7dd4969f28fc0ddbc",
   "size": 5490
  },
  {
   "id": "ops-F00345",
   "role": "ops",
   "path": "skills/dependabot-pr/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "93d2f62969a0dc1abde823e96dc10780db727081",
   "size": 1783
  },
  {
   "id": "ops-F00346",
   "role": "ops",
   "path": "skills/devflow-execute/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4d054d45204b321f335e48bfece5034d65f59a4",
   "size": 9058
  },
  {
   "id": "ops-F00347",
   "role": "ops",
   "path": "skills/devin-delegate/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "bc717c118d959d72fa21be5ec551b419048036c1",
   "size": 8626
  },
  {
   "id": "ops-F00348",
   "role": "ops",
   "path": "skills/devin-milestone/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "8601b4f7a804873a2c3b06bdb19ca37ba76e8755",
   "size": 14661
  },
  {
   "id": "ops-F00349",
   "role": "ops",
   "path": "skills/disk-reclaim/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "af57b1e0c3448a35fc3414d5b5b347f32d21b8af",
   "size": 13512
  },
  {
   "id": "ops-F00350",
   "role": "ops",
   "path": "skills/github-ci-failure/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a1c8d027167baed8249eec9e94def9385108dc25",
   "size": 2300
  },
  {
   "id": "ops-F00351",
   "role": "ops",
   "path": "skills/github-subscription/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "52babe7e4edb3deeef08c21fbad4959996114c96",
   "size": 2586
  },
  {
   "id": "ops-F00352",
   "role": "ops",
   "path": "skills/herdr-agents/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1a14ff97b39044b8f47c44010b1908eea0fbdc2b",
   "size": 4242
  },
  {
   "id": "ops-F00353",
   "role": "ops",
   "path": "skills/issue-refinement/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "62a570fb3dd017dde01c434579cfadd07151611a",
   "size": 2368
  },
  {
   "id": "ops-F00354",
   "role": "ops",
   "path": "skills/kanban-dev-action/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "0fbf0f09e2a133156cb9f34be0a9e46a035ad04a",
   "size": 31235
  },
  {
   "id": "ops-F00355",
   "role": "ops",
   "path": "skills/kanban-dev-status/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3935e6d480a4ba0ade0bb1674d47711f4684a4c0",
   "size": 10085
  },
  {
   "id": "ops-F00356",
   "role": "ops",
   "path": "skills/pr-sweep/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "757c4265d27c842a570d11216748940f249254b3",
   "size": 4930
  },
  {
   "id": "ops-F00357",
   "role": "ops",
   "path": "skills/repo-maintenance/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "105cb9d3597763057de394877997a45623f846c5",
   "size": 2145
  },
  {
   "id": "ops-F00358",
   "role": "ops",
   "path": "skills/shadow-review/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f0fb49a1c8acc876766a2b2cef529f09aee7385f",
   "size": 3837
  },
  {
   "id": "ops-F00359",
   "role": "ops",
   "path": "tests/fixtures/coding-worker-run-events.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f4f14f43f52d516bab8f4dfb4dbb26f429e174ea",
   "size": 520
  },
  {
   "id": "ops-F00360",
   "role": "ops",
   "path": "tests/fixtures/comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "833dd1c6f899eb5d54a5431735fe7a0b8b3bf804",
   "size": 143
  },
  {
   "id": "ops-F00361",
   "role": "ops",
   "path": "tests/fixtures/core_closed_issue.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "52d65aa66eb4ec38eb2fc241bebc0c1f471ff7cd",
   "size": 244
  },
  {
   "id": "ops-F00362",
   "role": "ops",
   "path": "tests/fixtures/devflow_vendor/devflow/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "5e9ee685d594a33530ff5a6c71abe63c34a836e5",
   "size": 75
  },
  {
   "id": "ops-F00363",
   "role": "ops",
   "path": "tests/fixtures/devflow_vendor/devflow/core/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "ops-F00364",
   "role": "ops",
   "path": "tests/fixtures/devflow_vendor/devflow/core/deps.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a1908b0a5f4b11bc7bb3c05c861f8d0afe3691d1",
   "size": 10053
  },
  {
   "id": "ops-F00365",
   "role": "ops",
   "path": "tests/fixtures/devflow_vendor/devflow/tracker/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "ops-F00366",
   "role": "ops",
   "path": "tests/fixtures/devflow_vendor/devflow/tracker/snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "cffaed070f08208c8e10ef8852d21f80bd13172b",
   "size": 3802
  },
  {
   "id": "ops-F00367",
   "role": "ops",
   "path": "tests/fixtures/events.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0e2f980b5d53dd255e260648e6dda4f5e2cac808",
   "size": 62
  },
  {
   "id": "ops-F00368",
   "role": "ops",
   "path": "tests/fixtures/herdr_agents.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b38f6bbf01295bab35f206132e46612b8ba89c0c",
   "size": 1825
  },
  {
   "id": "ops-F00369",
   "role": "ops",
   "path": "tests/fixtures/herdr_explain_blocked.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f96550c99886e23932dba8763827bfd3ef65fc95",
   "size": 384
  },
  {
   "id": "ops-F00370",
   "role": "ops",
   "path": "tests/fixtures/herdr_read_blocked.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "8a7db9396bd4e0163e9e9f93379b76ac22820c05",
   "size": 591
  },
  {
   "id": "ops-F00371",
   "role": "ops",
   "path": "tests/fixtures/herdr_read_done.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "0a4cf2a7cf909d0053313fafb2097b746e0031b0",
   "size": 656
  },
  {
   "id": "ops-F00372",
   "role": "ops",
   "path": "tests/fixtures/kanban_dev_actions_golden.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c33d524264e424282c7b542101a6f97b9674a013",
   "size": 2025
  },
  {
   "id": "ops-F00373",
   "role": "ops",
   "path": "tests/fixtures/kanban_dev_actions_golden.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e48ef0ecfe77d6afccd27b908d02e0ab284a7946",
   "size": 326
  },
  {
   "id": "ops-F00374",
   "role": "ops",
   "path": "tests/fixtures/kanban_dev_summary_golden.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "095c960f8f1ba7a278680173e69b71c0248d6c5a",
   "size": 4661
  },
  {
   "id": "ops-F00375",
   "role": "ops",
   "path": "tests/fixtures/kanban_dev_summary_golden.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d4e093397685e9e107a4fdccb40ece7deb1c7213",
   "size": 864
  },
  {
   "id": "ops-F00376",
   "role": "ops",
   "path": "tests/fixtures/pull_comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f158915869d20e88b95c5f4d8fde7767e889f802",
   "size": 175
  },
  {
   "id": "ops-F00377",
   "role": "ops",
   "path": "tests/fixtures/repo_items.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4ad6fdb1874a40329ab038150d1a0500757a1ff7",
   "size": 522
  },
  {
   "id": "ops-F00378",
   "role": "ops",
   "path": "tests/fixtures/reviews.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "05cb3448d9e0948bd9d0a329e2b9bbadaaae6fd8",
   "size": 269
  },
  {
   "id": "ops-F00379",
   "role": "ops",
   "path": "tests/fixtures/selection_0_4.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1872842b0b0b9f72fd7aeb6f363688998f7fce5f",
   "size": 11095
  },
  {
   "id": "ops-F00380",
   "role": "ops",
   "path": "tests/test_blocker_reeval.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e7bee6b20700bf29c2dc61347f3851c0edc0908c",
   "size": 57708
  },
  {
   "id": "ops-F00381",
   "role": "ops",
   "path": "tests/test_coding_worker_attention_run.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "6417365f584ca561a3d8048ebd472ff6a40a4bbd",
   "size": 59591
  },
  {
   "id": "ops-F00382",
   "role": "ops",
   "path": "tests/test_coding_worker_key_schema.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ca5032858f86029fe32f6accd797838c774289f8",
   "size": 11976
  },
  {
   "id": "ops-F00383",
   "role": "ops",
   "path": "tests/test_coding_worker_lane.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "790014b8be8d2cbe3c8aa221fa8aa73d1f72d221",
   "size": 663395
  },
  {
   "id": "ops-F00384",
   "role": "ops",
   "path": "tests/test_coding_worker_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "3b03e43a7af51b0f622242ea9f2884af1efa496a",
   "size": 396167
  },
  {
   "id": "ops-F00385",
   "role": "ops",
   "path": "tests/test_coding_worker_remote.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ce7286a9678da1e504765eea2541e4e763d38d43",
   "size": 409109
  },
  {
   "id": "ops-F00386",
   "role": "ops",
   "path": "tests/test_coding_worker_resume_filter.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "66145eaf919f2673d7e32378b9f3a42aeb39df21",
   "size": 15957
  },
  {
   "id": "ops-F00387",
   "role": "ops",
   "path": "tests/test_cw_github_token_refresh.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "1c2230297bd518e7a699aed2a36fd007c90691fd",
   "size": 30993,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_cw_github_token_refresh.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00388",
   "role": "ops",
   "path": "tests/test_cw_orphan_reaper.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "28a213b9687883f6fc162f6c466a49e365f0672a",
   "size": 26629,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_cw_orphan_reaper.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00389",
   "role": "ops",
   "path": "tests/test_cw_round_prune.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a5a85562e5127f4a3a8b540243b081f74cfc022d",
   "size": 37635,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_cw_round_prune.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00390",
   "role": "ops",
   "path": "tests/test_cw_run_reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e1d867e8b78447acd1ef8a21946fb458d56c5518",
   "size": 98991,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_cw_run_reclaim.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00391",
   "role": "ops",
   "path": "tests/test_devflow_exclusion_comment.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "779916e8e360ce184b52d3657aebf46f13e139c1",
   "size": 38056
  },
  {
   "id": "ops-F00392",
   "role": "ops",
   "path": "tests/test_devflow_notice.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "1d08ec232cb6555e31284cc6e3a926d69d874d7c",
   "size": 26174
  },
  {
   "id": "ops-F00393",
   "role": "ops",
   "path": "tests/test_devflow_remediation.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f59d33ecc56c4b512f24f7317c043444fc43eab5",
   "size": 108197
  },
  {
   "id": "ops-F00394",
   "role": "ops",
   "path": "tests/test_devflow_scheduler.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "8c477b9ebc49036d95e2be3d1a9054e5adfd2048",
   "size": 324797
  },
  {
   "id": "ops-F00395",
   "role": "ops",
   "path": "tests/test_devin_hooks.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc29a4fb2bb21ec2ff246803e1f9c06f8fa79012",
   "size": 17152
  },
  {
   "id": "ops-F00396",
   "role": "ops",
   "path": "tests/test_devin_kanban.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b700d27497231a6098eaf6fd6e010648169416ff",
   "size": 369495
  },
  {
   "id": "ops-F00397",
   "role": "ops",
   "path": "tests/test_devin_milestone.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "af258475bea2a27527c5f1b389242e061ab74d6d",
   "size": 939545
  },
  {
   "id": "ops-F00398",
   "role": "ops",
   "path": "tests/test_disk_reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "99f646147eb8ccb2580585bfef41f9f5a49aa425",
   "size": 38178
  },
  {
   "id": "ops-F00399",
   "role": "ops",
   "path": "tests/test_github_subscription.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "5a608505b6428f05f17f29e6d1e3f53b8dc594f3",
   "size": 38132
  },
  {
   "id": "ops-F00400",
   "role": "ops",
   "path": "tests/test_herdr_agents.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "056b8a28ec81952150283269b0c1c4861b7647e5",
   "size": 18662
  },
  {
   "id": "ops-F00401",
   "role": "ops",
   "path": "tests/test_hermes_bridge_lib.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5c1a1929efe00dc51f6088e201c7ad5de6b4929",
   "size": 10916
  },
  {
   "id": "ops-F00402",
   "role": "ops",
   "path": "tests/test_hermes_setup_sync.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c6a9c4ad91e98f9883e719d9ed2ea19c9df012a",
   "size": 6409,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_hermes_setup_sync.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00403",
   "role": "ops",
   "path": "tests/test_issue_refinement.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "30823ee10cefd13642c19b7c03a456cf6a4aebbd",
   "size": 96275
  },
  {
   "id": "ops-F00404",
   "role": "ops",
   "path": "tests/test_kanban_attention_cleanup.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f99d890cc32c928f08634d28f9803d89bf51bd65",
   "size": 49720
  },
  {
   "id": "ops-F00405",
   "role": "ops",
   "path": "tests/test_kanban_dev_action.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4da9086898316d50b3e4e5567d47afc7b0cded0b",
   "size": 38748
  },
  {
   "id": "ops-F00406",
   "role": "ops",
   "path": "tests/test_kanban_dev_action_skill.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "377305a9914890ca720ca72ba2301976bb8774ac",
   "size": 26066
  },
  {
   "id": "ops-F00407",
   "role": "ops",
   "path": "tests/test_kanban_dev_console_fixtures.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4216022ed05b3242c4ab3303ca3a890e41e3e7ed",
   "size": 20789
  },
  {
   "id": "ops-F00408",
   "role": "ops",
   "path": "tests/test_kanban_dev_console_install.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "7257438e23894b51de5b2ce16749801a9a168aad",
   "size": 37581
  },
  {
   "id": "ops-F00409",
   "role": "ops",
   "path": "tests/test_kanban_dev_console_package.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc0856816d736c6f5f23bf11ab475536c60ba3f7",
   "size": 47275
  },
  {
   "id": "ops-F00410",
   "role": "ops",
   "path": "tests/test_kanban_dev_console_snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "398a09b487a373dc1b6d72a48844e765ae9619d1",
   "size": 6739
  },
  {
   "id": "ops-F00411",
   "role": "ops",
   "path": "tests/test_kanban_dev_console_workflows.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e65834e85427976deae263bf46f40d5b8a46fe63",
   "size": 20492
  },
  {
   "id": "ops-F00412",
   "role": "ops",
   "path": "tests/test_kanban_dev_detail.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "7fcd896fab1794e04ea3d164b8c2a88824ce7555",
   "size": 30458
  },
  {
   "id": "ops-F00413",
   "role": "ops",
   "path": "tests/test_kanban_dev_status.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "c5ee75bb2a9ae454129d9824688b08900cb0fb52",
   "size": 259747
  },
  {
   "id": "ops-F00414",
   "role": "ops",
   "path": "tests/test_kanban_dev_status_e2e.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "95b2eedafd358619f9a28b3b97354e026860735c",
   "size": 19421
  },
  {
   "id": "ops-F00415",
   "role": "ops",
   "path": "tests/test_kanban_dev_status_mcp.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0c37b508818c8b4e4e819dbcac60db1e134b2f8b",
   "size": 32399
  },
  {
   "id": "ops-F00416",
   "role": "ops",
   "path": "tests/test_kanban_dev_status_rust.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "aca2558e6185e05d876f364c992109ed0c226e72",
   "size": 33010
  },
  {
   "id": "ops-F00417",
   "role": "ops",
   "path": "tests/test_kanban_unstick.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "77393fde9617e7defb8a4319e6bbe7d7ba7de270",
   "size": 33216,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_kanban_unstick.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00418",
   "role": "ops",
   "path": "tests/test_local_agent_events.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e6d077b43de0bc687072d971bb9bd0bb0d1f8959",
   "size": 39568
  },
  {
   "id": "ops-F00419",
   "role": "ops",
   "path": "tests/test_memory_monitor.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "1e7d9c5ef1dd83bbea4db08b5687a9dcbc56697a",
   "size": 30748
  },
  {
   "id": "ops-F00420",
   "role": "ops",
   "path": "tests/test_merged_pr_reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "33f44fd1a270decff4fef1b902e8b1ff6d786830",
   "size": 21204,
   "review_evidence": {
    "inspected_evidence": [
     "tests/test_merged_pr_reclaim.py:entire file"
    ],
    "regions": []
   }
  },
  {
   "id": "ops-F00421",
   "role": "ops",
   "path": "tests/test_observability_status.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a9064f5b8d12514b1898a036fbb2aa40d5fbb8ef",
   "size": 16448
  },
  {
   "id": "ops-F00422",
   "role": "ops",
   "path": "tests/test_opencode_plugin.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "dd0e9c1444ef9f7702d5670a075911afad9b40a9",
   "size": 15272
  },
  {
   "id": "ops-F00423",
   "role": "ops",
   "path": "tests/test_repo_maintenance.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b554b66181bf63db26f9127342746d53590a7720",
   "size": 104086
  },
  {
   "id": "ops-F00424",
   "role": "ops",
   "path": "tests/test_repo_root_from_gh.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ca06a76b5ccdf5ea2ea4a25e5a0730fef42b18bd",
   "size": 20131
  },
  {
   "id": "ops-F00425",
   "role": "ops",
   "path": "tests/test_session_ledger.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e30630cde6d4b411b68fd03b5dcd32a5840d0046",
   "size": 106730
  },
  {
   "id": "ops-F00426",
   "role": "ops",
   "path": "tests/test_shadow_review_filter.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4a083cd1cfa7efddcc1deddb4fee8bc54d3d8294",
   "size": 20108
  },
  {
   "id": "ops-F00427",
   "role": "ops",
   "path": "tests/test_triage_worktree_reclaim.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "7ee7c21b059f4824092c51663b63c1a83f81b259",
   "size": 18359
  },
  {
   "id": "ops-F00428",
   "role": "ops",
   "path": "tests/test_yuurei_nightly_e2e.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e68e97c65fc0d17909191c6ba4f9af59c413f147",
   "size": 5613
  },
  {
   "id": "worker-F00001",
   "role": "worker",
   "path": ".gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4b9ba07e0a673f7f7c8c81eb86548fbc59668f9",
   "size": 101
  },
  {
   "id": "worker-F00002",
   "role": "worker",
   "path": "AGENTS.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d28fdb70b51dccd277315b9231fd590de27f84f6",
   "size": 2747
  },
  {
   "id": "worker-F00003",
   "role": "worker",
   "path": "README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2c46f507aee140c6a9c005a957aeb853b18c4b9d",
   "size": 1814
  },
  {
   "id": "worker-F00004",
   "role": "worker",
   "path": "docs/devflow-adapter.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f44458f7479e0de014bc26e121f73ca5e16ff77",
   "size": 198607
  },
  {
   "id": "worker-F00005",
   "role": "worker",
   "path": "docs/dsh-compatibility-report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d72a13d0396132247a5d6b923e39b7890d7a9a3",
   "size": 8893
  },
  {
   "id": "worker-F00006",
   "role": "worker",
   "path": "docs/dsh-supervision.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d8b5b6b096355aad0280ff16b3c260ecc27cfab5",
   "size": 17874
  },
  {
   "id": "worker-F00007",
   "role": "worker",
   "path": "docs/future-agent-loop.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f3aec6c93a308493d96cfb71d3e8f5899c4afe47",
   "size": 11514
  },
  {
   "id": "worker-F00008",
   "role": "worker",
   "path": "docs/proposal.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "82a293c2fd953ba319a788b0672b6b1d9ab4eef0",
   "size": 4284
  },
  {
   "id": "worker-F00009",
   "role": "worker",
   "path": "docs/research/agent-failure-inventory.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "607f4baa8f98d240263596b5325ed4ea487d5f81",
   "size": 67720
  },
  {
   "id": "worker-F00010",
   "role": "worker",
   "path": "docs/review-projection-outbox.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "ea341972d4c12d54e6187506fbb44aa94e00d046",
   "size": 12813
  },
  {
   "id": "worker-F00011",
   "role": "worker",
   "path": "docs/shadow-eval.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6030491fcf553dccbe8fb5f1fe4a2baf6032ab1f",
   "size": 35547
  },
  {
   "id": "worker-F00012",
   "role": "worker",
   "path": "docs/shadow-review-devin-comparison.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3d04bbcdce51628344381baf0315a77378609648",
   "size": 16267
  },
  {
   "id": "worker-F00013",
   "role": "worker",
   "path": "docs/shadow-trial.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5603e66eaf170b52a82edd12a0cff1e986f67ccf",
   "size": 4792
  },
  {
   "id": "worker-F00014",
   "role": "worker",
   "path": "docs/shadow-trigger.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "55d247ba5a6575fa1fc3f4227dc592f20cea4cb3",
   "size": 10301
  },
  {
   "id": "worker-F00015",
   "role": "worker",
   "path": "docs/shadow-write-surface.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f83cf853a3c1bcf15c101c25c1d78867d2f0fc6a",
   "size": 50572
  },
  {
   "id": "worker-F00016",
   "role": "worker",
   "path": "docs/v0.1-plan.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "802d8bdbca07cb80d445a577f04780dba3b602b8",
   "size": 18075
  },
  {
   "id": "worker-F00017",
   "role": "worker",
   "path": "docs/worker-contract.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe1f377afbcbe43dccae2af91b318c5e3f5358ba",
   "size": 25024
  },
  {
   "id": "worker-F00018",
   "role": "worker",
   "path": "eval/evidence/issue-11/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "745d161dd5fc214f0316c6e17977f0e2b95c4384",
   "size": 3032
  },
  {
   "id": "worker-F00019",
   "role": "worker",
   "path": "eval/evidence/issue-11/SHA256SUMS",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a11711323d7f7ab1c7025a13f1f4fcb4936fd8a",
   "size": 15001
  },
  {
   "id": "worker-F00020",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-confirmed/ontology-prototype-9/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "de7ab4fe213342be481c3df7d3ebc8e2bb787a8f",
   "size": 1087
  },
  {
   "id": "worker-F00021",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-confirmed/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8336d101b2c9d9602ea14f7c5d791c91b28d8a8e",
   "size": 1641
  },
  {
   "id": "worker-F00022",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-confirmed/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe5610ee3133facdad5285ca28a2c714d9ed8dd3",
   "size": 629
  },
  {
   "id": "worker-F00023",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-probe/ontology-prototype-9/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4912c7c2869fa33914718679f674f9552ea8284e",
   "size": 1070
  },
  {
   "id": "worker-F00024",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-probe/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c0ff0eef9a8c0150dc09b4a886b4a8753bf93cb9",
   "size": 1624
  },
  {
   "id": "worker-F00025",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-probe/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e47ffe0da7ded5a76c4b34d9c56234cc90bfc033",
   "size": 629
  },
  {
   "id": "worker-F00026",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/devflow-56/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b1f5a31499cc95c881e014587e906c6a63c44be6",
   "size": 1041
  },
  {
   "id": "worker-F00027",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/devflow-57/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7d47ac3cc0b518001f3fb2f71bc126ffb11f0419",
   "size": 1047
  },
  {
   "id": "worker-F00028",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/devflow-58/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2816e54139390cecf7472a23835255a6fb304957",
   "size": 1036
  },
  {
   "id": "worker-F00029",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/ontology-prototype-17/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "710ad5c6f14df3d0bf95647adb31e00950f5e6ea",
   "size": 1073
  },
  {
   "id": "worker-F00030",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "00fe13d3be164301cf866b7ad59f0430b0f9db78",
   "size": 5213
  },
  {
   "id": "worker-F00031",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/deepseek-remaining/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "14faa87bab9531253676e9dd37370479c223c8fd",
   "size": 971
  },
  {
   "id": "worker-F00032",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/devflow-56/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "58749591e0641c9030cc4a1150a7ed0c9d00fefc",
   "size": 1018
  },
  {
   "id": "worker-F00033",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/devflow-57/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d2608562901bd0f415142c0fba9d2fe86253f7b4",
   "size": 1033
  },
  {
   "id": "worker-F00034",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/devflow-58/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "038caaa741667a01ca5cd22f9df050d78c30ece0",
   "size": 1021
  },
  {
   "id": "worker-F00035",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/ontology-prototype-17/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f69a4e9237f7f4d314592eea1051a82d135d1c9",
   "size": 1064
  },
  {
   "id": "worker-F00036",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/ontology-prototype-9/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b6e289eb1eb04cca437526208e61748ad94ca440",
   "size": 1072
  },
  {
   "id": "worker-F00037",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2a54f7bed90790e012c8745b378369368e903f7",
   "size": 6365
  },
  {
   "id": "worker-F00038",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/final-five/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4d0b69fdb82f3a557739c6ca2ddb3c30eb382235",
   "size": 1061
  },
  {
   "id": "worker-F00039",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/initial/devflow-56/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d9dda7776055feb01f7c1660b00bb03c88ed023b",
   "size": 1017
  },
  {
   "id": "worker-F00040",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/initial/devflow-57/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "42139dea1b784aad619427144a033a200a29410a",
   "size": 1032
  },
  {
   "id": "worker-F00041",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/initial/devflow-58/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e2d2bb21e9c2ffc6bd53d8d441eaab70f7fdf97f",
   "size": 1020
  },
  {
   "id": "worker-F00042",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/initial/ontology-prototype-17/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1208acaa62bfa7196f7fd2cd86804d467605c98a",
   "size": 1056
  },
  {
   "id": "worker-F00043",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/initial/ontology-prototype-9/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e1c935a7df3dabdc984613c052f1a9dec3d492dd",
   "size": 1062
  },
  {
   "id": "worker-F00044",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/longcat-retry/ontology-prototype-9/eval-result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0aad72092e8bf167da742533a595de1a7dac2de4",
   "size": 1072
  },
  {
   "id": "worker-F00045",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/longcat-retry/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3569c21eebfb71df88890669492db5ca7f87b7fc",
   "size": 1619
  },
  {
   "id": "worker-F00046",
   "role": "worker",
   "path": "eval/evidence/issue-11/batch-results/longcat-retry/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4afc1190617c65a6f1f3265de6d9e5d80afbe2c8",
   "size": 624
  },
  {
   "id": "worker-F00047",
   "role": "worker",
   "path": "eval/evidence/issue-11/manifests/deepseek-probe.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79e40b2de995e1f2c2b05fda700ee0d3f2c4be21",
   "size": 6784
  },
  {
   "id": "worker-F00048",
   "role": "worker",
   "path": "eval/evidence/issue-11/manifests/deepseek-remaining.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3e048ef3191741fc152da81aa33dc482638431c2",
   "size": 24319
  },
  {
   "id": "worker-F00049",
   "role": "worker",
   "path": "eval/evidence/issue-11/manifests/final-five.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9f27c956c3ab15325660e688d845b5aec46af5c9",
   "size": 30338
  },
  {
   "id": "worker-F00050",
   "role": "worker",
   "path": "eval/evidence/issue-11/manifests/retry-probe.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "45bb93d692e55dc429a7b2bbc402cfc6950bb057",
   "size": 6514
  },
  {
   "id": "worker-F00051",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc73c0b1b139823e573937ff41424667b7b8f5fe",
   "size": 507
  },
  {
   "id": "worker-F00052",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-56-57-review-packet.md.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "7d9bf8c109416312d00540e93a13bdef60b3ffe0",
   "size": 17618
  },
  {
   "id": "worker-F00053",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-56-57-review-request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e88cb22818dfa320709bb82354701f6e8cb89fe8",
   "size": 2921
  },
  {
   "id": "worker-F00054",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-56-57-review-summary.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "dd5a453230c7251a76b79d1cc4be9d1df73b0c98",
   "size": 3085
  },
  {
   "id": "worker-F00055",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-58-review-request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a13b6351b973d3dbc86de1b280cf8ecaeea954b",
   "size": 2704
  },
  {
   "id": "worker-F00056",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-58-review-summary.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c7552c8cc563f9400fdacfba064f83bda3c670d7",
   "size": 1981
  },
  {
   "id": "worker-F00057",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-9-review-request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6b9458e72573fbd5290b2cb34f00bcd0e9d87c4e",
   "size": 4414
  },
  {
   "id": "worker-F00058",
   "role": "worker",
   "path": "eval/evidence/issue-11/reviews/issue-9-review-summary.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "451d294bafe519fff442f2956c81b969816c3255",
   "size": 920
  },
  {
   "id": "worker-F00059",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T021518Z-66ae44a7/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "1e9142e18bba28ca92f282447d491d46ffc07abc",
   "size": 278
  },
  {
   "id": "worker-F00060",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T021518Z-66ae44a7/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5fdc5313e9b108bbb1b108a79a93769385e52bba",
   "size": 3722
  },
  {
   "id": "worker-F00061",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T021518Z-66ae44a7/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "86f395ec51630fa4b781f0c64e23cff8062477a3",
   "size": 732
  },
  {
   "id": "worker-F00062",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T021518Z-66ae44a7/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec61cc627600ed22f37b41185eaf88fb5cef4e48",
   "size": 870
  },
  {
   "id": "worker-F00063",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-1.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00064",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-1.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "2b872814e784d0abe8d1158498f280ec8dea3d0f",
   "size": 2577
  },
  {
   "id": "worker-F00065",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-2.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00066",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-2.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f5f344d054fae10f39140fb7199c99321d9190a",
   "size": 19
  },
  {
   "id": "worker-F00067",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-3.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00068",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/commands/check-check-3.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "aad7c6d871d678e903170f74a9942ec540d7d010",
   "size": 27
  },
  {
   "id": "worker-F00069",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "13f56960c9b402ef1b70e97dd3a40594b535b708",
   "size": 1383530
  },
  {
   "id": "worker-F00070",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "303f4227cc3439a3a8b9da238cfa86e61418555a",
   "size": 3812
  },
  {
   "id": "worker-F00071",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9e6b13dcef0e4d304dc431c233cd39a6ee2b1fff",
   "size": 874
  },
  {
   "id": "worker-F00072",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "c72518ef74dba62cb5c44cf012e5bfe1e3aa9bb0",
   "size": 5841
  },
  {
   "id": "worker-F00073",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-56/20261007T042413Z-8f16bdcb/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a5b0f3ece033918e598cd0f45403d46c118ed291",
   "size": 3856
  },
  {
   "id": "worker-F00074",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T021518Z-43ba471f/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "9b8d021939953e8a6eb8ad040f6132fbff164eb5",
   "size": 278
  },
  {
   "id": "worker-F00075",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T021518Z-43ba471f/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f023be7bf15c9fa96637872174f6efaa0b198a6",
   "size": 4928
  },
  {
   "id": "worker-F00076",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T021518Z-43ba471f/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ff1a92a200de3b9af782711cd70aa94736db07cf",
   "size": 732
  },
  {
   "id": "worker-F00077",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T021518Z-43ba471f/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "455844c86e180fc57b2b868710fc543d734a65cc",
   "size": 875
  },
  {
   "id": "worker-F00078",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-1.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00079",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-1.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "496fc0f1f8df33b70dcbffea4c1f6593d18dcb06",
   "size": 2742
  },
  {
   "id": "worker-F00080",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-2.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00081",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-2.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f5f344d054fae10f39140fb7199c99321d9190a",
   "size": 19
  },
  {
   "id": "worker-F00082",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-3.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00083",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-3.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "91b31b8d969831c4a6f037fb3123c4845138e049",
   "size": 27
  },
  {
   "id": "worker-F00084",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-4.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00085",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/commands/check-check-4.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "f34bf8b4c46eb1f383e7e5f6a4e50a8388a35a42",
   "size": 2963
  },
  {
   "id": "worker-F00086",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "8409047fc5b6d49a6ab427a578677a70c586afee",
   "size": 1353258
  },
  {
   "id": "worker-F00087",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1a4dd42ba9b873e3272e5a31c0b247a3d24bdbf0",
   "size": 5018
  },
  {
   "id": "worker-F00088",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e4bb4266a837bdd96d543ddcbf7c2f9e5e684a28",
   "size": 874
  },
  {
   "id": "worker-F00089",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "64a98bae1ac1b2d4b3b4a0221db1ec8d47cf7e93",
   "size": 12403
  },
  {
   "id": "worker-F00090",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-57/20261007T043447Z-7c3a9b5d/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "59fc7ffc0518fba117bdcaab0520d4f0b73bde41",
   "size": 5462
  },
  {
   "id": "worker-F00091",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T021518Z-ee47edf2/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "90336156e57569936d2ebe059e71c787d7bc5c91",
   "size": 278
  },
  {
   "id": "worker-F00092",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T021518Z-ee47edf2/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ae07192364a4feec013f0fd223ab777bef25940",
   "size": 4328
  },
  {
   "id": "worker-F00093",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T021518Z-ee47edf2/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3db9b91257ff4ba4e5abd60629254c52089290c1",
   "size": 732
  },
  {
   "id": "worker-F00094",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T021518Z-ee47edf2/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "434cf52ef41200d854db012f74ab6e511884cb9c",
   "size": 871
  },
  {
   "id": "worker-F00095",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-1.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00096",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-1.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d394b8299496050ec2dc2ff298a343c8b70cdde9",
   "size": 2488
  },
  {
   "id": "worker-F00097",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-2.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00098",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-2.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f5f344d054fae10f39140fb7199c99321d9190a",
   "size": 19
  },
  {
   "id": "worker-F00099",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-3.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00100",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/commands/check-check-3.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "91b31b8d969831c4a6f037fb3123c4845138e049",
   "size": 27
  },
  {
   "id": "worker-F00101",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "d0afd5dff6244086dcce2e3b43b8820cf3647939",
   "size": 1006370
  },
  {
   "id": "worker-F00102",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d08cc02d2230f41bec4e868dd7cb544bc9316719",
   "size": 4418
  },
  {
   "id": "worker-F00103",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8fa649d05a18108e18b1819c583f1274b7272e88",
   "size": 874
  },
  {
   "id": "worker-F00104",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "969b9842dfa8ad084f39757ff63b2dc651f468fd",
   "size": 5415
  },
  {
   "id": "worker-F00105",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/devflow-58/20261007T044333Z-790419f9/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ca13d9b2997fe03aa043c0a746609a9320e2271",
   "size": 3694
  },
  {
   "id": "worker-F00106",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T021334Z-768dc764/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "b1744019d6d3c8baa362e6e399d329196e35df66",
   "size": 263020
  },
  {
   "id": "worker-F00107",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T021334Z-768dc764/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "72e77d95d8ace8a83ab0a15b32dbe1121b64bcbf",
   "size": 6556
  },
  {
   "id": "worker-F00108",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T021334Z-768dc764/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7e2ec7aa053d5a504457f9d216792911b0483cab",
   "size": 743
  },
  {
   "id": "worker-F00109",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T021334Z-768dc764/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "672001a0cb6f1cb3302b49799643cd6a628ed638",
   "size": 873
  },
  {
   "id": "worker-F00110",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T032829Z-853d243a/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "951ce831ebfcc3441b1fdb707a5ec547e9178d73",
   "size": 3885965
  },
  {
   "id": "worker-F00111",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T032829Z-853d243a/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "41937da7cedc349e420079f4bb5cb33182e9e442",
   "size": 6646
  },
  {
   "id": "worker-F00112",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T032829Z-853d243a/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1e28e54230bd3af0e33fcf30c97cce352dbc5602",
   "size": 882
  },
  {
   "id": "worker-F00113",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T032829Z-853d243a/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "abd50b3f6e5dfc867e8ceeaf7201ce966005bb9a",
   "size": 21574
  },
  {
   "id": "worker-F00114",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-17/20261007T032829Z-853d243a/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "06028f5b84a96c38a1416551d6cd86afcd8653d1",
   "size": 1746
  },
  {
   "id": "worker-F00115",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T015737Z-06b15f51/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "a889bcb2b195a5be391cc62a9d840914cd5eccce",
   "size": 2931828
  },
  {
   "id": "worker-F00116",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T015737Z-06b15f51/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0f5c9bad2def1db9a74fe957a3676c83aa919ae5",
   "size": 5374
  },
  {
   "id": "worker-F00117",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T015737Z-06b15f51/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5faaf0951944cb8eb51333f6ff2e163224d7477d",
   "size": 740
  },
  {
   "id": "worker-F00118",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T015737Z-06b15f51/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c2f87833a051dee35b2dce97340ec74bce5766a7",
   "size": 2337
  },
  {
   "id": "worker-F00119",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T022042Z-294a5c63/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "108e167c015ff94024b4871c43192c9e532b63f5",
   "size": 2849460
  },
  {
   "id": "worker-F00120",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T022042Z-294a5c63/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0f5c9bad2def1db9a74fe957a3676c83aa919ae5",
   "size": 5374
  },
  {
   "id": "worker-F00121",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T022042Z-294a5c63/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2c06b59c319e21cfa3f8e342fd99ba692ebf02a4",
   "size": 886
  },
  {
   "id": "worker-F00122",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T022042Z-294a5c63/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d54a1fae2e4c71d4df93b45c0422b98f9aafc54",
   "size": 8410
  },
  {
   "id": "worker-F00123",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T022042Z-294a5c63/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b719043363732b9329b3afd5320f2def60c6ec75",
   "size": 2477
  },
  {
   "id": "worker-F00124",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T024913Z-f67455ee/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "bea114d8b597392fd5486dd86c053b6e8f2a24c9",
   "size": 2065205
  },
  {
   "id": "worker-F00125",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T024913Z-f67455ee/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a6b2d4052571f7f79dfda7e9cd2f5ac10405c424",
   "size": 5637
  },
  {
   "id": "worker-F00126",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T024913Z-f67455ee/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "04d1f707d088242f5c677949b69db10597486699",
   "size": 884
  },
  {
   "id": "worker-F00127",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T024913Z-f67455ee/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "6733c9bff38e18cb4a47346099d4ca49f1823c1b",
   "size": 7365
  },
  {
   "id": "worker-F00128",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T024913Z-f67455ee/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "29a8a28476282bf2c688983959fe8103d0be6deb",
   "size": 1014
  },
  {
   "id": "worker-F00129",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/commands/check-check-1.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00130",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/commands/check-check-1.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "b6a97dfbd5171f50f6aece62e7fb5d36d8694adc",
   "size": 500
  },
  {
   "id": "worker-F00131",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/commands/check-check-2.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00132",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/commands/check-check-2.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "7cc2005ecb53e635f68aef6bed10452fa848c3df",
   "size": 63
  },
  {
   "id": "worker-F00133",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "3dd7b00b20af310eb296da6264ffc73daa1f8d64",
   "size": 3901260
  },
  {
   "id": "worker-F00134",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a6b2d4052571f7f79dfda7e9cd2f5ac10405c424",
   "size": 5637
  },
  {
   "id": "worker-F00135",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "393f3b7f4e0e4dde142a834211ff034f58707c2d",
   "size": 884
  },
  {
   "id": "worker-F00136",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "58d4a28e1589f24411142a48d3eed940b08d9f16",
   "size": 16865
  },
  {
   "id": "worker-F00137",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b61a6a38bad7bca1d11c9256f513d51ad5a7cfe0",
   "size": 4557
  },
  {
   "id": "worker-F00138",
   "role": "worker",
   "path": "eval/evidence/issue-11/runs/ontology-prototype-9/20261007T031028Z-2e694a73/review_request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6b9458e72573fbd5290b2cb34f00bcd0e9d87c4e",
   "size": 4414
  },
  {
   "id": "worker-F00139",
   "role": "worker",
   "path": "eval/evidence/issue-11/security/gitleaks-report-redacted.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5e2ac69eba20553e15d8485fba97cc6a6300f01e",
   "size": 17104
  },
  {
   "id": "worker-F00140",
   "role": "worker",
   "path": "eval/evidence/issue-23/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "73c825677841baa1cd423bd7a6677fad6aa35bbd",
   "size": 6327
  },
  {
   "id": "worker-F00141",
   "role": "worker",
   "path": "eval/evidence/issue-23/SHA256SUMS",
   "mode": "100644",
   "kind": "blob",
   "blob": "921bc16d11c5420586b4ff8287d726919dee501d",
   "size": 7146
  },
  {
   "id": "worker-F00142",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/coding-worker/graphql.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "95c75b56438bfd3d518f4e0f879e4bad63803b72",
   "size": 28325
  },
  {
   "id": "worker-F00143",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/coding-worker/issue-comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe51488c7066f6687ef680d6bfaa4f7768ef205c",
   "size": 3
  },
  {
   "id": "worker-F00144",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/coding-worker/prs.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ce8e9d44d1fcd51c1bbb0b72680d539184ca8944",
   "size": 2963
  },
  {
   "id": "worker-F00145",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/coding-worker/review-comments.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "fce9c950187ea00589680a3aacbe73eb3cf4fc46",
   "size": 84358
  },
  {
   "id": "worker-F00146",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/collection-summary.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "aec96c41f2dbb7955b97fe23f15eee2b15bc4750",
   "size": 812
  },
  {
   "id": "worker-F00147",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/hermes-setup/graphql.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "0f23acdc3b7224642accaf2dc3c9a181a63e50eb",
   "size": 33621
  },
  {
   "id": "worker-F00148",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/hermes-setup/issue-comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a005b6eb10f11f81230a7835affb6e47d1e98441",
   "size": 16796
  },
  {
   "id": "worker-F00149",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/hermes-setup/prs.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4796b93b2abc80f10f997ad63bc49fae09a6fa2c",
   "size": 84923
  },
  {
   "id": "worker-F00150",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/hermes-setup/review-comments.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "64aad4fbd8c30739843fb9dfcfbd26c063d96db0",
   "size": 2078682
  },
  {
   "id": "worker-F00151",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/ontology-prototype/graphql.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0cc89904c197083903afbc4166cfe7847744dd44",
   "size": 126907
  },
  {
   "id": "worker-F00152",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/ontology-prototype/issue-comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fb815dab0f4255fa1e3772e616b03347d2a775aa",
   "size": 6784
  },
  {
   "id": "worker-F00153",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/ontology-prototype/prs.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "982bde38b0467698065b7ca535720a9b7dfe5ef3",
   "size": 17510
  },
  {
   "id": "worker-F00154",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/ontology-prototype/review-comments.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "c3dab285a3297c41b8ae31d38219e6fa774844d9",
   "size": 368555
  },
  {
   "id": "worker-F00155",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/radio-transcription/graphql.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "dacface07b768f514904ad9bf1b0c7bb14c1a7da",
   "size": 31394
  },
  {
   "id": "worker-F00156",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/radio-transcription/issue-comments.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b27fd643ee3d978b27ac2050c0782962cbe8fb8d",
   "size": 25891
  },
  {
   "id": "worker-F00157",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/radio-transcription/prs.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4455999410f1ddacea8a6d78357e3c912abf2dff",
   "size": 63409
  },
  {
   "id": "worker-F00158",
   "role": "worker",
   "path": "eval/evidence/issue-23/raw/radio-transcription/review-comments.json.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "119050995e1b86a3bc7408fa5cdf6cfcd91fd555",
   "size": 877673
  },
  {
   "id": "worker-F00159",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/coding-worker/pr-12.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "01b8d179378e860608c4e2388e64ed6918086811",
   "size": 125236
  },
  {
   "id": "worker-F00160",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/coding-worker/pr-14.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "aaea19e9f2c3ed657f12fd8f3fbb120f0ef0922e",
   "size": 5666
  },
  {
   "id": "worker-F00161",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/coding-worker/pr-15.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "467ee3ad3e5e1ffe67209a5cf970485d8e9235cf",
   "size": 120144
  },
  {
   "id": "worker-F00162",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/coding-worker/pr-16.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4ba36738d40ad843eff8911215920b4090bd611b",
   "size": 14048
  },
  {
   "id": "worker-F00163",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-120.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0ec06de3d6529ce2b03f2566c4e4e0ac35e6703b",
   "size": 10376
  },
  {
   "id": "worker-F00164",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-126.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "200664b511533a2138f55e6f731934f4d1999a67",
   "size": 9759
  },
  {
   "id": "worker-F00165",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-132.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f512b0b0fb16d30b0edd86047d8a061e0ea50fb8",
   "size": 4298
  },
  {
   "id": "worker-F00166",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-135.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "34069ed6b1d5f3ea1a3eec59a48207a934636746",
   "size": 1459
  },
  {
   "id": "worker-F00167",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-145.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "44dc9d9fdbe1b0105d8f51a8d70287204a0aaad1",
   "size": 11005
  },
  {
   "id": "worker-F00168",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-178.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f290f4acd554394392d4114ebe4066e499395977",
   "size": 6119
  },
  {
   "id": "worker-F00169",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-180.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c6ed05b6af749ade0592a0fb2168177d01a3c90",
   "size": 10535
  },
  {
   "id": "worker-F00170",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-186.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b1111dbefe8789086347f07929079530f846f793",
   "size": 3824
  },
  {
   "id": "worker-F00171",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-229.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "db1dddb1769196db9e7e52b547c2c3e78290c74e",
   "size": 5250
  },
  {
   "id": "worker-F00172",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-30.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "32f661c74de51ec3b93837c84aa27b46cbd2f7fb",
   "size": 5323
  },
  {
   "id": "worker-F00173",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-33.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "99bbaac65aee19df354c1390fb3e3ed7731a9f93",
   "size": 10805
  },
  {
   "id": "worker-F00174",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-36.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "06aa7ef620e6ed01a669f3054a45169c2c61c05c",
   "size": 12628
  },
  {
   "id": "worker-F00175",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-39.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1d10e892ee9c45118cf4b668b95472af0f57d9e1",
   "size": 1445
  },
  {
   "id": "worker-F00176",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-52.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "504cd25e84fd6c21fe263a5ff9e3c9b66781b11e",
   "size": 2483
  },
  {
   "id": "worker-F00177",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-65.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d3474d56a0ff3a68f040ef2af1a0966e0ac45af9",
   "size": 10250
  },
  {
   "id": "worker-F00178",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-79.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "445404818a5496a001fe8da0b688fb1e5a649757",
   "size": 6808
  },
  {
   "id": "worker-F00179",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-85.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c992733eeeccec7e19d5384b8635d45c27b12d50",
   "size": 14219
  },
  {
   "id": "worker-F00180",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/hermes-setup/pr-98.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1faa583843d7bc245c60b2506081150d0bcf45d7",
   "size": 20048
  },
  {
   "id": "worker-F00181",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/index.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ee0e7a2dbe05194ef52b854dfbdcc2463b5011a",
   "size": 21349
  },
  {
   "id": "worker-F00182",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-21.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3ed9795e1d1b356486fab2a64ddbff638302a1a2",
   "size": 20333
  },
  {
   "id": "worker-F00183",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-24.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f30e4c7a585102cc1c25e65a99331800a4410f0",
   "size": 59411
  },
  {
   "id": "worker-F00184",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-25.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "51f7ba25c181eae8d3f0fbca9d2cf0f1af57b675",
   "size": 50901
  },
  {
   "id": "worker-F00185",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-26.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f418a660318e7a01d9717b1d04a15b1969c6305a",
   "size": 143447
  },
  {
   "id": "worker-F00186",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-28.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cccb0f33aa61d4f3cfeb1f8bf181a317735ce30c",
   "size": 53065
  },
  {
   "id": "worker-F00187",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-34.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9da9e6faf801d3f14918ec2fdaf9a69128f9f8c7",
   "size": 88629
  },
  {
   "id": "worker-F00188",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-38.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cf0eee216a5047abb92a06ba81ed910310576edc",
   "size": 3262
  },
  {
   "id": "worker-F00189",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-41.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7a06bdd475f875103a7383f1019e4146c19902ad",
   "size": 8275
  },
  {
   "id": "worker-F00190",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-42.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ce7226985afa2622ef415fd6ee8027c6f41ecbc9",
   "size": 24620
  },
  {
   "id": "worker-F00191",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-45.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ead436337f07e7fe73a1b9133c49cc125976284c",
   "size": 153026
  },
  {
   "id": "worker-F00192",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/ontology-prototype/pr-46.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "baaddf6d784997d0af5f26b8aad4f14c80afa648",
   "size": 23098
  },
  {
   "id": "worker-F00193",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-108.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "06c93896e74cd1a2967b73af5d96b65bcaf703e3",
   "size": 1667
  },
  {
   "id": "worker-F00194",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-112.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67b8b0a8b27f3f839f68ebec941c17fa8f3822da",
   "size": 4238
  },
  {
   "id": "worker-F00195",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-122.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2fc739a2abf891f26fcf53509de46b0c33d9429",
   "size": 1503
  },
  {
   "id": "worker-F00196",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-132.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "57b2c2343d69ff54d58adb67ca5540629a6a8d72",
   "size": 24896
  },
  {
   "id": "worker-F00197",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-133.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fc54b601be3b17341335c575971ff7cdb9041a73",
   "size": 2301
  },
  {
   "id": "worker-F00198",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-158.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "47293d3f499caadc1c5fbeac3215e493c9b3fb0c",
   "size": 4211
  },
  {
   "id": "worker-F00199",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-16.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "41adb57c8f2db9dbb8f7683266242e418cb4000c",
   "size": 13011
  },
  {
   "id": "worker-F00200",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-27.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e6a5e0c445fcb34e7974a60b4f50562ceedc32c9",
   "size": 11990
  },
  {
   "id": "worker-F00201",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-28.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "86280777abad177a3641b1fd7da5005b9f040078",
   "size": 3392
  },
  {
   "id": "worker-F00202",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-29.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "36a257b93060ef89e4a4eec2f85dbac6f7b440ab",
   "size": 5077
  },
  {
   "id": "worker-F00203",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-30.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0e54a2b7d2ba5e50866378e2a62fd14365ac421c",
   "size": 1477
  },
  {
   "id": "worker-F00204",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-38.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6895b39db3578e2ccc8b0844f3ffbadd96ff8bfe",
   "size": 333141
  },
  {
   "id": "worker-F00205",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-44.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ab6d3a337136365a793abc2b88d0d32ba31767a",
   "size": 7964
  },
  {
   "id": "worker-F00206",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-49.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f151b0eda84b82b512074e42c8eabe18919d5b24",
   "size": 3730
  },
  {
   "id": "worker-F00207",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-88.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "476681551be9f2c641b4cbb6a0527629feeb6beb",
   "size": 15294
  },
  {
   "id": "worker-F00208",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/radio-transcription/pr-90.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b8ad93bb1556b96bd3d516bfb00492fdf345595d",
   "size": 18325
  },
  {
   "id": "worker-F00209",
   "role": "worker",
   "path": "eval/evidence/issue-23/sample/strata.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d505c8e077c0739be8030c6310a247f7eb130876",
   "size": 59268
  },
  {
   "id": "worker-F00210",
   "role": "worker",
   "path": "eval/evidence/issue-23/tools/collect.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "f7d07576178cb8c299ccae3be949f9b9aef7cf02",
   "size": 8605
  },
  {
   "id": "worker-F00211",
   "role": "worker",
   "path": "eval/evidence/issue-23/tools/select_sample.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "4ce34a6c886d42be32112b441d634b639c0203c2",
   "size": 11047
  },
  {
   "id": "worker-F00212",
   "role": "worker",
   "path": "eval/evidence/issue-260/.gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "21b31fe21fc4b21d70e07878acbcfe984c44d385",
   "size": 55
  },
  {
   "id": "worker-F00213",
   "role": "worker",
   "path": "eval/evidence/issue-260/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4cb8341a03db8fc1e9215fffe6bc0254b46f623d",
   "size": 4301
  },
  {
   "id": "worker-F00214",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/cw-226-v8-r3/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "505908b74ab87e548112f0dbef60cd80b81d0844",
   "size": 1116
  },
  {
   "id": "worker-F00215",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/cw-226-v8-r3/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3d9cde76d2aa6ada89d2a8b1abbd8a4904023d02",
   "size": 3250
  },
  {
   "id": "worker-F00216",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/cw-226-v9-r2/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "953b1e896ece4bce6bd5d389f78f32013a25174d",
   "size": 1116
  },
  {
   "id": "worker-F00217",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/cw-226-v9-r2/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "694b6dd64529a90cd69f2b07df06d984389826db",
   "size": 2820
  },
  {
   "id": "worker-F00218",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r10/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0be989a18829846c8ea2fd925e06aab9e51206f",
   "size": 1168
  },
  {
   "id": "worker-F00219",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r10/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8c57f7fa4fc4e56c99ee995cd5bf1fe4c3b3fe6e",
   "size": 542
  },
  {
   "id": "worker-F00220",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r6/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6c0b1a72f679ed84fb15bc04eed9c2221fc3dde8",
   "size": 1016
  },
  {
   "id": "worker-F00221",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r6/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3cb6ea312d27778d1aab522fc20e647a5dd40f0f",
   "size": 10220
  },
  {
   "id": "worker-F00222",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r8/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec2c71d641b7f3b5f2c10fdaa31b4d3993f71f36",
   "size": 1165
  },
  {
   "id": "worker-F00223",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r8/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "22bbdb9dbadb74c88fe0ff742860e5af0aac7142",
   "size": 545
  },
  {
   "id": "worker-F00224",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r9/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0be989a18829846c8ea2fd925e06aab9e51206f",
   "size": 1168
  },
  {
   "id": "worker-F00225",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v8-r9/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8c57f7fa4fc4e56c99ee995cd5bf1fe4c3b3fe6e",
   "size": 542
  },
  {
   "id": "worker-F00226",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v9-r2/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9e9a1ef9337dc284f94ff62c6fc98470c42f5b5c",
   "size": 1171
  },
  {
   "id": "worker-F00227",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-302-v9-r2/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a64ab728f88a07eea1fba52346446568d9cc9cf2",
   "size": 7405
  },
  {
   "id": "worker-F00228",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v8-r4/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ad7359ee56f7d0229fc2fcabf6fb1d2d1eee60f2",
   "size": 1015
  },
  {
   "id": "worker-F00229",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v8-r4/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c2cab47422bbfa12c630f355bc75d6b3bba21ae",
   "size": 4823
  },
  {
   "id": "worker-F00230",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v8-r5/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ded81ad462cf2a7f80ea8068bda99e81fba49f0c",
   "size": 1171
  },
  {
   "id": "worker-F00231",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v8-r5/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "86fccda2bb8728688e6f34bec499c56c03904e25",
   "size": 3751
  },
  {
   "id": "worker-F00232",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v9/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5cac15b5ca9e60f239d5edd6e5991027aa66dcd",
   "size": 1171
  },
  {
   "id": "worker-F00233",
   "role": "worker",
   "path": "eval/evidence/issue-260/artifacts/hs-303-v9/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "41501b2fe8a2a7a37c1324d5f2bd4cb8fbc0667f",
   "size": 5135
  },
  {
   "id": "worker-F00234",
   "role": "worker",
   "path": "eval/evidence/issue-260/build_report.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "1536047c4518dbaeb2623e6d73bb722551d24ca2",
   "size": 12857
  },
  {
   "id": "worker-F00235",
   "role": "worker",
   "path": "eval/evidence/issue-260/captures/coding-worker-pr-226-c099a10/capture.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b55f4b47ff6b7357ba47203ea41308f60ed81bce",
   "size": 44491
  },
  {
   "id": "worker-F00236",
   "role": "worker",
   "path": "eval/evidence/issue-260/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "83d927a63a2d2894108ab3d58fc2ea276646cf61",
   "size": 8410
  },
  {
   "id": "worker-F00237",
   "role": "worker",
   "path": "eval/evidence/issue-260/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "aae7d02b92bf987ea8b1ad4546a72fbc7cf31fee",
   "size": 10422
  },
  {
   "id": "worker-F00238",
   "role": "worker",
   "path": "eval/evidence/issue-260/runs.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "559c684619ced2121178cb0375de4e4be04f40ca",
   "size": 8692
  },
  {
   "id": "worker-F00239",
   "role": "worker",
   "path": "eval/evidence/issue-260/snapshots/coding-worker-pr-226-c099a10.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "290a389ccb6cf79f0a75a2fda52c2f81d0392571",
   "size": 42676
  },
  {
   "id": "worker-F00240",
   "role": "worker",
   "path": "eval/evidence/issue-260/summarize.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "7d7246a09d5361700c833e776b1a1f0b8dc42842",
   "size": 2576
  },
  {
   "id": "worker-F00241",
   "role": "worker",
   "path": "eval/evidence/issue-34/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "81800138545bea9a4ce7a9ad24d0567e4b87aca5",
   "size": 1911
  },
  {
   "id": "worker-F00242",
   "role": "worker",
   "path": "eval/evidence/issue-34/SHA256SUMS",
   "mode": "100644",
   "kind": "blob",
   "blob": "5182fb738c2190d847875aea1448517756291b14",
   "size": 4346
  },
  {
   "id": "worker-F00243",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-build.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec755dd81307c44705b9fdada57506cdd0a7b85f",
   "size": 14
  },
  {
   "id": "worker-F00244",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-build.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00245",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-format.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d32402082bce86e7ee134257528fb5e2437a714c",
   "size": 18
  },
  {
   "id": "worker-F00246",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-format.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "150cc848c482a105bfe8f842a56c67547cf04135",
   "size": 114
  },
  {
   "id": "worker-F00247",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-install-and-focused-tests.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e62be7733f0487d332d363e9272dd2eddb9be7e",
   "size": 130
  },
  {
   "id": "worker-F00248",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-install-and-focused-tests.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "12c799b49a882373086504c051762129f1ef8b40",
   "size": 1943
  },
  {
   "id": "worker-F00249",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-knip.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d0a120b502741d046ddba5792911f51391d626a1",
   "size": 7
  },
  {
   "id": "worker-F00250",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-knip.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00251",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-lint.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f154360916186c279f4f6f46e693409df272308",
   "size": 25
  },
  {
   "id": "worker-F00252",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/commands/check-lint.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "4715f80b776dfe7d4defa4720ee926068a65ba77",
   "size": 92
  },
  {
   "id": "worker-F00253",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/devflow-submission-out.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0cdd5313fad9e6ccee287936a6113252678f610e",
   "size": 16954
  },
  {
   "id": "worker-F00254",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/devflow-submission.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0cdd5313fad9e6ccee287936a6113252678f610e",
   "size": 16954
  },
  {
   "id": "worker-F00255",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "27d762b10ed4aef95d7178e2792ebd71f3aaef23",
   "size": 2254759
  },
  {
   "id": "worker-F00256",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/export-manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4fe226172895a4c5a7f74aa5909ebdce4f2e826d",
   "size": 715
  },
  {
   "id": "worker-F00257",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e5b0b42a47d0903b79f32a526a28cbc551513df6",
   "size": 888
  },
  {
   "id": "worker-F00258",
   "role": "worker",
   "path": "eval/evidence/issue-34/export/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "555426df111fb48cbc0ad2a1b9a07559db24e5ad",
   "size": 15824
  },
  {
   "id": "worker-F00259",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-build.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec755dd81307c44705b9fdada57506cdd0a7b85f",
   "size": 14
  },
  {
   "id": "worker-F00260",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-build.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00261",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-format.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d32402082bce86e7ee134257528fb5e2437a714c",
   "size": 18
  },
  {
   "id": "worker-F00262",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-format.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "150cc848c482a105bfe8f842a56c67547cf04135",
   "size": 114
  },
  {
   "id": "worker-F00263",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-install-and-focused-tests.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e62be7733f0487d332d363e9272dd2eddb9be7e",
   "size": 130
  },
  {
   "id": "worker-F00264",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-install-and-focused-tests.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "12c799b49a882373086504c051762129f1ef8b40",
   "size": 1943
  },
  {
   "id": "worker-F00265",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-knip.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "d0a120b502741d046ddba5792911f51391d626a1",
   "size": 7
  },
  {
   "id": "worker-F00266",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-knip.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "worker-F00267",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-lint.stderr.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f154360916186c279f4f6f46e693409df272308",
   "size": 25
  },
  {
   "id": "worker-F00268",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/commands/check-lint.stdout.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "4715f80b776dfe7d4defa4720ee926068a65ba77",
   "size": 92
  },
  {
   "id": "worker-F00269",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/devflow-submission.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0cdd5313fad9e6ccee287936a6113252678f610e",
   "size": 16954
  },
  {
   "id": "worker-F00270",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "27d762b10ed4aef95d7178e2792ebd71f3aaef23",
   "size": 2254759
  },
  {
   "id": "worker-F00271",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/job.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0915bd8087e9f85aa6eeb06fbcc0d59006677cc",
   "size": 4770
  },
  {
   "id": "worker-F00272",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e5b0b42a47d0903b79f32a526a28cbc551513df6",
   "size": 888
  },
  {
   "id": "worker-F00273",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/patch.diff",
   "mode": "100644",
   "kind": "blob",
   "blob": "01150cecbe31f6b4fbcd6254149ebc253fef928f",
   "size": 15333
  },
  {
   "id": "worker-F00274",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/patch.diff.gz",
   "mode": "100644",
   "kind": "blob",
   "blob": "938df2ae2d9f7d06566489f92af2d234dc1855ab",
   "size": 5362
  },
  {
   "id": "worker-F00275",
   "role": "worker",
   "path": "eval/evidence/issue-34/runs/v03-bounded-devflow-run-yuurei-195/20261008T033410Z-7f2a94ec/result.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "555426df111fb48cbc0ad2a1b9a07559db24e5ad",
   "size": 15824
  },
  {
   "id": "worker-F00276",
   "role": "worker",
   "path": "eval/evidence/issue-36/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6e83bbafaeed5ea9ff6a32da17a70f4652bb74a1",
   "size": 14470
  },
  {
   "id": "worker-F00277",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/1.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1ff00a4e73b0995d7d5589d9b31648682fad0233",
   "size": 5169
  },
  {
   "id": "worker-F00278",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/10.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "0173449882d3a67051897e64c8718b7a86172762",
   "size": 3340
  },
  {
   "id": "worker-F00279",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/11.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "910a3499c8bfb289b46c728079c8a05b98b17c28",
   "size": 2426
  },
  {
   "id": "worker-F00280",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/12.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "311317507df9e268f9067462872925fd16b9822d",
   "size": 2828
  },
  {
   "id": "worker-F00281",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/13.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6c1802f5e062eb0cc72556bc31126f1302f93844",
   "size": 3804
  },
  {
   "id": "worker-F00282",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/14.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7b48517a290c56caa347f06167d3acbd7cf2994d",
   "size": 2975
  },
  {
   "id": "worker-F00283",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/15.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "30acd9c303579bd9705286db4f5297f019ac94b4",
   "size": 2082
  },
  {
   "id": "worker-F00284",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/16.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "34db2fe1904bd56f43b885b10067bb5f283a3018",
   "size": 2406
  },
  {
   "id": "worker-F00285",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/17.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c7699399adbd95ab31196958b1f595ef1902e797",
   "size": 2695
  },
  {
   "id": "worker-F00286",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/18.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "af09987784f4a553f553e73e8acb0544b66b8a34",
   "size": 2727
  },
  {
   "id": "worker-F00287",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/19.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7fdc8c99cbaf6340ec1530a27b04b2b0be362e94",
   "size": 1829
  },
  {
   "id": "worker-F00288",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/2.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "636e05fe756d1a8753fdd7d30bee2a05f6fdf3e2",
   "size": 3693
  },
  {
   "id": "worker-F00289",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/20.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "21a55534a2a069bbdbbb9e7bf082cc31039e1193",
   "size": 2138
  },
  {
   "id": "worker-F00290",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/3.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a1e7052e1a6e7e8a13d5cf060894c5590beb46de",
   "size": 3006
  },
  {
   "id": "worker-F00291",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/4.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "8b25c3a8a047f2b09b256f0818589e6a1f9899fe",
   "size": 3118
  },
  {
   "id": "worker-F00292",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/5.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "132447896b7b0482ed2a93ac120f5c88a75db364",
   "size": 3799
  },
  {
   "id": "worker-F00293",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/6.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a6a99d9f88877dcc9fed2ec5e098667780b1355",
   "size": 3893
  },
  {
   "id": "worker-F00294",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/7.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "9fe6e9b8f745bc4ffef103e923ff7491746f0122",
   "size": 3271
  },
  {
   "id": "worker-F00295",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/8.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe90f99c0958f0a3fd8b23593284daca092aa6fb",
   "size": 2897
  },
  {
   "id": "worker-F00296",
   "role": "worker",
   "path": "eval/evidence/issue-36/briefs/9.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1b9bf284a6aea39acc71726558b12d69fa1eb851",
   "size": 2828
  },
  {
   "id": "worker-F00297",
   "role": "worker",
   "path": "eval/evidence/issue-36/dataset.sanitized.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "64027e5233de80003fc98a60089aa2ad5b5318ac",
   "size": 194554
  },
  {
   "id": "worker-F00298",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00299",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ed48c059b5446603ce26f2bcf6f98c936fb60024",
   "size": 113389
  },
  {
   "id": "worker-F00300",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "0d34bc1ef989c0a1fab71875c6c84392ed8a54ee",
   "size": 248343
  },
  {
   "id": "worker-F00301",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "b6bca32968fa35cc6d0d1cf13d48337d68eaf2e9",
   "size": 52
  },
  {
   "id": "worker-F00302",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "13342dc72a870024a4c9db34d90b0c0f094ca811",
   "size": 1778
  },
  {
   "id": "worker-F00303",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ed48c059b5446603ce26f2bcf6f98c936fb60024",
   "size": 113389
  },
  {
   "id": "worker-F00304",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/coding-worker-pr-15/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1314287cf51d0c3027706b83d2eff52dbfd49445",
   "size": 2615
  },
  {
   "id": "worker-F00305",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00306",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2610e26c3b698bd12b1a579ad908382c8425a4ec",
   "size": 2470
  },
  {
   "id": "worker-F00307",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "65d8e2c1a094474aca63b00d184cae4d08be116f",
   "size": 69832
  },
  {
   "id": "worker-F00308",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "467985cd5e9a237de28146e04acca6b5e9db625b",
   "size": 52
  },
  {
   "id": "worker-F00309",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9b0b653eef76256b0d4cd7e0607b89adbef551c5",
   "size": 1758
  },
  {
   "id": "worker-F00310",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2610e26c3b698bd12b1a579ad908382c8425a4ec",
   "size": 2470
  },
  {
   "id": "worker-F00311",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/hermes-setup-pr-79/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a1fbd284496e4f43e0795833e6dbaad23189580",
   "size": 2908
  },
  {
   "id": "worker-F00312",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/index.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2dab8fce13372502fdc93616688d8e692614a0bd",
   "size": 7441
  },
  {
   "id": "worker-F00313",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00314",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c92681717a76d9e1ff1427ac6dee72034039b5e",
   "size": 135306
  },
  {
   "id": "worker-F00315",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "0d810a7fa9cb42c1e47bf635bd7db97e4d2b3ec6",
   "size": 278295
  },
  {
   "id": "worker-F00316",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "828aece2797d9bcb72804c83376ea8b269b22c77",
   "size": 52
  },
  {
   "id": "worker-F00317",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "13679972db3256eb5f475daa49265369277281ec",
   "size": 1735
  },
  {
   "id": "worker-F00318",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c92681717a76d9e1ff1427ac6dee72034039b5e",
   "size": 135306
  },
  {
   "id": "worker-F00319",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-26/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8dfd8f94aac187faaaaada71ee666875acdce4a8",
   "size": 3540
  },
  {
   "id": "worker-F00320",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00321",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4be8b4dc5c7ec7906e952302ec475dd39d53ea0",
   "size": 19476
  },
  {
   "id": "worker-F00322",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "dceb85f9901bda83183a75a83ad680fac621927e",
   "size": 117351
  },
  {
   "id": "worker-F00323",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "a7f98354e9e929de0553bd4952ff2eee47575cea",
   "size": 52
  },
  {
   "id": "worker-F00324",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8cb8f8a9ea41fd2a0a4adf387aef0cb230280fb6",
   "size": 1766
  },
  {
   "id": "worker-F00325",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4be8b4dc5c7ec7906e952302ec475dd39d53ea0",
   "size": 19476
  },
  {
   "id": "worker-F00326",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/ontology-prototype-pr-46/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "70844c0669d0a5b20cafda6d53d9dca7b7ad8a54",
   "size": 2530
  },
  {
   "id": "worker-F00327",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00328",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "282b1efce359fd935a9dd153b9c88f18e0eb2767",
   "size": 244704
  },
  {
   "id": "worker-F00329",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "7607363161f6c9baaf6fbb6be47e21e19b2e6c3d",
   "size": 415090
  },
  {
   "id": "worker-F00330",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "715dcfb374237f050a1b386688ef9945f2ab4a3c",
   "size": 52
  },
  {
   "id": "worker-F00331",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2566a9e9f1489e46c7e6a358b07c1f343dd86feb",
   "size": 1736
  },
  {
   "id": "worker-F00332",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "282b1efce359fd935a9dd153b9c88f18e0eb2767",
   "size": 244704
  },
  {
   "id": "worker-F00333",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-38/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c9fdd0e3fbe57a2d0bba718406a47fb593fab9b",
   "size": 2821
  },
  {
   "id": "worker-F00334",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00335",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb1b7e32a043e03a123a56a4d0748e69c382caf4",
   "size": 4467
  },
  {
   "id": "worker-F00336",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "96e52804a6e36fc97325d30a334db39a9fa9a7f0",
   "size": 126597
  },
  {
   "id": "worker-F00337",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc3b117b2545ee822db7b789db4b20235c899180",
   "size": 104
  },
  {
   "id": "worker-F00338",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e34484221360834d93551f702d5add1c46827dc",
   "size": 2760
  },
  {
   "id": "worker-F00339",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb1b7e32a043e03a123a56a4d0748e69c382caf4",
   "size": 4467
  },
  {
   "id": "worker-F00340",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "11001114845ed3edbca6a9a7ba6960194dc1f4fd",
   "size": 2240
  },
  {
   "id": "worker-F00341",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-44/verdict.rejected-0.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "ba72fd39e7aab72b3f15e49573e1c07b100f9282",
   "size": 1228
  },
  {
   "id": "worker-F00342",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00343",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8e85048edde62c3c31e1696e4880859a73b5179",
   "size": 12531
  },
  {
   "id": "worker-F00344",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "43ca6f7ae0db77e37c5fcd5452f2b6152ef1f351",
   "size": 128697
  },
  {
   "id": "worker-F00345",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "d8d49d570786b24e5457422888b1483556667994",
   "size": 52
  },
  {
   "id": "worker-F00346",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fa7790ebdd222de2c5e8856718311795abfb0b3f",
   "size": 1784
  },
  {
   "id": "worker-F00347",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8e85048edde62c3c31e1696e4880859a73b5179",
   "size": 12531
  },
  {
   "id": "worker-F00348",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs-deepseek-v4-pro/radio-transcription-pr-90/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9c128ab7989c4d209c4695299f0435cd0df7b983",
   "size": 3230
  },
  {
   "id": "worker-F00349",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00350",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ed48c059b5446603ce26f2bcf6f98c936fb60024",
   "size": 113389
  },
  {
   "id": "worker-F00351",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "4bc1c42c4173203e1605951cf696926fc600b2d4",
   "size": 331395
  },
  {
   "id": "worker-F00352",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "401c986d4c6d7774ce7e0a7a521f393fdf008bda",
   "size": 52
  },
  {
   "id": "worker-F00353",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "aba21d3a2d12e210f0d687ed7c3f259aa8b53778",
   "size": 1816
  },
  {
   "id": "worker-F00354",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ed48c059b5446603ce26f2bcf6f98c936fb60024",
   "size": 113389
  },
  {
   "id": "worker-F00355",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/coding-worker-pr-15/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7f538b72492b7f93ad9d2a2f11702cbc5d0d7dd1",
   "size": 4067
  },
  {
   "id": "worker-F00356",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00357",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2610e26c3b698bd12b1a579ad908382c8425a4ec",
   "size": 2470
  },
  {
   "id": "worker-F00358",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "c217f5d161fc228931006e5d6c73a22ec3e377c3",
   "size": 60244
  },
  {
   "id": "worker-F00359",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "836f21a05c469ceecc172a17527c8327ae18460b",
   "size": 52
  },
  {
   "id": "worker-F00360",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7391ef5bacca93a50c3f48ad203c7a5a7a357911",
   "size": 1743
  },
  {
   "id": "worker-F00361",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2610e26c3b698bd12b1a579ad908382c8425a4ec",
   "size": 2470
  },
  {
   "id": "worker-F00362",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/hermes-setup-pr-79/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c13aff2163bb18af63356a3b4672aa1e7368efc1",
   "size": 3015
  },
  {
   "id": "worker-F00363",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/index.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "09aab3131c32d5ca4b29a86650b1f796de5e0d2a",
   "size": 7633
  },
  {
   "id": "worker-F00364",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00365",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c92681717a76d9e1ff1427ac6dee72034039b5e",
   "size": 135306
  },
  {
   "id": "worker-F00366",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "aebada587bc31f42db5247d7f79dcca4b3adc6bf",
   "size": 427477
  },
  {
   "id": "worker-F00367",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "698630025e38e3a2a11b944b9c4d332570d56231",
   "size": 52
  },
  {
   "id": "worker-F00368",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3e6223bae996199b631c259ee0edc1494a13e4c5",
   "size": 1799
  },
  {
   "id": "worker-F00369",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c92681717a76d9e1ff1427ac6dee72034039b5e",
   "size": 135306
  },
  {
   "id": "worker-F00370",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-26/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3e92710eda4e44a085f3c7d6b718ba16bf6d48ce",
   "size": 7183
  },
  {
   "id": "worker-F00371",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00372",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4be8b4dc5c7ec7906e952302ec475dd39d53ea0",
   "size": 19476
  },
  {
   "id": "worker-F00373",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "06764adce968add84d048a3d72f36a44053b00a1",
   "size": 184058
  },
  {
   "id": "worker-F00374",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "4743c80f97f0dd55388626cb9df8c6b50060fc8c",
   "size": 52
  },
  {
   "id": "worker-F00375",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e9d7c97048393b728298ab06b591465fc82c23f6",
   "size": 1838
  },
  {
   "id": "worker-F00376",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4be8b4dc5c7ec7906e952302ec475dd39d53ea0",
   "size": 19476
  },
  {
   "id": "worker-F00377",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/ontology-prototype-pr-46/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "db7fe9c8bffb287c41b78d1216f7eefd9c35581c",
   "size": 1339
  },
  {
   "id": "worker-F00378",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00379",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "282b1efce359fd935a9dd153b9c88f18e0eb2767",
   "size": 244704
  },
  {
   "id": "worker-F00380",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "f654bf55b5aaf980303bb7ce256ba94c5ee25c42",
   "size": 834668
  },
  {
   "id": "worker-F00381",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "add1f1e03f97d39604954314ce063020e5b329d4",
   "size": 104
  },
  {
   "id": "worker-F00382",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e80bef43b13c9a408a2ec6f0f8506f2250cb18cf",
   "size": 1972
  },
  {
   "id": "worker-F00383",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "282b1efce359fd935a9dd153b9c88f18e0eb2767",
   "size": 244704
  },
  {
   "id": "worker-F00384",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-38/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3d82672da801463ac9b09f3742a7955de8b4c772",
   "size": 1641
  },
  {
   "id": "worker-F00385",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00386",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb1b7e32a043e03a123a56a4d0748e69c382caf4",
   "size": 4467
  },
  {
   "id": "worker-F00387",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "335ab496dc43a9c86c4ff1d2846762a6c6656c59",
   "size": 102916
  },
  {
   "id": "worker-F00388",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "68e4042103d03724c6112cef47e4f975de320aa1",
   "size": 52
  },
  {
   "id": "worker-F00389",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "79c570337100ecbda92bc55967e88c00555873b3",
   "size": 1838
  },
  {
   "id": "worker-F00390",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb1b7e32a043e03a123a56a4d0748e69c382caf4",
   "size": 4467
  },
  {
   "id": "worker-F00391",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-44/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ad498a7af5e7a3a09c77833d918cd31638b26084",
   "size": 1275
  },
  {
   "id": "worker-F00392",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00393",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8e85048edde62c3c31e1696e4880859a73b5179",
   "size": 12531
  },
  {
   "id": "worker-F00394",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "9fd9895095dd0870be5dc51436d576cff20f3b12",
   "size": 161843
  },
  {
   "id": "worker-F00395",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "61f2ec5988a28b40d2d1fdb1aa82e78435550f7b",
   "size": 52
  },
  {
   "id": "worker-F00396",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7941df671f223be00daf18192d03291d6d024d09",
   "size": 1822
  },
  {
   "id": "worker-F00397",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8e85048edde62c3c31e1696e4880859a73b5179",
   "size": 12531
  },
  {
   "id": "worker-F00398",
   "role": "worker",
   "path": "eval/evidence/issue-36/judge-runs/radio-transcription-pr-90/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "481875f1d04014fd12c9838d5baa960a2a49de12",
   "size": 3524
  },
  {
   "id": "worker-F00399",
   "role": "worker",
   "path": "eval/evidence/issue-36/ledger.skeleton.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a366c67ca31a766921c258b57a12f1c86f8c0a0",
   "size": 4515
  },
  {
   "id": "worker-F00400",
   "role": "worker",
   "path": "eval/evidence/issue-36/model-profile.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0fbeb7f3511a3d8dde8c560a34eb8f66f9a66675",
   "size": 3244
  },
  {
   "id": "worker-F00401",
   "role": "worker",
   "path": "eval/evidence/issue-36/pilot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2633f314cd60a33dd2b8bd88ed91f78deb83bdbf",
   "size": 3629
  },
  {
   "id": "worker-F00402",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-44/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00403",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-44/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "994f624713801b9c46fc96b9a075624e00fa18c3",
   "size": 1024
  },
  {
   "id": "worker-F00404",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-44/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "388e0e5cd7e8c4d2f73c915f5d77250265e2149e",
   "size": 52
  },
  {
   "id": "worker-F00405",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/bundle/opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0e3cbefe8ecc08d5c46767249ecb00fd7446e76",
   "size": 990
  },
  {
   "id": "worker-F00406",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/bundle/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "951b37bbed21209eff9d5b471e9430bec45868d4",
   "size": 1039
  },
  {
   "id": "worker-F00407",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ced1a3a2605f82da1ecad592980d4107fe8b99a",
   "size": 328536
  },
  {
   "id": "worker-F00408",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/opencode-serve.log",
   "mode": "100644",
   "kind": "blob",
   "blob": "53c52029d896a5804f9c1f1b3ee88aeca452dad2",
   "size": 52
  },
  {
   "id": "worker-F00409",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "436ef6673ec63a4edde186546c3d5a8ad8421e05",
   "size": 1844
  },
  {
   "id": "worker-F00410",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "951b37bbed21209eff9d5b471e9430bec45868d4",
   "size": 1039
  },
  {
   "id": "worker-F00411",
   "role": "worker",
   "path": "eval/evidence/issue-36/probe-contaminated/radio-transcription-pr-90/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d1b632d9950c271128dc0dc98b6515f0a0a5ec3d",
   "size": 1755
  },
  {
   "id": "worker-F00412",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/coding-worker-pr-15.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ed48c059b5446603ce26f2bcf6f98c936fb60024",
   "size": 113389
  },
  {
   "id": "worker-F00413",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/hermes-setup-pr-79.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2610e26c3b698bd12b1a579ad908382c8425a4ec",
   "size": 2470
  },
  {
   "id": "worker-F00414",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/ontology-prototype-pr-26.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c92681717a76d9e1ff1427ac6dee72034039b5e",
   "size": 135306
  },
  {
   "id": "worker-F00415",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/ontology-prototype-pr-46.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4be8b4dc5c7ec7906e952302ec475dd39d53ea0",
   "size": 19476
  },
  {
   "id": "worker-F00416",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/radio-transcription-pr-38.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "282b1efce359fd935a9dd153b9c88f18e0eb2767",
   "size": 244704
  },
  {
   "id": "worker-F00417",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/radio-transcription-pr-44.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb1b7e32a043e03a123a56a4d0748e69c382caf4",
   "size": 4467
  },
  {
   "id": "worker-F00418",
   "role": "worker",
   "path": "eval/evidence/issue-36/snapshots/radio-transcription-pr-90.snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8e85048edde62c3c31e1696e4880859a73b5179",
   "size": 12531
  },
  {
   "id": "worker-F00419",
   "role": "worker",
   "path": "eval/evidence/issue-36/tools/audit-runs.mjs",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9efaea018fd6876092156f8f156c2bffd796791",
   "size": 13038
  },
  {
   "id": "worker-F00420",
   "role": "worker",
   "path": "eval/evidence/issue-36/tools/check-briefs.mjs",
   "mode": "100644",
   "kind": "blob",
   "blob": "a553d1772ac6e1e8cacf4407c93b2d4d1f9f1ef4",
   "size": 7106
  },
  {
   "id": "worker-F00421",
   "role": "worker",
   "path": "eval/evidence/issue-36/tools/finish.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "bd9800e9d2d56d31601848091c3030c465cdfa62",
   "size": 9857
  },
  {
   "id": "worker-F00422",
   "role": "worker",
   "path": "eval/evidence/issue-36/tools/tools.test.mjs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c8be8718bdfbc673af6881f84f93685a9c24b180",
   "size": 10704
  },
  {
   "id": "worker-F00423",
   "role": "worker",
   "path": "eval/evidence/issue-36/worksheet.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7e0e8a4f58ad7e4ef2a9ecab8407ebe47f5dbb6e",
   "size": 69402
  },
  {
   "id": "worker-F00424",
   "role": "worker",
   "path": "eval/evidence/shadow-existing-runs/report.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "32e6983d219b12444d7a656096ba955cd72ef3e8",
   "size": 131324
  },
  {
   "id": "worker-F00425",
   "role": "worker",
   "path": "eval/evidence/shadow-existing-runs/report.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e45681025c0e3133e7f013aad11e2501e94c6fd",
   "size": 31084
  },
  {
   "id": "worker-F00426",
   "role": "worker",
   "path": "eval/fixtures/baselines/devflow-56-mcp-pr-triage-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "161aa04babc662eeeddfb60a0824cf7688a2be38",
   "size": 1790
  },
  {
   "id": "worker-F00427",
   "role": "worker",
   "path": "eval/fixtures/baselines/devflow-57-frozen-pr-fixtures-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "337f178cda7add537ce8577cc8fe12790057879c",
   "size": 1819
  },
  {
   "id": "worker-F00428",
   "role": "worker",
   "path": "eval/fixtures/baselines/devflow-58-pr-triage-docs-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "48a56b5d13558523618d37b9a8aae98eb798bcd1",
   "size": 2140
  },
  {
   "id": "worker-F00429",
   "role": "worker",
   "path": "eval/fixtures/baselines/ontology-prototype-17-mapping-preview-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "43837d0e4de0469cd56503e730e3384e919c01fc",
   "size": 2186
  },
  {
   "id": "worker-F00430",
   "role": "worker",
   "path": "eval/fixtures/baselines/ontology-prototype-2-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9ea5fb91c12905f4c7a6127926bd4fb4fb15f3a1",
   "size": 1823
  },
  {
   "id": "worker-F00431",
   "role": "worker",
   "path": "eval/fixtures/baselines/ontology-prototype-8-reobservation-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c07eb823c08409f11cee2c1b5d822fde429bd072",
   "size": 2117
  },
  {
   "id": "worker-F00432",
   "role": "worker",
   "path": "eval/fixtures/baselines/ontology-prototype-9-outcome-evaluation-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "66e349fcdd923bd8128005466a0d2896d9dd6f29",
   "size": 2117
  },
  {
   "id": "worker-F00433",
   "role": "worker",
   "path": "eval/fixtures/baselines/radio-transcription-115-reaction-order-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7a84717ee2a66124291289d286e7ed3dd2ae47d7",
   "size": 1869
  },
  {
   "id": "worker-F00434",
   "role": "worker",
   "path": "eval/fixtures/baselines/radio-transcription-39-summary-quality-devin.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "11d388a6ebda3c1bf3f557fdf44502464e100156",
   "size": 1992
  },
  {
   "id": "worker-F00435",
   "role": "worker",
   "path": "eval/fixtures/devflow-workflow/baselines/devin-incomparable.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7074b6ccef4e5b2b95d7e67c1489038671d8d42b",
   "size": 1373
  },
  {
   "id": "worker-F00436",
   "role": "worker",
   "path": "eval/fixtures/devflow-workflow/manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "797a3241e9c77c5562f64cea97e10891e145ee35",
   "size": 8771
  },
  {
   "id": "worker-F00437",
   "role": "worker",
   "path": "eval/fixtures/devflow-workflow/recipes/app.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "439e4247b2a617c89409cc6593f4c8a717229857",
   "size": 760
  },
  {
   "id": "worker-F00438",
   "role": "worker",
   "path": "eval/fixtures/devflow-workflow/reviews/success-findings.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ac54e54c4f723caeee82fbd0bf7f3a945d0d7b3",
   "size": 447
  },
  {
   "id": "worker-F00439",
   "role": "worker",
   "path": "eval/fixtures/historical-devin-issues.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3d11ddf7863720cbe8e474f31776acf981feef83",
   "size": 47904
  },
  {
   "id": "worker-F00440",
   "role": "worker",
   "path": "eval/fixtures/shadow/snapshot-coding-worker-pr-12.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a720306efcc3ee1146df9c219d44723b62e6f61",
   "size": 118768
  },
  {
   "id": "worker-F00441",
   "role": "worker",
   "path": "eval/fixtures/shadow/verdict-coding-worker-pr-12.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ba60b0942609c64c16e348201123668bc8aebc88",
   "size": 479
  },
  {
   "id": "worker-F00442",
   "role": "worker",
   "path": "eval/jobs/issue-22/readiness.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "41a004bf5ed1b1b6fe5debba2e9bf2a2eef56269",
   "size": 9379
  },
  {
   "id": "worker-F00443",
   "role": "worker",
   "path": "eval/jobs/issue-22/task.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2586f41ef88f9dccb266bef3f0ea7e3f861cde8e",
   "size": 6339
  },
  {
   "id": "worker-F00444",
   "role": "worker",
   "path": "eval/jobs/issue-34/readiness.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1603e2a6c4dcd58de3d3eb3d1a709a7586646ac9",
   "size": 10524
  },
  {
   "id": "worker-F00445",
   "role": "worker",
   "path": "eval/jobs/issue-34/task.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9a9ba6f810296f0974d77b306d0bbbac6372513",
   "size": 5498
  },
  {
   "id": "worker-F00446",
   "role": "worker",
   "path": "eval/profiles/coding-worker.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9790e128ae6e9735d5cfd228e30e48eb141c8393",
   "size": 3346
  },
  {
   "id": "worker-F00447",
   "role": "worker",
   "path": "eval/profiles/yuurei.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "db2b28813af1a3069de67cf8065911a39f76a7ed",
   "size": 2991
  },
  {
   "id": "worker-F00448",
   "role": "worker",
   "path": "eval/reports/issue-11-live-validation.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f688edb13f9b2c69718a65baeec7b5084e906d50",
   "size": 17537
  },
  {
   "id": "worker-F00449",
   "role": "worker",
   "path": "graft/.cache/ask-index.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ddf47574d11daa2f3aa503601d52c50bcf146ab7",
   "size": 800888
  },
  {
   "id": "worker-F00450",
   "role": "worker",
   "path": "graft/.cache/extract.c3679fb5d24a63a0.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "65e3938818d3794eeca6230d5bf98da50a8ea484",
   "size": 3394558
  },
  {
   "id": "worker-F00451",
   "role": "worker",
   "path": "graft/.cache/extract.f5c422aeb1703a7b.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0727a8df8feb6be173723fec78cfe4f8dbf8709c",
   "size": 55
  },
  {
   "id": "worker-F00452",
   "role": "worker",
   "path": "graft/.cache/fingerprint.c3679fb5d24a63a0.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "05cca09fd3ebd3dccf758b59f97bedc6ca6b3d83",
   "size": 10244
  },
  {
   "id": "worker-F00453",
   "role": "worker",
   "path": "graft/.cache/fingerprint.f5c422aeb1703a7b.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0727a8df8feb6be173723fec78cfe4f8dbf8709c",
   "size": 55
  },
  {
   "id": "worker-F00454",
   "role": "worker",
   "path": "graft/.cache/session/hilarious-silica.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f90df9e315852425e7613f8ed874c7e8a084762a",
   "size": 5639
  },
  {
   "id": "worker-F00455",
   "role": "worker",
   "path": "graft/.cache/telemetry-repo-id.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9a4663487e61213634f14d005a10108d2d82e40d",
   "size": 54
  },
  {
   "id": "worker-F00456",
   "role": "worker",
   "path": "graft/.graph/wiring.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3bf4dd01f9f501a8ef98b69fea2e9b62eac7af9b",
   "size": 996364
  },
  {
   "id": "worker-F00457",
   "role": "worker",
   "path": "package-lock.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ef384c1eee95e2b4a39a14d33c67f54188305fbf",
   "size": 1581
  },
  {
   "id": "worker-F00458",
   "role": "worker",
   "path": "package.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9f450bafd0c67286107b47f4a5d525d664c2ef7a",
   "size": 644,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:package.json"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00459",
   "role": "worker",
   "path": "review_request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a64dac1dce98b75078a4fd7b5e9644c9c48f83e",
   "size": 5153
  },
  {
   "id": "worker-F00460",
   "role": "worker",
   "path": "src/checks/run.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "6a86b3de87a6741dd6fbac9f2587691b17f2e956",
   "size": 5348,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/checks/run.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00461",
   "role": "worker",
   "path": "src/cli.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "0e6ff0bd7d1af5fc50ec1a138b7d5145600040c2",
   "size": 308750,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00462",
   "role": "worker",
   "path": "src/contract/design-dimensions.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "91f0f288751271db1a5b085b28b1bcafdc73a5e9",
   "size": 3471,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/design-dimensions.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00463",
   "role": "worker",
   "path": "src/contract/impact-manifest.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "59ea407d1a8ef325e688f9467e0dca868b6b9d75",
   "size": 17578,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/impact-manifest.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00464",
   "role": "worker",
   "path": "src/contract/plan.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc5a21d9a7695d28d2c0c82846d754b4b34a176b",
   "size": 2056,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/plan.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00465",
   "role": "worker",
   "path": "src/contract/types.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "c9e4f19ce6d83d4b97db0cc9e291f744d11abd36",
   "size": 17168,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/types.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00466",
   "role": "worker",
   "path": "src/contract/validate.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e1096a09927901d25e838d515d96d972dc917e7",
   "size": 17590,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/validate.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00467",
   "role": "worker",
   "path": "src/credentials.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a4c7f410efd98b9617be20d4bc5fa63f5e0cd307",
   "size": 1453
  },
  {
   "id": "worker-F00468",
   "role": "worker",
   "path": "src/devflow/bound-runs.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "01ec87a48bf1572fc4424dfdb0141cdf37883b2b",
   "size": 9862,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/bound-runs.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00469",
   "role": "worker",
   "path": "src/devflow/claim.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "96ec12c6ee6ff89ac02ae680f138cb2debdc6056",
   "size": 23402,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/claim.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00470",
   "role": "worker",
   "path": "src/devflow/converge-plan.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "6d6973db3e0707d292c69d0031f3e0690f2d5e1c",
   "size": 18803
  },
  {
   "id": "worker-F00471",
   "role": "worker",
   "path": "src/devflow/converge.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f7885a2338f8564ef844459dc7160d3099f067bd",
   "size": 493857
  },
  {
   "id": "worker-F00472",
   "role": "worker",
   "path": "src/devflow/fix-strategy-judge.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "525bd72b02da76753fb7c9d758e03528eea63d01",
   "size": 11162
  },
  {
   "id": "worker-F00473",
   "role": "worker",
   "path": "src/devflow/gc.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "fde36ab4cb604f164fab36962d369f1da4f1faaa",
   "size": 13035
  },
  {
   "id": "worker-F00474",
   "role": "worker",
   "path": "src/devflow/github-app.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2c4412aac34d93bb3d9988ef237d58d34dea1f6b",
   "size": 27720
  },
  {
   "id": "worker-F00475",
   "role": "worker",
   "path": "src/devflow/halt-triage.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ed0fa3053d4e2c4f6392d3f0855d3ef3bbfc33d",
   "size": 11056
  },
  {
   "id": "worker-F00476",
   "role": "worker",
   "path": "src/devflow/metrics.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f51de6e5d75f933cbbfef9b1f6a325fa52a9162c",
   "size": 50269
  },
  {
   "id": "worker-F00477",
   "role": "worker",
   "path": "src/devflow/model-policy.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "b76b9b601d39d3b42f9c6740aab8b45691097f2e",
   "size": 3763
  },
  {
   "id": "worker-F00478",
   "role": "worker",
   "path": "src/devflow/onboard.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ff5efbe842e95e3d3d7b52d00bcdd5a6d144671",
   "size": 60864
  },
  {
   "id": "worker-F00479",
   "role": "worker",
   "path": "src/devflow/overrides.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "cb5917a6c16243cf02e9d2f4a53b0a31a73bdd06",
   "size": 13692
  },
  {
   "id": "worker-F00480",
   "role": "worker",
   "path": "src/devflow/pre-review.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "cb7f75131595d224a778b1fa8c31779467f2aa6c",
   "size": 134039
  },
  {
   "id": "worker-F00481",
   "role": "worker",
   "path": "src/devflow/preflight.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a43f094a7f553ba8826fd103f69313bfaa045466",
   "size": 1222,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/preflight.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00482",
   "role": "worker",
   "path": "src/devflow/prepare.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "5da6e191c406c9a811d86e79b1361efa07141701",
   "size": 17778
  },
  {
   "id": "worker-F00483",
   "role": "worker",
   "path": "src/devflow/pricing.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a04a88bb34a5a36c31fea3d402dc62a3278c016d",
   "size": 17169
  },
  {
   "id": "worker-F00484",
   "role": "worker",
   "path": "src/devflow/profile-recovery.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "29e69eee1e36944ea294ce9b0f74edfe5ab06f38",
   "size": 18139
  },
  {
   "id": "worker-F00485",
   "role": "worker",
   "path": "src/devflow/profile.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f9a5dc6791f5091f670d311846723115cc2e3df7",
   "size": 45344
  },
  {
   "id": "worker-F00486",
   "role": "worker",
   "path": "src/devflow/propose.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e36f6af64b708964418a0bba6a5ac426877f533c",
   "size": 62435
  },
  {
   "id": "worker-F00487",
   "role": "worker",
   "path": "src/devflow/recipe.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "cd8eb58b30d49c0ff773eb57b4e68bbcd889e2b3",
   "size": 7338
  },
  {
   "id": "worker-F00488",
   "role": "worker",
   "path": "src/devflow/repo.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c40cbfbdcc9ae41c4ae8848574170078ed296ed",
   "size": 4437
  },
  {
   "id": "worker-F00489",
   "role": "worker",
   "path": "src/devflow/report.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "5dffc3a7af3870c947cde3bfd0e542322eaff5e0",
   "size": 7815
  },
  {
   "id": "worker-F00490",
   "role": "worker",
   "path": "src/devflow/resume.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8b26c86dddf6bdf4b56933b50509ff088d315c00",
   "size": 12910,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/resume.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00491",
   "role": "worker",
   "path": "src/devflow/review-gate.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "9e63648eda021c176b90377935feec630d8739e3",
   "size": 10838
  },
  {
   "id": "worker-F00492",
   "role": "worker",
   "path": "src/devflow/run-evidence.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a0d64940cdbabb6bea530d28363209de088531b",
   "size": 13046,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/run-evidence.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00493",
   "role": "worker",
   "path": "src/devflow/server-probe.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "68103f29248a7cface4e31f31fe828dac15552b3",
   "size": 6590
  },
  {
   "id": "worker-F00494",
   "role": "worker",
   "path": "src/devflow/ship-checkpoint.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "578713c0382fffd3d0b8698338dd437f477b795c",
   "size": 6215,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/ship-checkpoint.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00495",
   "role": "worker",
   "path": "src/devflow/ship.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "d66a101b4ebaf467de02dd4610fddb980990bb61",
   "size": 162782
  },
  {
   "id": "worker-F00496",
   "role": "worker",
   "path": "src/devflow/spec-check.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f86e250693067b065d0cfc6d7a4d8f22c78e16b7",
   "size": 39678
  },
  {
   "id": "worker-F00497",
   "role": "worker",
   "path": "src/devflow/spec-constraints.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1bdde10af7ea75f3bfeea642df2e633041925322",
   "size": 5229,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/spec-constraints.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00498",
   "role": "worker",
   "path": "src/devflow/state.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1db32d2f32194e2a8af0d11e0096e194f055d85c",
   "size": 3261,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/state.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00499",
   "role": "worker",
   "path": "src/devflow/submit.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "0d3f64d745db2d74670d5d0f678e8cf895645818",
   "size": 10347,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/submit.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00500",
   "role": "worker",
   "path": "src/devflow/task.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "58b531ef85145362e8d9f3c89a72e4844362ff06",
   "size": 23040,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/task.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00501",
   "role": "worker",
   "path": "src/devflow/verify-remediate.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "6c55351df39428a2ba78c06263a491bcafca5ae8",
   "size": 21156
  },
  {
   "id": "worker-F00502",
   "role": "worker",
   "path": "src/devflow/verify.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "797b0bfe787150d91c74b2eaf999d3872a5c8306",
   "size": 18164,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/verify.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00503",
   "role": "worker",
   "path": "src/devflow/workflow.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f50ccdf1c146d9ba9e4507a3ee7da67ae5038c21",
   "size": 17926
  },
  {
   "id": "worker-F00504",
   "role": "worker",
   "path": "src/dsh/adapter.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "027a4c44a7a1ba335d9b8c120d7d30c64a8bef82",
   "size": 85733
  },
  {
   "id": "worker-F00505",
   "role": "worker",
   "path": "src/dsh/normalize.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e101fb8c06b4a3f223bc86370436dc858d26a134",
   "size": 11432
  },
  {
   "id": "worker-F00506",
   "role": "worker",
   "path": "src/dsh/prompts.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1b3e45f8108536ffe58b1aa7309d591e4c0b41ad",
   "size": 18475
  },
  {
   "id": "worker-F00507",
   "role": "worker",
   "path": "src/eval/manifest.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "22213808d5e5454e1f7e7e11718895cb29b47bf3",
   "size": 3411
  },
  {
   "id": "worker-F00508",
   "role": "worker",
   "path": "src/eval/report.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "df0a21c16427871a44e94f74b9f8ccfacbbde963",
   "size": 16003
  },
  {
   "id": "worker-F00509",
   "role": "worker",
   "path": "src/eval/runner.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "315b5367369f18fb959b67c0bc5ae88b03831636",
   "size": 9366
  },
  {
   "id": "worker-F00510",
   "role": "worker",
   "path": "src/evidence/export.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8be1cdbb752743c6fd96e539f6cf0476788ff064",
   "size": 3371,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/export.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00511",
   "role": "worker",
   "path": "src/evidence/redact.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "72bf1fe2f41048036223dad46314c897dc764759",
   "size": 8610,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/redact.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00512",
   "role": "worker",
   "path": "src/evidence/store.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "38549e2e7aec7f5ecc8c2763206f7069d29a626d",
   "size": 11077,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/store.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00513",
   "role": "worker",
   "path": "src/opencode/client.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "20c5ebd2d8e2b2d0e353fb9aaec8eb35af554226",
   "size": 12404
  },
  {
   "id": "worker-F00514",
   "role": "worker",
   "path": "src/opencode/connection.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "49dfc3629d9376f5effaec09379f877dc5b1b2cc",
   "size": 8407
  },
  {
   "id": "worker-F00515",
   "role": "worker",
   "path": "src/opencode/managed-runtime.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "dab09448d861d31e4f04a6ec1fbfb547ad526a62",
   "size": 13851
  },
  {
   "id": "worker-F00516",
   "role": "worker",
   "path": "src/opencode/serve.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a1eb16676b65609b1f8b5fd8439d84284739183f",
   "size": 27261
  },
  {
   "id": "worker-F00517",
   "role": "worker",
   "path": "src/review/backup.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "16c69daf493f8e58116e7c1ff2fb1b46462437c5",
   "size": 114
  },
  {
   "id": "worker-F00518",
   "role": "worker",
   "path": "src/review/check-run.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "0745a3cd0c23865cf2453b581a79ffa16e891076",
   "size": 50817
  },
  {
   "id": "worker-F00519",
   "role": "worker",
   "path": "src/review/criterion.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "99d54fcd52dd8539c24bec3954992a08a187b43e",
   "size": 8895,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/criterion.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00520",
   "role": "worker",
   "path": "src/review/head-signal.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "732b0a177b68112a4bcfc6f517998c52e16359ad",
   "size": 11623,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/head-signal.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00521",
   "role": "worker",
   "path": "src/review/identity.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "972d723bbd407c143fd4152728dd7d3e2cfe85ff",
   "size": 4787,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/identity.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00522",
   "role": "worker",
   "path": "src/review/ledger-backup.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7633e6b2c60b71f082cabf0b9e3fba035c915e90",
   "size": 22794,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/ledger-backup.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00523",
   "role": "worker",
   "path": "src/review/ledger-db.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a9c2db4e4aa7a2f57f8ad8391cd4a8c98da55c34",
   "size": 12035,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/ledger-db.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00524",
   "role": "worker",
   "path": "src/review/ledger-post.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bc9c7ba1777f0bfe4640767746b9930ac21b5abd",
   "size": 30013
  },
  {
   "id": "worker-F00525",
   "role": "worker",
   "path": "src/review/ledger-snapshot.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "66604fdcfee2ce2bc4c44f63a04874382907a0c2",
   "size": 116
  },
  {
   "id": "worker-F00526",
   "role": "worker",
   "path": "src/review/ledger.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "33add8322ac5c047ed3372acad26b44de9a17216",
   "size": 109032
  },
  {
   "id": "worker-F00527",
   "role": "worker",
   "path": "src/review/post.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "08cbc036c1af9adcce4e0de00d0dd62fbf647561",
   "size": 111855
  },
  {
   "id": "worker-F00528",
   "role": "worker",
   "path": "src/review/projection-drain.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "00fe91d51ae3d8e588b29ddf228ff7833bc4a7e5",
   "size": 43878,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/projection-drain.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00529",
   "role": "worker",
   "path": "src/review/quality.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f6b178a8ed0277daba83857e16ce781c6fbfde5f",
   "size": 10945,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/quality.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00530",
   "role": "worker",
   "path": "src/review/reconcile.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "00e71debf82f677917c23d966942b07eab2b8172",
   "size": 7692,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/reconcile.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00531",
   "role": "worker",
   "path": "src/review/remote-meta.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "539db3cad1216d77a63d36ef6008de383dee0eef",
   "size": 1053,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/remote-meta.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00532",
   "role": "worker",
   "path": "src/review/review-actor.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "3be75c4684a7a87dad7921b796b5b03019a37a36",
   "size": 5079,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/review-actor.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00533",
   "role": "worker",
   "path": "src/review/run.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "3077f1b7e857f46f653584505bc398032c1e15b8",
   "size": 55072
  },
  {
   "id": "worker-F00534",
   "role": "worker",
   "path": "src/review/snapshot.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "66604fdcfee2ce2bc4c44f63a04874382907a0c2",
   "size": 116
  },
  {
   "id": "worker-F00535",
   "role": "worker",
   "path": "src/runtime/adapter.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "5c4bc1310678948d1fe096e4e21a1233d55ba0f7",
   "size": 19279,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/adapter.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00536",
   "role": "worker",
   "path": "src/runtime/escalation.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a902120b7322337e9017686115cfdf9978f6792a",
   "size": 8286,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/escalation.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00537",
   "role": "worker",
   "path": "src/runtime/events.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1989302656cf709eec9d4cddd033803b47241940",
   "size": 667,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/events.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00538",
   "role": "worker",
   "path": "src/runtime/fixture.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "94f714eb5f546b98b5a0a421c69163d9933740e9",
   "size": 13830,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/fixture.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00539",
   "role": "worker",
   "path": "src/runtime/lifecycle.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "3ef5ea681857c89e6395a38d7db425c3b9c97802",
   "size": 2452,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/lifecycle.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00540",
   "role": "worker",
   "path": "src/runtime/policy.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "27af3eb11fde587dfd585cd63f8a9e09030b99aa",
   "size": 17874,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/policy.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00541",
   "role": "worker",
   "path": "src/runtime/provider-errors.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "ad33f2e70da60da268bec28b51c989c89aa632bc",
   "size": 4583,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/provider-errors.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00542",
   "role": "worker",
   "path": "src/runtime/readonly.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "aeb991aa53f7a69282f00bcea4f2575d0e87af7a",
   "size": 16445
  },
  {
   "id": "worker-F00543",
   "role": "worker",
   "path": "src/runtime/registry.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a7ca7de7b0112e03c621a243eb906a09b8db5d36",
   "size": 2937,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/registry.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00544",
   "role": "worker",
   "path": "src/runtime/runner.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8f5d190009551382d5a82d5c6bf9ed4e6e7c8e2",
   "size": 40190,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/runner.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00545",
   "role": "worker",
   "path": "src/shadow/adjudication.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bf20337621cf7ca833b1edb3349575238de74b55",
   "size": 12446
  },
  {
   "id": "worker-F00546",
   "role": "worker",
   "path": "src/shadow/compare.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "82ce296ef7865e82c4e46d90532ada64601aa72b",
   "size": 18459
  },
  {
   "id": "worker-F00547",
   "role": "worker",
   "path": "src/shadow/dataset.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "01386eab453dd0637b8449354eafabdd9cf15c62",
   "size": 12962
  },
  {
   "id": "worker-F00548",
   "role": "worker",
   "path": "src/shadow/existing-runs.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8a6f1ae95cbadee9da46c381f8e0c7679fefad9f",
   "size": 74031
  },
  {
   "id": "worker-F00549",
   "role": "worker",
   "path": "src/shadow/fetch.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "762c63e6bf4e6a9138f7f496b9f70ca7bbbc27e8",
   "size": 29033
  },
  {
   "id": "worker-F00550",
   "role": "worker",
   "path": "src/shadow/judge.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "90bcf08e1316be2cb75baa0352bc60a57e3c5271",
   "size": 102801
  },
  {
   "id": "worker-F00551",
   "role": "worker",
   "path": "src/shadow/repo-context.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "d57d193a8965cd6e15db12d8d2ad9e3d633b70cf",
   "size": 17352
  },
  {
   "id": "worker-F00552",
   "role": "worker",
   "path": "src/shadow/schema.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f312572c2ee114b3e8ae77a5e83e5513e0d762a9",
   "size": 28935
  },
  {
   "id": "worker-F00553",
   "role": "worker",
   "path": "src/shadow/snapshot.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "de89b7c295084538421b32a336ef6079c475ec83",
   "size": 3920
  },
  {
   "id": "worker-F00554",
   "role": "worker",
   "path": "src/shadow/trial.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "d84ffd5a7320eec6a2b5fe8cebe710e5db6fa945",
   "size": 16354
  },
  {
   "id": "worker-F00555",
   "role": "worker",
   "path": "src/util/json.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8f4d309099866f2e28b427c0c3abf961cba83397",
   "size": 1557
  },
  {
   "id": "worker-F00556",
   "role": "worker",
   "path": "src/util/proc.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8af30d98c859f7a016e5b64f307a9a9df1796ea8",
   "size": 4035
  },
  {
   "id": "worker-F00557",
   "role": "worker",
   "path": "src/util/sse.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f0ee2748549a7f93ce3270166bb76f33122f4302",
   "size": 2777
  },
  {
   "id": "worker-F00558",
   "role": "worker",
   "path": "src/workspace/checkout.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "c371b29c81ca3c5fb2032c45419359a4dd9ffa86",
   "size": 12591,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/workspace/checkout.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00559",
   "role": "worker",
   "path": "src/workspace/patch.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e62fd22425562a0951e1b11786d748e7d5ab846",
   "size": 14417,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/workspace/patch.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00560",
   "role": "worker",
   "path": "src/workspace/paths.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bf7ad1bbe19a3ef2ba53cc601af05bea8d8252a6",
   "size": 1991,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:src/workspace/paths.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00561",
   "role": "worker",
   "path": "test/checks-parallel.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "02a7f37563c14ebc84aa2ff258d1c579fd85a519",
   "size": 6057,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/checks-parallel.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00562",
   "role": "worker",
   "path": "test/cli-gh-deps.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "174d34ae1e22edabf2d3c3f99cccf414728546b2",
   "size": 28974,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli-gh-deps.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00563",
   "role": "worker",
   "path": "test/cli.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a7deff9b9b5a45d99b2673aac8eb83ed43e51d84",
   "size": 94171,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00564",
   "role": "worker",
   "path": "test/contract.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "4d15493029baeefa8acef2a05f7db5ac7eaa8a98",
   "size": 11134,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/contract.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00565",
   "role": "worker",
   "path": "test/deferred-checks.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "5e84fcf8b1d6da3595380afdce17d0a3aa4fe77d",
   "size": 1467
  },
  {
   "id": "worker-F00566",
   "role": "worker",
   "path": "test/devflow-converge-plan.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e539be322e8fa9928e8f8307aeeaade40bc92c0a",
   "size": 14296
  },
  {
   "id": "worker-F00567",
   "role": "worker",
   "path": "test/devflow-converge.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "70c372935f4cbaa34b42f0c01f4f1ba26c9e30b9",
   "size": 678800
  },
  {
   "id": "worker-F00568",
   "role": "worker",
   "path": "test/devflow-gc.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "45a0ba881d9df49142805102524ef2332b6c3045",
   "size": 12132
  },
  {
   "id": "worker-F00569",
   "role": "worker",
   "path": "test/devflow-halt-triage.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8e451960a39e9b97588b11057cc1de4419b452d9",
   "size": 9590
  },
  {
   "id": "worker-F00570",
   "role": "worker",
   "path": "test/devflow-human-decisions.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f3a39b366c098ba55a54d72b5931fec6c7dcdea5",
   "size": 51864
  },
  {
   "id": "worker-F00571",
   "role": "worker",
   "path": "test/devflow-judgment-handoff.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "4362958d6523d87f57fd5c5965602f690ebf3b5c",
   "size": 43664
  },
  {
   "id": "worker-F00572",
   "role": "worker",
   "path": "test/devflow-metrics.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "464b3d0f1e151f511014d79a4888b7bea1ea8b2c",
   "size": 47680
  },
  {
   "id": "worker-F00573",
   "role": "worker",
   "path": "test/devflow-onboard.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8f7a14715395366fc6c718383c485abeb61de43e",
   "size": 45310
  },
  {
   "id": "worker-F00574",
   "role": "worker",
   "path": "test/devflow-overrides.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "66ab16db4f1663554aeb8cf761af5c77364b4ebd",
   "size": 13889
  },
  {
   "id": "worker-F00575",
   "role": "worker",
   "path": "test/devflow-pre-review.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1a07a6a658d8ceacd3d2e70887283c0beb017aa2",
   "size": 56425
  },
  {
   "id": "worker-F00576",
   "role": "worker",
   "path": "test/devflow-prepare.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "a644db7879c59670d4d2ba1d9b20d788844a638b",
   "size": 50332
  },
  {
   "id": "worker-F00577",
   "role": "worker",
   "path": "test/devflow-profile-recovery.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "68c9bfe590aaeee7e2a2068ac62d2eafac8f3dca",
   "size": 9535
  },
  {
   "id": "worker-F00578",
   "role": "worker",
   "path": "test/devflow-propose.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "aadfe8271ea172ba8bbd1811772549f6afba4810",
   "size": 98794
  },
  {
   "id": "worker-F00579",
   "role": "worker",
   "path": "test/devflow-resume.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e976aaf2cf4479088b67df8bd1bdc2548db6c338",
   "size": 22075
  },
  {
   "id": "worker-F00580",
   "role": "worker",
   "path": "test/devflow-ship.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "11d8d14512ea8c4aca5df1b581a4957b7d0d989d",
   "size": 431732
  },
  {
   "id": "worker-F00581",
   "role": "worker",
   "path": "test/devflow-spec-check.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "ff6a7df4db6bc682851aa056d31758d44cfb6e6e",
   "size": 46533
  },
  {
   "id": "worker-F00582",
   "role": "worker",
   "path": "test/devflow-verify.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7f260c91136c86b78a7340ecf62a7dcd3eeba844",
   "size": 28524,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/devflow-verify.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00583",
   "role": "worker",
   "path": "test/devflow-workflow.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8334769eac0a75f1cf6f184fef26ff01a4eaabcc",
   "size": 13912
  },
  {
   "id": "worker-F00584",
   "role": "worker",
   "path": "test/devflow.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "db263bfd6e9527954083c5f6ceeb43d69ef7b033",
   "size": 20253
  },
  {
   "id": "worker-F00585",
   "role": "worker",
   "path": "test/doubles/fake-opencode-server.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "771aa31e1f7db8258da3fbc380a208f6b62e0a7d",
   "size": 15409
  },
  {
   "id": "worker-F00586",
   "role": "worker",
   "path": "test/doubles/managed-root-rm.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bc2a1df32f681280793eeaaa015c530256bb4cf3",
   "size": 3365
  },
  {
   "id": "worker-F00587",
   "role": "worker",
   "path": "test/dsh-adapter.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "c58ae8b6b7a277ccf6f092e810f6bf832713f7cc",
   "size": 93865
  },
  {
   "id": "worker-F00588",
   "role": "worker",
   "path": "test/dsh-fallback.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "4235c58ecbeb242a0c65b638ef57b5be36e2bb4b",
   "size": 27127
  },
  {
   "id": "worker-F00589",
   "role": "worker",
   "path": "test/dsh-repair-boundary.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2217f16ffe0277e355850525c0429a61099de072",
   "size": 12239
  },
  {
   "id": "worker-F00590",
   "role": "worker",
   "path": "test/escalation.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "403dc5b8be30c5aac8e9d1b26cd78c06e46fbc36",
   "size": 9604,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/escalation.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00591",
   "role": "worker",
   "path": "test/eval.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "0abf0951c36395ca85348abf3fdedc3131432a25",
   "size": 21196
  },
  {
   "id": "worker-F00592",
   "role": "worker",
   "path": "test/evidence.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "b7b2dfc530eaa1c8b221e15338f707e7d875f08e",
   "size": 23545,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/evidence.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00593",
   "role": "worker",
   "path": "test/findings.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "b02e689bd5f28d75b688fffb6297aa1c0e1598e7",
   "size": 33030
  },
  {
   "id": "worker-F00594",
   "role": "worker",
   "path": "test/fixtures/jobs/valid-job.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "42bf91d390abce8b0591851327af36a246feca74",
   "size": 1017
  },
  {
   "id": "worker-F00595",
   "role": "worker",
   "path": "test/fixtures/opencode-events.jsonl",
   "mode": "100644",
   "kind": "blob",
   "blob": "4badaf261c38e807593577004d8990af357ae1ed",
   "size": 2113
  },
  {
   "id": "worker-F00596",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-failed/runF/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "943d8ef8869c6c983fda1a2ffa2f8d49b34db5b0",
   "size": 501
  },
  {
   "id": "worker-F00597",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-failed/runF/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9789ccbe8057099059b1b0aa55e610dbb44395f1",
   "size": 387
  },
  {
   "id": "worker-F00598",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-fetch-timeout/runT/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "981446c52ca5ea9e1443f473cce5a5f50507e13b",
   "size": 300
  },
  {
   "id": "worker-F00599",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-old/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5d40ef71a7e840836d387baf010aa4ab2791e3b4",
   "size": 506
  },
  {
   "id": "worker-F00600",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-old/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4c4d797a5b3a15765a3166782ae039cf462cdd70",
   "size": 388
  },
  {
   "id": "worker-F00601",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-retry/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f534538236b4e40fb6b5910862006f60ff6855ca",
   "size": 905
  },
  {
   "id": "worker-F00602",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-retry/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f5264eab67c819efe9c44ececdcc3145d79e9e0",
   "size": 363
  },
  {
   "id": "worker-F00603",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-v2/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "33854082b66fd059ebaeda990e921343817dc168",
   "size": 721
  },
  {
   "id": "worker-F00604",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1-v2/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6f95ddac149370eb13d7344d748125b4e47e1b0a",
   "size": 362
  },
  {
   "id": "worker-F00605",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0c8132308d1569994786c229e643a45a899a5340",
   "size": 899
  },
  {
   "id": "worker-F00606",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-220-head1/run1/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "685977fd185a6e1acc00423d748b7f3fcfa75533",
   "size": 532
  },
  {
   "id": "worker-F00607",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-221-head2/run2/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc06c15070ab3bf6fc5174b13c5cf635feacf759",
   "size": 680
  },
  {
   "id": "worker-F00608",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-221-head2/run2/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c9a20c0466c318422bceee91552469dd4b312637",
   "size": 363
  },
  {
   "id": "worker-F00609",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-237-head3/run3/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "29bc4e34c1185225d9998ebaa9bc5829038ad5e3",
   "size": 1102
  },
  {
   "id": "worker-F00610",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/cw-237-head3/run3/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d206b745cd497eeb92120e746c587aceeca2bd06",
   "size": 364
  },
  {
   "id": "worker-F00611",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/devin-findings.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "42ef24eb3121520b20ceda30c698de1c481a29ce",
   "size": 1428
  },
  {
   "id": "worker-F00612",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/hs-15-inconclusive/runI/judge/verdict.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "497593e8100274432420fc1a3e02a9b3f38670ec",
   "size": 517
  },
  {
   "id": "worker-F00613",
   "role": "worker",
   "path": "test/fixtures/shadow-existing-runs/hs-15-inconclusive/runI/run.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8f57fee4fd3aad73ba1b544ec560ba2dba133e1b",
   "size": 401
  },
  {
   "id": "worker-F00614",
   "role": "worker",
   "path": "test/github-app.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "86ad6e532decf1722091590480e40056f77e3983",
   "size": 24415
  },
  {
   "id": "worker-F00615",
   "role": "worker",
   "path": "test/impact-manifest.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "2ece9fff353f6882cbeb2845918deeed6681131b",
   "size": 36061,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/impact-manifest.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00616",
   "role": "worker",
   "path": "test/lifecycle.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "d80914a5274e2bc72136198611ebdb061ba585fe",
   "size": 10465,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/lifecycle.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00617",
   "role": "worker",
   "path": "test/model-policy.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f43736de714952633984ee77a4fcf19c04588bd8",
   "size": 2233
  },
  {
   "id": "worker-F00618",
   "role": "worker",
   "path": "test/opencode-connection.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "e8f091595cff39e95c9bd59a9f97f34b693d9a9b",
   "size": 12514
  },
  {
   "id": "worker-F00619",
   "role": "worker",
   "path": "test/opencode-managed-exit.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7b6d81dd7d2bd849844b25f1e4a57fbff142c1d6",
   "size": 16144
  },
  {
   "id": "worker-F00620",
   "role": "worker",
   "path": "test/opencode-managed-runtime.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "78e3304c10afff7dd6b7ea8bf322b49c591470aa",
   "size": 19727
  },
  {
   "id": "worker-F00621",
   "role": "worker",
   "path": "test/opencode-serve-version.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "4e76dec41fa12b609ca58e33b1f0c5f4e1621a29",
   "size": 21290
  },
  {
   "id": "worker-F00622",
   "role": "worker",
   "path": "test/planner-directory.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "15f13b9a46f0d4715cf37d86eb03bbb919f13dc1",
   "size": 10337
  },
  {
   "id": "worker-F00623",
   "role": "worker",
   "path": "test/planner.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec3061162dee856b5dbc6562129aba2d656b0d5f",
   "size": 42556
  },
  {
   "id": "worker-F00624",
   "role": "worker",
   "path": "test/policy.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "87327764b125a8524b57f5a4d1dbda5947c996f8",
   "size": 24304
  },
  {
   "id": "worker-F00625",
   "role": "worker",
   "path": "test/registry.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "01bb24b922ddcedba74faaf6bc906678130aad40",
   "size": 2263,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/registry.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00626",
   "role": "worker",
   "path": "test/review-actor.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7977930adad95fbfbc9e330c10b48293e1cd15fe",
   "size": 3672,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-actor.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00627",
   "role": "worker",
   "path": "test/review-check-run.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "15ba81b6825be4f6f2fe7a4873f86e8e5fe5ae6c",
   "size": 82188
  },
  {
   "id": "worker-F00628",
   "role": "worker",
   "path": "test/review-criterion.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "1bf8515ba3bead6ca26b026ff565ef72e3599245",
   "size": 5959,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-criterion.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00629",
   "role": "worker",
   "path": "test/review-head-signal.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bf9b986c816e2af04e792128c685b83ddf621ac2",
   "size": 8200,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-head-signal.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00630",
   "role": "worker",
   "path": "test/review-ledger-backup.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "8528bed2929f38a1b814c873c0754d45c738363a",
   "size": 21319,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-ledger-backup.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00631",
   "role": "worker",
   "path": "test/review-ledger-sqlite.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "857f863b7f55d4f56c138f0a1c10e4532269bfa3",
   "size": 63644
  },
  {
   "id": "worker-F00632",
   "role": "worker",
   "path": "test/review-ledger.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "d4c6bbbf9f3089b3c783689d2bdf0aef5513a6da",
   "size": 66623
  },
  {
   "id": "worker-F00633",
   "role": "worker",
   "path": "test/review-post.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7e5274f0174aff1d536eb7e524e93d31c2dc1d63",
   "size": 122781
  },
  {
   "id": "worker-F00634",
   "role": "worker",
   "path": "test/review-projection.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a205531ecc83356ee120ec71ceec2519b05850f",
   "size": 94365
  },
  {
   "id": "worker-F00635",
   "role": "worker",
   "path": "test/review-quality-verdict.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "290b87070ca9939165517a66cea48b3550edb9a9",
   "size": 26315,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-quality-verdict.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00636",
   "role": "worker",
   "path": "test/review-reconcile.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ce1845e411ded982704064d445a3450ea0e93cc",
   "size": 25421,
   "review_evidence": {
    "inspected_evidence": [
     "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-reconcile.test.ts"
    ],
    "regions": []
   }
  },
  {
   "id": "worker-F00637",
   "role": "worker",
   "path": "test/review-run.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d91f0150df2f6da1d813ac5aa30866e3817197a",
   "size": 83546
  },
  {
   "id": "worker-F00638",
   "role": "worker",
   "path": "test/server-probe.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "64f8f95b8f04a1264efd702ed4142e77b4c389d0",
   "size": 18039
  },
  {
   "id": "worker-F00639",
   "role": "worker",
   "path": "test/shadow-briefs.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "6c9eccadc74ef66b040a1f3b4d56c967afe35564",
   "size": 4656
  },
  {
   "id": "worker-F00640",
   "role": "worker",
   "path": "test/shadow-existing-runs.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "229453943a9c332efa0047a59fc1149ff815a27e",
   "size": 142695
  },
  {
   "id": "worker-F00641",
   "role": "worker",
   "path": "test/shadow-fetch.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "bfc30e5143485c2b191f64f9abf1b9a9d8e91c4f",
   "size": 27388
  },
  {
   "id": "worker-F00642",
   "role": "worker",
   "path": "test/shadow-provenance.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "7217d97d8f4fd42c8a75c112d970c5a34447bfdc",
   "size": 6981
  },
  {
   "id": "worker-F00643",
   "role": "worker",
   "path": "test/shadow-repo.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "f85c0ef55c8c1639f7d9ec235a963574335b1359",
   "size": 68356
  },
  {
   "id": "worker-F00644",
   "role": "worker",
   "path": "test/shadow-trial.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "01d66235eaa5a88debfbafd8e68fd6a12502b982",
   "size": 13800
  },
  {
   "id": "worker-F00645",
   "role": "worker",
   "path": "test/shadow.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "17e18aecc9c3b33515f77f06cffee04457fecfa9",
   "size": 155417
  },
  {
   "id": "worker-F00646",
   "role": "worker",
   "path": "test/spike.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "b57ce3c551d3c51b04fb8dd1a31dc671f942323e",
   "size": 4246
  },
  {
   "id": "worker-F00647",
   "role": "worker",
   "path": "test/workspace.test.ts",
   "mode": "100644",
   "kind": "blob",
   "blob": "b08fe20b15e3dd77936cd759264acb2bca538314",
   "size": 30275
  },
  {
   "id": "worker-F00648",
   "role": "worker",
   "path": "tsconfig.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "523bf50a8bd50729dcf78453db1ae90ba3237f5a",
   "size": 490
  },
  {
   "id": "selector-F00001",
   "role": "selector",
   "path": ".github/workflows/ci.yml",
   "mode": "100644",
   "kind": "blob",
   "blob": "d7a6abee1f5002ead0a33aae7e78cd56a8f95e25",
   "size": 1044,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:.github/workflows/ci.yml"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00002",
   "role": "selector",
   "path": ".gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "f71408326da39a631e2bf045398e0912414442f8",
   "size": 84
  },
  {
   "id": "selector-F00003",
   "role": "selector",
   "path": "AGENTS.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d864156027ca6acafadb910164545b54bfd891e9",
   "size": 2467,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:AGENTS.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00004",
   "role": "selector",
   "path": "README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "9d826a9c80e8d92ff15ff703e224088acc348595",
   "size": 5371,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:README.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00005",
   "role": "selector",
   "path": "bindings/devin-execute/CONTRACT.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "65b8bd7af499eecfc385ac26cebe9d719adcd42c",
   "size": 2540,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/devin-execute/CONTRACT.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00006",
   "role": "selector",
   "path": "bindings/devin-execute/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5674d2f661a6fcee981ea12c9acf91c253446327",
   "size": 3371,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/devin-execute/SKILL.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00007",
   "role": "selector",
   "path": "bindings/devin-flow/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "558bba546b076501d3266767094d718f35e6b860",
   "size": 2796,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/devin-flow/SKILL.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00008",
   "role": "selector",
   "path": "bindings/devin-pr-sweep/CONTRACT.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2f0e549c4cd217c37b1ced7ee4aed47e8a9871ac",
   "size": 3322,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/devin-pr-sweep/CONTRACT.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00009",
   "role": "selector",
   "path": "bindings/devin-pr-sweep/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b6505f49018531a4e046fe8edd2cd3ec06fe58f6",
   "size": 3482,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/devin-pr-sweep/SKILL.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00010",
   "role": "selector",
   "path": "bindings/next-batch/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1f0609d8808b0d9ab39b71fadc91517a4451b930",
   "size": 5211,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/next-batch/SKILL.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00011",
   "role": "selector",
   "path": "bindings/task-plan/SKILL.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d95f67a576917feaf83d1f870f060b37af85e420",
   "size": 9507,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/SKILL.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00012",
   "role": "selector",
   "path": "bindings/task-plan/templates/plan-existing-milestone.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f32867aeb6392fb3090dccb2502d982f81d0edb2",
   "size": 1401,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/templates/plan-existing-milestone.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:bindings/task-plan/templates/plan-existing-milestone.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00013",
   "role": "selector",
   "path": "bindings/task-plan/templates/plan-milestone-batch.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c214f8af565f421dffd030e2799c796891f10362",
   "size": 4366,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/templates/plan-milestone-batch.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:bindings/task-plan/templates/plan-milestone-batch.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00014",
   "role": "selector",
   "path": "bindings/task-plan/templates/plan-open-decisions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "23ce7981c2db2620a48a7f22ea537455676ebbbc",
   "size": 1568,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/templates/plan-open-decisions.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:bindings/task-plan/templates/plan-open-decisions.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00015",
   "role": "selector",
   "path": "bindings/task-plan/templates/plan-single.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "13a8ba12998a746de1fec927ff83c65dab7f106d",
   "size": 1550,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/templates/plan-single.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:bindings/task-plan/templates/plan-single.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00016",
   "role": "selector",
   "path": "bindings/task-plan/templates/resume.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "113e9c8a682981657d0c1a337a8b09e64485b72e",
   "size": 2003,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:bindings/task-plan/templates/resume.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00017",
   "role": "selector",
   "path": "docs/architecture.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "014dec9c46693f53414c98a1eb5e9f3477bddd52",
   "size": 10887,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/architecture.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00018",
   "role": "selector",
   "path": "docs/bindings.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a621ee5db6a7a4567fd84b1a96199472c0f80c8a",
   "size": 10268,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/bindings.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00019",
   "role": "selector",
   "path": "docs/dependency-markers.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a852bd6aa9a36c4fd8ab7cd47a82b12628b619cd",
   "size": 6133,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/dependency-markers.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00020",
   "role": "selector",
   "path": "docs/json-contract.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f633e3cf7ade2fb5b5e6e1528c6042f6e3efbf2",
   "size": 55453,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/json-contract.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00021",
   "role": "selector",
   "path": "docs/principles.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "badfe7a8809d730b3aff7e654f75d95c623901e1",
   "size": 4822,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/principles.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00022",
   "role": "selector",
   "path": "docs/v0.1-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "226411ee86eeaef7ad4c1534bdc55f7f6ba646a3",
   "size": 5809,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.1-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00023",
   "role": "selector",
   "path": "docs/v0.2-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c5dd7d5f3e8b4370c1acb5e29523277fefd587d1",
   "size": 4341,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.2-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00024",
   "role": "selector",
   "path": "docs/v0.3-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9b1e8a81e321afc1c1748fa24931c7d5dfb46d0",
   "size": 4139,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.3-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00025",
   "role": "selector",
   "path": "docs/v0.4-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "35739bfe8860c9ea0e0ac91d0544a6a626b091ba",
   "size": 3512,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.4-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00026",
   "role": "selector",
   "path": "docs/v0.5-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9d881999a9ff185c0478ba10d6be721324f482b",
   "size": 6034,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.5-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00027",
   "role": "selector",
   "path": "docs/v0.6-scope.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "270aa6d2a44ba7e23a0cca47aaaa18fa5d19caf8",
   "size": 6895,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/v0.6-scope.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00028",
   "role": "selector",
   "path": "docs/verification-contracts.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "cf4fd5bc95c50c22698846ff6fc2968d9962efad",
   "size": 46990,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:docs/verification-contracts.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00029",
   "role": "selector",
   "path": "policies/dubplate.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "e4f96336dedfc39da24b0237bfb99127ff8de121",
   "size": 423,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:policies/dubplate.yaml"
    ],
    "regions": [
     {
      "scope": "entire YAML keys/defaults/comment rationale"
     }
    ]
   }
  },
  {
   "id": "selector-F00030",
   "role": "selector",
   "path": "policies/gatefold.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "b945bc4a1fa58ac8f1b5966d0684956b0ebd5aa2",
   "size": 344,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:policies/gatefold.yaml"
    ],
    "regions": [
     {
      "scope": "entire YAML keys/defaults/comment rationale"
     }
    ]
   }
  },
  {
   "id": "selector-F00031",
   "role": "selector",
   "path": "policies/hermes-setup.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "563c01c71d5d9cafabb5d2b613c44473ea3eff6c",
   "size": 793,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:policies/hermes-setup.yaml"
    ],
    "regions": [
     {
      "scope": "entire YAML keys/defaults/comment rationale"
     }
    ]
   }
  },
  {
   "id": "selector-F00032",
   "role": "selector",
   "path": "pyproject.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "f877dc3c15e6e527776087d15d3e3ffea7a1e574",
   "size": 826,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:pyproject.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00033",
   "role": "selector",
   "path": "review_request.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "4bb3aa78f15ecb520513ca610f8f1f2a06df5737",
   "size": 4653,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:review_request.md"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00034",
   "role": "selector",
   "path": "src/devflow/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "5e9ee685d594a33530ff5a6c71abe63c34a836e5",
   "size": 75
  },
  {
   "id": "selector-F00035",
   "role": "selector",
   "path": "src/devflow/__main__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "421fe344a3e438168f7c0761eaf47102cf3fc915",
   "size": 73
  },
  {
   "id": "selector-F00036",
   "role": "selector",
   "path": "src/devflow/acceptance.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "cc3ee1f0752cf2a0a8e693b2b90ac194c1a6241e",
   "size": 93062,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/acceptance.py"
    ],
    "regions": [
     {
      "symbol": "CheckFailure",
      "start": 66,
      "end": 67,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "Check",
      "start": 71,
      "end": 76,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_cli_run",
      "start": 79,
      "end": 87,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_cli_json",
      "start": 90,
      "end": 94,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_dirs",
      "start": 97,
      "end": 107,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_cli_argv",
      "start": 110,
      "end": 119,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_query_argv",
      "start": 122,
      "end": 126,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_exclude_issues",
      "start": 129,
      "end": 136,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_selection_status_reasons",
      "start": 139,
      "end": 164,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_mcp_args",
      "start": 167,
      "end": 209,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_mcp_call",
      "start": 212,
      "end": 355,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_cli_returns_groups",
      "start": 358,
      "end": 372,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_devin_skill_invokes_cli",
      "start": 375,
      "end": 393,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_mcp_obtainable",
      "start": 396,
      "end": 402,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_fingerprint_agreement",
      "start": 405,
      "end": 417,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_reasons_emitted",
      "start": 420,
      "end": 433,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_stable_json",
      "start": 436,
      "end": 451,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_read_only",
      "start": 454,
      "end": 478,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_no_secret_leaks",
      "start": 481,
      "end": 529,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fixture_display_set",
      "start": 532,
      "end": 550,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_query_parity",
      "start": 553,
      "end": 586,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_query_schema",
      "start": 589,
      "end": 626,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_mcp_query_parity",
      "start": 629,
      "end": 662,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_plan_fixture_dir",
      "start": 673,
      "end": 677,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fake_gh_state",
      "start": 681,
      "end": 702,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_fake_gh",
      "start": 706,
      "end": 710,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_gh_calls",
      "start": 713,
      "end": 717,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_gh_writes",
      "start": 720,
      "end": 721,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_remote",
      "start": 724,
      "end": 725,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_marker_of",
      "start": 728,
      "end": 730,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_validate",
      "start": 733,
      "end": 761,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_preview",
      "start": 764,
      "end": 811,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_mcp_parity",
      "start": 814,
      "end": 889,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_publish_approval",
      "start": 892,
      "end": 946,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_publish_flow",
      "start": 949,
      "end": 1010,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_publish_resume",
      "start": 1013,
      "end": 1060,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_design_considerations",
      "start": 1063,
      "end": 1124,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_design_render_is_data",
      "start": 1127,
      "end": 1166,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_design_fingerprint",
      "start": 1169,
      "end": 1276,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_plan_published_feeds_selection",
      "start": 1279,
      "end": 1393,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_no_duplicated_logic",
      "start": 1396,
      "end": 1424,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_pr_fixture_dirs",
      "start": 1435,
      "end": 1446,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_pr_fixture_cli_argv",
      "start": 1449,
      "end": 1459,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_pr_fixture_mcp_args",
      "start": 1462,
      "end": 1493,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_schema_reason_types",
      "start": 1539,
      "end": 1541,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_pr_triage_replay",
      "start": 1544,
      "end": 1613,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_pr_triage_schema",
      "start": 1616,
      "end": 1634,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_pr_triage_mcp_parity",
      "start": 1637,
      "end": 1651,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_pr_triage_read_only",
      "start": 1654,
      "end": 1697,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_pr_snapshot_replay",
      "start": 1700,
      "end": 1746,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_root",
      "start": 1757,
      "end": 1761,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_fixture_dirs",
      "start": 1764,
      "end": 1773,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_fixture_dir",
      "start": 1776,
      "end": 1781,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_cli_argv",
      "start": 1784,
      "end": 1794,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_tracker",
      "start": 1797,
      "end": 1801,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_normalized_units",
      "start": 1804,
      "end": 1816,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_snapshot_sources",
      "start": 1819,
      "end": 1845,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_refine_findings",
      "start": 1848,
      "end": 1921,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_evidence",
      "start": 1924,
      "end": 1951,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_refine_schema",
      "start": 1954,
      "end": 1989,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_refine_apply",
      "start": 1992,
      "end": 2012,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_apply_argv",
      "start": 2015,
      "end": 2025,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_refine_apply",
      "start": 2028,
      "end": 2136,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "_check_refine_read_only",
      "start": 2139,
      "end": 2159,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "run_acceptance",
      "start": 2307,
      "end": 2318,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     },
     {
      "symbol": "report",
      "start": 2321,
      "end": 2333,
      "scope": "Definition/decision/guard/assertion region; fixture setup excluded as described"
     }
    ]
   }
  },
  {
   "id": "selector-F00037",
   "role": "selector",
   "path": "src/devflow/boundary.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "8f7976f75770db0d3b3b0a579e3a5db614233f94",
   "size": 13383,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/boundary.py"
    ],
    "regions": [
     {
      "symbol": "obtain_snapshot",
      "start": 36,
      "end": 42,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_snapshot_file",
      "start": 45,
      "end": 57,
      "scope": "Complete production body"
     },
     {
      "symbol": "obtain_pr_snapshot",
      "start": 60,
      "end": 68,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_pr_snapshot_file",
      "start": 71,
      "end": 83,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_json_document",
      "start": 86,
      "end": 100,
      "scope": "Complete production body"
     },
     {
      "symbol": "validate_limit",
      "start": 103,
      "end": 107,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_policy_file",
      "start": 115,
      "end": 138,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_pr_policy_file",
      "start": 141,
      "end": 155,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_refine_policy_file",
      "start": 158,
      "end": 172,
      "scope": "Complete production body"
     },
     {
      "symbol": "_discover_checkout_policy",
      "start": 175,
      "end": 199,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_policy",
      "start": 202,
      "end": 234,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_pr_policy",
      "start": 237,
      "end": 255,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_refine_policy",
      "start": 258,
      "end": 271,
      "scope": "Complete production body"
     },
     {
      "symbol": "obtain_refine_snapshot",
      "start": 274,
      "end": 280,
      "scope": "Complete production body"
     },
     {
      "symbol": "load_refine_snapshot_file",
      "start": 283,
      "end": 295,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_milestone_ref",
      "start": 298,
      "end": 303,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_issue_ref",
      "start": 306,
      "end": 311,
      "scope": "Complete production body"
     },
     {
      "symbol": "resolve_now",
      "start": 314,
      "end": 335,
      "scope": "Complete production body"
     }
    ]
   }
  },
  {
   "id": "selector-F00038",
   "role": "selector",
   "path": "src/devflow/checkout.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "29a6e48b60136d0c46f5fdbc882a1d6c8e61cba6",
   "size": 2708,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/checkout.py"
    ],
    "regions": [
     {
      "symbol": "checkout_root",
      "start": 20,
      "end": 23,
      "scope": "Complete production body"
     },
     {
      "symbol": "checkout_remotes",
      "start": 26,
      "end": 38,
      "scope": "Complete production body"
     },
     {
      "symbol": "normalize_repo_name",
      "start": 41,
      "end": 43,
      "scope": "Complete production body"
     },
     {
      "symbol": "parse_repo_url",
      "start": 46,
      "end": 70,
      "scope": "Complete production body"
     },
     {
      "symbol": "_run_git",
      "start": 73,
      "end": 83,
      "scope": "Complete production body"
     }
    ]
   }
  },
  {
   "id": "selector-F00039",
   "role": "selector",
   "path": "src/devflow/cli/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "selector-F00040",
   "role": "selector",
   "path": "src/devflow/cli/main.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2d9f689b47850e101cb34128be0fdaf69994f77",
   "size": 27280,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/cli/main.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00041",
   "role": "selector",
   "path": "src/devflow/core/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "selector-F00042",
   "role": "selector",
   "path": "src/devflow/core/decisions.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "35a86da2e92c4b514a3075016371c8a242362ebe",
   "size": 3283,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/decisions.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00043",
   "role": "selector",
   "path": "src/devflow/core/deps.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "0a011a7604dc72d1f215960a50151a1a73b45ebe",
   "size": 23562,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/deps.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00044",
   "role": "selector",
   "path": "src/devflow/core/graph.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "02e679a51fbe384b01ff2e0c23b12fe11dc9280f",
   "size": 3026,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/graph.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00045",
   "role": "selector",
   "path": "src/devflow/core/grouping.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "3bfdb56d0e98fb7d778d53d1b8de82915ca662c6",
   "size": 2502,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/grouping.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00046",
   "role": "selector",
   "path": "src/devflow/core/plan.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e540aa05cb5dc9d04abb8c1e28c04d2fbb311676",
   "size": 49413,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/plan.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00047",
   "role": "selector",
   "path": "src/devflow/core/policy.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "c55ef7f79ef242902ddfab2e7641aa61023c0808",
   "size": 7897,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/policy.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00048",
   "role": "selector",
   "path": "src/devflow/core/pr_policy.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a574c304102b99acf51101b4e200fa16ae8c419b",
   "size": 3658,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/pr_policy.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00049",
   "role": "selector",
   "path": "src/devflow/core/pr_triage.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "c91fc8c47880ce753b699b1f65a6d5cfbeeecb44",
   "size": 17982,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/pr_triage.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00050",
   "role": "selector",
   "path": "src/devflow/core/refine.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b810c27b80560754427f054b60e47f5868d8af37",
   "size": 24146,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/refine.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00051",
   "role": "selector",
   "path": "src/devflow/core/refine_policy.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ea79131e5c5b6348375fa3cdab53e96425f7a9f5",
   "size": 4821,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/refine_policy.py"
    ],
    "regions": [
     {
      "symbol": "RefinePolicy",
      "start": 26,
      "end": 102,
      "scope": "Complete production body"
     },
     {
      "symbol": "refine_policy_fingerprint",
      "start": 105,
      "end": 111,
      "scope": "Complete production body"
     }
    ]
   }
  },
  {
   "id": "selector-F00052",
   "role": "selector",
   "path": "src/devflow/core/scoring.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "763ea063e041c091de46dcb3af3342df60307723",
   "size": 2419,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/scoring.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00053",
   "role": "selector",
   "path": "src/devflow/core/select.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "fa38e044a8fe80e95e48920e2cf966fdda409f1b",
   "size": 23957,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/core/select.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00054",
   "role": "selector",
   "path": "src/devflow/mcp_server.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "af77f5c3a054a0a3d7a590eb5d1c67dad933719a",
   "size": 23793,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/mcp_server.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00055",
   "role": "selector",
   "path": "src/devflow/preview.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b81238bb642b7a73409f37b68818ad89e9407b3a",
   "size": 10542,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/preview.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00056",
   "role": "selector",
   "path": "src/devflow/publish.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "eb9077b898a0a45ec1588f91fa3777ce4290e097",
   "size": 27755,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/publish.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00057",
   "role": "selector",
   "path": "src/devflow/refine.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "11991db2b8a3f0d303e611e06190723b92b5c1fc",
   "size": 15639,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/refine.py"
    ],
    "regions": [
     {
      "symbol": "run_apply",
      "start": 53,
      "end": 149,
      "scope": "Complete production body"
     },
     {
      "symbol": "_verify_artifact",
      "start": 152,
      "end": 182,
      "scope": "Complete production body"
     },
     {
      "symbol": "_split_candidates",
      "start": 185,
      "end": 208,
      "scope": "Complete production body"
     },
     {
      "symbol": "_apply_one",
      "start": 211,
      "end": 392,
      "scope": "Complete production body"
     },
     {
      "symbol": "_retryable",
      "start": 395,
      "end": 400,
      "scope": "Complete production body"
     },
     {
      "symbol": "_item",
      "start": 403,
      "end": 409,
      "scope": "Complete production body"
     },
     {
      "symbol": "_item_message",
      "start": 412,
      "end": 414,
      "scope": "Complete production body"
     },
     {
      "symbol": "render_apply_text",
      "start": 417,
      "end": 431,
      "scope": "Complete production body"
     }
    ]
   }
  },
  {
   "id": "selector-F00058",
   "role": "selector",
   "path": "src/devflow/schema/issue-graph-0.3.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe6b1e06b8b5e342030e525f6cde21ef7fde1452",
   "size": 3994,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-graph-0.3.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-graph-0.3.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00059",
   "role": "selector",
   "path": "src/devflow/schema/issue-graph-0.4.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3a3374cc0d3461422926397ab6325f80dbebf4d1",
   "size": 4030,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-graph-0.4.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-graph-0.4.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00060",
   "role": "selector",
   "path": "src/devflow/schema/issue-graph-0.5.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5726dc3a44177296f482d9a28d2cf608452a3aa2",
   "size": 4060,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-graph-0.5.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-graph-0.5.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00061",
   "role": "selector",
   "path": "src/devflow/schema/issue-status-0.3.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "92e48dc654e7b7e894302d9fdc3b3df657416f34",
   "size": 3759,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-status-0.3.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-status-0.3.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00062",
   "role": "selector",
   "path": "src/devflow/schema/issue-status-0.4.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "38ec2704ed015f83a11a7a51b2d61066d6fe453d",
   "size": 3795,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-status-0.4.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-status-0.4.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00063",
   "role": "selector",
   "path": "src/devflow/schema/issue-status-0.5.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7811685636f0507713369e2273f8f6fb603d3060",
   "size": 3825,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/issue-status-0.5.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/issue-status-0.5.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00064",
   "role": "selector",
   "path": "src/devflow/schema/plan-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8d367b24ecd0b7c086ef8711957f5ae4c0021cdb",
   "size": 4588,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/plan-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/plan-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00065",
   "role": "selector",
   "path": "src/devflow/schema/plan-preview-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4c0aa2246356d7b59e77a08acd25369d4e01f86f",
   "size": 3660,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/plan-preview-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/plan-preview-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00066",
   "role": "selector",
   "path": "src/devflow/schema/plan-publish-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cbc5bb5e65f964dfa3240b83291ef99ef544756b",
   "size": 2515,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/plan-publish-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/plan-publish-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00067",
   "role": "selector",
   "path": "src/devflow/schema/plan-receipt-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "da422de59584c82e89a3c7a1fbe7ff424fbe9858",
   "size": 1772,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/plan-receipt-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/plan-receipt-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00068",
   "role": "selector",
   "path": "src/devflow/schema/plan-validation-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3be3cff27daf2e11b1eb3873974bc17ad908ac24",
   "size": 1409,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/plan-validation-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/plan-validation-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00069",
   "role": "selector",
   "path": "src/devflow/schema/pr-triage-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f27a7e0bb804ccd6fe76c2fc3897f65b195ec286",
   "size": 4808,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/pr-triage-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/pr-triage-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00070",
   "role": "selector",
   "path": "src/devflow/schema/ready-issues-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "06b1bd44736c5c290b98b6bb4152b25a24e82248",
   "size": 4073,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/ready-issues-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/ready-issues-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00071",
   "role": "selector",
   "path": "src/devflow/schema/ready-issues-0.2.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "198a2782d22936a8cba4f87328c7ebce4aa80444",
   "size": 4158,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/ready-issues-0.2.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/ready-issues-0.2.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00072",
   "role": "selector",
   "path": "src/devflow/schema/ready-issues-0.4.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "61a509f32445a212aa4e88f41184ec1a2512d753",
   "size": 4686,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/ready-issues-0.4.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/ready-issues-0.4.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00073",
   "role": "selector",
   "path": "src/devflow/schema/ready-issues-0.5.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1eb8ac3d0ade6e09af525d5bb3ba97106aa99718",
   "size": 4722,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/ready-issues-0.5.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/ready-issues-0.5.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00074",
   "role": "selector",
   "path": "src/devflow/schema/ready-issues-0.6.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "890f1d656ec0c5b2aa6e7e69b0f9b785bb279ae0",
   "size": 4722,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/ready-issues-0.6.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/ready-issues-0.6.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00075",
   "role": "selector",
   "path": "src/devflow/schema/refine-apply-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "339c11bae930949f29dd52ac661ad151c51388b5",
   "size": 6667,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/refine-apply-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/refine-apply-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00076",
   "role": "selector",
   "path": "src/devflow/schema/refine-preview-0.1.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "180f2911a6e5c012150f19ee2298e3d8a3239a00",
   "size": 9449,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/schema/refine-preview-0.1.json"
    ],
    "regions": [
     {
      "scope": "schema fields/constraints",
      "evidence": "selector-data-regions.json:src/devflow/schema/refine-preview-0.1.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00077",
   "role": "selector",
   "path": "src/devflow/tracker/__init__.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
   "size": 0
  },
  {
   "id": "selector-F00078",
   "role": "selector",
   "path": "src/devflow/tracker/github.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "3786f0600babdc721cb2882aae018e2727fbe951",
   "size": 30190,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/tracker/github.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00079",
   "role": "selector",
   "path": "src/devflow/tracker/github_write.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "3499e67d315658269f36c9376f8723a1a2fae3c1",
   "size": 6295,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/tracker/github_write.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00080",
   "role": "selector",
   "path": "src/devflow/tracker/pr_snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d24b5fce4d41f3c866299abecaa2c69d3eef5717",
   "size": 5097,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/tracker/pr_snapshot.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00081",
   "role": "selector",
   "path": "src/devflow/tracker/refine_snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "955ec66d53f201ce90b605d175183339df54aa8d",
   "size": 3517,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/tracker/refine_snapshot.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00082",
   "role": "selector",
   "path": "src/devflow/tracker/snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "cffaed070f08208c8e10ef8852d21f80bd13172b",
   "size": 3802,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/tracker/snapshot.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00083",
   "role": "selector",
   "path": "tests/fixtures/allow-unlabeled/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "38ec1c01f1254724a219664835a7a4e69e901f4b",
   "size": 55,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/allow-unlabeled/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/allow-unlabeled/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00084",
   "role": "selector",
   "path": "tests/fixtures/allow-unlabeled/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "1d7997f5e436fbb4d21e4382dedbd2accb0026c7",
   "size": 3082,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/allow-unlabeled/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/allow-unlabeled/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00085",
   "role": "selector",
   "path": "tests/fixtures/allow-unlabeled/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ccfe86231498c52a78f78d44fa09255433fa2734",
   "size": 1582,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/allow-unlabeled/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/allow-unlabeled/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00086",
   "role": "selector",
   "path": "tests/fixtures/cross-repo/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a895af153fd5e752c0a163ee63d08c496ca58d6a",
   "size": 38,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/cross-repo/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/cross-repo/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00087",
   "role": "selector",
   "path": "tests/fixtures/cross-repo/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "43b82731993a89e2eb815f0b8b4fe6a5dfa51fec",
   "size": 3089,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/cross-repo/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/cross-repo/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00088",
   "role": "selector",
   "path": "tests/fixtures/cross-repo/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2268cf0c9665195f7985ce0a0f14cdeebc5f037f",
   "size": 2119,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/cross-repo/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/cross-repo/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00089",
   "role": "selector",
   "path": "tests/fixtures/decisions-pending/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5f65f39020c66dbd487679f61fa6aa0b3ef010b",
   "size": 34,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/decisions-pending/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/decisions-pending/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00090",
   "role": "selector",
   "path": "tests/fixtures/decisions-pending/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "abd3787a96ccd66d3c376aaecc4991f96cee2fa1",
   "size": 1981,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/decisions-pending/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/decisions-pending/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00091",
   "role": "selector",
   "path": "tests/fixtures/decisions-pending/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a98b56baf2942dce73ba72cb8e39e2b5043218be",
   "size": 1748,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/decisions-pending/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/decisions-pending/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00092",
   "role": "selector",
   "path": "tests/fixtures/empty/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "3487c75869750da287da4fc0ca1de75ed662fed9",
   "size": 12,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/empty/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/empty/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00093",
   "role": "selector",
   "path": "tests/fixtures/empty/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7868cea537cd8ccaa47aa35e2ec84c08ab705676",
   "size": 447,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/empty/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/empty/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00094",
   "role": "selector",
   "path": "tests/fixtures/empty/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a5c2d4166c287304d69c67a9a08dd2923a2764a2",
   "size": 270,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/empty/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/empty/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00095",
   "role": "selector",
   "path": "tests/fixtures/exclude-issue/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fd8f1b9aadb6ff3d678147cb04ba6f95065a5206",
   "size": 82,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-issue/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-issue/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00096",
   "role": "selector",
   "path": "tests/fixtures/exclude-issue/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67a94f2f386c6acb20cd20df958e5f5fcabe9e82",
   "size": 1349,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-issue/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-issue/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00097",
   "role": "selector",
   "path": "tests/fixtures/exclude-issue/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a2ede58bc1c6616be2f805fd12e92e5484a996eb",
   "size": 1382,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-issue/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-issue/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00098",
   "role": "selector",
   "path": "tests/fixtures/exclude-label/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "db2d3a5fef286327166c32289e5b3e056b72c53d",
   "size": 60,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-label/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-label/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00099",
   "role": "selector",
   "path": "tests/fixtures/exclude-label/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "56760784227a6824d6fa63e65800dcfcb3cc4711",
   "size": 2071,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-label/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-label/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00100",
   "role": "selector",
   "path": "tests/fixtures/exclude-label/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "03cd481c924b41723ce9e34488940410d2e68039",
   "size": 1366,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/exclude-label/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/exclude-label/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00101",
   "role": "selector",
   "path": "tests/fixtures/label-scope/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "23c33ca91ff5af384dae2969fce293462457354f",
   "size": 45,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/label-scope/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/label-scope/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00102",
   "role": "selector",
   "path": "tests/fixtures/label-scope/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0fb71c15b6f10176e5e2406dd93cb0043365a111",
   "size": 2418,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/label-scope/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/label-scope/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00103",
   "role": "selector",
   "path": "tests/fixtures/label-scope/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "922ea79e1c112fa827bd40cd33899002c6c224dd",
   "size": 1498,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/label-scope/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/label-scope/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00104",
   "role": "selector",
   "path": "tests/fixtures/mixed/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4f09824fd3ee3b3637ab25fe195606a6d7045dc",
   "size": 49,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/mixed/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/mixed/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00105",
   "role": "selector",
   "path": "tests/fixtures/mixed/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "06145e309ae3380330502bc9ff617713c4035009",
   "size": 3856,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/mixed/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/mixed/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00106",
   "role": "selector",
   "path": "tests/fixtures/mixed/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a5c48a239774c123bcaa8556ff7a96ca54c66153",
   "size": 4199,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/mixed/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/mixed/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00107",
   "role": "selector",
   "path": "tests/fixtures/needs-human/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5f65f39020c66dbd487679f61fa6aa0b3ef010b",
   "size": 34,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/needs-human/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/needs-human/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00108",
   "role": "selector",
   "path": "tests/fixtures/needs-human/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "255ca6d532580da8b5c07d336ef743b311e80513",
   "size": 1319,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/needs-human/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/needs-human/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00109",
   "role": "selector",
   "path": "tests/fixtures/needs-human/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ce8c83433db61ee9a1e6700d7478bffe90a02b4",
   "size": 823,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/needs-human/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/needs-human/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00110",
   "role": "selector",
   "path": "tests/fixtures/no-milestone/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/no-milestone/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/no-milestone/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00111",
   "role": "selector",
   "path": "tests/fixtures/no-milestone/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "a22f8e5926212f770d628ef67c33d4d530781748",
   "size": 2581,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/no-milestone/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/no-milestone/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00112",
   "role": "selector",
   "path": "tests/fixtures/no-milestone/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "890cc0f232828cf9012f48071cf3792e5173c8f7",
   "size": 1522,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/no-milestone/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/no-milestone/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00113",
   "role": "selector",
   "path": "tests/fixtures/plan/bin/gh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2f7a9ad8451cd050861f1bc49575115a8c5d4e6b",
   "size": 9273,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/bin/gh"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00114",
   "role": "selector",
   "path": "tests/fixtures/plan/plan-invalid.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "29fe1d2a5486cd7f1aebc5a0eaa99a5303b66534",
   "size": 336,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/plan-invalid.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/plan/plan-invalid.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00115",
   "role": "selector",
   "path": "tests/fixtures/plan/plan-markdown-spoof.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "51f97faf341a9eca69a5473172a55606b0015fc5",
   "size": 1169,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/plan-markdown-spoof.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/plan/plan-markdown-spoof.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00116",
   "role": "selector",
   "path": "tests/fixtures/plan/plan-ok-design.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "797b791af04910ea4972fd70e053535c10c4ed8e",
   "size": 1343,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/plan-ok-design.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/plan/plan-ok-design.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00117",
   "role": "selector",
   "path": "tests/fixtures/plan/plan.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2aa2b60d9d397b6ed78f43f457c2bf3a6bf01a0c",
   "size": 1051,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/plan.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/plan/plan.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00118",
   "role": "selector",
   "path": "tests/fixtures/plan/remote.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ef06cad91b524619f63da7b244e0092bc926d159",
   "size": 2149,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/remote.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/plan/remote.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00119",
   "role": "selector",
   "path": "tests/fixtures/pr/ci-failing/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c1924b4917535f41e340858442aae46733de958b",
   "size": 27,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/ci-failing/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/ci-failing/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00120",
   "role": "selector",
   "path": "tests/fixtures/pr/ci-failing/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "779dd82c0d57f86f6e4f83ba91d9937efe53517a",
   "size": 1207,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/ci-failing/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/ci-failing/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00121",
   "role": "selector",
   "path": "tests/fixtures/pr/ci-failing/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "95afa7d6b51578cb42fff49cf256e2afa311ccd5",
   "size": 1412,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/ci-failing/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/ci-failing/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00122",
   "role": "selector",
   "path": "tests/fixtures/pr/conflicting/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/conflicting/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/conflicting/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00123",
   "role": "selector",
   "path": "tests/fixtures/pr/conflicting/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7eac80a064a0320c6e33fdb2309379dcf251abec",
   "size": 786,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/conflicting/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/conflicting/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00124",
   "role": "selector",
   "path": "tests/fixtures/pr/conflicting/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d58854cd0526d0ec663a25d847cfdc60669c3ca9",
   "size": 680,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/conflicting/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/conflicting/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00125",
   "role": "selector",
   "path": "tests/fixtures/pr/draft/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ffdda70ca0a5b519dc8c85c3ec3e57b896bdd486",
   "size": 62,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/draft/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/draft/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00126",
   "role": "selector",
   "path": "tests/fixtures/pr/draft/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6fd5e64c956096b1d9b3397f63b473c8f37adbb2",
   "size": 996,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/draft/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/draft/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00127",
   "role": "selector",
   "path": "tests/fixtures/pr/draft/policy.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "1021a5b2e1aa9974779d5d3479cba0c9d3681218",
   "size": 20,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/draft/policy.yaml"
    ],
    "regions": [
     {
      "scope": "entire YAML keys/defaults/comment rationale"
     }
    ]
   }
  },
  {
   "id": "selector-F00128",
   "role": "selector",
   "path": "tests/fixtures/pr/draft/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "7534f0a6504fbbc17b704d285f9a2be4bf40847c",
   "size": 1274,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/draft/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/draft/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00129",
   "role": "selector",
   "path": "tests/fixtures/pr/green/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/green/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/green/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00130",
   "role": "selector",
   "path": "tests/fixtures/pr/green/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0af6ea5b8df172f502b71fd19465c301bbc80e32",
   "size": 716,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/green/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/green/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00131",
   "role": "selector",
   "path": "tests/fixtures/pr/green/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a60fd1cd08c296ecc5097043f8ccd332f4d9ca1",
   "size": 1029,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/green/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/green/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00132",
   "role": "selector",
   "path": "tests/fixtures/pr/marker-blocking/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8ddc8490c9f4465544e999b639c2935a7045fb54",
   "size": 31,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/marker-blocking/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/marker-blocking/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00133",
   "role": "selector",
   "path": "tests/fixtures/pr/marker-blocking/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "4f6c433aa15f9d48f76ee3faf74e1d9836ab1db1",
   "size": 1910,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/marker-blocking/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/marker-blocking/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00134",
   "role": "selector",
   "path": "tests/fixtures/pr/marker-blocking/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bd5dfa75725994c264ae920f9e0b0e4196d9ea62",
   "size": 2295,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/marker-blocking/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/marker-blocking/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00135",
   "role": "selector",
   "path": "tests/fixtures/pr/pending/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/pending/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/pending/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00136",
   "role": "selector",
   "path": "tests/fixtures/pr/pending/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6ded115d9e84eb05401714c08e05e5a6738ef92d",
   "size": 972,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/pending/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/pending/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00137",
   "role": "selector",
   "path": "tests/fixtures/pr/pending/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "24d6959537e3d4f5325155fcc6f045e8d7838307",
   "size": 1429,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/pending/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/pending/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00138",
   "role": "selector",
   "path": "tests/fixtures/pr/review-unresolved/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/review-unresolved/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/review-unresolved/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00139",
   "role": "selector",
   "path": "tests/fixtures/pr/review-unresolved/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "8b9feada3b09ee7b7b245eb4ec44a244ffb5a639",
   "size": 1199,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/review-unresolved/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/review-unresolved/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00140",
   "role": "selector",
   "path": "tests/fixtures/pr/review-unresolved/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f7b5d7f9379d293516ec4964cb5a83846385d903",
   "size": 1703,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/review-unresolved/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/review-unresolved/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00141",
   "role": "selector",
   "path": "tests/fixtures/pr/unknown-signal/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/unknown-signal/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/unknown-signal/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00142",
   "role": "selector",
   "path": "tests/fixtures/pr/unknown-signal/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ce02a348b7226b2c1b9d08d75fc12f0e398b4149",
   "size": 918,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/unknown-signal/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/unknown-signal/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00143",
   "role": "selector",
   "path": "tests/fixtures/pr/unknown-signal/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "dae664bd1da69df4b24fef4077a464531f03b764",
   "size": 1091,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr/unknown-signal/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr/unknown-signal/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00144",
   "role": "selector",
   "path": "tests/fixtures/pr_snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f3c46b80779f7cd3b8efaf5f4c6679851f445fba",
   "size": 1328,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/pr_snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/pr_snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00145",
   "role": "selector",
   "path": "tests/fixtures/refine/ambiguous-answered/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/ambiguous-answered/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/ambiguous-answered/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00146",
   "role": "selector",
   "path": "tests/fixtures/refine/ambiguous-answered/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "54fc219ca34cfbab82e2220bb8a232a1eb37335b",
   "size": 5461,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/ambiguous-answered/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/ambiguous-answered/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00147",
   "role": "selector",
   "path": "tests/fixtures/refine/ambiguous-answered/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "6462653e535b4f32d05ec3eff9660adf77670fff",
   "size": 3737,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/ambiguous-answered/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/ambiguous-answered/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00148",
   "role": "selector",
   "path": "tests/fixtures/refine/human-action/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/human-action/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/human-action/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00149",
   "role": "selector",
   "path": "tests/fixtures/refine/human-action/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "0986090737047cac85b2d0f81ee917422a4ded71",
   "size": 2034,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/human-action/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/human-action/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00150",
   "role": "selector",
   "path": "tests/fixtures/refine/human-action/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "c7489ea1fb47ac65cdc14c78c2f228bf41db0751",
   "size": 2028,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/human-action/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/human-action/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00151",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-resolved/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-resolved/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-resolved/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00152",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-resolved/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "68e2f96ed6715d2e6198f52040181b44a244d125",
   "size": 3945,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-resolved/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-resolved/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00153",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-resolved/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe47bf4152fc4796693c78e965cca69e799134b8",
   "size": 2374,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-resolved/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-resolved/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00154",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-unresolved/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-unresolved/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-unresolved/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00155",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-unresolved/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "ee39abba5fd57299fce89cc88b90c377d8ef91b0",
   "size": 4468,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-unresolved/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-unresolved/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00156",
   "role": "selector",
   "path": "tests/fixtures/refine/needs-human-unresolved/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "60a52596c8f8709a579c9f039bc551f7c3fa32a1",
   "size": 2459,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/needs-human-unresolved/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/needs-human-unresolved/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00157",
   "role": "selector",
   "path": "tests/fixtures/refine/no-label/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "67573133a2946a8c01d11d54ad7e017241fc030a",
   "size": 13,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/no-label/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/no-label/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00158",
   "role": "selector",
   "path": "tests/fixtures/refine/no-label/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "9ee011eb22001c332fc42290bdff10ae12ce4fea",
   "size": 2930,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/no-label/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/no-label/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00159",
   "role": "selector",
   "path": "tests/fixtures/refine/no-label/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "82a46fd426f77666470722fd24a0ed9a19ada75e",
   "size": 1589,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/no-label/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/no-label/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00160",
   "role": "selector",
   "path": "tests/fixtures/refine/remote.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "acebd5999c1619ece1ed5acdb6a96a6d6917b679",
   "size": 4871,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/refine/remote.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/refine/remote.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00161",
   "role": "selector",
   "path": "tests/fixtures/simple/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "21f8d87cb65f871c8dd48b6d0f459a374c158aa8",
   "size": 33,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/simple/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/simple/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00162",
   "role": "selector",
   "path": "tests/fixtures/simple/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "74d5bac3de5f2e970dfb405e67188bcfa25f6617",
   "size": 1874,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/simple/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/simple/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00163",
   "role": "selector",
   "path": "tests/fixtures/simple/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc4688daaba3b187dccc773ad9ac5885340a684a",
   "size": 1107,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/simple/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/simple/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00164",
   "role": "selector",
   "path": "tests/fixtures/split/args.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5f65f39020c66dbd487679f61fa6aa0b3ef010b",
   "size": 34,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/split/args.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/split/args.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00165",
   "role": "selector",
   "path": "tests/fixtures/split/expected.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "bd7c2c531f57418dd0d9917a911b7e9b67f58ae5",
   "size": 4669,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/split/expected.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/split/expected.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00166",
   "role": "selector",
   "path": "tests/fixtures/split/snapshot.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "46f15d026bfba90f12d0a412f4ce6ace2076f29a",
   "size": 3030,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/split/snapshot.json"
    ],
    "regions": [
     {
      "scope": "JSON args, state/status/reasons, dependencies, group/order, label actions and receipt fields",
      "evidence": "selector-data-regions.json:tests/fixtures/split/snapshot.json"
     }
    ]
   }
  },
  {
   "id": "selector-F00167",
   "role": "selector",
   "path": "tests/refine_helpers.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "319454840ec8e9968aec520dc10334cb84337286",
   "size": 2421,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/refine_helpers.py"
    ],
    "regions": [
     {
      "symbol": "comment",
      "start": 19,
      "end": 22,
      "scope": "Complete production body"
     },
     {
      "symbol": "issue",
      "start": 25,
      "end": 44,
      "scope": "Complete production body"
     },
     {
      "symbol": "finding_for",
      "start": 56,
      "end": 63,
      "scope": "Complete production body"
     },
     {
      "symbol": "snapshot",
      "start": 69,
      "end": 74,
      "scope": "Complete production body"
     },
     {
      "symbol": "preview_artifact",
      "start": 77,
      "end": 90,
      "scope": "Complete production body"
     }
    ]
   }
  },
  {
   "id": "selector-F00168",
   "role": "selector",
   "path": "tests/test_acceptance.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ad6ddcee8654e7de8a72819d2b269bb2a0df5e5f",
   "size": 6334,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_acceptance.py"
    ],
    "regions": [
     {
      "symbol": "test_acceptance_green_over_frozen_fixtures",
      "start": 25,
      "end": 31,
      "assertions": [
       {
        "line": 28,
        "assertion": "rc == 0"
       },
       {
        "line": 31,
        "assertion": "'all 31 conditions hold' in out"
       },
       {
        "line": 30,
        "assertion": "f'[{n}] ok' in out"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_check_inventory_maps_to_cards",
      "start": 34,
      "end": 38,
      "assertions": [
       {
        "line": 36,
        "assertion": "len(acceptance._CHECKS) == 31"
       },
       {
        "line": 37,
        "assertion": "[n for n, *_ in acceptance._CHECKS] == list(range(1, 32))"
       },
       {
        "line": 38,
        "assertion": "all((card.startswith('VC-') for _, _, card, _ in acceptance._CHECKS))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_fixtures_dir_fails_condition_1",
      "start": 41,
      "end": 46,
      "assertions": [
       {
        "line": 44,
        "assertion": "rc == 6"
       },
       {
        "line": 45,
        "assertion": "'[1] FAIL' in out"
       },
       {
        "line": 46,
        "assertion": "'fixtures directory not found' in out"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_diverged_expected_fails_condition_1",
      "start": 49,
      "end": 60,
      "assertions": [
       {
        "line": 59,
        "assertion": "rc == 6"
       },
       {
        "line": 60,
        "assertion": "'diverged from expected.json' in out"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_every_check_runs_even_after_failure",
      "start": 63,
      "end": 75,
      "assertions": [
       {
        "line": 66,
        "assertion": "len(checks) == len(acceptance._CHECKS)"
       },
       {
        "line": 67,
        "assertion": "checks[0].ok is False"
       },
       {
        "line": 70,
        "assertion": "by_number[2].ok"
       },
       {
        "line": 71,
        "assertion": "by_number[7].ok"
       },
       {
        "line": 72,
        "assertion": "by_number[9].ok"
       },
       {
        "line": 74,
        "assertion": "not by_number[13].ok"
       },
       {
        "line": 75,
        "assertion": "not by_number[14].ok"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_sentinel_env_is_restored",
      "start": 78,
      "end": 83,
      "assertions": [
       {
        "line": 83,
        "assertion": "os.environ['GH_TOKEN'] == 'original-value'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_unknown_fixture_arg_fails_loud",
      "start": 95,
      "end": 99,
      "assertions": [],
      "raises": [
       {
        "line": 98,
        "context": "pytest.raises(acceptance.CheckFailure, match='unsupported arg')"
       }
      ]
     },
     {
      "symbol": "test_flag_without_value_fails_clearly",
      "start": 102,
      "end": 106,
      "assertions": [],
      "raises": [
       {
        "line": 105,
        "context": "pytest.raises(acceptance.CheckFailure, match='no value')"
       }
      ]
     },
     {
      "symbol": "test_unknown_pr_fixture_arg_fails_loud",
      "start": 117,
      "end": 120,
      "assertions": [],
      "raises": [
       {
        "line": 119,
        "context": "pytest.raises(acceptance.CheckFailure, match='unsupported arg')"
       }
      ]
     },
     {
      "symbol": "test_pr_flag_without_value_fails_clearly",
      "start": 123,
      "end": 126,
      "assertions": [],
      "raises": [
       {
        "line": 125,
        "context": "pytest.raises(acceptance.CheckFailure, match='no value')"
       }
      ]
     },
     {
      "symbol": "test_missing_pr_fixtures_dir_fails_condition_23",
      "start": 129,
      "end": 136,
      "assertions": [
       {
        "line": 135,
        "assertion": "not by_number[n].ok"
       },
       {
        "line": 136,
        "assertion": "'pr fixtures directory not found' in by_number[n].detail"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_refine_fixtures_dir_fails_the_refine_checks",
      "start": 139,
      "end": 146,
      "assertions": [
       {
        "line": 145,
        "assertion": "not by_number[n].ok"
       },
       {
        "line": 146,
        "assertion": "'refine fixtures directory not found' in by_number[n].detail"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_vacuous_reasons_check_fails",
      "start": 149,
      "end": 156,
      "assertions": [],
      "raises": [
       {
        "line": 155,
        "context": "pytest.raises(acceptance.CheckFailure, match='vacuous')"
       }
      ]
     },
     {
      "symbol": "test_mcp_call_over_stdio_returns_payload",
      "start": 159,
      "end": 165,
      "assertions": [
       {
        "line": 164,
        "assertion": "payload['schema_version'] == '0.6'"
       },
       {
        "line": 165,
        "assertion": "payload['fingerprint'].startswith('sha256:')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mcp_call_expect_error_returns_is_error_text",
      "start": 168,
      "end": 174,
      "assertions": [
       {
        "line": 174,
        "assertion": "'not found' in text"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00169",
   "role": "selector",
   "path": "tests/test_binding.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d4390e79ddbba1470bd830d779a11c0f00732a18",
   "size": 24448,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_binding.py"
    ],
    "regions": [
     {
      "symbol": "test_every_binding_dir_has_a_skill",
      "start": 34,
      "end": 35,
      "assertions": [
       {
        "line": 35,
        "assertion": "SKILLS"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_skill_exists_with_frontmatter",
      "start": 39,
      "end": 43,
      "assertions": [
       {
        "line": 41,
        "assertion": "skill.is_file()"
       },
       {
        "line": 42,
        "assertion": "re.search(f'^name:\\\\s*{re.escape(skill.parent.name)}$', text, re.M)"
       },
       {
        "line": 43,
        "assertion": "re.search('^description:\\\\s*\\\\S', text, re.M)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_binding_invokes_a_devflow_entrypoint",
      "start": 47,
      "end": 53,
      "assertions": [
       {
        "line": 49,
        "assertion": "'devflow ready-issues' in text or 'select_ready_issues' in text or 'devflow pr triage' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_binding_invokes_no_gh",
      "start": 57,
      "end": 63,
      "assertions": [
       {
        "line": 61,
        "assertion": "not re.search('\\\\bgh\\\\s+(issue|pr|api|label|repo|search|graphql)', _content(skill))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_binding_parses_no_issue_body",
      "start": 67,
      "end": 70,
      "assertions": [
       {
        "line": 70,
        "assertion": "pattern not in _content(skill)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_binding_references_contract_discipline",
      "start": 74,
      "end": 80,
      "assertions": [
       {
        "line": 78,
        "assertion": "'VC-D-01' in text"
       },
       {
        "line": 79,
        "assertion": "'VC-D-05' in text"
       },
       {
        "line": 80,
        "assertion": "re.search('再(選別|ランク)[^\\\\n]*しない', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_exists",
      "start": 86,
      "end": 87,
      "assertions": [
       {
        "line": 87,
        "assertion": "NEXT_BATCH.is_file()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_recommends_first_group_verbatim",
      "start": 90,
      "end": 95,
      "assertions": [
       {
        "line": 92,
        "assertion": "re.search('先頭グループ', text)"
       },
       {
        "line": 95,
        "assertion": "re.search('(groups?\\\\[?0|最初|先頭)', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_emits_selection_and_group_id_for_handoff",
      "start": 98,
      "end": 104,
      "assertions": [
       {
        "line": 102,
        "assertion": "'selection:' in text"
       },
       {
        "line": 103,
        "assertion": "'group:' in text"
       },
       {
        "line": 104,
        "assertion": "'devin-milestone' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_defers_milestone_default_to_devflow",
      "start": 107,
      "end": 112,
      "assertions": [
       {
        "line": 110,
        "assertion": "re.search('milestone[^\\\\n]*推定[^\\\\n]*しない|推定[^\\\\n]*しない[^\\\\n]*milestone', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_reports_reasons_and_fingerprint",
      "start": 115,
      "end": 118,
      "assertions": [
       {
        "line": 117,
        "assertion": "'fingerprint' in text"
       },
       {
        "line": 118,
        "assertion": "re.search('(理由|reason)', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_next_batch_forbids_writes_and_execution",
      "start": 121,
      "end": 124,
      "assertions": [
       {
        "line": 124,
        "assertion": "re.search('(起動|worktree)[^\\\\n]*しない', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_exists",
      "start": 133,
      "end": 134,
      "assertions": [
       {
        "line": 134,
        "assertion": "TASK_PLAN.is_file()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_uses_the_plan_commands",
      "start": 137,
      "end": 140,
      "assertions": [
       {
        "line": 140,
        "assertion": "cmd in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_requires_human_approval_before_publish",
      "start": 143,
      "end": 148,
      "assertions": [
       {
        "line": 145,
        "assertion": "'--approved' in text"
       },
       {
        "line": 146,
        "assertion": "'fingerprint' in text"
       },
       {
        "line": 148,
        "assertion": "re.search('承認[^\\\\n]*のみ|承認[^\\\\n]*後', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_reapproves_on_content_change",
      "start": 151,
      "end": 154,
      "assertions": [
       {
        "line": 154,
        "assertion": "re.search('(変えたら|変更したら)[^\\\\n]*(preview|承認)', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_covers_resume",
      "start": 157,
      "end": 160,
      "assertions": [
       {
        "line": 159,
        "assertion": "'--state' in text"
       },
       {
        "line": 160,
        "assertion": "'--adopt' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_does_not_implement_contract_logic",
      "start": 163,
      "end": 167,
      "assertions": [
       {
        "line": 167,
        "assertion": "re.search('(検証|登録順|依存)[^\\\\n]*(独自に|このスキル)[^\\\\n]*しない', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_distinguishes_creatable_from_agent_ready",
      "start": 170,
      "end": 172,
      "assertions": [
       {
        "line": 172,
        "assertion": "'creatable' in text or '作成可能' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_task_plan_templates_are_valid_plans",
      "start": 180,
      "end": 187,
      "assertions": [
       {
        "line": 187,
        "assertion": "plan is not None"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_execute_contract_exists",
      "start": 228,
      "end": 229,
      "assertions": [
       {
        "line": 229,
        "assertion": "EXECUTE_CONTRACT.is_file()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_execute_contract_required_clauses",
      "start": 233,
      "end": 239,
      "assertions": [
       {
        "line": 237,
        "assertion": "re.search(heading_pat, heading)"
       },
       {
        "line": 239,
        "assertion": "re.search(pat, body)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_execute_skill_references_contract",
      "start": 242,
      "end": 244,
      "assertions": [
       {
        "line": 244,
        "assertion": "'CONTRACT.md' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_execute_skill_does_not_duplicate_contract_clauses",
      "start": 247,
      "end": 259,
      "assertions": [
       {
        "line": 259,
        "assertion": "phrase not in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_execute_adopts_first_group_verbatim",
      "start": 262,
      "end": 265,
      "assertions": [
       {
        "line": 264,
        "assertion": "'groups[0]' in text"
       },
       {
        "line": 265,
        "assertion": "re.search('先頭グループ', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_contract_exists",
      "start": 309,
      "end": 310,
      "assertions": [
       {
        "line": 310,
        "assertion": "SWEEP_CONTRACT.is_file()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_contract_required_clauses",
      "start": 314,
      "end": 320,
      "assertions": [
       {
        "line": 318,
        "assertion": "re.search(heading_pat, heading)"
       },
       {
        "line": 320,
        "assertion": "re.search(pat, body)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_contract_covers_pc1_through_pc7",
      "start": 323,
      "end": 328,
      "assertions": [
       {
        "line": 328,
        "assertion": "re.search(f'^### PC-{i}\\\\b', text, re.M)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_skill_references_contract",
      "start": 331,
      "end": 333,
      "assertions": [
       {
        "line": 333,
        "assertion": "'CONTRACT.md' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_adopts_execution_order_verbatim",
      "start": 336,
      "end": 339,
      "assertions": [
       {
        "line": 338,
        "assertion": "'execution_order' in text"
       },
       {
        "line": 339,
        "assertion": "'devflow pr triage' in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_contract_inherits_without_restatement",
      "start": 342,
      "end": 355,
      "assertions": [
       {
        "line": 355,
        "assertion": "not re.search(pat, text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_inherited_clause_ids_resolve",
      "start": 358,
      "end": 362,
      "assertions": [],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_references_only_documented_triage_fields",
      "start": 365,
      "end": 396,
      "assertions": [
       {
        "line": 394,
        "assertion": "part in defined"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_skill_does_not_duplicate_contract_clauses",
      "start": 399,
      "end": 409,
      "assertions": [
       {
        "line": 409,
        "assertion": "not re.search(pat, text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_devin_pr_sweep_skill_states_no_numeric_bound",
      "start": 412,
      "end": 419,
      "assertions": [
       {
        "line": 418,
        "assertion": "not re.search('最大\\\\s*\\\\d+', text)"
       },
       {
        "line": 419,
        "assertion": "not re.search('\\\\d+\\\\s*(ラウンド|巡|runs?|rounds?)', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_sweep_round_bound_lives_only_in_contract_pc4",
      "start": 422,
      "end": 434,
      "assertions": [
       {
        "line": 426,
        "assertion": "re.search('最大\\\\s*\\\\d+\\\\s*ラウンド', section)"
       },
       {
        "line": 432,
        "assertion": "not re.search('最大\\\\s*\\\\d+\\\\s*ラウンド', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_binding_is_documented",
      "start": 445,
      "end": 455,
      "assertions": [
       {
        "line": 452,
        "assertion": "re.search(f'^## {re.escape(name)}$', bindings_doc, re.M)"
       },
       {
        "line": 455,
        "assertion": "row and name in row.group(0)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_docs_do_not_restate_execute_contract",
      "start": 458,
      "end": 464,
      "assertions": [
       {
        "line": 464,
        "assertion": "phrase not in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mcp_server_registers_no_write_or_publish_tool",
      "start": 632,
      "end": 641,
      "assertions": [
       {
        "line": 639,
        "assertion": "tools"
       },
       {
        "line": 641,
        "assertion": "write_named == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mcp_server_never_imports_the_write_path",
      "start": 644,
      "end": 652,
      "assertions": [
       {
        "line": 649,
        "assertion": "'github_write' not in src"
       },
       {
        "line": 650,
        "assertion": "'devflow.publish' not in src"
       },
       {
        "line": 651,
        "assertion": "'from devflow import publish' not in src"
       },
       {
        "line": 652,
        "assertion": "'run_publish' not in src"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_review_round_bound_lives_only_in_contract_ec6",
      "start": 655,
      "end": 668,
      "assertions": [
       {
        "line": 660,
        "assertion": "re.search('最大\\\\s*\\\\d+\\\\s*巡', section)"
       },
       {
        "line": 666,
        "assertion": "not re.search('最大\\\\s*\\\\d+\\\\s*巡', text)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec1_first_group_and_execution_order_adopted_verbatim",
      "start": 503,
      "end": 533,
      "assertions": [
       {
        "line": 509,
        "assertion": "_missing(cli, ('groups\\\\[0\\\\]', 'execution_order', 'execution_order[^\\\\n]*変化[^\\\\n]*replan')) == []"
       },
       {
        "line": 523,
        "assertion": "_missing(lib, ('execution order is the contract', 'instead of reordering', 'def launch_devin_selector\\\\([^)]*execution_order')) == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec2_single_pr",
      "start": 535,
      "end": 537,
      "assertions": [
       {
        "line": 537,
        "assertion": "_missing(lib, ('Create exactly one PR', 'as one PR')) == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec3_issue_set_reaches_the_agent",
      "start": 539,
      "end": 555,
      "assertions": [
       {
        "line": 545,
        "assertion": "_missing(lib, ('Selected issues: \\\\{issues\\\\}', '#\\\\{n\\\\}', 'def launch_devin_selector\\\\([^)]*issue_numbers')) == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec4_no_merge_no_force_push",
      "start": 557,
      "end": 569,
      "assertions": [
       {
        "line": 560,
        "assertion": "len(re.findall('do not merge', lib, re.I)) >= 2"
       },
       {
        "line": 566,
        "assertion": "not re.search('\\\\[\\\\s*[\\'\\\\\"]git[\\'\\\\\"][^\\\\]]*[\\'\\\\\"]push[\\'\\\\\"][^\\\\]]*(-f\\\\b|--force)', lib + cli)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec5_ci_green_gates_readiness",
      "start": 571,
      "end": 586,
      "assertions": [
       {
        "line": 576,
        "assertion": "_missing(lib, ('required checks[^\\\\n]*(pass|green)', 'verification_confirmed', 'verification_unconfirmed')) == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec6_review_round_bound_matches_contract",
      "start": 588,
      "end": 600,
      "assertions": [
       {
        "line": 597,
        "assertion": "bound"
       },
       {
        "line": 600,
        "assertion": "rounds == {n}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ec7_tracker_text_is_untrusted_data",
      "start": 602,
      "end": 609,
      "assertions": [
       {
        "line": 609,
        "assertion": "found >= 2"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00170",
   "role": "selector",
   "path": "tests/test_cli.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "946ca7c275ca3b5f6c2e1f218a6612aecc880bb7",
   "size": 37690,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_cli.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00171",
   "role": "selector",
   "path": "tests/test_contract.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "06bd0cd22bc88994c557ef16d0d3aea4ed265119",
   "size": 27155,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_contract.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00172",
   "role": "selector",
   "path": "tests/test_decisions.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ab66fbce4de1e41364e4b54f0b7fc3260436d567",
   "size": 4525,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_decisions.py"
    ],
    "regions": [
     {
      "symbol": "test_option_only_resolution_stays_pending",
      "start": 112,
      "end": 116,
      "assertions": [
       {
        "line": 116,
        "assertion": "unresolved_decisions(body) == ('Choose reviewer — Alice',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_option_then_rationale_resolves",
      "start": 119,
      "end": 123,
      "assertions": [
       {
        "line": 123,
        "assertion": "unresolved_decisions(body) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_later_decisions_section_is_not_hidden",
      "start": 126,
      "end": 132,
      "assertions": [
       {
        "line": 132,
        "assertion": "unresolved_decisions(body) == ('Choose reviewer',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_no_section",
      "start": 13,
      "end": 14,
      "assertions": [
       {
        "line": 14,
        "assertion": "unresolved_decisions(_body('## Scope\\n\\n- x\\n')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_none_body",
      "start": 16,
      "end": 18,
      "assertions": [
       {
        "line": 17,
        "assertion": "unresolved_decisions(None) == ()"
       },
       {
        "line": 18,
        "assertion": "unresolved_decisions('') == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_empty_section",
      "start": 20,
      "end": 21,
      "assertions": [
       {
        "line": 21,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_none_entry",
      "start": 23,
      "end": 29,
      "assertions": [
       {
        "line": 24,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\nNone\\n\\n<!-- devflow:plan=p item=i -->')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_none_with_period",
      "start": 31,
      "end": 32,
      "assertions": [
       {
        "line": 32,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\nNone.\\n')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_heading_case_and_level",
      "start": 34,
      "end": 37,
      "assertions": [
       {
        "line": 35,
        "assertion": "unresolved_decisions(_body('### decisions required\\n\\n- pick a model\\n')) == ('pick a model',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_plain_bullet_is_pending",
      "start": 41,
      "end": 44,
      "assertions": [
       {
        "line": 42,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- Choose the reviewer\\n')) == ('Choose the reviewer',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_unchecked_checkbox_is_pending",
      "start": 46,
      "end": 49,
      "assertions": [
       {
        "line": 47,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- [ ] Choose the reviewer\\n')) == ('Choose the reviewer',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_multiple_pending",
      "start": 51,
      "end": 53,
      "assertions": [
       {
        "line": 53,
        "assertion": "unresolved_decisions(body) == ('one', 'two', 'three', 'four')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_checked_without_annotation_is_pending",
      "start": 55,
      "end": 58,
      "assertions": [
       {
        "line": 56,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- [x] Choose the reviewer\\n')) == ('Choose the reviewer',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_checked_with_annotation_is_resolved",
      "start": 60,
      "end": 69,
      "assertions": [
       {
        "line": 61,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- [x] Choose the reviewer — OpenCode space-bunny. Cheapest.\\n')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_uppercase_checked_resolved",
      "start": 71,
      "end": 77,
      "assertions": [
       {
        "line": 72,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- [X] pick — option A. it won\\n')) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_annotation_needs_text_after_dash",
      "start": 79,
      "end": 82,
      "assertions": [
       {
        "line": 80,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\n- [x] pick —\\n')) == ('pick —',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_non_none_prose_fails_closed",
      "start": 84,
      "end": 87,
      "assertions": [
       {
        "line": 85,
        "assertion": "unresolved_decisions(_body('## Decisions required\\n\\ndecide the reviewer later\\n')) == ('decide the reviewer later',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_trailing_marker_comment_ignored",
      "start": 89,
      "end": 94,
      "assertions": [
       {
        "line": 94,
        "assertion": "unresolved_decisions(body) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_section_ends_at_next_heading",
      "start": 96,
      "end": 101,
      "assertions": [
       {
        "line": 101,
        "assertion": "unresolved_decisions(body) == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mixed_resolved_and_pending",
      "start": 103,
      "end": 109,
      "assertions": [
       {
        "line": 109,
        "assertion": "unresolved_decisions(body) == ('budget unit',)"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00173",
   "role": "selector",
   "path": "tests/test_deps.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "79c7872dbc75420062990ad08b65e7e1ab45348b",
   "size": 39043,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_deps.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00174",
   "role": "selector",
   "path": "tests/test_graph.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "184c7314baa84c99d94515b706dbe6d105f8cdf8",
   "size": 4047,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_graph.py"
    ],
    "regions": [
     {
      "symbol": "test_hermes_setup_ms2_shape",
      "start": 27,
      "end": 38,
      "assertions": [
       {
        "line": 37,
        "assertion": "graph.order == (75, 76, 77, 78)"
       },
       {
        "line": 38,
        "assertion": "graph.depth == 1"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_chain_order",
      "start": 40,
      "end": 44,
      "assertions": [
       {
        "line": 43,
        "assertion": "graph.order == (1, 2, 3)"
       },
       {
        "line": 44,
        "assertion": "graph.depth == 2"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ties_break_by_issue_number",
      "start": 46,
      "end": 50,
      "assertions": [
       {
        "line": 49,
        "assertion": "graph.order == (1, 3, 5)"
       },
       {
        "line": 50,
        "assertion": "graph.depth == 0"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_cycle_members_excluded_from_order",
      "start": 52,
      "end": 60,
      "assertions": [
       {
        "line": 59,
        "assertion": "graph.order == (3,)"
       },
       {
        "line": 60,
        "assertion": "graph.cyclic == frozenset({1, 2})"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_input_order_independent",
      "start": 62,
      "end": 70,
      "assertions": [
       {
        "line": 70,
        "assertion": "a.order == b.order == (1, 2, 3)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dependent_of_cycle_member_is_orderable",
      "start": 72,
      "end": 83,
      "assertions": [
       {
        "line": 81,
        "assertion": "graph.order == (3,)"
       },
       {
        "line": 82,
        "assertion": "graph.internal[3] == frozenset({1})"
       },
       {
        "line": 83,
        "assertion": "graph.cyclic == frozenset({1, 2})"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dep_on_open_issue_outside_group_is_external",
      "start": 87,
      "end": 91,
      "assertions": [
       {
        "line": 90,
        "assertion": "graph.external[1] == frozenset({2})"
       },
       {
        "line": 91,
        "assertion": "graph.internal[1] == frozenset()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dep_on_member_is_internal",
      "start": 93,
      "end": 97,
      "assertions": [
       {
        "line": 96,
        "assertion": "graph.external[1] == frozenset()"
       },
       {
        "line": 97,
        "assertion": "graph.internal[1] == frozenset({2})"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dep_on_closed_issue_is_not_external",
      "start": 99,
      "end": 102,
      "assertions": [
       {
        "line": 102,
        "assertion": "graph.external[1] == frozenset()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dep_on_missing_issue_is_not_external",
      "start": 104,
      "end": 107,
      "assertions": [
       {
        "line": 107,
        "assertion": "graph.external[1] == frozenset()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_empty_group",
      "start": 109,
      "end": 112,
      "assertions": [
       {
        "line": 111,
        "assertion": "graph.order == ()"
       },
       {
        "line": 112,
        "assertion": "graph.depth == 0"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00175",
   "role": "selector",
   "path": "tests/test_grouping.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "ee54db5250a7cf9dfa54b5fc603960e1dd492e33",
   "size": 4308,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_grouping.py"
    ],
    "regions": [
     {
      "symbol": "test_namespace_precedence",
      "start": 24,
      "end": 29,
      "assertions": [
       {
        "line": 29,
        "assertion": "key == ('type:implementation', 'area:core', 'priority:high', 'misc')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_other_labels_sorted_alphabetically",
      "start": 31,
      "end": 34,
      "assertions": [
       {
        "line": 34,
        "assertion": "key == ('enhancement', 'local-agent-observability')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_same_set_same_key_regardless_of_order",
      "start": 36,
      "end": 39,
      "assertions": [
       {
        "line": 39,
        "assertion": "a == b"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_multiple_labels_in_one_namespace_sorted",
      "start": 41,
      "end": 43,
      "assertions": [
       {
        "line": 43,
        "assertion": "key == ('type:x', 'area:a', 'area:b')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_same_labels_same_milestone_one_group",
      "start": 47,
      "end": 56,
      "assertions": [
       {
        "line": 54,
        "assertion": "len(groups) == 1"
       },
       {
        "line": 55,
        "assertion": "groups[0].members == (96, 97, 99, 102)"
       },
       {
        "line": 56,
        "assertion": "groups[0].key == ('type:implementation', 'area:core')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_different_milestone_splits",
      "start": 58,
      "end": 64,
      "assertions": [
       {
        "line": 64,
        "assertion": "len(groups) == 2"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_different_labels_split",
      "start": 66,
      "end": 72,
      "assertions": [
       {
        "line": 72,
        "assertion": "len(groups) == 2"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_label_order_difference_does_not_split",
      "start": 74,
      "end": 81,
      "assertions": [
       {
        "line": 80,
        "assertion": "len(groups) == 1"
       },
       {
        "line": 81,
        "assertion": "groups[0].members == (1, 2)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_split_over_eight",
      "start": 83,
      "end": 89,
      "assertions": [
       {
        "line": 86,
        "assertion": "[len(g.members) for g in groups] == [8, 8, 3]"
       },
       {
        "line": 87,
        "assertion": "groups[0].members == tuple(range(1, 9))"
       },
       {
        "line": 89,
        "assertion": "{g.key for g in groups} == {('type:implementation',)}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_unlabeled_issues_get_unique_keys",
      "start": 91,
      "end": 96,
      "assertions": [
       {
        "line": 94,
        "assertion": "len(groups) == 2"
       },
       {
        "line": 95,
        "assertion": "{g.key for g in groups} == {('issue:1',), ('issue:2',)}"
       },
       {
        "line": 96,
        "assertion": "all((len(g.members) == 1 for g in groups))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_unlabeled_never_merges_with_labeled",
      "start": 98,
      "end": 101,
      "assertions": [
       {
        "line": 101,
        "assertion": "len(groups) == 2"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_label_named_issue_colon_n_never_merges_with_unlabeled",
      "start": 103,
      "end": 108,
      "assertions": [
       {
        "line": 108,
        "assertion": "len(groups) == 2"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_deterministic_regardless_of_input_order",
      "start": 110,
      "end": 114,
      "assertions": [
       {
        "line": 112,
        "assertion": "group_issues(issues, PRECEDENCE) == group_issues(list(reversed(issues)), PRECEDENCE)"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00176",
   "role": "selector",
   "path": "tests/test_mcp_server.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "2be86ad5d6b8372d7404bb3dfe02e1c8f256134f",
   "size": 43543,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_mcp_server.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00177",
   "role": "selector",
   "path": "tests/test_plan.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b5cca16333129dda5c3863cfdbe2cafdc47521b7",
   "size": 43925,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_plan.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00178",
   "role": "selector",
   "path": "tests/test_plan_schemas.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "39e061f8a0550758a3698aa4ece522c8b3047e0f",
   "size": 4373,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_plan_schemas.py"
    ],
    "regions": [
     {
      "symbol": "test_plan_documents_conform",
      "start": 41,
      "end": 43,
      "assertions": [
       {
        "line": 43,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_valid_fixture_conforms",
      "start": 46,
      "end": 48,
      "assertions": [
       {
        "line": 48,
        "assertion": "not list(PLAN_SCHEMA.iter_errors(doc))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_invalid_fixture_is_schema_and_core_rejected",
      "start": 51,
      "end": 57,
      "assertions": [
       {
        "line": 53,
        "assertion": "list(PLAN_SCHEMA.iter_errors(doc))"
       },
       {
        "line": 57,
        "assertion": "plan is None and diags"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_validate_payload_conforms",
      "start": 67,
      "end": 71,
      "assertions": [
       {
        "line": 71,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_validate_failure_payload_conforms",
      "start": 74,
      "end": 80,
      "assertions": [
       {
        "line": 78,
        "assertion": "payload['ok'] is False"
       },
       {
        "line": 80,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_preview_artifact_conforms",
      "start": 83,
      "end": 88,
      "assertions": [
       {
        "line": 88,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_publish_result_conforms",
      "start": 91,
      "end": 97,
      "assertions": [
       {
        "line": 95,
        "assertion": "code == 0"
       },
       {
        "line": 97,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_publish_incomplete_result_conforms",
      "start": 100,
      "end": 108,
      "assertions": [
       {
        "line": 105,
        "assertion": "code == 5"
       },
       {
        "line": 106,
        "assertion": "payload['complete'] is False"
       },
       {
        "line": 108,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_receipt_conforms",
      "start": 111,
      "end": 119,
      "assertions": [
       {
        "line": 116,
        "assertion": "code == 0 and payload['complete']"
       },
       {
        "line": 119,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_partial_receipt_conforms",
      "start": 122,
      "end": 130,
      "assertions": [
       {
        "line": 127,
        "assertion": "code == 5 and (not payload['complete'])"
       },
       {
        "line": 130,
        "assertion": "not errors"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00179",
   "role": "selector",
   "path": "tests/test_policy.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "7f0e36ab6db2bcfef7803ff761e1ce8e295a2cab",
   "size": 26231,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_policy.py"
    ],
    "regions": [
     {
      "symbol": "test_defaults_match_next_batch",
      "start": 22,
      "end": 30,
      "assertions": [
       {
        "line": 24,
        "assertion": "p.exclusion_labels == ('epic', 'tracking')"
       },
       {
        "line": 25,
        "assertion": "p.exclusion_label_prefixes == ('needs-',)"
       },
       {
        "line": 26,
        "assertion": "p.require_labels is True"
       },
       {
        "line": 27,
        "assertion": "p.group_key_precedence == ('type:', 'area:', 'priority:')"
       },
       {
        "line": 28,
        "assertion": "p.priority_namespaces == ('priority:',)"
       },
       {
        "line": 29,
        "assertion": "p.detect_in_flight is True"
       },
       {
        "line": 30,
        "assertion": "p.max_group_size == 8"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_config_merges_over_defaults",
      "start": 33,
      "end": 39,
      "assertions": [
       {
        "line": 37,
        "assertion": "p.max_group_size == 4"
       },
       {
        "line": 38,
        "assertion": "p.detect_in_flight is False"
       },
       {
        "line": 39,
        "assertion": "p.exclusion_labels == ('epic', 'tracking')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_shipped_policies_load",
      "start": 198,
      "end": 202,
      "assertions": [
       {
        "line": 202,
        "assertion": "RepoPolicy.load(ROOT / 'policies' / 'hermes-setup.yaml').max_group_size == 3"
       },
       {
        "line": 201,
        "assertion": "'epic' in p.exclusion_labels"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_shipped_policies_equal_defaults",
      "start": 205,
      "end": 216,
      "assertions": [
       {
        "line": 216,
        "assertion": "hermes == replace(RepoPolicy.defaults(), max_group_size=3)"
       },
       {
        "line": 214,
        "assertion": "p == RepoPolicy.defaults()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_effective",
      "start": 253,
      "end": 255,
      "assertions": [
       {
        "line": 255,
        "assertion": "json.loads(out)['group_key_precedence'] == ['type:', 'area:', 'priority:']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_raw_overrides",
      "start": 258,
      "end": 262,
      "assertions": [
       {
        "line": 262,
        "assertion": "json.loads(out) == {'max_group_size': 4}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_bad_policy_exit_code",
      "start": 265,
      "end": 269,
      "assertions": [
       {
        "line": 267,
        "assertion": "rc == 4"
       },
       {
        "line": 269,
        "assertion": "'policy file not found' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_raw_view_also_validates",
      "start": 272,
      "end": 277,
      "assertions": [
       {
        "line": 276,
        "assertion": "rc == 2"
       },
       {
        "line": 277,
        "assertion": "'unknown policy keys' in capsys.readouterr().err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_block_list",
      "start": 43,
      "end": 47,
      "assertions": [
       {
        "line": 47,
        "assertion": "p.exclusion_labels == ('epic', 'tracking', 'wontfix')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_flow_list",
      "start": 49,
      "end": 53,
      "assertions": [
       {
        "line": 53,
        "assertion": "p.group_key_precedence == ('type:', 'kind:')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_comments_and_blank_lines",
      "start": 55,
      "end": 59,
      "assertions": [
       {
        "line": 59,
        "assertion": "p.max_group_size == 3"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_quoted_values",
      "start": 61,
      "end": 65,
      "assertions": [
       {
        "line": 65,
        "assertion": "p.exclusion_labels == ('epic',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_nested_map_rejected",
      "start": 67,
      "end": 73,
      "assertions": [],
      "raises": [
       {
        "line": 72,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_anchor_on_known_key_rejected",
      "start": 75,
      "end": 79,
      "assertions": [],
      "raises": [
       {
        "line": 78,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_scalar_for_list_field_rejected",
      "start": 81,
      "end": 86,
      "assertions": [
       {
        "line": 86,
        "assertion": "e.value.exit_code == 2"
       }
      ],
      "raises": [
       {
        "line": 84,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_non_int_max_group_size_rejected",
      "start": 88,
      "end": 93,
      "assertions": [
       {
        "line": 93,
        "assertion": "e.value.exit_code == 2"
       }
      ],
      "raises": [
       {
        "line": 91,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_unreadable_config_errors",
      "start": 95,
      "end": 103,
      "assertions": [],
      "raises": [
       {
        "line": 100,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_undecodable_config_errors",
      "start": 105,
      "end": 109,
      "assertions": [],
      "raises": [
       {
        "line": 108,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_missing_config_errors",
      "start": 111,
      "end": 114,
      "assertions": [
       {
        "line": 114,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 112,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_quoted_hash_preserved",
      "start": 116,
      "end": 120,
      "assertions": [
       {
        "line": 120,
        "assertion": "p.exclusion_labels == ('a # b', 'epic')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_quoted_hash_in_scalar",
      "start": 122,
      "end": 129,
      "assertions": [],
      "raises": [
       {
        "line": 128,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_deep_indented_list_item_rejected",
      "start": 131,
      "end": 135,
      "assertions": [],
      "raises": [
       {
        "line": 134,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_block_scalar_rejected",
      "start": 137,
      "end": 141,
      "assertions": [],
      "raises": [
       {
        "line": 140,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_tag_rejected",
      "start": 143,
      "end": 147,
      "assertions": [],
      "raises": [
       {
        "line": 146,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_flow_map_rejected",
      "start": 149,
      "end": 153,
      "assertions": [],
      "raises": [
       {
        "line": 152,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_document_marker_rejected",
      "start": 155,
      "end": 159,
      "assertions": [],
      "raises": [
       {
        "line": 158,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_non_positive_max_group_size_rejected",
      "start": 161,
      "end": 169,
      "assertions": [
       {
        "line": 166,
        "assertion": "e.value.exit_code == 2"
       }
      ],
      "raises": [
       {
        "line": 164,
        "context": "pytest.raises(PolicyError)"
       },
       {
        "line": 168,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_duplicate_key_rejected",
      "start": 171,
      "end": 175,
      "assertions": [],
      "raises": [
       {
        "line": 174,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_quoted_int_rejected",
      "start": 177,
      "end": 182,
      "assertions": [],
      "raises": [
       {
        "line": 181,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_unindented_list_item_rejected",
      "start": 184,
      "end": 188,
      "assertions": [],
      "raises": [
       {
        "line": 187,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_unknown_key_rejected",
      "start": 190,
      "end": 195,
      "assertions": [
       {
        "line": 195,
        "assertion": "e.value.exit_code == 2"
       }
      ],
      "raises": [
       {
        "line": 193,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_shape",
      "start": 220,
      "end": 223,
      "assertions": [
       {
        "line": 222,
        "assertion": "fp.startswith('sha256:')"
       },
       {
        "line": 223,
        "assertion": "len(fp) == len('sha256:') + 64"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_same_policy_same_digest",
      "start": 225,
      "end": 228,
      "assertions": [
       {
        "line": 226,
        "assertion": "policy_fingerprint(RepoPolicy.defaults()) == policy_fingerprint(RepoPolicy.defaults())"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_different_policy_different_digest",
      "start": 230,
      "end": 232,
      "assertions": [
       {
        "line": 232,
        "assertion": "policy_fingerprint(changed) != policy_fingerprint(RepoPolicy.defaults())"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_exclude_labels_change_digest",
      "start": 234,
      "end": 241,
      "assertions": [
       {
        "line": 241,
        "assertion": "policy_fingerprint(changed) != policy_fingerprint(RepoPolicy.defaults())"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_label_order_changes_digest",
      "start": 243,
      "end": 250,
      "assertions": [
       {
        "line": 250,
        "assertion": "policy_fingerprint(a_then_b) != policy_fingerprint(b_then_a)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_applies_when_remote_matches_snapshot_repo",
      "start": 363,
      "end": 372,
      "assertions": [
       {
        "line": 370,
        "assertion": "rc == 0"
       },
       {
        "line": 371,
        "assertion": "unlabeled_grouped(json.loads(out))"
       },
       {
        "line": 372,
        "assertion": "str(checkout / '.devflow' / 'policy.yaml') in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_explicit_policy_wins_over_discovered",
      "start": 374,
      "end": 390,
      "assertions": [
       {
        "line": 384,
        "assertion": "rc == 0"
       },
       {
        "line": 386,
        "assertion": "not unlabeled_grouped(payload)"
       },
       {
        "line": 387,
        "assertion": "payload['policy_fingerprint'] == policy_fingerprint(RepoPolicy.load(explicit))"
       },
       {
        "line": 390,
        "assertion": "'policy' not in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_skipped_when_snapshot_repo_differs",
      "start": 392,
      "end": 399,
      "assertions": [
       {
        "line": 397,
        "assertion": "rc == 0"
       },
       {
        "line": 398,
        "assertion": "not unlabeled_grouped(json.loads(out))"
       },
       {
        "line": 399,
        "assertion": "err == ''"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_repo_flag_does_not_override_snapshot_repository",
      "start": 401,
      "end": 412,
      "assertions": [
       {
        "line": 411,
        "assertion": "rc == 0"
       },
       {
        "line": 412,
        "assertion": "not unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_repo_matching_remote_applies",
      "start": 414,
      "end": 421,
      "assertions": [
       {
        "line": 420,
        "assertion": "rc == 0"
       },
       {
        "line": 421,
        "assertion": "unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_subdirectory_of_checkout_discovers",
      "start": 423,
      "end": 431,
      "assertions": [
       {
        "line": 430,
        "assertion": "rc == 0"
       },
       {
        "line": 431,
        "assertion": "unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_no_checkout_falls_back_to_defaults",
      "start": 433,
      "end": 442,
      "assertions": [
       {
        "line": 441,
        "assertion": "rc == 0"
       },
       {
        "line": 442,
        "assertion": "not unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_checkout_without_remotes_skips_policy",
      "start": 444,
      "end": 450,
      "assertions": [
       {
        "line": 449,
        "assertion": "rc == 0"
       },
       {
        "line": 450,
        "assertion": "not unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_invalid_discovered_policy_fails_loudly",
      "start": 452,
      "end": 460,
      "assertions": [
       {
        "line": 459,
        "assertion": "rc == 2"
       },
       {
        "line": 460,
        "assertion": "'max_group_size' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mcp_matches_cli_fingerprint",
      "start": 462,
      "end": 473,
      "assertions": [
       {
        "line": 469,
        "assertion": "rc == 0"
       },
       {
        "line": 472,
        "assertion": "mcp['policy_fingerprint'] == cli['policy_fingerprint']"
       },
       {
        "line": 473,
        "assertion": "mcp['fingerprint'] == cli['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_reports_checkout_source",
      "start": 475,
      "end": 481,
      "assertions": [
       {
        "line": 479,
        "assertion": "rc == 0"
       },
       {
        "line": 480,
        "assertion": "json.loads(out)['require_labels'] is False"
       },
       {
        "line": 481,
        "assertion": "'repository checkout' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_reports_mismatch",
      "start": 483,
      "end": 489,
      "assertions": [
       {
        "line": 487,
        "assertion": "rc == 0"
       },
       {
        "line": 488,
        "assertion": "json.loads(out)['require_labels'] is True"
       },
       {
        "line": 489,
        "assertion": "'ignored' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_show_flag_source",
      "start": 491,
      "end": 499,
      "assertions": [
       {
        "line": 497,
        "assertion": "rc == 0"
       },
       {
        "line": 498,
        "assertion": "json.loads(out) == {'max_group_size': 4}"
       },
       {
        "line": 499,
        "assertion": "'--policy' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fingerprint_depends_on_cwd",
      "start": 501,
      "end": 516,
      "assertions": [
       {
        "line": 515,
        "assertion": "inside['policy_fingerprint'] != outside['policy_fingerprint']"
       },
       {
        "line": 516,
        "assertion": "inside['fingerprint'] != outside['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_load_policy_explicit_none_means_checkout_subject",
      "start": 518,
      "end": 527,
      "assertions": [
       {
        "line": 527,
        "assertion": "policy.require_labels is False"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_malformed_remote_url_is_ignored",
      "start": 529,
      "end": 538,
      "assertions": [
       {
        "line": 537,
        "assertion": "rc == 0"
       },
       {
        "line": 538,
        "assertion": "unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_cross_host_same_owner_repo_adopted",
      "start": 540,
      "end": 551,
      "assertions": [
       {
        "line": 550,
        "assertion": "rc == 0"
       },
       {
        "line": 551,
        "assertion": "unlabeled_grouped(json.loads(out))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_adoption_diagnostic_on_query_and_mcp_paths",
      "start": 553,
      "end": 565,
      "assertions": [
       {
        "line": 562,
        "assertion": "rc == 0"
       },
       {
        "line": 563,
        "assertion": "'repository policy file' in err"
       },
       {
        "line": 565,
        "assertion": "'repository policy file' in capsys.readouterr().err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_parse",
      "start": 589,
      "end": 590,
      "assertions": [
       {
        "line": 590,
        "assertion": "parse_repo_url(url) == expected"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_normalize",
      "start": 602,
      "end": 603,
      "assertions": [
       {
        "line": 603,
        "assertion": "normalize_repo_name(name) == expected"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_int_list_accepted",
      "start": 611,
      "end": 615,
      "assertions": [
       {
        "line": 615,
        "assertion": "p.exclusion_issues == (5, 9)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_string_list_rejected",
      "start": 617,
      "end": 621,
      "assertions": [],
      "raises": [
       {
        "line": 620,
        "context": "pytest.raises(PolicyError, match='list of positive integers')"
       }
      ]
     },
     {
      "symbol": "test_bool_rejected",
      "start": 623,
      "end": 627,
      "assertions": [],
      "raises": [
       {
        "line": 626,
        "context": "pytest.raises(PolicyError, match='list of positive integers')"
       }
      ]
     },
     {
      "symbol": "test_nonpositive_rejected",
      "start": 630,
      "end": 634,
      "assertions": [],
      "raises": [
       {
        "line": 633,
        "context": "pytest.raises(PolicyError, match='list of positive integers')"
       }
      ]
     },
     {
      "symbol": "test_policy_file_order_and_dupes_canonicalized",
      "start": 636,
      "end": 645,
      "assertions": [
       {
        "line": 644,
        "assertion": "pa.exclusion_issues == pb.exclusion_issues == (5, 9)"
       },
       {
        "line": 645,
        "assertion": "policy_fingerprint(pa) == policy_fingerprint(pb)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_cli_exclude_issues_merge_sorted_with_policy",
      "start": 647,
      "end": 657,
      "assertions": [
       {
        "line": 656,
        "assertion": "a.exclusion_issues == (3, 7, 9)"
       },
       {
        "line": 657,
        "assertion": "policy_fingerprint(a) == policy_fingerprint(b)"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00180",
   "role": "selector",
   "path": "tests/test_pr_policy.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "6334a8322ad07336fad29b690b7b3a5b5be6be24",
   "size": 2793,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_pr_policy.py"
    ],
    "regions": [
     {
      "symbol": "test_pr_policy_merge_and_fingerprint",
      "start": 9,
      "end": 41,
      "assertions": [
       {
        "line": 11,
        "assertion": "defaults.blocking_review_decisions == ('CHANGES_REQUESTED',)"
       },
       {
        "line": 12,
        "assertion": "defaults.blocking_markers == ('🔴', 'must-fix', 'blocker', 'critical')"
       },
       {
        "line": 13,
        "assertion": "defaults.required_checks == ()"
       },
       {
        "line": 14,
        "assertion": "defaults.include_draft is False"
       },
       {
        "line": 15,
        "assertion": "defaults.exclude_labels == ()"
       },
       {
        "line": 24,
        "assertion": "merged.required_checks == ('ci/test', 'lint')"
       },
       {
        "line": 25,
        "assertion": "merged.include_draft is True"
       },
       {
        "line": 26,
        "assertion": "merged.exclude_labels == ('wip',)"
       },
       {
        "line": 29,
        "assertion": "pr_policy_fingerprint(merged) == pr_policy_fingerprint(PrPolicy(required_checks=('ci/test', 'lint'), include_draft=True, exclude_labels=('wip',)))"
       },
       {
        "line": 36,
        "assertion": "pr_policy_fingerprint(merged) != pr_policy_fingerprint(defaults)"
       },
       {
        "line": 40,
        "assertion": "policy_fingerprint(RepoPolicy.defaults()).startswith('sha256:')"
       },
       {
        "line": 41,
        "assertion": "pr_policy_fingerprint(defaults).startswith('sha256:')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_pr_policy_merge_rejects_unknown_and_mistyped",
      "start": 44,
      "end": 58,
      "assertions": [
       {
        "line": 47,
        "assertion": "exc_info.value.exit_code == 2"
       }
      ],
      "raises": [
       {
        "line": 45,
        "context": "pytest.raises(PolicyError)"
       },
       {
        "line": 49,
        "context": "pytest.raises(PolicyError)"
       },
       {
        "line": 51,
        "context": "pytest.raises(PolicyError)"
       },
       {
        "line": 53,
        "context": "pytest.raises(PolicyError)"
       },
       {
        "line": 57,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     },
     {
      "symbol": "test_pr_policy_load_file",
      "start": 61,
      "end": 74,
      "assertions": [
       {
        "line": 67,
        "assertion": "policy.required_checks == ('ci/test',)"
       },
       {
        "line": 68,
        "assertion": "policy.include_draft is True"
       },
       {
        "line": 69,
        "assertion": "policy.exclude_labels == ('wip',)"
       },
       {
        "line": 74,
        "assertion": "exc_info.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 72,
        "context": "pytest.raises(PolicyError)"
       }
      ]
     }
    ]
   }
  },
  {
   "id": "selector-F00181",
   "role": "selector",
   "path": "tests/test_pr_snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "05ace8edbdc296014a0b3d90a160cf9cc343f410",
   "size": 14026,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_pr_snapshot.py"
    ],
    "regions": [
     {
      "symbol": "test_pr_snapshot_save_load_roundtrip",
      "start": 93,
      "end": 133,
      "assertions": [
       {
        "line": 133,
        "assertion": "loaded == snap"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fetch_pr_triage_snapshot_assembles_fields",
      "start": 136,
      "end": 169,
      "assertions": [
       {
        "line": 144,
        "assertion": "snap.repository == 'o/r'"
       },
       {
        "line": 145,
        "assertion": "snap.captured_at != ''"
       },
       {
        "line": 146,
        "assertion": "len(snap.pull_requests) == 1"
       },
       {
        "line": 148,
        "assertion": "pr.number == 1"
       },
       {
        "line": 149,
        "assertion": "pr.is_draft is False"
       },
       {
        "line": 150,
        "assertion": "pr.mergeable == 'MERGEABLE'"
       },
       {
        "line": 151,
        "assertion": "pr.merge_state_status == 'CLEAN'"
       },
       {
        "line": 152,
        "assertion": "pr.review_decision == 'APPROVED'"
       },
       {
        "line": 153,
        "assertion": "pr.labels == ('bug',)"
       },
       {
        "line": 154,
        "assertion": "len(pr.checks) == 2"
       },
       {
        "line": 156,
        "assertion": "pr.checks[0].name == 'test'"
       },
       {
        "line": 157,
        "assertion": "pr.checks[0].conclusion == 'SUCCESS'"
       },
       {
        "line": 158,
        "assertion": "pr.checks[0].details_url == 'https://ci/1'"
       },
       {
        "line": 159,
        "assertion": "pr.checks[1].name == 'lint'"
       },
       {
        "line": 160,
        "assertion": "pr.checks[1].status == 'SUCCESS'"
       },
       {
        "line": 163,
        "assertion": "pr.checks[1].conclusion == 'SUCCESS'"
       },
       {
        "line": 164,
        "assertion": "pr.checks[1].details_url == 'https://ci/2'"
       },
       {
        "line": 165,
        "assertion": "len(pr.review_threads) == 1"
       },
       {
        "line": 166,
        "assertion": "pr.review_threads[0].is_resolved is True"
       },
       {
        "line": 167,
        "assertion": "pr.review_threads[0].path == 'src/x.py'"
       },
       {
        "line": 168,
        "assertion": "pr.review_threads[0].line == 12"
       },
       {
        "line": 169,
        "assertion": "pr.review_threads[0].comments == ('looks good',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_pr_ceiling_exits_6",
      "start": 172,
      "end": 179,
      "assertions": [
       {
        "line": 179,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 174,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_pr_ceiling_detects_cap_at_200",
      "start": 182,
      "end": 189,
      "assertions": [
       {
        "line": 189,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 184,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_review_threads_truncation_fails",
      "start": 192,
      "end": 205,
      "assertions": [
       {
        "line": 205,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 197,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_pr_missing_from_graphql_fails",
      "start": 208,
      "end": 217,
      "assertions": [
       {
        "line": 217,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 209,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_graphql_errors_field_fails_loudly",
      "start": 220,
      "end": 236,
      "assertions": [
       {
        "line": 236,
        "assertion": "exc_info.value.exit_code == 5"
       }
      ],
      "raises": [
       {
        "line": 228,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_null_review_threads_fails_loudly",
      "start": 239,
      "end": 249,
      "assertions": [
       {
        "line": 249,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 241,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_incomplete_review_threads_connection_fails_loudly",
      "start": 252,
      "end": 269,
      "assertions": [
       {
        "line": 269,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 261,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_thread_comments_truncation_fails",
      "start": 272,
      "end": 293,
      "assertions": [
       {
        "line": 293,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 285,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_thread_comment_bodies_are_redacted",
      "start": 296,
      "end": 312,
      "assertions": [
       {
        "line": 311,
        "assertion": "snap.pull_requests[0].review_threads[0].comments == ('token is *** oops',)"
       },
       {
        "line": 312,
        "assertion": "'ghp_sentineltoken1234567890abcd' not in json.dumps(snap.to_dict())"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fetch_argv_passes_the_read_only_choke_point",
      "start": 315,
      "end": 340,
      "assertions": [
       {
        "line": 336,
        "assertion": "len(calls) == 2"
       },
       {
        "line": 340,
        "assertion": "snap.pull_requests[0].review_threads[0].is_resolved is True"
       },
       {
        "line": 339,
        "assertion": "not any(('SENTINEL-TOKEN-9f8e7d6c' in a for a in cmd))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_graphql_read_query_allowed",
      "start": 343,
      "end": 354,
      "assertions": [],
      "raises": []
     },
     {
      "symbol": "test_graphql_write_vectors_rejected",
      "start": 382,
      "end": 385,
      "assertions": [
       {
        "line": 385,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 383,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_graphql_requires_query_field",
      "start": 388,
      "end": 397,
      "assertions": [
       {
        "line": 397,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 395,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_graphql_variable_value_mutation_word_is_not_a_write",
      "start": 400,
      "end": 405,
      "assertions": [],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00182",
   "role": "selector",
   "path": "tests/test_pr_triage.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e67a5b3dd5ce5b783de6d272e676375cad89c417",
   "size": 15135,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_pr_triage.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00183",
   "role": "selector",
   "path": "tests/test_preview.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "e4bb52141f6244139ab36c77e6971cac9a14a20b",
   "size": 20726,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_preview.py"
    ],
    "regions": [
     {
      "symbol": "test_ok",
      "start": 188,
      "end": 199,
      "assertions": [
       {
        "line": 191,
        "assertion": "payload['ok'] is True"
       },
       {
        "line": 192,
        "assertion": "payload['kind'] == 'plan-validation'"
       },
       {
        "line": 193,
        "assertion": "payload['schema_version'] == '0.1'"
       },
       {
        "line": 194,
        "assertion": "payload['plan_id'].startswith('plan-')"
       },
       {
        "line": 195,
        "assertion": "payload['registration_order'] == ['a', 'b', 'c']"
       },
       {
        "line": 197,
        "assertion": "payload['diagnostics']"
       },
       {
        "line": 198,
        "assertion": "all((d['severity'] == 'warning' for d in payload['diagnostics']))"
       },
       {
        "line": 199,
        "assertion": "all((d['code'] == 'missing_field' for d in payload['diagnostics']))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_invalid_plan_reports_diagnostics",
      "start": 201,
      "end": 206,
      "assertions": [
       {
        "line": 204,
        "assertion": "payload['ok'] is False"
       },
       {
        "line": 205,
        "assertion": "any((d['code'] == 'empty_field' for d in payload['diagnostics']))"
       },
       {
        "line": 206,
        "assertion": "'plan_id' not in payload"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_validate_never_touches_the_tracker",
      "start": 208,
      "end": 216,
      "assertions": [
       {
        "line": 216,
        "assertion": "payload['ok'] is True"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_file_maps_to_exit_4",
      "start": 218,
      "end": 221,
      "assertions": [
       {
        "line": 221,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 219,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_artifact_shape",
      "start": 225,
      "end": 242,
      "assertions": [
       {
        "line": 227,
        "assertion": "artifact['schema_version'] == '0.1'"
       },
       {
        "line": 228,
        "assertion": "artifact['kind'] == 'plan-preview'"
       },
       {
        "line": 229,
        "assertion": "artifact['repository'] == 'o/r'"
       },
       {
        "line": 230,
        "assertion": "artifact['plan_id'].startswith('plan-')"
       },
       {
        "line": 231,
        "assertion": "artifact['generated_at'] == '2026-01-02T00:00:00Z'"
       },
       {
        "line": 232,
        "assertion": "artifact['fingerprint'].startswith('sha256:')"
       },
       {
        "line": 233,
        "assertion": "artifact['registration_order'] == ['a', 'b', 'c']"
       },
       {
        "line": 235,
        "assertion": "denormalize_plan(artifact['normalized_plan']).plan_id == artifact['plan_id']"
       },
       {
        "line": 240,
        "assertion": "confirmed['milestone'] is None"
       },
       {
        "line": 241,
        "assertion": "confirmed['dependency_issues']['12']['state'] == 'OPEN'"
       },
       {
        "line": 242,
        "assertion": "confirmed['labels'] == {'type:implementation': True}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_discovered_policy_changes_preview",
      "start": 245,
      "end": 283,
      "assertions": [
       {
        "line": 272,
        "assertion": "adopted['warnings'] == []"
       },
       {
        "line": 273,
        "assertion": "'repository policy file' in capsys.readouterr().err"
       },
       {
        "line": 282,
        "assertion": "[w['code'] for w in defaulted['warnings']] == ['policy_excluded']"
       },
       {
        "line": 283,
        "assertion": "adopted['fingerprint'] != defaulted['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_rendered_bodies_keep_plan_local_ids",
      "start": 285,
      "end": 292,
      "assertions": [
       {
        "line": 289,
        "assertion": "'- Blocked by a' in bodies['b']"
       },
       {
        "line": 290,
        "assertion": "'- Blocked by a' in bodies['c']"
       },
       {
        "line": 291,
        "assertion": "'- Blocked by #12' in bodies['c']"
       },
       {
        "line": 292,
        "assertion": "'devflow:plan=' in bodies['a']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_milestone_description_rendered",
      "start": 294,
      "end": 299,
      "assertions": [
       {
        "line": 297,
        "assertion": "ms['title'] == 'v9'"
       },
       {
        "line": 298,
        "assertion": "'ship v9' in ms['description']"
       },
       {
        "line": 299,
        "assertion": "'item=milestone' in ms['description']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fingerprint_is_deterministic",
      "start": 301,
      "end": 306,
      "assertions": [
       {
        "line": 305,
        "assertion": "b['fingerprint'] == c['fingerprint']"
       },
       {
        "line": 306,
        "assertion": "a['fingerprint'] != b['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_generated_at_excluded_from_fingerprint",
      "start": 308,
      "end": 313,
      "assertions": [
       {
        "line": 313,
        "assertion": "a['fingerprint'] == b['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_repository_mismatch_fails",
      "start": 315,
      "end": 319,
      "assertions": [
       {
        "line": 319,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 317,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_existing_milestone_confirmed_by_number",
      "start": 321,
      "end": 331,
      "assertions": [
       {
        "line": 328,
        "assertion": "artifact['confirmed']['milestone']['number'] == 7"
       },
       {
        "line": 329,
        "assertion": "artifact['confirmed']['milestone']['title'] == 'v9'"
       },
       {
        "line": 331,
        "assertion": "'milestone' not in artifact['render']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_milestone_fails",
      "start": 333,
      "end": 341,
      "assertions": [
       {
        "line": 341,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 339,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_new_milestone_title_collision_warns_but_stays_new",
      "start": 343,
      "end": 348,
      "assertions": [
       {
        "line": 347,
        "assertion": "'milestone_title_collision' in codes"
       },
       {
        "line": 348,
        "assertion": "artifact['render']['milestone']['title'] == 'v9'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_dependency_pull_request_rejected",
      "start": 350,
      "end": 354,
      "assertions": [
       {
        "line": 354,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 352,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_missing_dependency_fails",
      "start": 356,
      "end": 360,
      "assertions": [
       {
        "line": 360,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 358,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_closed_dependency_allowed",
      "start": 362,
      "end": 365,
      "assertions": [
       {
        "line": 365,
        "assertion": "artifact['confirmed']['dependency_issues']['12']['state'] == 'CLOSED'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_label_fails",
      "start": 367,
      "end": 371,
      "assertions": [
       {
        "line": 371,
        "assertion": "e.value.exit_code == 4"
       }
      ],
      "raises": [
       {
        "line": 369,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_no_labels_allowed",
      "start": 373,
      "end": 377,
      "assertions": [
       {
        "line": 377,
        "assertion": "artifact['confirmed']['labels'] == {}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_policy_warning_for_excluded_issue",
      "start": 379,
      "end": 384,
      "assertions": [
       {
        "line": 384,
        "assertion": "'policy_excluded' in codes"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_invalid_plan_raises_diagnostics",
      "start": 386,
      "end": 393,
      "assertions": [
       {
        "line": 393,
        "assertion": "any((d.code == 'empty_field' for d in e.value.diagnostics))"
       }
      ],
      "raises": [
       {
        "line": 388,
        "context": "pytest.raises(PlanValidationError)"
       }
      ]
     },
     {
      "symbol": "test_preview_cannot_write",
      "start": 395,
      "end": 405,
      "assertions": [],
      "raises": []
     },
     {
      "symbol": "test_out_path_unencodable_leaves_no_stranded_file",
      "start": 407,
      "end": 415,
      "assertions": [
       {
        "line": 415,
        "assertion": "not out.exists()"
       }
      ],
      "raises": [
       {
        "line": 413,
        "context": "pytest.raises(TrackerError, match='UTF-8')"
       }
      ]
     },
     {
      "symbol": "test_out_path_outside_working_directory_refused",
      "start": 417,
      "end": 425,
      "assertions": [
       {
        "line": 425,
        "assertion": "not (tmp_path.parent / 'escape.json').exists()"
       }
      ],
      "raises": [
       {
        "line": 423,
        "context": "pytest.raises(TrackerError, match='working directory')"
       }
      ]
     },
     {
      "symbol": "test_validate_json_stdout",
      "start": 438,
      "end": 444,
      "assertions": [
       {
        "line": 441,
        "assertion": "rc == 0"
       },
       {
        "line": 443,
        "assertion": "payload['ok'] is True"
       },
       {
        "line": 444,
        "assertion": "err == ''"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_validate_invalid_exits_2",
      "start": 446,
      "end": 452,
      "assertions": [
       {
        "line": 449,
        "assertion": "rc == 2"
       },
       {
        "line": 451,
        "assertion": "payload['ok'] is False"
       },
       {
        "line": 452,
        "assertion": "'empty_field' in {d['code'] for d in payload['diagnostics']}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_preview_out_writes_artifact",
      "start": 454,
      "end": 474,
      "assertions": [
       {
        "line": 471,
        "assertion": "rc == 0"
       },
       {
        "line": 473,
        "assertion": "artifact['kind'] == 'plan-preview'"
       },
       {
        "line": 474,
        "assertion": "json.loads(out)['fingerprint'] == artifact['fingerprint']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_preview_invalid_plan_exits_2",
      "start": 476,
      "end": 480,
      "assertions": [
       {
        "line": 479,
        "assertion": "rc == 2"
       },
       {
        "line": 480,
        "assertion": "'empty_field' in err"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_plan_help",
      "start": 482,
      "end": 485,
      "assertions": [
       {
        "line": 484,
        "assertion": "rc == 0"
       },
       {
        "line": 485,
        "assertion": "'validate' in out and 'preview' in out and ('publish' in out)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_spec_warnings_surface_in_preview",
      "start": 505,
      "end": 508,
      "assertions": [
       {
        "line": 508,
        "assertion": "{'scope_untraced', 'ac_unanchored'} <= codes"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_warnings_are_fingerprint_covered",
      "start": 510,
      "end": 517,
      "assertions": [
       {
        "line": 514,
        "assertion": "artifact['fingerprint'] == fingerprint_payload(artifact)"
       },
       {
        "line": 517,
        "assertion": "fingerprint_payload(stripped) != artifact['fingerprint']"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00184",
   "role": "selector",
   "path": "tests/test_publish.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "fad693a442ccd4e1a96ab0ea1257185a1efddf43",
   "size": 29544,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_publish.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00185",
   "role": "selector",
   "path": "tests/test_refine.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "143c100cbb786d8f43b9b9a75337cd609fec7fc2",
   "size": 15713,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_refine.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00186",
   "role": "selector",
   "path": "tests/test_refine_apply.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "37b7d8761dfadec65d290c0ebe0bea2ef119f4df",
   "size": 17829,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_refine_apply.py"
    ],
    "regions": [
     {
      "symbol": "test_wrong_approval_is_refused_before_any_write",
      "start": 97,
      "end": 104,
      "assertions": [
       {
        "line": 103,
        "assertion": "e.value.exit_code == 2"
       },
       {
        "line": 104,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": [
       {
        "line": 101,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_a_mutated_artifact_is_refused_before_any_write",
      "start": 107,
      "end": 116,
      "assertions": [
       {
        "line": 115,
        "assertion": "e.value.exit_code == 6"
       },
       {
        "line": 116,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": [
       {
        "line": 113,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_a_policy_change_after_approval_is_refused",
      "start": 119,
      "end": 130,
      "assertions": [
       {
        "line": 129,
        "assertion": "e.value.exit_code == 2"
       },
       {
        "line": 130,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": [
       {
        "line": 125,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_a_repo_mismatch_is_refused",
      "start": 133,
      "end": 142,
      "assertions": [
       {
        "line": 141,
        "assertion": "e.value.exit_code == 2"
       },
       {
        "line": 142,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": [
       {
        "line": 137,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_apply_writes_the_body_and_only_the_resolved_labels",
      "start": 145,
      "end": 160,
      "assertions": [
       {
        "line": 152,
        "assertion": "code == 0"
       },
       {
        "line": 153,
        "assertion": "result['complete'] is True"
       },
       {
        "line": 154,
        "assertion": "[i['status'] for i in result['items']] == ['updated']"
       },
       {
        "line": 156,
        "assertion": "kinds == ['body', 'labels']"
       },
       {
        "line": 157,
        "assertion": "tracker.writes[1][2] == ('spec:ambiguous',)"
       },
       {
        "line": 158,
        "assertion": "'devflow:refine=' in tracker.issues[12].body"
       },
       {
        "line": 159,
        "assertion": "tracker.issues[12].labels == ('type:implementation',)"
       },
       {
        "line": 160,
        "assertion": "result['items'][0]['changes'][0]['field'] == 'body'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_human_required_and_unresolved_candidates_are_never_written",
      "start": 163,
      "end": 179,
      "assertions": [
       {
        "line": 172,
        "assertion": "code == 0"
       },
       {
        "line": 173,
        "assertion": "[i['number'] for i in result['items']] == [12]"
       },
       {
        "line": 174,
        "assertion": "[n['number'] for n in result['not_applied']] == [14, 15]"
       },
       {
        "line": 175,
        "assertion": "[n['status'] for n in result['not_applied']] == ['human-required', 'unresolved']"
       },
       {
        "line": 179,
        "assertion": "all((w[1] == 12 for w in tracker.writes))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_second_apply_is_a_noop",
      "start": 182,
      "end": 193,
      "assertions": [
       {
        "line": 191,
        "assertion": "code == 0"
       },
       {
        "line": 192,
        "assertion": "[i['status'] for i in result['items']] == ['noop']"
       },
       {
        "line": 193,
        "assertion": "len(tracker.writes) == writes_after_first"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_stale_issue_is_skipped_without_a_write",
      "start": 196,
      "end": 210,
      "assertions": [
       {
        "line": 206,
        "assertion": "code == 0"
       },
       {
        "line": 207,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 208,
        "assertion": "tracker.writes == []"
       },
       {
        "line": 209,
        "assertion": "result['warnings'][0]['code'] == 'stale_issue'"
       },
       {
        "line": 210,
        "assertion": "result['next'] is not None"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_new_answer_comment_is_stale_not_written",
      "start": 213,
      "end": 233,
      "assertions": [
       {
        "line": 231,
        "assertion": "code == 0"
       },
       {
        "line": 232,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 233,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_4xx_failure_is_reported_and_the_run_continues",
      "start": 236,
      "end": 256,
      "assertions": [
       {
        "line": 252,
        "assertion": "code == 5"
       },
       {
        "line": 253,
        "assertion": "[i['status'] for i in result['items']] == ['failed', 'updated']"
       },
       {
        "line": 254,
        "assertion": "result['complete'] is False"
       },
       {
        "line": 255,
        "assertion": "result['error']['exit_code'] == 5"
       },
       {
        "line": 256,
        "assertion": "[w[1] for w in tracker.writes] == [12, 13, 13]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_unknown_write_outcome_stops_before_later_items",
      "start": 259,
      "end": 285,
      "assertions": [
       {
        "line": 275,
        "assertion": "code == 5"
       },
       {
        "line": 276,
        "assertion": "[i['number'] for i in result['items']] == [12]"
       },
       {
        "line": 277,
        "assertion": "result['items'][0]['status'] == 'unknown'"
       },
       {
        "line": 278,
        "assertion": "result['complete'] is False"
       },
       {
        "line": 282,
        "assertion": "[w[1] for w in tracker.writes] == [12]"
       },
       {
        "line": 284,
        "assertion": "unseen[13]['status'] is None"
       },
       {
        "line": 285,
        "assertion": "unseen[13]['reasons'][0]['type'] == 'not_attempted'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_failing_read_is_not_a_write",
      "start": 288,
      "end": 299,
      "assertions": [
       {
        "line": 297,
        "assertion": "code == 4"
       },
       {
        "line": 298,
        "assertion": "result['items'][0]['status'] == 'failed'"
       },
       {
        "line": 299,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_github_write_keeps_the_payload_out_of_argv",
      "start": 302,
      "end": 317,
      "assertions": [
       {
        "line": 314,
        "assertion": "'sentinel-token-value' not in ' '.join(seen['argv'])"
       },
       {
        "line": 315,
        "assertion": "seen['argv'][:5] == ['gh', 'api', '-X', 'PATCH', 'repos/o/r/issues/1']"
       },
       {
        "line": 316,
        "assertion": "not Path(seen['argv'][seen['argv'].index('--input') + 1]).exists()"
       },
       {
        "line": 317,
        "assertion": "_redact('bad sentinel-token-value') == 'bad ***'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_partial_apply_resumes_without_going_stale",
      "start": 320,
      "end": 345,
      "assertions": [
       {
        "line": 333,
        "assertion": "first_code == 5"
       },
       {
        "line": 334,
        "assertion": "first['items'][0]['status'] == 'failed'"
       },
       {
        "line": 335,
        "assertion": "[c['field'] for c in first['items'][0]['changes']] == ['body']"
       },
       {
        "line": 336,
        "assertion": "'spec:ambiguous' in tracker.issues[12].labels"
       },
       {
        "line": 337,
        "assertion": "'devflow:refine=' in tracker.issues[12].body"
       },
       {
        "line": 342,
        "assertion": "second_code == 0"
       },
       {
        "line": 343,
        "assertion": "second['items'][0]['status'] == 'updated'"
       },
       {
        "line": 344,
        "assertion": "[c['field'] for c in second['items'][0]['changes']] == ['labels']"
       },
       {
        "line": 345,
        "assertion": "tracker.issues[12].labels == ('type:implementation',)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_different_fresh_revision_is_never_written",
      "start": 348,
      "end": 375,
      "assertions": [
       {
        "line": 369,
        "assertion": "code == 0"
       },
       {
        "line": 370,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 371,
        "assertion": "'differs from the approved revision' in result['items'][0]['reasons'][0]['message']"
       },
       {
        "line": 375,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_third_party_label_change_is_stale",
      "start": 378,
      "end": 391,
      "assertions": [
       {
        "line": 389,
        "assertion": "code == 0"
       },
       {
        "line": 390,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 391,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_redacted_issue_is_never_a_write_candidate",
      "start": 394,
      "end": 406,
      "assertions": [
       {
        "line": 402,
        "assertion": "code == 0"
       },
       {
        "line": 403,
        "assertion": "result['items'] == []"
       },
       {
        "line": 404,
        "assertion": "[n['number'] for n in result['not_applied']] == [12]"
       },
       {
        "line": 405,
        "assertion": "result['not_applied'][0]['status'] == 'unresolved'"
       },
       {
        "line": 406,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_closed_issue_is_stale_not_written",
      "start": 409,
      "end": 419,
      "assertions": [
       {
        "line": 417,
        "assertion": "code == 0"
       },
       {
        "line": 418,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 419,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_updated_at_only_change_is_stale_when_the_body_is_not_done",
      "start": 422,
      "end": 436,
      "assertions": [
       {
        "line": 434,
        "assertion": "code == 0"
       },
       {
        "line": 435,
        "assertion": "result['items'][0]['status'] == 'stale'"
       },
       {
        "line": 436,
        "assertion": "tracker.writes == []"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_write_setup_failure_is_reported_as_failed",
      "start": 439,
      "end": 461,
      "assertions": [
       {
        "line": 459,
        "assertion": "code == 5"
       },
       {
        "line": 460,
        "assertion": "[i['status'] for i in result['items']] == ['failed', 'updated']"
       },
       {
        "line": 461,
        "assertion": "result['complete'] is False"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_a_closed_issue_after_refinement_is_not_a_noop",
      "start": 464,
      "end": 475,
      "assertions": [
       {
        "line": 469,
        "assertion": "tracker.issues[12].labels == ('type:implementation',)"
       },
       {
        "line": 474,
        "assertion": "code == 0"
       },
       {
        "line": 475,
        "assertion": "result['items'][0]['status'] == 'stale'"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00187",
   "role": "selector",
   "path": "tests/test_refine_contract.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "dc2961efd3710080973bf53bd5ea69e236436708",
   "size": 5214,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_refine_contract.py"
    ],
    "regions": [
     {
      "symbol": "test_github_write_importers_are_the_explicit_write_paths",
      "start": 42,
      "end": 47,
      "assertions": [
       {
        "line": 47,
        "assertion": "importers == WRITE_CALLERS"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_no_selection_or_query_module_reaches_the_write_adapter",
      "start": 50,
      "end": 58,
      "assertions": [
       {
        "line": 57,
        "assertion": "'github_write' not in text"
       },
       {
        "line": 58,
        "assertion": "'GhWriteError' not in text"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_both_refine_schemas_are_valid_and_versioned",
      "start": 61,
      "end": 74,
      "assertions": [
       {
        "line": 72,
        "assertion": "schema['properties']['schema_version']['const'] == version"
       },
       {
        "line": 74,
        "assertion": "schema['properties']['kind']['const'] == kind"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_schema_enums_match_the_core_vocabulary",
      "start": 77,
      "end": 89,
      "assertions": [
       {
        "line": 83,
        "assertion": "set(defs['status']['enum']) == set(STATUSES)"
       },
       {
        "line": 84,
        "assertion": "set(defs['finding_status']['enum']) == set(FINDING_STATUSES)"
       },
       {
        "line": 85,
        "assertion": "set(defs['reason']['properties']['type']['enum']) == set(REASON_TYPES)"
       },
       {
        "line": 87,
        "assertion": "set(item_status) == set(ITEM_STATUSES)"
       },
       {
        "line": 89,
        "assertion": "set(apply_reasons) == set(REASON_TYPES)"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_every_refine_fixture_validates_against_the_preview_schema",
      "start": 92,
      "end": 110,
      "assertions": [
       {
        "line": 106,
        "assertion": "dirs"
       },
       {
        "line": 110,
        "assertion": "not errors"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_review_command_leaks_no_secret",
      "start": 113,
      "end": 132,
      "assertions": [
       {
        "line": 129,
        "assertion": "rc == 0"
       },
       {
        "line": 130,
        "assertion": "sentinel not in out.getvalue()"
       },
       {
        "line": 131,
        "assertion": "sentinel not in err.getvalue()"
       },
       {
        "line": 132,
        "assertion": "os.environ['GH_TOKEN'] == sentinel"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_review_command_is_absent_from_mcp",
      "start": 135,
      "end": 139,
      "assertions": [
       {
        "line": 139,
        "assertion": "'refine' not in text"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00188",
   "role": "selector",
   "path": "tests/test_refine_snapshot.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "bb633c9a66991f75a083d387599c24b5d8886817",
   "size": 7586,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_refine_snapshot.py"
    ],
    "regions": [
     {
      "symbol": "test_fetch_refine_snapshot_records_issues_and_comments",
      "start": 76,
      "end": 87,
      "assertions": [
       {
        "line": 79,
        "assertion": "snap.repository == 'o/r'"
       },
       {
        "line": 80,
        "assertion": "len(snap.issues) == 1"
       },
       {
        "line": 82,
        "assertion": "issue.number == 12"
       },
       {
        "line": 83,
        "assertion": "issue.labels == ('spec:ambiguous',)"
       },
       {
        "line": 84,
        "assertion": "issue.updated_at == '2026-01-01T00:00:00Z'"
       },
       {
        "line": 85,
        "assertion": "[c.id for c in issue.comments] == ['IC_12']"
       },
       {
        "line": 86,
        "assertion": "issue.comments[0].author == 'shimpeiws'"
       },
       {
        "line": 87,
        "assertion": "issue.redacted is False"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fetch_refine_issue_reads_one_issue",
      "start": 90,
      "end": 94,
      "assertions": [
       {
        "line": 93,
        "assertion": "issue.number == 12"
       },
       {
        "line": 94,
        "assertion": "[c.id for c in issue.comments] == ['IC_12']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_truncated_comment_page_fails_loud",
      "start": 97,
      "end": 106,
      "assertions": [
       {
        "line": 106,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 98,
        "context": "pytest.raises(TrackerError, match='comments truncated')"
       }
      ]
     },
     {
      "symbol": "test_missing_comment_connection_fails_loud",
      "start": 109,
      "end": 116,
      "assertions": [
       {
        "line": 116,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 111,
        "context": "pytest.raises(TrackerError, match='no comments')"
       }
      ]
     },
     {
      "symbol": "test_missing_issue_node_fails_loud",
      "start": 119,
      "end": 126,
      "assertions": [
       {
        "line": 126,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 121,
        "context": "pytest.raises(TrackerError, match='no issue node')"
       }
      ]
     },
     {
      "symbol": "test_graphql_errors_fail_loud",
      "start": 129,
      "end": 136,
      "assertions": [
       {
        "line": 136,
        "assertion": "exc_info.value.exit_code == 5"
       }
      ],
      "raises": [
       {
        "line": 131,
        "context": "pytest.raises(TrackerError)"
       }
      ]
     },
     {
      "symbol": "test_issue_ceiling_fails_loud",
      "start": 139,
      "end": 146,
      "assertions": [
       {
        "line": 146,
        "assertion": "exc_info.value.exit_code == 6"
       }
      ],
      "raises": [
       {
        "line": 141,
        "context": "pytest.raises(TrackerError, match='ceiling')"
       }
      ]
     },
     {
      "symbol": "test_no_issues_makes_no_graphql_call",
      "start": 149,
      "end": 172,
      "assertions": [
       {
        "line": 158,
        "assertion": "snap.issues == ()"
       },
       {
        "line": 159,
        "assertion": "calls == [['issue', 'list', '--repo', 'o/r', '--state', 'open', '--limit', str(MAX_ISSUES + 1), '--json', 'number,title,state,labels,url,updatedAt,body']]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_label_names_and_urls_are_redacted",
      "start": 175,
      "end": 188,
      "assertions": [
       {
        "line": 186,
        "assertion": "'sentinel-secret-value' not in ' '.join(snap.issues[0].labels)"
       },
       {
        "line": 187,
        "assertion": "'sentinel-secret-value' not in snap.issues[0].url"
       },
       {
        "line": 188,
        "assertion": "'bug' in snap.issues[0].labels"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_comment_queries_are_batched",
      "start": 191,
      "end": 218,
      "assertions": [
       {
        "line": 215,
        "assertion": "len(snap.issues) == total"
       },
       {
        "line": 216,
        "assertion": "len(queries) == 3"
       },
       {
        "line": 218,
        "assertion": "len(re.findall('i\\\\d+: issue\\\\(', query)) <= MAX_COMMENT_NODES"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_comment_and_body_text_is_redacted",
      "start": 221,
      "end": 242,
      "assertions": [
       {
        "line": 239,
        "assertion": "'sentinel-secret-value' not in snap.issues[0].body"
       },
       {
        "line": 240,
        "assertion": "'sentinel-secret-value' not in snap.issues[0].comments[0].body"
       },
       {
        "line": 241,
        "assertion": "snap.issues[0].redacted is True"
       },
       {
        "line": 242,
        "assertion": "os.environ['GH_TOKEN'] == 'sentinel-secret-value'"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00189",
   "role": "selector",
   "path": "tests/test_scaffold.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "79886355b8c7ab6e75c8da7b3b03e04651eff0c0",
   "size": 2175,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_scaffold.py"
    ],
    "regions": [
     {
      "symbol": "test_no_runtime_dependencies",
      "start": 10,
      "end": 12,
      "assertions": [
       {
        "line": 12,
        "assertion": "'dependencies = []' in pyproject"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_package_layout_matches_architecture",
      "start": 15,
      "end": 19,
      "assertions": [
       {
        "line": 16,
        "assertion": "(ROOT / 'src/devflow/core').is_dir()"
       },
       {
        "line": 17,
        "assertion": "(ROOT / 'src/devflow/cli').is_dir()"
       },
       {
        "line": 18,
        "assertion": "(ROOT / 'src/devflow/tracker').is_dir()"
       },
       {
        "line": 19,
        "assertion": "(ROOT / 'src/devflow/mcp_server.py').is_file()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_console_scripts_registered",
      "start": 22,
      "end": 25,
      "assertions": [
       {
        "line": 24,
        "assertion": "'devflow = \"devflow.cli.main:main\"' in pyproject"
       },
       {
        "line": 25,
        "assertion": "'devflow-mcp = \"devflow.mcp_server:main\"' in pyproject"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_version_exposed",
      "start": 28,
      "end": 29,
      "assertions": [
       {
        "line": 29,
        "assertion": "devflow.__version__ == '0.1.0'"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_cli_help_exits_zero",
      "start": 32,
      "end": 40,
      "assertions": [
       {
        "line": 39,
        "assertion": "result.returncode == 0"
       },
       {
        "line": 40,
        "assertion": "'ready-issues' in result.stdout"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_version_prints_version",
      "start": 43,
      "end": 51,
      "assertions": [
       {
        "line": 50,
        "assertion": "result.returncode == 0"
       },
       {
        "line": 51,
        "assertion": "'devflow 0.1.0' in result.stdout"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_subcommands_registered",
      "start": 54,
      "end": 71,
      "assertions": [
       {
        "line": 61,
        "assertion": "result.returncode == 0"
       },
       {
        "line": 63,
        "assertion": "cmd in result.stdout"
       },
       {
        "line": 71,
        "assertion": "sub in issue_help.stdout"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_ci_workflow_yaml_valid",
      "start": 74,
      "end": 80,
      "assertions": [
       {
        "line": 79,
        "assertion": "'jobs' in parsed"
       },
       {
        "line": 80,
        "assertion": "'verify' in parsed['jobs']"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00190",
   "role": "selector",
   "path": "tests/test_scoring.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d10556dc4e91d40e7f8f418d1a9ae912fe4bbc8c",
   "size": 4723,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_scoring.py"
    ],
    "regions": [
     {
      "symbol": "test_no_external_deps_first",
      "start": 44,
      "end": 52,
      "assertions": [
       {
        "line": 52,
        "assertion": "rank(issues) == [(1, 2), (9,), (3,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_more_members_first",
      "start": 54,
      "end": 57,
      "assertions": [
       {
        "line": 57,
        "assertion": "rank(issues) == [(1, 2, 3), (4,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_priority_label_first",
      "start": 59,
      "end": 64,
      "assertions": [
       {
        "line": 64,
        "assertion": "rank(issues) == [(1,), (2,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_lower_milestone_first",
      "start": 66,
      "end": 71,
      "assertions": [
       {
        "line": 71,
        "assertion": "rank(issues, milestones={'v1': 1, 'v2': 2}) == [(2,), (1,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_simpler_chain_first",
      "start": 73,
      "end": 85,
      "assertions": [
       {
        "line": 85,
        "assertion": "rank(issues) == [(4, 5, 6), (1, 2, 3)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_older_update_first",
      "start": 87,
      "end": 92,
      "assertions": [
       {
        "line": 92,
        "assertion": "rank(issues) == [(1,), (2,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_missing_updated_at_never_promoted",
      "start": 94,
      "end": 100,
      "assertions": [
       {
        "line": 100,
        "assertion": "rank(issues) == [(2,), (1,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_mixed_group_uses_oldest_member",
      "start": 102,
      "end": 109,
      "assertions": [
       {
        "line": 109,
        "assertion": "rank(issues) == [(1, 2), (3,)]"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_cyclic_group_ranks_last",
      "start": 111,
      "end": 124,
      "assertions": [
       {
        "line": 122,
        "assertion": "ranked[-1].group.members == (1, 2)"
       },
       {
        "line": 123,
        "assertion": "ranked[-1].graph.cyclic == frozenset({1, 2})"
       },
       {
        "line": 124,
        "assertion": "ranked[-1].graph.order == ()"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_deterministic",
      "start": 126,
      "end": 132,
      "assertions": [
       {
        "line": 132,
        "assertion": "rank(issues) == rank(list(reversed(issues)))"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00191",
   "role": "selector",
   "path": "tests/test_select.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b1292dcf471d3835a2ddf5adcf5dd3d47aab4894",
   "size": 33318,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_select.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00192",
   "role": "selector",
   "path": "tests/test_tracker.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "b4e57e518a7fbc6026fd27b29de110afafd5354f",
   "size": 10744,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_tracker.py"
    ],
    "regions": []
   }
  },
  {
   "id": "selector-F00193",
   "role": "selector",
   "path": "tests/test_triage.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "655ede534be557530a48ad30432b740c66858720",
   "size": 5041,
   "review_evidence": {
    "inspected_evidence": [
     "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_triage.py"
    ],
    "regions": [
     {
      "symbol": "test_pr_fixture_replays_to_expected",
      "start": 69,
      "end": 73,
      "assertions": [
       {
        "line": 72,
        "assertion": "payload == expected"
       },
       {
        "line": 73,
        "assertion": "payload['fingerprint'].startswith('sha256:')"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_pr_fixture_payload_invariants",
      "start": 77,
      "end": 92,
      "assertions": [
       {
        "line": 86,
        "assertion": "payload['execution_order'] == [e['number'] for e in work]"
       },
       {
        "line": 88,
        "assertion": "sorted(numbers) == sorted((p['number'] for p in snapshot['pull_requests']))"
       },
       {
        "line": 85,
        "assertion": "e['status'] in _WORK_STATUSES"
       },
       {
        "line": 90,
        "assertion": "e['status']"
       },
       {
        "line": 91,
        "assertion": "e['action']"
       },
       {
        "line": 92,
        "assertion": "e['reasons']"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_empty_check_rollup_is_unresolved_in_every_fixture",
      "start": 95,
      "end": 114,
      "assertions": [
       {
        "line": 114,
        "assertion": "exercised > 0"
       },
       {
        "line": 112,
        "assertion": "entry['status'] == 'unresolved'"
       },
       {
        "line": 113,
        "assertion": "any((r['type'] == 'empty_check_rollup' for r in entry['reasons']))"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_every_status_tier_is_exercised",
      "start": 117,
      "end": 136,
      "assertions": [
       {
        "line": 125,
        "assertion": "statuses == {'unresolved', 'conflicting', 'ci_failing', 'changes_requested', 'review_unresolved', 'ci_pending', 'awaiting_review', 'draft', 'ready_to_merge', 'excluded'}"
       }
      ],
      "raises": []
     },
     {
      "symbol": "test_fixture_scenarios_cover_expected_names",
      "start": 139,
      "end": 149,
      "assertions": [
       {
        "line": 140,
        "assertion": "{d.name for d in _fixture_dirs()} == {'green', 'ci-failing', 'conflicting', 'review-unresolved', 'unknown-signal', 'marker-blocking', 'draft', 'pending'}"
       }
      ],
      "raises": []
     }
    ]
   }
  },
  {
   "id": "selector-F00194",
   "role": "selector",
   "path": "uv.lock",
   "mode": "100644",
   "kind": "blob",
   "blob": "aacdf8dc2d5b407c32535c6adff8c78779c3cb99",
   "size": 163997
  },
  {
   "id": "infra-F00001",
   "role": "infra",
   "path": ".gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "b0f356ee69c620ff79163bebc3527b4d88421520",
   "size": 304
  },
  {
   "id": "infra-F00002",
   "role": "infra",
   "path": "AGENTS.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e4484a2a65dd196546c29899c0c7ca07256cd13c",
   "size": 2548
  },
  {
   "id": "infra-F00003",
   "role": "infra",
   "path": "Cargo.lock",
   "mode": "100644",
   "kind": "blob",
   "blob": "a013f6a2b5c6fe32227e38fa55980d38bb48b2b3",
   "size": 8703
  },
  {
   "id": "infra-F00004",
   "role": "infra",
   "path": "Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "1618c3f50c49b44efc84266c102e7b553f5843f9",
   "size": 329
  },
  {
   "id": "infra-F00005",
   "role": "infra",
   "path": "README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "416c0ed978ded6aa2f04d35e1fd41aed769b1450",
   "size": 5677,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:README.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00006",
   "role": "infra",
   "path": "docs/decision-register.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5b183dc34f21ffb0aa085f195e4b97b0c3526841",
   "size": 7233,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/decision-register.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00007",
   "role": "infra",
   "path": "docs/execution/lifecycle.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "0306060e4433ab4ee8c5422dc27a87e13bcc4fb8",
   "size": 20142,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/execution/lifecycle.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00008",
   "role": "infra",
   "path": "docs/host/bootstrap.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "9ee425c2dbf550d0e879d09f354bdab8119b637c",
   "size": 13714,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/host/bootstrap.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00009",
   "role": "infra",
   "path": "docs/host/helper-update.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "58907919f5d27ef679f8b6b185c098410f98b4a1",
   "size": 7712,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/host/helper-update.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00010",
   "role": "infra",
   "path": "docs/host/inventory.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f19b961e980b2b2d3519a93609e3cb6b9411ab20",
   "size": 3961
  },
  {
   "id": "infra-F00011",
   "role": "infra",
   "path": "docs/host/m1-observed.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "85ac2d84a28c353172b70264e3cf5ddf0f7819aa",
   "size": 5925
  },
  {
   "id": "infra-F00012",
   "role": "infra",
   "path": "docs/host/restricted-ssh.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e27438eef3a5876acacb9398b7c8b3f8ba37eb59",
   "size": 50575
  },
  {
   "id": "infra-F00013",
   "role": "infra",
   "path": "docs/host/ssh-transport.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a542bc22391e8b3d79cc3101d8b52c0bc9f38b7",
   "size": 4216
  },
  {
   "id": "infra-F00014",
   "role": "infra",
   "path": "docs/incus/worker-runtime.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "30aead2341d409c352656ba4b268e84e7ed0c1fd",
   "size": 19431
  },
  {
   "id": "infra-F00015",
   "role": "infra",
   "path": "docs/integration/coding-worker-dsh-interface.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5267edb81f9dade1e24d6a3c64f4526a37277bde",
   "size": 24882,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/integration/coding-worker-dsh-interface.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00016",
   "role": "infra",
   "path": "docs/integration/macbook-contract.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "8c7cfb9b380d29aa883c52a753271f835e5c8c91",
   "size": 12256,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/integration/macbook-contract.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00017",
   "role": "infra",
   "path": "docs/operations/m2-m3-runbook.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4f7a3c0b4378dd08b897264338a07dc1c099512",
   "size": 19224,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/operations/m2-m3-runbook.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00018",
   "role": "infra",
   "path": "docs/operations/m4-observed.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "ef04de050efcb5badeecefa3da987b8e6d3c3660",
   "size": 14174,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/operations/m4-observed.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00019",
   "role": "infra",
   "path": "docs/operations/runtime-credential-setup.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "aa69e23ff319aae120cc7aa887c57a9ad7927e0c",
   "size": 20329,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/operations/runtime-credential-setup.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00020",
   "role": "infra",
   "path": "docs/proposal.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "52007cc13ba187c96c3cc54c3b2214f91a7f6ee5",
   "size": 12165,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:docs/proposal.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00021",
   "role": "infra",
   "path": "docs/repository-boundaries.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "a0f971b0b10de932947544a55f8c29d2af1133e9",
   "size": 3380,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/repository-boundaries.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00022",
   "role": "infra",
   "path": "docs/rust-cli.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4672cc3c16aac789339334632c674de8ccf4a30",
   "size": 58544,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:docs/rust-cli.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00023",
   "role": "infra",
   "path": "docs/testing.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7c350fe19b974be956702c6170486264fc545db3",
   "size": 15248,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/testing.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00024",
   "role": "infra",
   "path": "docs/v0.1-plan.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "0b1257b6b3ee31a5149f40b70b01e4a97e3de802",
   "size": 3416,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:docs/v0.1-plan.md"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00025",
   "role": "infra",
   "path": "evidence/image-validation/cw-worker-unvalidated-20261029T080532Z-40463.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "f795ee8cee50eb8c5ee070fff3c4e37327198234",
   "size": 4140,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:evidence/image-validation/cw-worker-unvalidated-20261029T080532Z-40463.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00026",
   "role": "infra",
   "path": "evidence/image-validation/cw-worker-unvalidated-20261030T140010Z-407926.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "d180d7fc48403e0f44f30cbeb4b7b1f0a799bff0",
   "size": 4386,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:evidence/image-validation/cw-worker-unvalidated-20261030T140010Z-407926.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00027",
   "role": "infra",
   "path": "evidence/image-validation/cw-worker-unvalidated-20261030T200116Z-540633.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "37ce72e43508991155f03b5be7b6ca252a1e6aa7",
   "size": 4386,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:evidence/image-validation/cw-worker-unvalidated-20261030T200116Z-540633.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00028",
   "role": "infra",
   "path": "evidence/image-validation/cw-worker-unvalidated-20261106T233349Z-1950282.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "b3c7a42d42a87f033712ef08913f4ebd249ea974",
   "size": 4390,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:evidence/image-validation/cw-worker-unvalidated-20261106T233349Z-1950282.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00029",
   "role": "infra",
   "path": "m4-trial-checkpoint.txt",
   "mode": "100644",
   "kind": "blob",
   "blob": "9403e4b26804025befa6f74bed4fd9d9f3c45502",
   "size": 123,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:m4-trial-checkpoint.txt"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00030",
   "role": "infra",
   "path": "profiles/verify/coding-worker-smoke.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "cc4d0dce8635afcef700c3d763da70ebfe5be05b",
   "size": 1161,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:profiles/verify/coding-worker-smoke.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00031",
   "role": "infra",
   "path": "scripts/host/bootstrap.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "f4a8a8fb6e0a271feda983800c9e21df5fbd0d88",
   "size": 26011
  },
  {
   "id": "infra-F00032",
   "role": "infra",
   "path": "scripts/host/cw-host-helper",
   "mode": "100644",
   "kind": "blob",
   "blob": "d7093d84c4a28fb6dcdcc258367bf28e0b153b35",
   "size": 153
  },
  {
   "id": "infra-F00033",
   "role": "infra",
   "path": "scripts/host/helper-update.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "9949c10fa38c7e4474decb5c2a7ed8131a457a24",
   "size": 14243,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/host/helper-update.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00034",
   "role": "infra",
   "path": "scripts/host/helper.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "9aa9d7ea2390e6b0f61020cbd6440512309d6271",
   "size": 156514,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/host/helper.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00035",
   "role": "infra",
   "path": "scripts/host/host-lib.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "e2103ce3dba80cc078ecd2598719e87d534f8ecd",
   "size": 5518
  },
  {
   "id": "infra-F00036",
   "role": "infra",
   "path": "scripts/host/incus-preseed.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "3ed29cca33d4c4eace53995f1feadb23152159a2",
   "size": 1126
  },
  {
   "id": "infra-F00037",
   "role": "infra",
   "path": "scripts/host/publication-validation.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "7a7460bc98fbe565d957cc21b17e455594ad3b71",
   "size": 8461,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/host/publication-validation.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00038",
   "role": "infra",
   "path": "scripts/host/run-evidence.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "308af03facd0e8a5881c67e56a83a4c70b5bb143",
   "size": 13053,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/host/run-evidence.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00039",
   "role": "infra",
   "path": "scripts/host/ssh-transport.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "d77fa79e16bfc768f92e9485c998828c9d88ef8a",
   "size": 48171,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/host/ssh-transport.py"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00040",
   "role": "infra",
   "path": "scripts/host/ssh-transport.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "aeb21fcf16ea2a1b9ac0ce73e05045f5f600a6a1",
   "size": 1057,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/host/ssh-transport.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00041",
   "role": "infra",
   "path": "scripts/host/versions.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "77abe38651737dbb8f0a678523311c5bbe2ddece",
   "size": 2522
  },
  {
   "id": "infra-F00042",
   "role": "infra",
   "path": "scripts/host/zabbly-incus-stable.sources",
   "mode": "100644",
   "kind": "blob",
   "blob": "f3b7dcf6d70898cfbd9114d1f620a74f40734b6c",
   "size": 538
  },
  {
   "id": "infra-F00043",
   "role": "infra",
   "path": "scripts/incus/build-validate-worker-image.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "5640feb94017f3b3aca468ae84e5827d13399de9",
   "size": 33501,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/incus/build-validate-worker-image.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00044",
   "role": "infra",
   "path": "scripts/incus/build-worker-image.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "18cece74e4ba8f93e63703e1ba9a3979021df027",
   "size": 13152
  },
  {
   "id": "infra-F00045",
   "role": "infra",
   "path": "scripts/incus/component-manifest.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "78f826e014be28ec2ab44e1ea0e0242dfa54cb52",
   "size": 6677
  },
  {
   "id": "infra-F00046",
   "role": "infra",
   "path": "scripts/incus/package-coding-worker.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "c292168d3f61fe8d00ac67027f8152ce543aed50",
   "size": 1116
  },
  {
   "id": "infra-F00047",
   "role": "infra",
   "path": "scripts/incus/smoke-worker.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "d149c2649226f899be89a2bb1130a74c652f9293",
   "size": 4558,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/incus/smoke-worker.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00048",
   "role": "infra",
   "path": "scripts/incus/validate-profile.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "a4168721c32be9cadb61a02ec2376d49b91ad4ca",
   "size": 4815
  },
  {
   "id": "infra-F00049",
   "role": "infra",
   "path": "scripts/incus/worker-profile.yaml",
   "mode": "100644",
   "kind": "blob",
   "blob": "09ee5690e7aed748a12c8bcbe3eea308df520354",
   "size": 2208
  },
  {
   "id": "infra-F00050",
   "role": "infra",
   "path": "scripts/worker/entrypoint.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "985f99307591607e66612401b656e17f0c5b93e1",
   "size": 19738,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/worker/entrypoint.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00051",
   "role": "infra",
   "path": "scripts/worker/gh",
   "mode": "100755",
   "kind": "blob",
   "blob": "972e8af8c23d025b8a9864e53654dafbdda0c7db",
   "size": 282,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/gh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00052",
   "role": "infra",
   "path": "scripts/worker/lib.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "810a8d7f63b01b574f4a05cb86a11c050bdfcf30",
   "size": 1804,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/lib.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00053",
   "role": "infra",
   "path": "scripts/worker/lifecycle.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "884dc8b67ac5ee7b06a3be352c2c8f4480143a7b",
   "size": 18083,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/worker/lifecycle.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00054",
   "role": "infra",
   "path": "scripts/worker/propose-classify.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "5172719ae60246148900b20d4b51e5ad4694a38b",
   "size": 13393,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/propose-classify.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00055",
   "role": "infra",
   "path": "scripts/worker/propose.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "273c653bf72a6a6353772cc742e196b7124e1b76",
   "size": 49968,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/propose.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00056",
   "role": "infra",
   "path": "scripts/worker/start-detached.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "5f578986b5c3d03c25357287d4e4b57286e784b3",
   "size": 1331,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/start-detached.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00057",
   "role": "infra",
   "path": "scripts/worker/supervisor.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "9453085a282ca7204b3831b0d712581a31fe74f3",
   "size": 2475,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:scripts/worker/supervisor.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00058",
   "role": "infra",
   "path": "scripts/worker/verify-pr.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "01e6db636ffdedcba125729cbd7875359c322f07",
   "size": 440,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:scripts/worker/verify-pr.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00059",
   "role": "infra",
   "path": "src/fake.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "b210a292e980cc633f3b5c8b80ac13d2c171eb8f",
   "size": 176245,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:src/fake.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00060",
   "role": "infra",
   "path": "src/ids.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "e2cedc1899b93c96ce387d4243d89ab33d19219e",
   "size": 5314,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:src/ids.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00061",
   "role": "infra",
   "path": "src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "6f11d02e00e764c02272b69064586e53d1765e0a",
   "size": 770
  },
  {
   "id": "infra-F00062",
   "role": "infra",
   "path": "src/lifecycle.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "2a886664a8a9d27fe30123461c5e7114f8a2dd63",
   "size": 127628,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:src/lifecycle.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00063",
   "role": "infra",
   "path": "src/main.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "61726451bd071ad66c533a1a295f107fa187e5e5",
   "size": 24556,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:src/main.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00064",
   "role": "infra",
   "path": "src/record.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "51a3789e3dd16547d78ab73852e9d751b216a59b",
   "size": 31497,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:src/record.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00065",
   "role": "infra",
   "path": "src/ssh.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "522134bb338a89f7e2125bba6fa85fec9e82fc01",
   "size": 28557,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:src/ssh.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00066",
   "role": "infra",
   "path": "src/time.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "fa2309d62e10ac8724f20bacf6f602d2670bd207",
   "size": 4252,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:src/time.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00067",
   "role": "infra",
   "path": "src/transport.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "bd0fd1964664adc01d3c502e29027e911b9480e0",
   "size": 57624,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:src/transport.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00068",
   "role": "infra",
   "path": "test/entrypoint-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "1f93239e6ed664c3c94d6bcd14c46eab5e4d0173",
   "size": 28178,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:test/entrypoint-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00069",
   "role": "infra",
   "path": "test/fake-host.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "7dd27ce93925a39d368723e4c39e03e324d8bafd",
   "size": 3757,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/fake-host.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00070",
   "role": "infra",
   "path": "test/fake-hostctl.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "2539f7be97b63f201679c4caf8fdcd7e6c5c2781",
   "size": 7840,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/fake-hostctl.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00071",
   "role": "infra",
   "path": "test/fixtures/publication/cases.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "dfc6eddca15ea087a268a0cbd54ecc1d349219ca",
   "size": 14447,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/fixtures/publication/cases.json"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00072",
   "role": "infra",
   "path": "test/helper-update-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "9a50c2a78dca58c293fd92bc0b02226b14a11728",
   "size": 29191,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/helper-update-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00073",
   "role": "infra",
   "path": "test/host-lifecycle-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "f7b3241eaa7f2f9cc4bbd0aa2b7804796fbefdde",
   "size": 159691,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/host-lifecycle-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00074",
   "role": "infra",
   "path": "test/host-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "cdbc9e64a2ba479aca9d6486316a39461e5061e0",
   "size": 33333,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/host-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00075",
   "role": "infra",
   "path": "test/lifecycle-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "0747b3330cc222a3746710b0900e84ac4b5f8203",
   "size": 18535,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:test/lifecycle-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00076",
   "role": "infra",
   "path": "test/propose-classify-test.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "86024a462bf175fd22da98dd3e50d3d29afac6fa",
   "size": 11330,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/propose-classify-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00077",
   "role": "infra",
   "path": "test/propose-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "f89cc6d73b4978e7146cb27a99fbf874426326c2",
   "size": 76637,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/propose-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00078",
   "role": "infra",
   "path": "test/publication-validation-test.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "ba65c6f475c5e440b221826e6f24478ba9a97af4",
   "size": 5394,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/publication-validation-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00079",
   "role": "infra",
   "path": "test/run-evidence-test.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "7158f86dfedd7b80965ef3afa1b66dd45e051b6f",
   "size": 17186,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/run-evidence-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00080",
   "role": "infra",
   "path": "test/run-sh-test.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "03f20c1cf6952dd2df893448714bfc2cb78fd40d",
   "size": 4994,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/run-sh-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00081",
   "role": "infra",
   "path": "test/run.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "79035ad5da563d06f2780a02dd0ddd411f51f5ed",
   "size": 15144,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/run.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00082",
   "role": "infra",
   "path": "test/ssh-transport-test.py",
   "mode": "100644",
   "kind": "blob",
   "blob": "a03c6495ea9b63cb567d9d084fc3e1b087736632",
   "size": 16231,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/ssh-transport-test.py"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00083",
   "role": "infra",
   "path": "test/supervisor-test.sh",
   "mode": "100644",
   "kind": "blob",
   "blob": "1492d27e41a9ccb5735fc56b2e0b169f797c46ce",
   "size": 3188,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:test/supervisor-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00084",
   "role": "infra",
   "path": "test/worker-image-build-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "3a0440f629b43d56ccdba8a154e0687346f93969",
   "size": 12181,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/worker-image-build-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00085",
   "role": "infra",
   "path": "test/worker-image-validate-test.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "31c42425b0617baf650f207d22b63a5afd916da8",
   "size": 33756,
   "review_evidence": {
    "inspected_evidence": [
     "7430304:test/worker-image-validate-test.sh"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00086",
   "role": "infra",
   "path": "tests/cli.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c4545d338b8eee842ec85fcc5d794df6f82189a2",
   "size": 9969,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:tests/cli.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00087",
   "role": "infra",
   "path": "tests/lifecycle.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "772218af4427b230f1b58b0ce796349f4c7626bf",
   "size": 362758,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:tests/lifecycle.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "infra-F00088",
   "role": "infra",
   "path": "tests/publication.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "6bd21c2320e2bdcda1887f13e6a962a5ead70944",
   "size": 1854,
   "review_evidence": {
    "inspected_evidence": [
     "74303040aea74e6cfc931234be198e557bc696d5:tests/publication.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00001",
   "role": "target",
   "path": ".gitignore",
   "mode": "100644",
   "kind": "blob",
   "blob": "ea8c4bf7f35f6f77f75d92ad8ce8349f6e81ddba",
   "size": 8
  },
  {
   "id": "target-F00002",
   "role": "target",
   "path": "AGENTS.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "dceb0d9b23bf1af44c749abb3bd04a8266afcfc1",
   "size": 1331,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:AGENTS.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# AGENTS.md",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 18,
      "heading": "## Source references",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 19,
      "end": 22,
      "heading": "## Pull requests",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 23,
      "end": 34,
      "heading": "## Verification",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00003",
   "role": "target",
   "path": "Cargo.lock",
   "mode": "100644",
   "kind": "blob",
   "blob": "34f6a8d10e9e2470a6007e6eb35d24c7976039f1",
   "size": 12066
  },
  {
   "id": "target-F00004",
   "role": "target",
   "path": "Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "b649b76d367060a610bd916e8cc3bbc968855d4d",
   "size": 88,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00005",
   "role": "target",
   "path": "README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "9c10c741f20f3eb9ce68cbdf839710f8e1ed9a5d",
   "size": 6929,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:README.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 8,
      "heading": "# taisan",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 9,
      "end": 14,
      "heading": "## Why build this?",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 15,
      "end": 36,
      "heading": "## Core model",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 37,
      "end": 49,
      "heading": "## Scope and boundaries",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 50,
      "end": 70,
      "heading": "## Build and run",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 71,
      "end": 85,
      "heading": "## Design documents",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 86,
      "end": 89,
      "heading": "## v0.1 direction",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 90,
      "end": 92,
      "heading": "## License",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00006",
   "role": "target",
   "path": "assets/README.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f770b47f066234ea6fb396835b64fbee478f9c71",
   "size": 491
  },
  {
   "id": "target-F00007",
   "role": "target",
   "path": "assets/taisan-hero.jpg",
   "mode": "100644",
   "kind": "blob",
   "blob": "a9892c0201841c27cad885c032223158e87a16ec",
   "size": 380992
  },
  {
   "id": "target-F00008",
   "role": "target",
   "path": "crates/workloop-cli/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "ea063b2606daf267c07afc8805153852cc75c796",
   "size": 502,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00009",
   "role": "target",
   "path": "crates/workloop-cli/src/bin/taisan.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "b507ac3097bebf4cbf8b1e194b2695665c744798",
   "size": 64,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/src/bin/taisan.rs",
     "Four-line shared-main include wrapper; primary/legacy branding contract assertions in branding.rs"
    ],
    "regions": [
     {
      "start": 4,
      "end": 6,
      "symbol": "fn main() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00010",
   "role": "target",
   "path": "crates/workloop-cli/src/main.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "47772f5b4ff8bcc82aecd53db28113b54395e93e",
   "size": 22221,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/src/main.rs"
    ],
    "regions": [
     {
      "start": 142,
      "end": 149,
      "symbol": "fn uuidish() -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 150,
      "end": 156,
      "symbol": "fn now() -> i64 {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 157,
      "end": 168,
      "symbol": "fn parse_issue(s: &str) -> Result<GitHubIssueRef, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 169,
      "end": 188,
      "symbol": "fn parse_resource(s: &str) -> Result<WriteResource, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 189,
      "end": 205,
      "symbol": "fn parse_decision(s: &str) -> Result<FindingDecision, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 206,
      "end": 209,
      "symbol": "fn shlex_split(s: &str) -> Vec<String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 210,
      "end": 216,
      "symbol": "fn print(value: &impl serde::Serialize) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 217,
      "end": 224,
      "symbol": "pub(crate) fn main() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 225,
      "end": 462,
      "symbol": "fn run(cli: Cli) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 463,
      "end": 481,
      "symbol": "fn build_supervisor(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 482,
      "end": 514,
      "symbol": "fn run_tick(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 515,
      "end": 526,
      "symbol": "fn config() -> WorkloopConfig {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 527,
      "end": 539,
      "symbol": "fn installation() -> Installation {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 540,
      "end": 573,
      "symbol": "fn submit(store: &SqliteStore, key: &str) -> (WorkId, ExecutionId) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 574,
      "end": 587,
      "symbol": "fn force_waiting_on_github_write(store: &SqliteStore, eid: &ExecutionId) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 588,
      "end": 610,
      "symbol": "fn queue_publish(store: &SqliteStore, work_id: &WorkId) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 611,
      "end": 672,
      "symbol": "fn github_state_persists_across_ticks_of_one_run() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00011",
   "role": "target",
   "path": "crates/workloop-cli/tests/branding.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "d9cceb9699c254ba415f88cd2a02fa5e4d3bfafc",
   "size": 483,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/tests/branding.rs"
    ],
    "regions": [
     {
      "symbol": "primary_and_legacy_commands_expose_taisan_help",
      "start": 4,
      "end": 15,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00012",
   "role": "target",
   "path": "crates/workloop-cli/tests/cli_surface.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "d505b34dbbdb795c4abe04cc799177355d5c0cf3",
   "size": 5424,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/tests/cli_surface.rs"
    ],
    "regions": [
     {
      "symbol": "command_and_query_surface_round_trip",
      "start": 48,
      "end": 165,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "actor_identity_is_stable_across_invocations",
      "start": 166,
      "end": 180,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00013",
   "role": "target",
   "path": "crates/workloop-config/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "397415d45e8e24941c59de97658b9abaf27b021e",
   "size": 194,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-config/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00014",
   "role": "target",
   "path": "crates/workloop-config/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "a52424bf9b4051a62b35941624fad200437a7de1",
   "size": 15938,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-config/src/lib.rs"
    ],
    "regions": [
     {
      "start": 36,
      "end": 38,
      "symbol": "fn default_repository() -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 39,
      "end": 41,
      "symbol": "fn default_capacity() -> u32 {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 42,
      "end": 45,
      "symbol": "fn default_workspace_root() -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 46,
      "end": 59,
      "symbol": "fn uuidish() -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 60,
      "end": 82,
      "symbol": "fn scratch_path(path: &Path) -> PathBuf {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 83,
      "end": 100,
      "symbol": "fn read(path: &Path) -> Result<Option<Installation>, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 101,
      "end": 112,
      "symbol": "fn dir_sync_unsupported(error: &std::io::Error) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 113,
      "end": 134,
      "symbol": "fn sync_parent(path: &Path) -> std::io::Result<()> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 135,
      "end": 147,
      "symbol": "fn lock_path(path: &Path) -> PathBuf {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 148,
      "end": 172,
      "symbol": "fn lock_publish(path: &Path) -> Result<std::fs::File, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 173,
      "end": 221,
      "symbol": "fn publish_with(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 222,
      "end": 225,
      "symbol": "pub fn load_or_create(path: &Path) -> Result<Installation, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 226,
      "end": 266,
      "symbol": "fn load_or_create_with(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 267,
      "end": 277,
      "symbol": "pub fn actor(&self) -> Actor {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 278,
      "end": 295,
      "symbol": "pub fn workloop_config(&self) -> WorkloopConfig {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 296,
      "end": 306,
      "symbol": "fn temp_root(tag: &str) -> PathBuf {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 307,
      "end": 336,
      "symbol": "fn publish_falls_back_to_rename_without_hard_links() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 337,
      "end": 368,
      "symbol": "fn publish_fallback_adopts_a_concurrent_winner() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 369,
      "end": 382,
      "symbol": "fn directory_fsync_rejection_is_not_a_publish_failure() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 383,
      "end": 419,
      "symbol": "fn concurrent_fallback_writers_share_one_actor() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00015",
   "role": "target",
   "path": "crates/workloop-config/tests/installation.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "d3d2b99230975bd9830913ffc8a529ac1f3e35ff",
   "size": 3705,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-config/tests/installation.rs"
    ],
    "regions": [
     {
      "symbol": "tui_actor_identity_is_stable_and_cli_compatible",
      "start": 22,
      "end": 50,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "concurrent_first_writers_share_one_actor",
      "start": 51,
      "end": 79,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "foreign_empty_or_malformed_config_errors",
      "start": 80,
      "end": 98,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00016",
   "role": "target",
   "path": "crates/workloop-core/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "14c3ebb4105ea47ee8be2f6b16b583c90eec7399",
   "size": 140,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00017",
   "role": "target",
   "path": "crates/workloop-core/src/command.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "f957621a27b34d9220d858a84d665809db2fe3a1",
   "size": 5900,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/command.rs"
    ],
    "regions": [
     {
      "start": 73,
      "end": 87,
      "symbol": "pub fn work_id(&self) -> Option<&WorkId> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 88,
      "end": 183,
      "symbol": "pub fn expected_work_version(&self) -> Option<WorkVersion> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 184,
      "end": 193,
      "symbol": "pub fn command_id(&self) -> &CommandId {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 194,
      "end": 203,
      "symbol": "pub fn payload_digest(origin: &Origin, actor: &Actor, key: &str, body: &CommandBody) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00018",
   "role": "target",
   "path": "crates/workloop-core/src/domain.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "32e35b22ee04ac5159a8764ea9ef3d1fc605542f",
   "size": 11714,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/domain.rs"
    ],
    "regions": [
     {
      "start": 62,
      "end": 98,
      "symbol": "pub fn is_terminal(&self) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 99,
      "end": 135,
      "symbol": "pub fn is_unresolved_writer(&self) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 136,
      "end": 213,
      "symbol": "fn default() -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 214,
      "end": 220,
      "symbol": "pub fn terminal_code(&self) -> &'static str {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 221,
      "end": 233,
      "symbol": "pub fn detail(&self) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 234,
      "end": 250,
      "symbol": "pub fn budget_exhaustion(&self, role: &InvocationRole) -> Option<BudgetLimit> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 251,
      "end": 311,
      "symbol": "pub fn accepts_invocation(&self, role: &InvocationRole) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 312,
      "end": 351,
      "symbol": "pub fn key(&self) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00019",
   "role": "target",
   "path": "crates/workloop-core/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "fe277c690c5ea1e6fad486cefdd64de1d8a3b9a0",
   "size": 133,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/lib.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00020",
   "role": "target",
   "path": "crates/workloop-core/src/observation.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "7af85c4ef0c38fa7b30024cef00a774704ffa190",
   "size": 4034,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/observation.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00021",
   "role": "target",
   "path": "crates/workloop-core/src/ports.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "82987e19d08dd94a3b5ac835d410aa5655d868e6",
   "size": 3977,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/ports.rs"
    ],
    "regions": [
     {
      "start": 72,
      "end": 78,
      "symbol": "fn spawn(&self, argv: &[String], cwd: &Path) -> Result<ProcessIdentity, SpawnError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 79,
      "end": 81,
      "symbol": "fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 82,
      "end": 85,
      "symbol": "fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 86,
      "end": 90,
      "symbol": "fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 91,
      "end": 94,
      "symbol": "fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 95,
      "end": 96,
      "symbol": "fn reap(&self, pid: i32);",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00022",
   "role": "target",
   "path": "crates/workloop-core/src/projection.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "1018b15997b4626002d896cab922e98c5d752d51",
   "size": 19128,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/projection.rs"
    ],
    "regions": [
     {
      "start": 138,
      "end": 146,
      "symbol": "fn store_source() -> SourceStatus {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 147,
      "end": 166,
      "symbol": "pub fn phase_of(execution: &Execution, invocations: &[Invocation]) -> WorkPhase {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 167,
      "end": 180,
      "symbol": "pub(crate) fn current_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 181,
      "end": 197,
      "symbol": "pub(crate) fn derivable_attention_ids(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 198,
      "end": 211,
      "symbol": "fn derive_attention(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 212,
      "end": 290,
      "symbol": "fn attention_views(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 291,
      "end": 330,
      "symbol": "pub fn work_list(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 331,
      "end": 452,
      "symbol": "pub fn work_detail(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 453,
      "end": 476,
      "symbol": "pub fn attention(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 477,
      "end": 507,
      "symbol": "pub fn capacity(tx: &dyn ReadView) -> Result<QueryResult<CapacityView>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 508,
      "end": 546,
      "symbol": "pub fn command_result(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 547,
      "end": 560,
      "symbol": "pub fn outstanding_actions(tx: &dyn ReadView, work_id: &WorkId) -> Result<Vec<String>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 561,
      "end": 581,
      "symbol": "pub fn artifacts_complete(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 582,
      "end": 599,
      "symbol": "pub fn latest_process_fact(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00023",
   "role": "target",
   "path": "crates/workloop-core/src/stage.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "a9c87bd42d2ed6373a09e0d03cdc5548b883ac1f",
   "size": 8495,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/stage.rs"
    ],
    "regions": [
     {
      "start": 65,
      "end": 167,
      "symbol": "pub fn classify(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 168,
      "end": 196,
      "symbol": "pub fn validate_review_matrix(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00024",
   "role": "target",
   "path": "crates/workloop-core/src/store.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "db3eeb620e3474f7dba700b37d8e4db8f6b8d8af",
   "size": 12285,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/store.rs"
    ],
    "regions": [
     {
      "start": 129,
      "end": 189,
      "symbol": "fn pending() -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 190,
      "end": 219,
      "symbol": "fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 220,
      "end": 226,
      "symbol": "fn stored_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 227,
      "end": 231,
      "symbol": "fn command_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 232,
      "end": 232,
      "symbol": "fn work(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 233,
      "end": 234,
      "symbol": "fn execution(&self, execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 235,
      "end": 235,
      "symbol": "fn active_execution(&self, work_id: &WorkId) -> Result<Option<Execution>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 236,
      "end": 237,
      "symbol": "fn executions_for_work(&self, work_id: &WorkId) -> Result<Vec<Execution>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 238,
      "end": 239,
      "symbol": "fn queued_executions(&self) -> Result<Vec<Execution>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 240,
      "end": 240,
      "symbol": "fn reservation(&self, resource_key: &str) -> Result<Option<ResourceReservation>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 241,
      "end": 242,
      "symbol": "fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 243,
      "end": 243,
      "symbol": "fn slot_limit(&self) -> Result<u32, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 244,
      "end": 246,
      "symbol": "fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 247,
      "end": 247,
      "symbol": "fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 248,
      "end": 248,
      "symbol": "fn request(&self, request_id: &str) -> Result<Option<InvocationRequest>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 249,
      "end": 254,
      "symbol": "fn invocations_for_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 255,
      "end": 256,
      "symbol": "fn invocation_for_request(&self, request_id: &str) -> Result<Option<Invocation>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 257,
      "end": 258,
      "symbol": "fn unresolved_invocations(&self) -> Result<Vec<Invocation>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 259,
      "end": 259,
      "symbol": "fn works(&self) -> Result<Vec<Work>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 260,
      "end": 261,
      "symbol": "fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 262,
      "end": 265,
      "symbol": "fn stop_intent(&self, invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 266,
      "end": 268,
      "symbol": "fn deferrals(&self) -> Result<Vec<Deferral>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 269,
      "end": 272,
      "symbol": "fn attention_acks(&self, work_id: &WorkId) -> Result<Vec<String>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 273,
      "end": 276,
      "symbol": "fn observations_for_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 277,
      "end": 279,
      "symbol": "fn observations_for_work(&self, work_id: &WorkId) -> Result<Vec<Observation>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 280,
      "end": 282,
      "symbol": "fn action_intents(&self) -> Result<Vec<PrActionIntent>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 283,
      "end": 288,
      "symbol": "fn judgments(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 289,
      "end": 297,
      "symbol": "fn latest_judgment(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 298,
      "end": 299,
      "symbol": "fn latest_pr_head(&self, work_id: &WorkId) -> Result<Option<PrHeadSha>, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 300,
      "end": 305,
      "symbol": "fn last_seq(&self) -> Result<EventSequence, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 306,
      "end": 307,
      "symbol": "fn alloc_seq(&mut self) -> Result<EventSequence, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 308,
      "end": 309,
      "symbol": "fn alloc_id(&mut self, prefix: &str) -> Result<String, StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 310,
      "end": 313,
      "symbol": "fn apply(&mut self, effect: &Effect) -> Result<(), StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 314,
      "end": 315,
      "symbol": "fn begin_attempt(&mut self) -> Result<(), StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 316,
      "end": 317,
      "symbol": "fn revert_attempt(&mut self) -> Result<(), StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 318,
      "end": 321,
      "symbol": "fn commit_attempt(&mut self) -> Result<(), StoreError>;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 322,
      "end": 335,
      "symbol": "fn save_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 336,
      "end": 347,
      "symbol": "fn transact<R>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 348,
      "end": 354,
      "symbol": "fn transact_deferred<R>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00025",
   "role": "target",
   "path": "crates/workloop-core/src/work.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "f70aa40cdc1b9753fda73b209b4eefaf8616279f",
   "size": 81223,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs"
    ],
    "regions": [
     {
      "start": 33,
      "end": 115,
      "symbol": "pub fn handle_command(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 116,
      "end": 152,
      "symbol": "pub fn preview(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 153,
      "end": 192,
      "symbol": "fn decide_attempts(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 193,
      "end": 209,
      "symbol": "fn observed(tx: &dyn ReadView, req: &CommandRequest) -> Result<ActionPreview, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 210,
      "end": 235,
      "symbol": "fn preview_of_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 236,
      "end": 244,
      "symbol": "fn apply_all(tx: &mut dyn TxView, effects: &[Effect]) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 245,
      "end": 274,
      "symbol": "fn conflict_rejection(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 275,
      "end": 283,
      "symbol": "fn rejected(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 284,
      "end": 304,
      "symbol": "fn rejected_now(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 305,
      "end": 333,
      "symbol": "fn check_work(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 334,
      "end": 352,
      "symbol": "pub fn work_resources(work: &Work, config: &WorkloopConfig) -> Vec<WriteResource> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 353,
      "end": 431,
      "symbol": "fn decide(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 432,
      "end": 501,
      "symbol": "fn decide_submit(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 502,
      "end": 505,
      "symbol": "fn external_claimable(res: &WriteResource) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 506,
      "end": 590,
      "symbol": "fn decide_claim(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 591,
      "end": 595,
      "symbol": "fn is_expired(res: &ResourceReservation, now_unix_seconds: i64) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 596,
      "end": 641,
      "symbol": "fn decide_release(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 642,
      "end": 751,
      "symbol": "fn decide_retry(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 752,
      "end": 866,
      "symbol": "fn decide_resume(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 867,
      "end": 1037,
      "symbol": "fn decide_cancel(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1038,
      "end": 1055,
      "symbol": "fn resources_keys_of(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1056,
      "end": 1106,
      "symbol": "fn decide_acknowledge(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1107,
      "end": 1212,
      "symbol": "fn decide_judge(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1213,
      "end": 1327,
      "symbol": "fn decide_release_judgment(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1328,
      "end": 1339,
      "symbol": "fn fix_command_id(tx: &mut dyn TxView, decision: &mut CommandDecision) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1340,
      "end": 1360,
      "symbol": "pub fn admit_queued(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1361,
      "end": 1527,
      "symbol": "pub fn admit_queued_role(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1528,
      "end": 1555,
      "symbol": "pub fn handle_observation(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1556,
      "end": 1565,
      "symbol": "pub fn is_dedupe_conflict(e: &StoreError) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1566,
      "end": 1697,
      "symbol": "fn assess(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1698,
      "end": 1718,
      "symbol": "fn mark_invocation(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1719,
      "end": 1808,
      "symbol": "fn assess_process(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1809,
      "end": 1888,
      "symbol": "fn settle_after_writer_gone(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1889,
      "end": 1926,
      "symbol": "fn assess_artifact(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1927,
      "end": 1997,
      "symbol": "fn maybe_publish(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1998,
      "end": 2099,
      "symbol": "fn assess_completion(tx: &mut dyn TxView, execution: &Execution) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 2100,
      "end": 2119,
      "symbol": "fn latest_observed_ok(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 2120,
      "end": 2161,
      "symbol": "pub fn dispatch_once(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 2162,
      "end": 2215,
      "symbol": "pub fn sweep(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 2216,
      "end": 2258,
      "symbol": "fn fail_terminal(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00026",
   "role": "target",
   "path": "crates/workloop-core/tests/stage_contract.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "30829714f7d33a664cf95aeeee8e04f5ad7c9b90",
   "size": 8355,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/tests/stage_contract.rs"
    ],
    "regions": [
     {
      "symbol": "pushed_head_equal_to_live_head_classifies_fixed",
      "start": 21,
      "end": 31,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "pushed_head_verified_as_ancestor_of_live_head_classifies_fixed",
      "start": 32,
      "end": 44,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "claimed_head_without_a_push_is_a_failure_that_preserves_evidence",
      "start": 45,
      "end": 72,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "produced_head_not_published_is_a_failure_even_when_the_old_head_is_live",
      "start": 73,
      "end": 104,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "no_op_push_of_the_unchanged_head_is_nothing_to_fix",
      "start": 105,
      "end": 129,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "pushed_head_that_does_not_reach_the_live_head_is_a_failure",
      "start": 130,
      "end": 144,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "no_change_and_no_push_classifies_nothing_to_fix",
      "start": 145,
      "end": 158,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "converge_record_serializes_with_the_contract_shape",
      "start": 159,
      "end": 195,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "every_supported_matrix_cell_preserves_the_reviewer_axis",
      "start": 196,
      "end": 224,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "unknown_executor_is_refused_at_configuration_validation",
      "start": 225,
      "end": 234,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "unknown_reviewer_is_refused_at_configuration_validation",
      "start": 235,
      "end": 242,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00027",
   "role": "target",
   "path": "crates/workloop-core/tests/store_error_semantics.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "70289f7715a06011683a035b300a72b372845bc6",
   "size": 7832,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/tests/store_error_semantics.rs"
    ],
    "regions": [
     {
      "symbol": "internal_apply_failure_reaches_the_caller_unpersisted",
      "start": 214,
      "end": 235,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "constraint_conflict_still_retries_then_persists_ineligible",
      "start": 236,
      "end": 255,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00028",
   "role": "target",
   "path": "crates/workloop-exec/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "df210a7e34784a78102371bbe919d67df1098f5a",
   "size": 198,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00029",
   "role": "target",
   "path": "crates/workloop-exec/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "d81d55de2bd955132a1ece25262336dd724ac264",
   "size": 280,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/lib.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00030",
   "role": "target",
   "path": "crates/workloop-exec/src/supervisor.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "6813c8f8e6279b405f16a10f17f2d9d41984a010",
   "size": 54714,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs"
    ],
    "regions": [
     {
      "start": 27,
      "end": 31,
      "symbol": "fn ps_output(args: &[&str]) -> std::io::Result<String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 32,
      "end": 52,
      "symbol": "fn checked_ps_output(out: Output, allows_absence: bool) -> std::io::Result<String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 53,
      "end": 67,
      "symbol": "fn normalize_lstart(s: &str) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 68,
      "end": 83,
      "symbol": "fn classify_lstart_lookup(out: std::io::Result<String>) -> LstartLookup {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 84,
      "end": 87,
      "symbol": "fn lstart_of(pid: i32) -> LstartLookup {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 88,
      "end": 98,
      "symbol": "fn stat_of(pid: i32) -> Option<String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 99,
      "end": 132,
      "symbol": "fn spawn(&self, argv: &[String], cwd: &Path) -> Result<ProcessIdentity, SpawnError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 133,
      "end": 182,
      "symbol": "fn snapshot_descendants(&self, leader_pid: i32) -> Result<Vec<ProcessIdentity>, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 183,
      "end": 217,
      "symbol": "fn inspect(&self, identity: &ProcessIdentity) -> ProcessEvidence {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 218,
      "end": 246,
      "symbol": "fn signal_verified(&self, identity: &ProcessIdentity, signal: i32) -> SignalOutcome {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 247,
      "end": 262,
      "symbol": "fn signal_group(&self, pgid: i32, signal: i32) -> SignalOutcome {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 263,
      "end": 279,
      "symbol": "fn reap(&self, pid: i32) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 280,
      "end": 289,
      "symbol": "fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 290,
      "end": 294,
      "symbol": "fn from(e: StoreError) -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 295,
      "end": 328,
      "symbol": "fn from(e: std::io::Error) -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 329,
      "end": 343,
      "symbol": "pub fn new(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 344,
      "end": 353,
      "symbol": "fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 354,
      "end": 357,
      "symbol": "pub fn locks_len(&self) -> usize {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 358,
      "end": 365,
      "symbol": "pub fn workspace_of(&self, work_id: &WorkId) -> PathBuf {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 366,
      "end": 376,
      "symbol": "pub fn run_once(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 377,
      "end": 387,
      "symbol": "pub fn consume_requests(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 388,
      "end": 537,
      "symbol": "fn reconcile_or_start(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 538,
      "end": 552,
      "symbol": "fn mark_consumed(&self, store: &impl Store, request_id: &str) -> Result<(), ExecError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 553,
      "end": 564,
      "symbol": "fn mark_done_if_resolved(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 565,
      "end": 651,
      "symbol": "pub fn poll_invocations(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 652,
      "end": 700,
      "symbol": "fn finish_invocation(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 701,
      "end": 864,
      "symbol": "pub fn drive_stops(&self, store: &impl Store, now_unix_seconds: i64) -> Result<(), ExecError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 865,
      "end": 880,
      "symbol": "pub fn request_stop(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 881,
      "end": 903,
      "symbol": "pub fn record_stop_intent(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 904,
      "end": 938,
      "symbol": "pub fn stop_unless_persisted(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 939,
      "end": 980,
      "symbol": "fn persist_stop_snapshot(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 981,
      "end": 1055,
      "symbol": "pub fn run_recovery(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1056,
      "end": 1068,
      "symbol": "fn report(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1069,
      "end": 1118,
      "symbol": "fn report_tagged(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1119,
      "end": 1179,
      "symbol": "fn report_evidence(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1180,
      "end": 1193,
      "symbol": "fn persist_intent_row(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1194,
      "end": 1209,
      "symbol": "fn required_artifacts(request: &InvocationRequest) -> Vec<String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1210,
      "end": 1227,
      "symbol": "fn find_artifact(workspace: &Path, name: &str) -> Option<PathBuf> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1228,
      "end": 1237,
      "symbol": "fn artifact_present(workspace: &Path, name: &str) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1238,
      "end": 1250,
      "symbol": "pub fn write_artifact(path: &Path, content: &str) -> std::io::Result<()> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1251,
      "end": 1266,
      "symbol": "fn failed_ps_status_cannot_prove_absence() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1267,
      "end": 1269,
      "symbol": "fn spawn(&self, _: &[String], _: &Path) -> Result<ProcessIdentity, SpawnError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1270,
      "end": 1272,
      "symbol": "fn snapshot_descendants(&self, _: i32) -> Result<Vec<ProcessIdentity>, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1273,
      "end": 1275,
      "symbol": "fn inspect(&self, _: &ProcessIdentity) -> ProcessEvidence {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1276,
      "end": 1278,
      "symbol": "fn signal_verified(&self, _: &ProcessIdentity, _: i32) -> SignalOutcome {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1279,
      "end": 1281,
      "symbol": "fn signal_group(&self, _: i32, _: i32) -> SignalOutcome {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1282,
      "end": 1285,
      "symbol": "fn reap(&self, _: i32) {}",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1286,
      "end": 1329,
      "symbol": "fn failed_snapshot_neither_persists_partial_stop_intent_nor_signals() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1330,
      "end": 1355,
      "symbol": "fn lstart_lookup_keeps_failure_distinct_from_absence() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00031",
   "role": "target",
   "path": "crates/workloop-exec/tests/supervisor_contract.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "8b8b3e284755db2ef56c967137c8d20f8945448e",
   "size": 71333,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/tests/supervisor_contract.rs"
    ],
    "regions": [
     {
      "symbol": "spawn_records_identity_in_own_process_group",
      "start": 169,
      "end": 184,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "signal_reverified_never_hits_a_reused_pid",
      "start": 185,
      "end": 215,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "zombie_counts_as_stopped",
      "start": 216,
      "end": 244,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "reaped_identity_inspects_stopped_and_signals_already_gone",
      "start": 245,
      "end": 281,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "setsid_escapee_is_in_the_snapshot_and_signaled",
      "start": 282,
      "end": 319,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "persist_failure_sends_no_signal",
      "start": 320,
      "end": 360,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "crash_after_ready_replays_the_request_exactly_once",
      "start": 361,
      "end": 390,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "crash_near_spawn_leaves_uncertain_with_no_second_writer",
      "start": 391,
      "end": 439,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "live_process_is_rereported_running_after_a_lost_running_observation",
      "start": 440,
      "end": 506,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "exited_writer_on_a_ready_execution_enters_reconciliation",
      "start": 507,
      "end": 581,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "launch_refused_pauses_and_releases_the_slot",
      "start": 582,
      "end": 608,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_on_running_reaches_cancelled_only_after_stop_proof",
      "start": 609,
      "end": 661,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "missing_artifact_runs_bounded_recovery_then_fails",
      "start": 662,
      "end": 731,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "recovery_artifact_in_the_workspace_root_is_not_truncated",
      "start": 732,
      "end": 794,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "failed_artifact_promotion_is_not_reported_present",
      "start": 795,
      "end": 873,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "clean_exit_records_artifact_and_publish_intent",
      "start": 874,
      "end": 901,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_while_waiting_settles_once_no_writer_remains",
      "start": 902,
      "end": 980,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_on_ready_settles_without_ever_spawning",
      "start": 981,
      "end": 1007,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "unknown_inspection_maps_to_uncertain",
      "start": 1008,
      "end": 1048,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "failed_finish_reports_are_replayed_after_restart",
      "start": 1049,
      "end": 1147,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stale_or_unverified_leader_never_authorizes_a_group_signal",
      "start": 1148,
      "end": 1237,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stop_receipts_distinguish_delivered_signals_from_stop_proof",
      "start": 1238,
      "end": 1313,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "repeated_liveness_transitions_recover_without_poll_observation_growth",
      "start": 1314,
      "end": 1388,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stop_intent_is_durable_before_the_first_signal",
      "start": 1412,
      "end": 1474,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "setsid_escapee_survives_the_group_signal_but_blocks_the_terminal_commit",
      "start": 1475,
      "end": 1597,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "kill9_after_intent_persisted_resumes_from_the_durable_intent",
      "start": 1598,
      "end": 1688,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "the_supervisor_reaps_the_children_it_spawned",
      "start": 1689,
      "end": 1751,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "a_workers_done_claim_alone_releases_nothing",
      "start": 1752,
      "end": 1837,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "an_exited_report_settles_a_requested_stop",
      "start": 1838,
      "end": 1908,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "a_missing_pid_keeps_the_execution_uncertain_and_releases_nothing",
      "start": 1909,
      "end": 1965,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "a_runtime_deadline_expiry_requests_stop_and_releases_nothing",
      "start": 1966,
      "end": 2008,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00032",
   "role": "target",
   "path": "crates/workloop-fixture-worker/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "0737fe5b7137913b493e079c097de671def396a7",
   "size": 101,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-fixture-worker/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00033",
   "role": "target",
   "path": "crates/workloop-fixture-worker/src/main.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "0dfd99fdfb9ef6d5214732f159689af7c1b8e64c",
   "size": 6600,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-fixture-worker/src/main.rs"
    ],
    "regions": [
     {
      "start": 25,
      "end": 54,
      "symbol": "fn parse() -> Args {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 55,
      "end": 58,
      "symbol": "fn workspace() -> PathBuf {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 59,
      "end": 66,
      "symbol": "fn write_artifact(path: &Path, body: &str) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 67,
      "end": 73,
      "symbol": "fn self_exe() -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 74,
      "end": 181,
      "symbol": "fn main() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 182,
      "end": 187,
      "symbol": "fn libc_setsid() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 188,
      "end": 192,
      "symbol": "fn setsid() -> i32;",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00034",
   "role": "target",
   "path": "crates/workloop-github/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "28e852f3f6649305637ee3d3c1defb9a3d541080",
   "size": 187,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00035",
   "role": "target",
   "path": "crates/workloop-github/src/fixture.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "1c1641345874c742e54d8371df409b198acc820d",
   "size": 8139,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/src/fixture.rs"
    ],
    "regions": [
     {
      "start": 30,
      "end": 34,
      "symbol": "pub fn new() -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 35,
      "end": 40,
      "symbol": "pub fn set_head(&mut self, repository: &str, number: u64, head_sha: &str) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 41,
      "end": 47,
      "symbol": "pub fn applied(&self) -> &[String] {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 48,
      "end": 64,
      "symbol": "pub fn emit(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 65,
      "end": 90,
      "symbol": "pub fn apply_outbox(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 91,
      "end": 210,
      "symbol": "fn attempt(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00036",
   "role": "target",
   "path": "crates/workloop-github/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "36e561a3b232ab7496c001071715bd1d0b3c8de8",
   "size": 225,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/src/lib.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00037",
   "role": "target",
   "path": "crates/workloop-github/tests/fixture_adapter.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c9195da439d84114041c26382e0fd00a1098c201",
   "size": 36755,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/tests/fixture_adapter.rs"
    ],
    "regions": [
     {
      "symbol": "publish_writes_only_from_a_recorded_intent_and_reports_applied",
      "start": 111,
      "end": 147,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "head_mismatch_aborts_the_write_and_surfaces_the_fresh_head",
      "start": 148,
      "end": 197,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "head_match_applies_the_write",
      "start": 198,
      "end": 217,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "unknown_result_stays_outstanding_until_reconciled_by_action_id",
      "start": 218,
      "end": 272,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "head_bound_evidence_drives_success_only_for_the_current_head",
      "start": 273,
      "end": 383,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "repeated_artifact_evidence_does_not_queue_a_second_publish",
      "start": 384,
      "end": 451,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "terminal_execution_still_authorizes_outstanding_effect_reconciliation",
      "start": 452,
      "end": 488,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "execution_denial_is_not_overridden_by_installation_permissions",
      "start": 489,
      "end": 546,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "github_write_denied_leaves_the_outbox_pending",
      "start": 547,
      "end": 583,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "superseded_head_evidence_does_not_complete_the_work",
      "start": 584,
      "end": 678,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "delayed_check_success_cannot_complete_the_work",
      "start": 713,
      "end": 808,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "delayed_review_approval_cannot_complete_the_work",
      "start": 809,
      "end": 886,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_defers_terminal_settlement_until_the_outbox_reconciles",
      "start": 887,
      "end": 1021,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "budget_failure_waits_for_the_outstanding_outbox_effect",
      "start": 1022,
      "end": 1076,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00038",
   "role": "target",
   "path": "crates/workloop-store/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "0e119380049d864210fccffbe54adc2f3e904dd4",
   "size": 206,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00039",
   "role": "target",
   "path": "crates/workloop-store/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "8885a518096279ee988b2cd9820b5c07a1254ae4",
   "size": 78,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/src/lib.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00040",
   "role": "target",
   "path": "crates/workloop-store/src/schema.sql",
   "mode": "100644",
   "kind": "blob",
   "blob": "8ea8a0ad9b35754cff073903707f7529fc96c7b9",
   "size": 4796,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/src/schema.sql"
    ],
    "regions": [
     {
      "start": 1,
      "end": 159,
      "scope": "Complete DDL/index/FK/uniqueness review"
     }
    ]
   }
  },
  {
   "id": "target-F00041",
   "role": "target",
   "path": "crates/workloop-store/src/sqlite.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "07d1c4f885a0ff29f7e834008b6b442de5e71df9",
   "size": 41350,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/src/sqlite.rs"
    ],
    "regions": [
     {
      "start": 34,
      "end": 39,
      "symbol": "pub fn open(path: &std::path::Path, capacity: u32) -> Result<Self, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 40,
      "end": 44,
      "symbol": "pub fn in_memory(capacity: u32) -> Result<Self, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 45,
      "end": 68,
      "symbol": "fn init(conn: Connection, capacity: u32) -> Result<Self, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 69,
      "end": 75,
      "symbol": "fn internal(e: impl std::fmt::Display) -> StoreError {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 76,
      "end": 88,
      "symbol": "fn map_err(e: rusqlite::Error) -> StoreError {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 89,
      "end": 94,
      "symbol": "fn json<T: serde::Serialize>(v: &T) -> Result<String, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 95,
      "end": 101,
      "symbol": "fn tag<T: serde::Serialize>(v: &T) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 102,
      "end": 105,
      "symbol": "fn untag<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 106,
      "end": 113,
      "symbol": "fn opt_untag<T: serde::de::DeserializeOwned>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 114,
      "end": 117,
      "symbol": "fn conv(e: impl std::fmt::Display) -> rusqlite::Error {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 118,
      "end": 121,
      "symbol": "fn de_row<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 122,
      "end": 132,
      "symbol": "fn opt_de_row<T: serde::de::DeserializeOwned>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 133,
      "end": 159,
      "symbol": "fn run<R>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 160,
      "end": 166,
      "symbol": "fn transact<R>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 167,
      "end": 175,
      "symbol": "fn transact_deferred<R>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 176,
      "end": 179,
      "symbol": "fn exec(&self, sql: &str, p: impl rusqlite::Params) -> Result<usize, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 180,
      "end": 188,
      "symbol": "fn query_opt<T>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 189,
      "end": 203,
      "symbol": "fn query_all<T>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 204,
      "end": 209,
      "symbol": "fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 210,
      "end": 243,
      "symbol": "fn execution_from_row(row: &rusqlite::Row<'_>) -> Result<Execution, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 244,
      "end": 256,
      "symbol": "fn invocation_from_row(row: &rusqlite::Row<'_>) -> Result<Invocation, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 257,
      "end": 266,
      "symbol": "fn request_from_row(row: &rusqlite::Row<'_>) -> Result<InvocationRequest, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 267,
      "end": 276,
      "symbol": "fn observation_from_row(row: &rusqlite::Row<'_>) -> Result<Observation, rusqlite::Error> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 277,
      "end": 310,
      "symbol": "fn write_execution(tx: &Tx<'_>, e: &Execution) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 311,
      "end": 369,
      "symbol": "fn update_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 370,
      "end": 384,
      "symbol": "fn stored_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 385,
      "end": 395,
      "symbol": "fn command_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 396,
      "end": 415,
      "symbol": "fn work(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 416,
      "end": 423,
      "symbol": "fn execution(&self, execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 424,
      "end": 434,
      "symbol": "fn active_execution(&self, work_id: &WorkId) -> Result<Option<Execution>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 435,
      "end": 442,
      "symbol": "fn executions_for_work(&self, work_id: &WorkId) -> Result<Vec<Execution>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 443,
      "end": 453,
      "symbol": "fn queued_executions(&self) -> Result<Vec<Execution>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 454,
      "end": 469,
      "symbol": "fn reservation(&self, resource_key: &str) -> Result<Option<ResourceReservation>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 470,
      "end": 485,
      "symbol": "fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 486,
      "end": 493,
      "symbol": "fn slot_limit(&self) -> Result<u32, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 494,
      "end": 503,
      "symbol": "fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 504,
      "end": 520,
      "symbol": "fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 521,
      "end": 529,
      "symbol": "fn request(&self, request_id: &str) -> Result<Option<InvocationRequest>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 530,
      "end": 543,
      "symbol": "fn invocations_for_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 544,
      "end": 551,
      "symbol": "fn invocation_for_request(&self, request_id: &str) -> Result<Option<Invocation>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 552,
      "end": 562,
      "symbol": "fn unresolved_invocations(&self) -> Result<Vec<Invocation>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 563,
      "end": 582,
      "symbol": "fn works(&self) -> Result<Vec<Work>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 583,
      "end": 594,
      "symbol": "fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 595,
      "end": 609,
      "symbol": "fn stop_intent(&self, invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 610,
      "end": 624,
      "symbol": "fn deferrals(&self) -> Result<Vec<Deferral>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 625,
      "end": 632,
      "symbol": "fn attention_acks(&self, work_id: &WorkId) -> Result<Vec<String>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 633,
      "end": 645,
      "symbol": "fn observations_for_execution(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 646,
      "end": 657,
      "symbol": "fn observations_for_work(&self, work_id: &WorkId) -> Result<Vec<Observation>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 658,
      "end": 674,
      "symbol": "fn action_intents(&self) -> Result<Vec<PrActionIntent>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 675,
      "end": 686,
      "symbol": "fn judgments(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 687,
      "end": 698,
      "symbol": "fn latest_judgment(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 699,
      "end": 720,
      "symbol": "fn latest_pr_head(&self, work_id: &WorkId) -> Result<Option<PrHeadSha>, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 721,
      "end": 730,
      "symbol": "fn last_seq(&self) -> Result<EventSequence, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 731,
      "end": 738,
      "symbol": "fn alloc_seq(&mut self) -> Result<EventSequence, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 739,
      "end": 755,
      "symbol": "fn alloc_id(&mut self, prefix: &str) -> Result<String, StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 756,
      "end": 760,
      "symbol": "fn begin_attempt(&mut self) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 761,
      "end": 766,
      "symbol": "fn revert_attempt(&mut self) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 767,
      "end": 771,
      "symbol": "fn commit_attempt(&mut self) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 772,
      "end": 1065,
      "symbol": "fn apply(&mut self, effect: &Effect) -> Result<(), StoreError> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1066,
      "end": 1098,
      "symbol": "fn save_decision(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 1099,
      "end": 1107,
      "symbol": "fn open_sets_a_busy_timeout() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00042",
   "role": "target",
   "path": "crates/workloop-store/tests/contract_commands.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "5d7a1cbc336cd55717325db5cd73bd3eb53296ae",
   "size": 99032,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_commands.rs"
    ],
    "regions": [
     {
      "symbol": "submit_replays_the_same_decision",
      "start": 218,
      "end": 224,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "same_key_different_payload_is_idempotency_conflict",
      "start": 225,
      "end": 238,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "submit_creates_work_and_queued_execution",
      "start": 239,
      "end": 249,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "retry_requires_confirmed_failure",
      "start": 250,
      "end": 293,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stale_work_version_is_rejected",
      "start": 294,
      "end": 315,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "resume_requeues_paused_and_rejects_others",
      "start": 316,
      "end": 359,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_queued_is_terminal_and_second_cancel_is_noop",
      "start": 360,
      "end": 399,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "claim_conflict_returns_holder_and_expiry_frees",
      "start": 408,
      "end": 431,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "release_only_frees_the_claimers_rows",
      "start": 432,
      "end": 455,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "dispatch_admits_oldest_eligible_and_reserves_atomically",
      "start": 456,
      "end": 486,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "queued_execution_defers_when_capacity_is_full",
      "start": 487,
      "end": 503,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "earlier_external_claim_defers_queued_execution",
      "start": 504,
      "end": 528,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "dispatch_conflict_defers_that_item_and_later_items_still_admit",
      "start": 529,
      "end": 583,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "conflicting_payload_replays_the_conflict_decision",
      "start": 584,
      "end": 603,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "external_claim_names_only_branch_pairs",
      "start": 604,
      "end": 638,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_noop_reports_the_current_version",
      "start": 639,
      "end": 670,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "release_judgment_requires_a_matching_recorded_judgment",
      "start": 671,
      "end": 718,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "resume_readmission_keeps_reservations_sequence_and_budget_anchor",
      "start": 719,
      "end": 792,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "workspace_claim_cannot_block_dispatch",
      "start": 793,
      "end": 852,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "resume_replays_and_readmits_once",
      "start": 853,
      "end": 936,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "retry_replays_the_same_decision",
      "start": 937,
      "end": 968,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_replays_the_same_decision",
      "start": 969,
      "end": 992,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "claim_resources_replays_without_renewing",
      "start": 993,
      "end": 1014,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "release_resources_replays_the_same_decision",
      "start": 1015,
      "end": 1042,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "acknowledge_replays_the_same_decision",
      "start": 1043,
      "end": 1078,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judge_findings_replays_the_same_decision",
      "start": 1079,
      "end": 1118,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "release_judgment_replays_the_same_decision",
      "start": 1119,
      "end": 1175,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "admission_never_exceeds_max_invocations",
      "start": 1207,
      "end": 1262,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "resume_fails_when_invocation_budget_exhausted",
      "start": 1263,
      "end": 1321,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "review_admission_never_exceeds_max_review_rounds",
      "start": 1322,
      "end": 1380,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judge_rejects_head_with_no_observation",
      "start": 1381,
      "end": 1408,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judge_rejects_arbitrary_head",
      "start": 1409,
      "end": 1436,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judge_accepts_the_observed_head",
      "start": 1437,
      "end": 1459,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "release_rejects_head_changed_after_judgment",
      "start": 1460,
      "end": 1504,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stale_head_observation_cannot_move_head_backward",
      "start": 1505,
      "end": 1537,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "admission_rolls_back_on_request_persistence_failure",
      "start": 1538,
      "end": 1593,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_resumed_queued_execution_releases_retained_reservations",
      "start": 1594,
      "end": 1664,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_from_queued_leaves_foreign_claims_alone",
      "start": 1665,
      "end": 1723,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "admission_refuses_to_honor_a_judgment_for_a_moved_head",
      "start": 1724,
      "end": 1828,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "recording_an_invocation_consumes_only_its_request",
      "start": 1846,
      "end": 1913,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "recording_an_invocation_without_a_request_row_leaves_pending_alone",
      "start": 1914,
      "end": 1955,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "every_command_kind_conflicts_on_a_payload_change_under_the_same_key",
      "start": 1956,
      "end": 2202,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_noop_reports_the_stored_version",
      "start": 2203,
      "end": 2255,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "cancel_queued_releases_the_resumed_executions_reservations",
      "start": 2256,
      "end": 2315,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judgments_bind_to_the_recorded_head",
      "start": 2316,
      "end": 2407,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "repeated_idempotency_conflict_replays_the_saved_decision",
      "start": 2408,
      "end": 2425,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "resumed_execution_reuses_its_own_reservations",
      "start": 2426,
      "end": 2471,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "invocation_budget_exhaustion_is_a_committed_failure",
      "start": 2472,
      "end": 2515,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "launch_refused_parks_a_ready_execution_and_resolves_the_invocation",
      "start": 2548,
      "end": 2590,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "late_launch_refusal_never_overwrites_a_stopping_execution",
      "start": 2591,
      "end": 2647,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "late_launch_refusal_never_resurrects_a_terminal_execution",
      "start": 2648,
      "end": 2665,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "work_scoped_check_and_review_evidence_completes_the_work",
      "start": 2666,
      "end": 2755,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "completion_requires_artifacts_publication_and_resolved_writers",
      "start": 2756,
      "end": 2861,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "dispatch_state_conflict_defers_only_the_conflicted_execution",
      "start": 2862,
      "end": 2920,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00043",
   "role": "target",
   "path": "crates/workloop-store/tests/contract_preview.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "22f984702e51ed48c9e39d311aea387958688b2d",
   "size": 13758,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_preview.rs"
    ],
    "regions": [
     {
      "symbol": "preview_persists_nothing",
      "start": 141,
      "end": 202,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "preview_mirrors_decision_outcome",
      "start": 203,
      "end": 279,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "preview_reports_observed_head",
      "start": 280,
      "end": 309,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "preview_never_substitutes_for_submit",
      "start": 310,
      "end": 354,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "preview_mirrors_apply_time_rejection",
      "start": 355,
      "end": 408,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00044",
   "role": "target",
   "path": "crates/workloop-store/tests/contract_queries.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c0c75e97678e9feb180bc756ccc02f391b82108b",
   "size": 25325,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_queries.rs"
    ],
    "regions": [
     {
      "symbol": "work_list_phases_derive_from_records",
      "start": 80,
      "end": 99,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "work_detail_binds_findings_to_the_current_head",
      "start": 100,
      "end": 159,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "delayed_head_observation_does_not_change_detail_head_or_findings",
      "start": 160,
      "end": 229,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "work_detail_reports_the_terminal_outcome",
      "start": 230,
      "end": 256,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "attention_versions_and_acknowledgement",
      "start": 257,
      "end": 316,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "acknowledge_requires_a_currently_derivable_attention_version",
      "start": 317,
      "end": 393,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "repeated_pause_and_new_evidence_get_fresh_attention_versions",
      "start": 394,
      "end": 494,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "uncertain_beyond_threshold_surfaces_attention",
      "start": 495,
      "end": 530,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "capacity_explains_every_slot_reservation_and_deferral",
      "start": 531,
      "end": 589,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "command_result_recovers_a_lost_response_by_id_and_replay_key",
      "start": 590,
      "end": 648,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "runtime_limit_is_a_committed_stopping_decision",
      "start": 649,
      "end": 669,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "deferred_head_does_not_block_later_items",
      "start": 670,
      "end": 711,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "expired_external_claim_forfeits_exclusion",
      "start": 712,
      "end": 741,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00045",
   "role": "target",
   "path": "crates/workloop-store/tests/publication_recovery.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "d8a0a4e6a4b594455409cf3cdcc6ebef33e39814",
   "size": 5215,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/publication_recovery.rs"
    ],
    "regions": [
     {
      "symbol": "recovery_does_not_publish_from_an_earlier_invocations_artifacts",
      "start": 11,
      "end": 149,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00046",
   "role": "target",
   "path": "crates/workloop-store/tests/review_reconciliation.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "e73c9a9448f01d011390718a9505aa39ca823919",
   "size": 10093,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/review_reconciliation.rs"
    ],
    "regions": [
     {
      "symbol": "completion_is_independent_of_delivery_order_and_subject_scope",
      "start": 110,
      "end": 198,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "exit_publishes_artifacts_recorded_before_the_exit",
      "start": 199,
      "end": 240,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stop_settlement_preserves_a_never_started_invocation",
      "start": 241,
      "end": 272,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00047",
   "role": "target",
   "path": "crates/workloop-store/tests/store_concurrency.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "be8c1ea6fdf7d7db5ebee9751ed9ee3dcd84646e",
   "size": 7271,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/store_concurrency.rs"
    ],
    "regions": [
     {
      "symbol": "second_connection_waits_for_write_lock",
      "start": 12,
      "end": 44,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "read_then_write_waits_before_observing_a_snapshot",
      "start": 45,
      "end": 87,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "deferred_read_does_not_wait_on_write_lock",
      "start": 88,
      "end": 126,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "preview_fails_fast_under_held_write_lock",
      "start": 127,
      "end": 197,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00048",
   "role": "target",
   "path": "crates/workloop-tui/Cargo.toml",
   "mode": "100644",
   "kind": "blob",
   "blob": "821b0411af3784f39700beaa70a3d89fb3c40b3f",
   "size": 351,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/Cargo.toml"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00049",
   "role": "target",
   "path": "crates/workloop-tui/src/bin/taisan-tui.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "b507ac3097bebf4cbf8b1e194b2695665c744798",
   "size": 64,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/bin/taisan-tui.rs",
     "Four-line shared-main include wrapper; primary/legacy branding contract assertions in branding.rs"
    ],
    "regions": [
     {
      "start": 4,
      "end": 6,
      "symbol": "fn main() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00050",
   "role": "target",
   "path": "crates/workloop-tui/src/board.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "bad543c02362ff5b8578231bed4d706310235298",
   "size": 2829,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/board.rs"
    ],
    "regions": [
     {
      "start": 53,
      "end": 84,
      "symbol": "pub fn from_queries(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 85,
      "end": 91,
      "symbol": "pub fn order(&self) -> Vec<WorkId> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00051",
   "role": "target",
   "path": "crates/workloop-tui/src/judgment.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "c6464c184b1e20cc2f174655aa88d08d4e0b7d2c",
   "size": 4895,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/judgment.rs"
    ],
    "regions": [
     {
      "start": 18,
      "end": 48,
      "symbol": "pub fn distinct_findings(detail: &WorkDetailView) -> impl Iterator<Item = &str> + '_ {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 49,
      "end": 76,
      "symbol": "pub fn from_detail(detail: &WorkDetailView) -> Option<Self> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 77,
      "end": 85,
      "symbol": "pub fn covers(&self, finding_ids: &[String]) -> bool {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 86,
      "end": 98,
      "symbol": "pub fn choose(&mut self, finding_id: &str, choice: FindingChoice) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 99,
      "end": 117,
      "symbol": "pub fn command_body(&self) -> Result<CommandBody, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00052",
   "role": "target",
   "path": "crates/workloop-tui/src/lib.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "53dbbe8dc2ab1a1261633d1981d270a6b636f942",
   "size": 628,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/lib.rs"
    ],
    "regions": []
   }
  },
  {
   "id": "target-F00053",
   "role": "target",
   "path": "crates/workloop-tui/src/main.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "1d6717b30ef6b08a6e4d3015aebde2948428a2c1",
   "size": 10288,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/main.rs"
    ],
    "regions": [
     {
      "start": 34,
      "end": 40,
      "symbol": "fn now() -> i64 {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 41,
      "end": 58,
      "symbol": "fn usage() -> &'static str {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 59,
      "end": 66,
      "symbol": "pub(crate) fn main() {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 67,
      "end": 109,
      "symbol": "fn run(args: Args) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 110,
      "end": 229,
      "symbol": "fn dispatch<S: Store>(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 230,
      "end": 233,
      "symbol": "fn print_decision(decision: &CommandDecision) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 234,
      "end": 240,
      "symbol": "fn kind_of(req: &workloop_core::command::CommandRequest) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 241,
      "end": 254,
      "symbol": "fn parse_choice(s: &str) -> Result<FindingChoice, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 255,
      "end": 264,
      "symbol": "fn resolve_target<S: Store>(session: &TuiSession<'_, S>, target: &str) -> Result<WorkId, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00054",
   "role": "target",
   "path": "crates/workloop-tui/src/render.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "cc4241c77478320d72f43c278063b35789dc813c",
   "size": 7971,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/render.rs"
    ],
    "regions": [
     {
      "start": 15,
      "end": 29,
      "symbol": "fn phase_name(phase: &WorkPhase) -> &'static str {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 30,
      "end": 42,
      "symbol": "fn cell(text: &str) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 43,
      "end": 108,
      "symbol": "pub fn board_frame(board: &Board, selected: Option<&WorkId>) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 109,
      "end": 149,
      "symbol": "pub fn detail_frame(detail: &WorkDetailView) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 150,
      "end": 166,
      "symbol": "pub fn sources_frame(sources: &[SourceStatus]) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 167,
      "end": 183,
      "symbol": "pub fn draft_frame(draft: &JudgmentDraft) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 184,
      "end": 200,
      "symbol": "pub fn preview_frame(preview: &ActionPreview) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 201,
      "end": 227,
      "symbol": "pub fn decision_frame(decision: &CommandDecision) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 228,
      "end": 235,
      "symbol": "pub fn pending_frame(idempotency_key: &str, head_hint: Option<&PrHeadSha>) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00055",
   "role": "target",
   "path": "crates/workloop-tui/src/session.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "af38cce79302a1dd5749e45d8d6b32fb71db1d99",
   "size": 23588,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/session.rs"
    ],
    "regions": [
     {
      "start": 82,
      "end": 98,
      "symbol": "pub fn new(store: &'a S, config: WorkloopConfig, actor: Actor) -> Self {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 99,
      "end": 102,
      "symbol": "pub fn store(&self) -> &'a S {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 103,
      "end": 106,
      "symbol": "pub fn board(&self) -> Option<&Board> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 107,
      "end": 111,
      "symbol": "pub fn detail(&self) -> Option<&WorkDetailView> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 112,
      "end": 115,
      "symbol": "pub fn detail_sources(&self) -> &[SourceStatus] {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 116,
      "end": 119,
      "symbol": "pub fn draft(&self) -> Option<&JudgmentDraft> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 120,
      "end": 124,
      "symbol": "pub fn staged(&self) -> Option<&CommandRequest> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 125,
      "end": 128,
      "symbol": "pub fn pending(&self) -> Option<&CommandRequest> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 129,
      "end": 132,
      "symbol": "pub fn actor(&self) -> &Actor {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 133,
      "end": 145,
      "symbol": "fn next_key(&mut self) -> String {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 146,
      "end": 183,
      "symbol": "pub fn refresh(&mut self, now_unix_seconds: i64) -> Result<&Board, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 184,
      "end": 237,
      "symbol": "pub fn open(&mut self, work_id: &WorkId, now_unix_seconds: i64) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 238,
      "end": 259,
      "symbol": "pub fn begin_judgment(&mut self) -> Result<&JudgmentDraft, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 260,
      "end": 266,
      "symbol": "pub fn choose(&mut self, finding_id: &str, choice: FindingChoice) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 267,
      "end": 277,
      "symbol": "fn request(&mut self, body: CommandBody) -> CommandRequest {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 278,
      "end": 289,
      "symbol": "pub fn stage_judgment(&mut self) -> Result<&CommandRequest, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 290,
      "end": 336,
      "symbol": "pub fn stage_action(&mut self, action: &Action) -> Result<&CommandRequest, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 337,
      "end": 349,
      "symbol": "fn ensure_staged(&mut self) -> Result<(), String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 350,
      "end": 366,
      "symbol": "pub fn preview(&mut self, now_unix_seconds: i64) -> Result<ActionPreview, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 367,
      "end": 409,
      "symbol": "pub fn submit(&mut self, now_unix_seconds: i64) -> Result<SubmitOutcome, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 410,
      "end": 416,
      "symbol": "pub fn note_lost(&mut self, request: CommandRequest) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 417,
      "end": 459,
      "symbol": "pub fn recover(&mut self, now_unix_seconds: i64) -> Result<RecoverOutcome, String> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 460,
      "end": 522,
      "symbol": "pub fn recover_by_id(",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 523,
      "end": 560,
      "symbol": "fn on_decision(&mut self, decision: &CommandDecision, now_unix_seconds: i64) {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 561,
      "end": 565,
      "symbol": "pub fn compose(&mut self, body: CommandBody) -> CommandRequest {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     },
     {
      "start": 566,
      "end": 571,
      "symbol": "pub fn open_execution(&self) -> Option<ExecutionId> {",
      "scope": "Body/guard/effect review; imports/type/trait definitions also reviewed; test-only helper boilerplate not separate requirements."
     }
    ]
   }
  },
  {
   "id": "target-F00056",
   "role": "target",
   "path": "crates/workloop-tui/tests/branding.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "3fd120a57e468b47f49b1100121b9b15ca6ed7c4",
   "size": 491,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/tests/branding.rs"
    ],
    "regions": [
     {
      "symbol": "primary_and_legacy_commands_expose_taisan_help",
      "start": 4,
      "end": 15,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00057",
   "role": "target",
   "path": "crates/workloop-tui/tests/tui_surface.rs",
   "mode": "100644",
   "kind": "blob",
   "blob": "844ff9040fd5025275fa2407377fcebb446d315f",
   "size": 53752,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/tests/tui_surface.rs"
    ],
    "regions": [
     {
      "symbol": "board_columns_derive_from_queries",
      "start": 168,
      "end": 214,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "tui_commands_record_tui_origin",
      "start": 215,
      "end": 252,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "session_preview_is_advisory",
      "start": 253,
      "end": 277,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "judge_findings_flow_end_to_end",
      "start": 278,
      "end": 325,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "incomplete_draft_cannot_submit",
      "start": 326,
      "end": 362,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stale_head_rejection_requeries_and_clears_choices",
      "start": 363,
      "end": 413,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "stale_work_version_rejection_requeries",
      "start": 414,
      "end": 451,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "lost_submit_recovers_recorded_decision",
      "start": 452,
      "end": 479,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "lost_submit_replays_identical_request",
      "start": 480,
      "end": 510,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "submit_refused_while_result_unknown",
      "start": 511,
      "end": 536,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "submit_local_fault_is_not_unknown",
      "start": 537,
      "end": 605,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "lost_submit_from_ambiguous_fault_is_unknown",
      "start": 606,
      "end": 668,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "lost_submit_when_result_is_unreadable_is_unknown",
      "start": 669,
      "end": 744,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "disconnect_changes_no_execution_truth",
      "start": 745,
      "end": 790,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "local_refusals_never_reach_work",
      "start": 791,
      "end": 829,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "new_findings_on_same_head_invalidate_draft",
      "start": 830,
      "end": 893,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "console_eof_and_malformed_input",
      "start": 936,
      "end": 982,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "lost_submit_recovers_by_command_id",
      "start": 983,
      "end": 1009,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "board_numbers_match_open_order",
      "start": 1010,
      "end": 1047,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "duplicate_findings_share_one_decision_slot",
      "start": 1048,
      "end": 1093,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "detail_frame_numbers_match_pick_slots",
      "start": 1121,
      "end": 1162,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "console_pick_uses_displayed_numbers_with_duplicates",
      "start": 1163,
      "end": 1222,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "detail_frame_shows_recorded_evidence",
      "start": 1223,
      "end": 1245,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "accepted_submit_refreshes_open_detail",
      "start": 1246,
      "end": 1277,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "recover_by_id_rejects_unrelated_command_id",
      "start": 1278,
      "end": 1325,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "new_judgment_clears_staged_request",
      "start": 1326,
      "end": 1350,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "empty_findings_under_unread_source_is_not_clean",
      "start": 1351,
      "end": 1374,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "moved_snapshot_clears_staged_request",
      "start": 1375,
      "end": 1421,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "session_is_generic_over_the_store_port",
      "start": 1422,
      "end": 1453,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     },
     {
      "symbol": "store_faults_surface_as_errors",
      "start": 1454,
      "end": 1472,
      "scope": "definition and assertion expressions; unrelated reusable setup excluded"
     }
    ]
   }
  },
  {
   "id": "target-F00058",
   "role": "target",
   "path": "docs/adapter-boundaries.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "49dda7e9ca32f02833e1def7ab7536d2458df2b8",
   "size": 32452,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/adapter-boundaries.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# Selector and worker adapter boundaries",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 17,
      "heading": "## Goal",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 18,
      "end": 29,
      "heading": "## Ownership matrix",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 30,
      "end": 44,
      "heading": "### Selector contract",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 45,
      "end": 70,
      "heading": "### Worker contract",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 71,
      "end": 76,
      "heading": "## Separation of control surfaces and adapters",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 77,
      "end": 89,
      "heading": "## Required contract tests",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 90,
      "end": 93,
      "heading": "## Non-goals",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 94,
      "end": 97,
      "heading": "## Assessment policy from #51",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 98,
      "end": 103,
      "heading": "## ADR-001: Core authorizes publication; GitHub integration performs it",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 104,
      "end": 112,
      "heading": "### Alternatives and rationale",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 113,
      "end": 129,
      "heading": "### Stage responsibility and permission matrix",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 130,
      "end": 141,
      "heading": "### Worker pipeline disposition",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 142,
      "end": 153,
      "heading": "### Durable effects and partial success",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 154,
      "end": 165,
      "heading": "### Evidence validity and lifecycle decisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 166,
      "end": 171,
      "heading": "### Permissions, reviewers, and managed bot events",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 172,
      "end": 181,
      "heading": "### GitHub authentication belongs to the integration",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 182,
      "end": 189,
      "heading": "### Impact and implementation evidence",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 190,
      "end": 191,
      "heading": "#17 must implement real publication/observation and test partial push/PR creation, duplicate delivery, ambiguous reply/ready results, stale-head races, and restart reconciliation against the same port. #21 / PR #31 remain valid evidence for their narrow classifier invariant, not proof that real publication, all converge postconditions, or bot event filtering work. #36 maps these gaps and the two Worker implementations into implementation units and live S1 evidence. This decision authorizes no runtime implementation or E2E execution by itself.",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 192,
      "end": 201,
      "heading": "### Design aspects and remaining work",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00059",
   "role": "target",
   "path": "docs/agent-interface.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "3f358f53edc5012516eb4c44509acb708da644d7",
   "size": 8000,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/agent-interface.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# CLI-backed agent interface requirements",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 8,
      "heading": "## Status and release scope",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 9,
      "end": 24,
      "heading": "## Workflow and authority",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 25,
      "end": 32,
      "heading": "## CLI result and failure requirements",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 33,
      "end": 38,
      "heading": "## Installation and invocation deliverables",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 39,
      "end": 55,
      "heading": "## Acceptance and evidence",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00060",
   "role": "target",
   "path": "docs/architecture.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7f9bb6350d2199be80ee5662ff0e49134f8c1022",
   "size": 16956,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/architecture.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# Architecture and interface boundaries",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 16,
      "heading": "## Responsibilities",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 17,
      "end": 62,
      "heading": "## Module map",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 63,
      "end": 68,
      "heading": "### Repository host exchangeability",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 69,
      "end": 79,
      "heading": "## Command contract",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 80,
      "end": 97,
      "heading": "### Stage contracts are executor-independent",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 98,
      "end": 103,
      "heading": "## Work and execution",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 104,
      "end": 109,
      "heading": "## Built-in work queue",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 110,
      "end": 126,
      "heading": "## Adapters",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 127,
      "end": 134,
      "heading": "## Recovery rules",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 135,
      "end": 137,
      "heading": "## Installation boundary",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00061",
   "role": "target",
   "path": "docs/decisions.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "55eacbc6063eae28a08e2d94df06b94ed6258dbf",
   "size": 15210,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/decisions.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# Decisions and open questions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 44,
      "heading": "## Working decisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 45,
      "end": 48,
      "heading": "## Identity/storage decisions from #50",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 49,
      "end": 52,
      "heading": "## Assessment policy decisions from #51",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 53,
      "end": 61,
      "heading": "## Questions to resolve through a spike",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00062",
   "role": "target",
   "path": "docs/evidence-gates.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b9406d8c68ef09265aa7131cf7f4c3c3bd0691c7",
   "size": 37161,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# Evidence, invariant checks, and review gates",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 26,
      "heading": "## Owner-approved lanes and caller flow",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 27,
      "end": 124,
      "heading": "## Evidence schema and validity",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 125,
      "end": 186,
      "heading": "## Immutable review snapshots and finding identity",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 187,
      "end": 196,
      "heading": "### Same-head growth and exact-set guards",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 197,
      "end": 256,
      "heading": "## Stage effects and reconciliation",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 257,
      "end": 264,
      "heading": "## CI identity and requirement snapshot",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 265,
      "end": 335,
      "heading": "## Structural storage and transactions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 336,
      "end": 365,
      "heading": "## Outcome, counters, and repair",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 366,
      "end": 385,
      "heading": "## Literal acceptance and rejection cases",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 386,
      "end": 399,
      "heading": "## Six aspects and verification scope",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00063",
   "role": "target",
   "path": "docs/glossary.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "e071144c9e9b6a21c29df8f07753c21fc829d241",
   "size": 7840,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/glossary.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# Glossary",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 25,
      "heading": "## Units of work",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 26,
      "end": 46,
      "heading": "## Requests, facts, and views",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 47,
      "end": 64,
      "heading": "## Execution ownership",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 65,
      "end": 68,
      "heading": "## External identities",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 69,
      "end": 71,
      "heading": "## Repository host terminology",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00064",
   "role": "target",
   "path": "docs/identity-storage.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d7459b80609fbe12a0869e27afdc2876fbb390a5",
   "size": 42337,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# Durable identity, storage, and admission design",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 21,
      "heading": "## Usage and authority",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 22,
      "end": 122,
      "heading": "## Concrete identity model",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 123,
      "end": 222,
      "heading": "## Storage shape and invariant ownership",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 223,
      "end": 230,
      "heading": "## Serialization and command replay",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 231,
      "end": 246,
      "heading": "## Admission, overlap, and source revisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 247,
      "end": 263,
      "heading": "## Maintenance cutover and legacy reference",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 264,
      "end": 288,
      "heading": "### Literal examples and limits",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 289,
      "end": 311,
      "heading": "## Caller and persisted-data change inventory",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 312,
      "end": 326,
      "heading": "## Verification plan and six aspects",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 327,
      "end": 333,
      "heading": "## Alternatives and owner review",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00065",
   "role": "target",
   "path": "docs/implementation-plan.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "5ca30bf639dfd0649e244134a943a795c21b7b69",
   "size": 45519,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/implementation-plan.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# taisan v0.1 implementation plan",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 18,
      "heading": "## Scope and reviewed decisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 19,
      "end": 22,
      "heading": "## Detailed identity/storage design",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 23,
      "end": 26,
      "heading": "## Detailed evidence and gate design",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 27,
      "end": 41,
      "heading": "## Revisions and evidence baseline",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 42,
      "end": 53,
      "heading": "### Evidence classification",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 54,
      "end": 72,
      "heading": "## Existing work and dependency disposition",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 73,
      "end": 119,
      "heading": "## Retained audit requirement inventory",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 120,
      "end": 138,
      "heading": "## Dependent delivery units",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 139,
      "end": 144,
      "heading": "### Required infrastructure and runtime setup",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 145,
      "end": 148,
      "heading": "## Representative-scenario coverage",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 149,
      "end": 163,
      "heading": "### S1 setup and expected path",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 164,
      "end": 184,
      "heading": "### S1 branches and abnormal inputs",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 185,
      "end": 204,
      "heading": "### Remaining lifecycle acceptance rows",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 205,
      "end": 220,
      "heading": "## CLI and skill acceptance coverage",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 221,
      "end": 231,
      "heading": "## Six design aspects and remaining decisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 232,
      "end": 241,
      "heading": "## Human review and follow-up authorization",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00066",
   "role": "target",
   "path": "docs/lifecycle.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "b6541547de9ee98ccb7797fb9f541527435aa056",
   "size": 25702,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/lifecycle.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 6,
      "heading": "# Work lifecycle and built-in queue",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 7,
      "end": 22,
      "heading": "## Caller behavior",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 23,
      "end": 100,
      "heading": "## Authoritative records",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 101,
      "end": 118,
      "heading": "## Queue admission",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 119,
      "end": 155,
      "heading": "## Transition rules",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 156,
      "end": 163,
      "heading": "## Artifact and publication assessment",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 164,
      "end": 169,
      "heading": "## Completion, review, and Attention",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 170,
      "end": 179,
      "heading": "## Crash and concurrency rules",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 180,
      "end": 182,
      "heading": "## Deliberately deferred",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00067",
   "role": "target",
   "path": "docs/module-contracts.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "7ed49049bc8bcb1a9ce32e4486e5e2d6f5ab64da",
   "size": 51227,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/module-contracts.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 4,
      "heading": "# Module contracts",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 5,
      "end": 8,
      "heading": "## Evidence and lane-policy refinement",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 9,
      "end": 12,
      "heading": "## Caller view",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 13,
      "end": 434,
      "heading": "## Type sketch",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 435,
      "end": 468,
      "heading": "## Ports and all module connections",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 469,
      "end": 477,
      "heading": "## Built-in queue admission",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 478,
      "end": 492,
      "heading": "## Operator review judgment",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 493,
      "end": 504,
      "heading": "## Explicit retry after a failed Execution",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 505,
      "end": 512,
      "heading": "## Worker result and PR review",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 513,
      "end": 529,
      "heading": "## State store model",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 530,
      "end": 538,
      "heading": "## Implementation-facing decisions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00068",
   "role": "target",
   "path": "docs/name-migration.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "bd3cbc8c4207a0d9f45b833e87c10f16ef8dcba6",
   "size": 3141,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/name-migration.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 16,
      "heading": "# Name migration to taisan",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 17,
      "end": 26,
      "heading": "## Update existing installations",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 27,
      "end": 39,
      "heading": "## Update Git remotes",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00069",
   "role": "target",
   "path": "docs/principles.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "1b7de9ddbb8752c0c6de058b1551dff643cae447",
   "size": 3074,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/principles.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 15,
      "heading": "# Principles",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 16,
      "end": 24,
      "heading": "## Intended workflow",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00070",
   "role": "target",
   "path": "docs/provenance.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "ec9181ea8cfd18604bef9880b97d6690f98dd227",
   "size": 9720,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/provenance.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 15,
      "heading": "# Provenance",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 16,
      "end": 33,
      "heading": "## Source systems",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 34,
      "end": 54,
      "heading": "## Provenance versus dependency",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 55,
      "end": 111,
      "heading": "## Implementation status",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 112,
      "end": 118,
      "heading": "## Design aspects: applicability, rationale, and open questions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 119,
      "end": 132,
      "heading": "### State transitions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 133,
      "end": 145,
      "heading": "### Failure semantics",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 146,
      "end": 156,
      "heading": "### Trust boundaries",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 157,
      "end": 167,
      "heading": "### Identity invariants",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 168,
      "end": 177,
      "heading": "### Information preservation",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 178,
      "end": 186,
      "heading": "### Resource bounds",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00071",
   "role": "target",
   "path": "docs/repository-host-contract.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "d174d5021b048a7e5d3d07fd758a88a3d7a239d5",
   "size": 17567,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/repository-host-contract.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# Repository host adapter contract",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 8,
      "heading": "## ADR-002: select a conforming host adapter without changing Core",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 9,
      "end": 17,
      "heading": "### Alternatives",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 18,
      "end": 27,
      "heading": "## Identity and configuration binding",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 28,
      "end": 56,
      "heading": "## Common semantics and optional capabilities",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 57,
      "end": 77,
      "heading": "## Typed boundary and adapter obligations",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 78,
      "end": 92,
      "heading": "## GitHub and GitLab mapping pressure test",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 93,
      "end": 103,
      "heading": "## Verification, release scope, and migration",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00072",
   "role": "target",
   "path": "docs/source-workflow-audit.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "f22aced1d169dde385b70e01193efb074bf05fa2",
   "size": 56908,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/source-workflow-audit.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 34,
      "heading": "# Source Workflow Audit",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 35,
      "end": 72,
      "heading": "## Inspected revisions and scope",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 73,
      "end": 83,
      "heading": "## Behavior map",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 84,
      "end": 101,
      "heading": "### 1. Selection — which issues can start",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 102,
      "end": 119,
      "heading": "### 2. Admission — turning a request into tracked work",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 120,
      "end": 137,
      "heading": "### 3. Queueing — what runs next",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 138,
      "end": 155,
      "heading": "### 4. Dispatch — starting work",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 156,
      "end": 175,
      "heading": "### 5. Execution — the worker attempt",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 176,
      "end": 193,
      "heading": "### 6. Verification — did the attempt produce a sound result",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 194,
      "end": 212,
      "heading": "### 7. Publication — making the result visible",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 213,
      "end": 235,
      "heading": "### 8. Review / fix — convergence on a PR head",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 236,
      "end": 257,
      "heading": "### 9. Recovery — returning from failure",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 258,
      "end": 274,
      "heading": "### 10. Attention — surfacing what needs a human",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 275,
      "end": 291,
      "heading": "### 11. Environment setup — making a host able to run work",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 292,
      "end": 298,
      "heading": "## Incident audit",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 299,
      "end": 322,
      "heading": "### Card / run disagreement",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 323,
      "end": 343,
      "heading": "### Duplicate dispatch",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 344,
      "end": 359,
      "heading": "### Uncertain stops",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 360,
      "end": 377,
      "heading": "### Missing PR publication",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 378,
      "end": 397,
      "heading": "### Stale-head reviews",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 398,
      "end": 419,
      "heading": "### Stalled convergence",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 420,
      "end": 428,
      "heading": "## Design evidence issues",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 429,
      "end": 454,
      "heading": "## Required behavior vs personal-environment mechanisms",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 455,
      "end": 459,
      "heading": "## Design aspects",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 460,
      "end": 475,
      "heading": "### State transitions",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 476,
      "end": 491,
      "heading": "### Failure semantics",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 492,
      "end": 503,
      "heading": "### Trust boundaries",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 504,
      "end": 517,
      "heading": "### Identity invariants",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 518,
      "end": 529,
      "heading": "### Information preservation",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 530,
      "end": 543,
      "heading": "### Resource bounds",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 544,
      "end": 564,
      "heading": "## Evidence classification",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00073",
   "role": "target",
   "path": "docs/v0.1.md",
   "mode": "100644",
   "kind": "blob",
   "blob": "2e19806730b25f7f3c3e2325abea812bc4bd894b",
   "size": 31081,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/v0.1.md"
    ],
    "regions": [
     {
      "start": 1,
      "end": 2,
      "heading": "# v0.1: built-in queue, CLI, and agent skills",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 3,
      "end": 8,
      "heading": "## Goal",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 9,
      "end": 22,
      "heading": "## Vertical slice",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 23,
      "end": 28,
      "heading": "## Configured assessment lanes",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 29,
      "end": 52,
      "heading": "## Slice 0: core-truth harness",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 53,
      "end": 79,
      "heading": "## Acceptance scenarios",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 80,
      "end": 83,
      "heading": "## Representative live acceptance scenario",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 84,
      "end": 97,
      "heading": "### Scenario selection and rationale",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 98,
      "end": 108,
      "heading": "### Scenario definition",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 109,
      "end": 129,
      "heading": "### Expected path",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 130,
      "end": 136,
      "heading": "### External dependencies",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 137,
      "end": 149,
      "heading": "### Abnormal-input matrix",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 150,
      "end": 153,
      "heading": "### Completion boundary",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 154,
      "end": 162,
      "heading": "### Isolated environment, rerun, and cleanup",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 163,
      "end": 174,
      "heading": "### Execution evidence validity",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 175,
      "end": 187,
      "heading": "### Design aspects of this scenario",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 188,
      "end": 197,
      "heading": "## Worker adapter interchangeability",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     },
     {
      "start": 198,
      "end": 208,
      "heading": "## Build order",
      "scope": "Normative requirement/ownership/failure/evidence assertions reviewed. Repeated illustrative formatting/diagram layout has no additional lifecycle requirement."
     }
    ]
   }
  },
  {
   "id": "target-F00074",
   "role": "target",
   "path": "opencode.json",
   "mode": "100644",
   "kind": "blob",
   "blob": "84766cfa71c049b2cdd38e8f13b3bac8d2bef1a0",
   "size": 552
  },
  {
   "id": "target-F00075",
   "role": "target",
   "path": "scripts/worker/setup.sh",
   "mode": "100755",
   "kind": "blob",
   "blob": "c3fdc4382afaa15bee865ba640fb95915d98a619",
   "size": 1190,
   "review_evidence": {
    "inspected_evidence": [
     "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:scripts/worker/setup.sh"
    ],
    "regions": []
   }
  }
 ],
 "behaviors": [
  {
   "id": "ops-B001",
   "trigger": "live converge sweep",
   "preconditions": "configured positive stall threshold; structured progress origin available",
   "inputs_outputs": "converge.json/round artifacts/start stamp → progress epoch, source, episode and Attention",
   "owners": {
    "decision": "sweep records; projection displays",
    "effect": "sweep records; projection displays"
   },
   "durable_state": "converge_progress_at/source; converge_stall {at,progress_at,episode,source}",
   "identities": "run + progress epoch",
   "transitions": "progress advances → clears episode; stale progress → once-per-episode notice; proven timeout ends episode",
   "failure_semantics": "unprovable origin produces no stall; log/CPU/PID activity never proves progress; save errors are swallowed",
   "concurrency_restart": "restart reconstructs from durable progress; notification dedup depends on episode",
   "bounds": "source default 1800s; 0 disables; not a target-policy recommendation",
   "human_action": "inspect stall; no automatic stop solely for no-progress",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:6726-6809"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin-milestone.py:13877-13949"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/coding-worker.md:2137-2227"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_coding_worker_lane.py:11242-11289"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue326 queried2026-11-07 stateOPEN updated2026-11-03; detailed owner decisions describe structured-progress notice,120min per-run deadline,one-per-repo/PR/head successor"
    }
   ],
   "evidence_classification": "observed implementation + tests inspected, not executed",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Structured per-Work no-progress is distinct from deadline",
    "proposed_test": "same work with growing log but unchanged semantic progress diagnoses stall without stopping; new semantic progress rearms"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B002",
   "trigger": "converge deadline reached then terminal-card sweep",
   "preconditions": "proven start; positive deadline; proven process-group stop; PR open/current unchanged; no wait/live equivalent work; allowlist",
   "inputs_outputs": "timeout record → terminal card + at most one successor per repo/PR/head",
   "owners": {
    "decision": "dispatch owns timeout/re-enqueue; board adapter applies writes",
    "effect": "dispatch owns timeout/re-enqueue; board adapter applies writes"
   },
   "durable_state": "timeout_outcome/timed_out_at/timeout_stopped_pid/timeout_reenqueue; request and successor key",
   "identities": "(repository,PR,head) episode; deterministic converge-timeout hash key",
   "transitions": "running → proven stopped → durable timed_out → blocked → settled → bounded successor",
   "failure_semantics": "unknown live head/allowlist/ledger is transient unresolved, known changed head/repo forbidden is permanent; stopped natural exit settles normally",
   "concurrency_restart": "deterministic successor key plus scans of prior records/requests prevent equivalent duplicate cards; attempt-save failures leave disk count unchanged",
   "bounds": "one automatic successor per source episode; _REPLAN_MAX_ATTEMPTS=3 transient probes; target defaults require owner decision",
   "human_action": "human wait never auto-released; permanent refusal requires new operator decision",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:6812-6835"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin-milestone.py:13588-13705,13952-14266"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/coding-worker.md:403-467"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_coding_worker_lane.py:11291-11513"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue326 queried2026-11-07 stateOPEN updated2026-11-03; detailed owner decisions describe structured-progress notice,120min per-run deadline,one-per-repo/PR/head successor"
    }
   ],
   "evidence_classification": "observed implementation + tests inspected; local history merge 6367813/2fe4674/48c8fd6 inspected by log",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Safe bounded timeout recovery survives replacement attempts",
    "proposed_test": "repeat equivalent failure across successor Execution exhausts one Work recovery budget; unknown read retries same identity; failed budget persistence cannot reset cap"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B003",
   "trigger": "deadline/cancel",
   "preconditions": "verified PID identity/start stamp; liveness positively known",
   "inputs_outputs": "record → SIGTERM/grace/SIGKILL → stopped/already_dead/unverified/unknown",
   "owners": {
    "decision": "supervisor signals; caller persists intent",
    "effect": "supervisor signals; caller persists intent"
   },
   "durable_state": "timeout_stop_attempted_at then timeout outcome",
   "identities": "recorded process group and exact start stamp",
   "transitions": "live → intent → signal → group death proof; uncertain holds",
   "failure_semantics": "known remaining defect: caller _stamp_stop_attempt catches OSError then signal still proceeds; unknown ps after signal holds; dead leader is not group death",
   "concurrency_restart": "callback immediately before signal; restart marker distinguishes forced stop; current persistence failure violates intended ordering",
   "bounds": "source grace 15s + 5s confirmation; these are source facts only",
   "human_action": "uncertain stop requires investigation, no replacement writer",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:7107-7212"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin-milestone.py:13976-13994"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue326 queried2026-11-07 stateOPEN updated2026-11-03; detailed owner decisions describe structured-progress notice,120min per-run deadline,one-per-repo/PR/head successor"
    }
   ],
   "evidence_classification": "observed implementation + known defect",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Persist stop intent before any signal; group-wide stop proof",
    "proposed_test": "inject intent-write failure and prove no signal; leader death with live child holds; probe outage never frees reservation"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B004",
   "trigger": "worker run_timed_out result",
   "preconditions": "trusted out_dir containment; actual timed_out record",
   "inputs_outputs": "patch/workspace/result and staged-merge git probes → partial evidence; possible finishable staged merge",
   "owners": {
    "decision": "coordinator extracts; git probes establish machine facts",
    "effect": "coordinator extracts; git probes establish machine facts"
   },
   "durable_state": "partial evidence merge keyed run_dir; consumed/finish_verify/probe/task anchors retained",
   "identities": "run_dir, base_revision, repo, PR, staged HEAD/MERGE_HEAD",
   "transitions": "timeout → preserved partial evidence; rederive preserves consumed proof; no arbitrary worker prose promoted",
   "failure_semantics": "outside-root path rejected; unprobeable checkout is unknown; linked worktree cannot prove staged clone resolution",
   "concurrency_restart": "idempotent evidence merge prevents downgrade or lost consume provenance",
   "bounds": "evidence merge retains last 50 entries; git probe 30s; assess target retention independently",
   "human_action": "operator verifies and finishes staged resolution through supported path",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:6838-7104"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/cw-round-prune.md"
    }
   ],
   "evidence_classification": "observed selected implementation; full finish command pending",
   "proposed_target": {
    "owner_issue": "issue54",
    "section_or_ac": "Preserved timed-out WIP is recoverable evidence, not disposable output",
    "proposed_test": "timeout retains exact patch/git objects; replay evidence merge retains consumed/verification fields; outside-root refs rejected"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B005",
   "trigger": "round completed/superseded or remote staging accepted",
   "preconditions": "record/root binding; completion marker; safe git state; no open files; shared lock",
   "inputs_outputs": "artifact candidates → removed/retained/warnings byte accounting + audit",
   "owners": {
    "decision": "prune library delegates git/workspace deletion",
    "effect": "prune library delegates git/workspace deletion"
   },
   "durable_state": "round-prune.jsonl; source records/log/findings outside deletion allowlist",
   "identities": "run out_dir + repo + candidate path",
   "transitions": "completed rederivable bulk → removed; dirty/unknown/live → retained",
   "failure_semantics": "known-open and unknown both retain; metadata prune failure does not rewrite successful removal; audit-write errors silently swallowed; scheduled sweep preserves evidence and local work via continuation/sibling/record/git gates; nested ignored build artifacts can be reclaimed while dirty checkout retained. Manual apply snapshots lsof before slow git calls, unlike early-prune final refresh. CLI returns nonzero for errors; cron wrapper prints errors but returns0. These are recorded source boundaries/gaps, not operational success.",
   "concurrency_restart": "nonblocking shared disk reclaim lock; candidate lsof refreshed after slow predicates; repeated pass idempotent",
   "bounds": "allowlist node_modules/generated worktrees/staging clone only; no age/storage policy here",
   "human_action": "dry run; operator inspects retained reasons; do not map pruning mechanics directly into target",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw_round_prune_lib.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw-round-prune.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/cw-round-prune.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_cw_round_prune.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw_run_reclaim_lib.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw-run-reclaim.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw-run-reclaim-run.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-disk-reclaim.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "config/disk-reclaim.yaml.example:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_cw_run_reclaim.py:entire file"
    }
   ],
   "evidence_classification": "observed implementation and full test bodies inspected, not executed; provider cw_run_reclaim_lib pending",
   "proposed_target": {
    "owner_issue": "issue54",
    "section_or_ac": "Allowlisted retention must recheck ownership and preserve evidence",
    "proposed_test": "active writers/unresolved effects/paused WIP/recovery refs protect bodies despite limits; lsof failure retains; metadata failure reports removed plus warning; audit failure observable"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B006",
   "trigger": "periodic plan/apply",
   "preconditions": "terminal status allowlist + grace + correct recorded root + same uid noninteractive identity",
   "inputs_outputs": "run records + ps/cwd → digest plan; fresh recheck → SIGTERM/audit",
   "owners": {
    "decision": "reaper",
    "effect": "reaper"
   },
   "durable_state": "operations.jsonl; digest-addressed plans",
   "identities": "PID/start stamp/uid + run out_dir attribution",
   "transitions": "terminal aged record → candidate → fresh validated signal; live/unknown → report only",
   "failure_semantics": "missing/corrupt/unknown status/out_dir mismatch never kill; exec/PID recycle/run resurrection refuse",
   "concurrency_restart": "flock; plan SHA256; fresh record/ps at apply",
   "bounds": "source grace 30min; SIGTERM only; latest 200 managed plans; lsof 10s",
   "human_action": "review initial plan; enable paused cron explicitly",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/cw-orphan-reaper.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw_orphan_reaper_lib.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_cw_orphan_reaper.py:entire file"
    }
   ],
   "evidence_classification": "observed full implementation and all tests inspected; no test execution or production verification",
   "proposed_target": {
    "owner_issue": "issue54",
    "section_or_ac": "Orphan process reclamation requires terminal evidence and fresh attribution",
    "proposed_test": "foreign uid/interactive/recycled pid/unknown status never reclaimed; digest mismatch rejected"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B007",
   "trigger": "reeval tick",
   "preconditions": "allowlisted repo; due record; cause is recoverable",
   "inputs_outputs": "recorded stop class + disk/capacity/dependency/current triage facts → hold or bounded retry",
   "owners": {
    "decision": "reeval triggers; existing recovery playbook remains final authority",
    "effect": "reeval triggers; existing recovery playbook remains final authority"
   },
   "durable_state": "evaluations.json last/next checks, attempts, evidence signature",
   "identities": "repo+task/park/incident; deterministic retry-task key",
   "transitions": "temporary hold → proof repaired → bounded new card; spec/manual/unknown remain held",
   "failure_semantics": "unknown not zero; cleared issues_closed park inert; unreadable parks remain unknown; original card/request/park/history preserved",
   "concurrency_restart": "reeval lock + scheduler jurisdiction; attempt persisted before dispatch; final equivalent/pending/capacity guards rechecked",
   "bounds": "items/tick8, source recovery_attempts1/backoff60..480min; not target defaults",
   "human_action": "spec/manual needs human; unknown legacy evidence requires migration/operator decision",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/blocker-reeval.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/blocker-reeval.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/blocker-reeval-run.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "config/blocker-reeval.yaml.example:entire file"
    }
   ],
   "evidence_classification": "documented behavior + CLI inspected; main library/tests pending",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Blocker reevaluation uses present authoritative repair evidence",
    "proposed_test": "same unchanged evidence cannot unblock; attempts committed before effect; repaired capacity still requires final admission; unknown source holds"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B008",
   "trigger": "daily cleanup or explicit apply",
   "preconditions": "blocked terminal card; complete identity/evidence stores; no active/unknown run",
   "inputs_outputs": "all linked issues/PRs and newer-card coverage → retain/skip/archive",
   "owners": {
    "decision": "cleanup library; CLI enforces enabled+apply",
    "effect": "cleanup library; CLI enforces enabled+apply"
   },
   "durable_state": "normalized ledger task identity/reason/time",
   "identities": "repo + full issue/PR sets; newer covering card",
   "transitions": "obsolete terminal card → archived history only",
   "failure_semantics": "corrupt one store file aborts pass; lookup failures/budget exhaustion skip; unknown status active; partial archive reports error",
   "concurrency_restart": "fresh liveness per card; current newer-card/gh status rechecked before archive",
   "bounds": "source lookups200/liveness refresh200; ledger512; no target defaults implied",
   "human_action": "explicit enable after dry-run; source remote archived recovery uses supported recollect",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/kanban-attention-cleanup.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-attention-cleanup.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-attention-cleanup-run.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "config/kanban-attention-cleanup.yaml.example:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban_dev_status_lib.py:3253-4017 full build_attention current/history,unknown/actionable/dedup/owner projection"
    }
   ],
   "evidence_classification": "documented behavior + entrypoints inspected; main library/tests pending",
   "proposed_target": {
    "owner_issue": "issue53",
    "section_or_ac": "Attention cleanup cannot erase active or uncertain work",
    "proposed_test": "unknown linked record never hides Attention; one malformed reference cannot let partial issue set appear complete"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B009",
   "trigger": "operator reclaim command",
   "preconditions": "MERGED + merge SHA and recorded same-repo PR binding",
   "inputs_outputs": "PR facts + run stores + card/process → done sync, identity-strict SIGTERM, ledger",
   "owners": {
    "decision": "merged reclaim library",
    "effect": "merged reclaim library"
   },
   "durable_state": "repo#PR#mergeSHA ledger only when complete",
   "identities": "recorded repo/PR/task/run/PID/start",
   "transitions": "confirmed merge + bound active record → card done then safe process handling; partial stays retryable",
   "failure_semantics": "any target store scan failure holds all; unknown liveness/link/save failure partial not completed ledger; verified remaining failure class: _default_terminate returns normally when _terminate_recorded_process declines identity or swallows kill OSError, so caller marks process settled/saves terminal and may ledger success without a delivered signal. See dml9589-9611.",
   "concurrency_restart": "idempotent ledger; terminal run saved only after process verdict known",
   "bounds": "one recovery per exact merged revision",
   "human_action": "default dry run/disabled; no scheduled installer; unlinkable requires operator",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/merged-pr-reclaim.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/merged-pr-reclaim.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/merged_pr_reclaim_lib.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_merged_pr_reclaim.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin_milestone_lib.py:9589-9611 full _terminate_recorded_process"
    }
   ],
   "evidence_classification": "Full implementation and all injected-boundary tests inspected, no test execution/live operations",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Merged-PR reconciliation never guesses run/card identity",
    "proposed_test": "merged PR without run binding causes no guessed action; process unknown keeps recovery pending"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B010",
   "trigger": "scan ready cards or explicit unstick",
   "preconditions": "last_failure_error blocker pattern superseded by latest ended-run own text; recurrence priced; ready status",
   "inputs_outputs": "board/card/runs/events → clean/cooldown/stale/fresh/unknown; explicit block then unblock",
   "owners": {
    "decision": "classifier/unstick library; board CLI applies mutations",
    "effect": "classifier/unstick library; board CLI applies mutations"
   },
   "durable_state": "notification state; board events marker",
   "identities": "board/task + error digest + ended_at; local unstick lock",
   "transitions": "stale ready → block → unblock → re-read ready+clear stamp",
   "failure_semantics": "fresh/unknown/cooldown refuse; events unknown refuse; block successful/unblock failed reports current state + manual recovery; scan errors per item isolated",
   "concurrency_restart": "flock protects concurrent escape; fresh show before mutations; no automatic repair in cron",
   "bounds": "source recurrence limit2 permits effectively one untyped escape; no forced fresh-suspect bypass",
   "human_action": "operator invokes unstick; cron only diagnoses/notifies",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban_unstick_lib.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-unstick.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-unstick-run.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-kanban-unstick.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/kanban-unstick.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_kanban_unstick.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue545 queried2026-11-07 stateCLOSED closed2026-11-07; body records 3h blocker_auth stall and directSQL incident escape. Source tool alternative independently inspected."
    }
   ],
   "evidence_classification": "observed implementation + tests fully read, not executed; history 849d9f6/221f09c log inspected",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Stale blocker diagnosis and operator escape preserve partial truth",
    "proposed_test": "old auth failure superseded by proven newer run does not hold forever; unknown latest evidence cannot clear; two-step repair interruption stays explicit"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B011",
   "trigger": "scan --notify or cron",
   "preconditions": "new stale episode",
   "inputs_outputs": "sanitized card fields → message; successful send → dedup stamp",
   "owners": {
    "decision": "notification helper/transport",
    "effect": "notification helper/transport"
   },
   "durable_state": "notified map keyed board/task/error hash/latest-ended-at",
   "identities": "episode key, separate from Work result",
   "transitions": "unsent → attempted → stamp after successful send; failure retains pending",
   "failure_semantics": "send failure preserves keys; corrupt dedup file renotifies; crash after send may duplicate; CLI scan --notify ignores notify.error in exit status while cron exits1",
   "concurrency_restart": "notification map not protected by unstick flock, so simultaneous scans can duplicate; no exactly-once guarantee",
   "bounds": "source field truncation; map retention unbounded in inspected file",
   "human_action": "operator sees failed notification; cannot imply work repaired",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban_unstick_lib.py:notify_findings/load_state/save_state"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-unstick.py:cmd_scan"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban-unstick-run.py:main"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_kanban_unstick.py:NotifyTests/RunWrapperTests"
    }
   ],
   "evidence_classification": "observed implementation + possible CLI failure-reporting gap; no runtime verification",
   "proposed_target": {
    "owner_issue": "issue53",
    "section_or_ac": "Notification dedup never substitutes for lifecycle evidence",
    "proposed_test": "acknowledgement does not resolve underlying Attention; repeated transport failure never settles Work"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B012",
   "trigger": "installer or --check",
   "preconditions": "runtime dependencies installed; canonical owned files",
   "inputs_outputs": "canonical script hashes → drift output; copy/chmod; doctor; paused cron",
   "owners": {
    "decision": "feature installer owns only its canonical files",
    "effect": "feature installer owns only its canonical files"
   },
   "durable_state": "installed copies; state dir; existing cron state",
   "identities": "feature-owned file list; job name; shared provider ownership",
   "transitions": "missing/drift → copy; existing job preserved; unknown cron listing refuses duplicate create",
   "failure_semantics": "unstick installer --check only owned-file drift; dependency missing and doctor failure warn/continue (not verified healthy install)",
   "concurrency_restart": "no overwrite of shared newer dependencies; exact job-name existing detection",
   "bounds": "paused default; source owner-only modes; no target numeric policy",
   "human_action": "doctor + manual scan before explicit cron enable",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-kanban-unstick.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/cw-round-prune.md:Install"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_cw_round_prune.py:test_lane_installer_ships_the_reclaim_provider/test_manual_cli_is_deployed_by_the_prune_installers"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-coding-worker.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-devin-milestone.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-devin-kanban.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-devflow-scheduler.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-blocker-reeval.sh:entire file"
    }
   ],
   "evidence_classification": "observed selected installer and consumer/provider tests; broad installers pending",
   "proposed_target": {
    "owner_issue": "issue52",
    "section_or_ac": "Install ownership and deployed-copy drift are separate from source state",
    "proposed_test": "clean install missing provider refuses activation; drift check covers required runtime dependencies; failed doctor cannot claim operational health"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B013",
   "trigger": "operator lane diagnosis",
   "preconditions": "card/run/wait/event/PR linkage and current-head proof",
   "inputs_outputs": "current records + connectivity + findings → bounded recovery or human escalation",
   "owners": {
    "decision": "operator skill; runtime commands enforce supported subset",
    "effect": "operator skill; runtime commands enforce supported subset"
   },
   "durable_state": "existing run/wait/events; no separate doctor authority",
   "identities": "lane-owned PR, run/card/wait/finding/head",
   "transitions": "transient restored → explicit new work; unresolved finding → fix/judgment; spec ambiguity → human",
   "failure_semantics": "empty parseable wait list not proof no corrupt wait; unbound pr0 alone not ownership; foreign PR never write-capable converge",
   "concurrency_restart": "do not retry during outage; fixing directive survives failed worker; avoid external contradictory actor",
   "bounds": "source per-lane capacities are personal; cross-task doctor deferred target",
   "human_action": "spec/design fork/destructive operations require human; diagnostics route to structural issue rather than unlogged repairs",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": ".agents/skills/lane-doctor/SKILL.md:entire file"
    }
   ],
   "evidence_classification": "documented operator procedure; not production evidence and not executed by audit",
   "proposed_target": {
    "owner_issue": "issue53",
    "section_or_ac": "Human operational recovery needs complete pipeline evidence and ownership",
    "proposed_test": "parseable-empty plus corrupt ledger surfaces unknown; orphan finding on foreign PR cannot dispatch mutation"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B014",
   "trigger": "refinement scan",
   "preconditions": "allowlist; open nonexcluded issue; structured judge proposal validated",
   "inputs_outputs": "title/body/labels → existing labels; spec evidence quote; narrowly verified duplicate/fixed close",
   "owners": {
    "decision": "refinement judge suggests; code validates/applies",
    "effect": "refinement judge suggests; code validates/applies"
   },
   "durable_state": "ledger + append-only history/scans; failed-output dump",
   "identities": "repo/issue; fingerprint of spec comment; daily caps",
   "transitions": "eligible → spec ambiguous/needs-human quarantine; no automatic body rewrite/unquarantine",
   "failure_semantics": "invalid close→needs human; unavailable clone/fetch only blocks relevant close; no matching label fallback to needs-human; comments not judge input",
   "concurrency_restart": "run/writer locks; daily close caps use history not mutable last state; protected running labels cannot change",
   "bounds": "10issues/repo/run;20labelchanges/run;1close/repo/day; latest20 private dumps source facts",
   "human_action": "human makes issue body self-consistent then explicitly removes quarantine",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/issue-refinement.md:entire file"
    }
   ],
   "evidence_classification": "documented behavior; underlying library/tests pending",
   "proposed_target": {
    "owner_issue": "issue50",
    "section_or_ac": "Ambiguous issue quarantine remains upstream and exact-body based",
    "proposed_test": "source changed at final pre-dispatch read holds same Work for renewed specification/invariant approval"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B015",
   "trigger": "maintenance scout/critic run",
   "preconditions": "allowlist; explicit expectation evidence; independent critic; promotion enabled",
   "inputs_outputs": "pinned detached checkout → candidate+fingerprint → issue-only/eligible; optional issue/handoff",
   "owners": {
    "decision": "scout/critic propose; code gates issue and handoff",
    "effect": "scout/critic propose; code gates issue and handoff"
   },
   "durable_state": "candidate store + history/scans; run registration",
   "identities": "candidate fingerprint; repo/base commit; issue/handoff run",
   "transitions": "new/dismissed/accepted/fixed/regressed; regression starts fresh sighting window",
   "failure_semantics": "failed issue create response reconciles fingerprint; lookup failure delays; launch/register failure stops process before cleanup",
   "concurrency_restart": "writer+issue locks; remote issue recovery preserves daily creation date; human dismissal during gh call wins",
   "bounds": "default issue/handoff false; source per-day caps; scouts bounded",
   "human_action": "review dry-run precision; explicit rollout; no unsolicited target automatic intake",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/repo-maintenance.md:entire file"
    }
   ],
   "evidence_classification": "documented behavior; main library/tests pending",
   "proposed_target": {
    "owner_issue": "issue50",
    "section_or_ac": "Maintenance candidate automation is independent admission policy",
    "proposed_test": "manual/Selector admission both validate source identity; automatic discovery cannot bypass submit"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B016",
   "trigger": "proposed post-dispatch recovery exhaustion",
   "preconditions": "actually attempted failed bounded recovery; positive quiescence; no equivalent pending work",
   "inputs_outputs": "exhaustion evidence → suppression intent → blocked readback → parked record",
   "owners": {
    "decision": "proposed scheduler jurisdiction",
    "effect": "proposed scheduler jurisdiction"
   },
   "durable_state": "proposed versioned parked-work registry/operator history",
   "identities": "work record/repo/group/issue/run/card identities",
   "transitions": "intent → quiescence proof → card terminal confirmation → parked; explicit operator unpark removes suppression only",
   "failure_semantics": "disabled remediation is not exhausted; stalled live/unknown never kill/retry/park; ambiguous transition keeps intent",
   "concurrency_restart": "same scheduler lock; replay unfinished intent from fresh evidence; separate merged ledger lock",
   "bounds": "independent bounded retries/remediation/reclaim; source design default off/dryrun",
   "human_action": "explicit unpark preview+confirm; never automatically reopen parked work",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/recovery-first-park-advance.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue276 queried2026-11-07 stateCLOSED closed2026-10-17; design-only acceptance, explicitly no runtime change. Closed design issue not proof M1 runtime installed."
    }
   ],
   "evidence_classification": "intended future requirement; document explicitly says M1+ unimplemented; selector parking is separate implemented mechanism",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Recovery-first park/advance is design-only, not installed capability",
    "proposed_test": "disabled recovery does not appear exhausted; exhausted cannot erase unknown effects; crash at each step preserves safe hold"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B017",
   "trigger": "classified transient settle or prelaunch triage environment failure",
   "preconditions": "known environment error; retry policy; for ship only independently probe existing PR first",
   "inputs_outputs": "worker/subprocess failure → category and bounded pending_retry or capability hold",
   "owners": {
    "decision": "coordinator classifier and settle; adapters supply PR facts",
    "effect": "coordinator classifier and settle; adapters supply PR facts"
   },
   "durable_state": "cw-attempts env_attempts/last_key/category/next_attempt_at; run env_failures bounded history",
   "identities": "task ID + attempt key started_at|pid|category|detail prefix",
   "transitions": "retryable → per-attempt count → pending_retry; count > source limit → fix-env/capability; completion/fresh task resets",
   "failure_semantics": "same attempt replay no double count; unreadable ledger refuses retry; ship PR unknown holds, found PR avoids duplicate creation; shadow/converge never shortcut via existing PR",
   "concurrency_restart": "pending_retry relaunch keeps count; fresh successor/fresh launch gets full count, so source task limit is not Work-global limit",
   "bounds": "source default three retries; limit0 means first failure holds; PR-probe candidate cap; env_failures last10",
   "human_action": "repair after exhaustion; target retry classes and cross-Execution defaults need owner consultation",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:7302-7373"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin-milestone.py:6506-6605,6835-6870,12670-12705"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_coding_worker_lib.py:3228-3264"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_coding_worker_lane.py:12535-12659"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue488 read2026-11-07 stateCLOSED closed2026-11-02; body specifies per-task bounded env retries"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:1955-2007 full environment_failure_category and explicit pattern table"
    }
   ],
   "evidence_classification": "observed selected implementation/tests; issue status freshly read; no production evidence",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Environment retries are durable within task but successor budget is fresh",
    "proposed_test": "three equivalent failures split across automatic Executions consume one Work policy budget; corrupted counter refuses retry; existing PR does not mark unfinished review complete"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B018",
   "trigger": "webhook observation then debounce drain",
   "preconditions": "PR scoped event ledger; configured route; coverage or successfully persisted successor; no unresolved human wait bypass",
   "inputs_outputs": "event ID/head/seen epoch → pending/consumed/drop/quarantine and dispatched task ID",
   "owners": {
    "decision": "event writer + sweep; adapters enqueue",
    "effect": "event writer + sweep; adapters enqueue"
   },
   "durable_state": "per repo/PR resume/shadow JSON under same-key flock; seen IDs bounded; durable typed dispositions",
   "identities": "repo+PR ledger hash, event ID, last_seen_epoch, reviewed head and worker-start watermark",
   "transitions": "new event opens pending; duplicate same class no-op; provenance-bearing class change may reopen; observed epoch consume leaves later event pending",
   "failure_semantics": "corrupt ledger is silently skipped/loadNone in these helpers; unbound TTL expires without proving completion; unknown waits hold; new activity after running worker start stays pending",
   "concurrency_restart": "per-PR flock prevents consume overwriting newer observation; through_epoch guards newer records; failed enqueue remains pending",
   "bounds": "seen event IDs last50; sweep deadline; unbound TTL; personal debounce",
   "human_action": "human wait requires explicit judgment/release; dropped/quarantined provenance queryable",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:2797-3130"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/devin-milestone.py:15420-15645,15655-15800"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:6296-6355 retire_shadow_wake full function; ownership+observed_epoch guard under same PR lock, errors treated advisory with cron fallback"
    }
   ],
   "evidence_classification": "observed selected implementation; terminal cleanup behavior not fully established, matching tests pending",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Pending trigger retirement is version-bound and must preserve newer activity",
    "proposed_test": "a terminal Execution cannot retire a newer command/effect intent; newer event during reconciliation remains pending; invalid ledger surfaces unavailable instead of known-empty"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B019",
   "trigger": "multiple failed attempts or nonterminal unchanged phase",
   "preconditions": "reliable ordered failure/termination/progress evidence",
   "inputs_outputs": "failure sequence → actionable repeated-failure Attention with repair hint",
   "owners": {
    "decision": "proposed read projection; no scheduler action authorized",
    "effect": "proposed read projection; no scheduler action authorized"
   },
   "durable_state": "existing run failure/attempt evidence; proposed projection only",
   "identities": "card/task attempt sequence + kind + termination reason; target must use same Work across Executions",
   "transitions": "second equivalent failure → actionable diagnosis; unavailable sequence remains unknown",
   "failure_semantics": "OPEN source proposal is not proof implemented or deployed; environment retry counters do not implement all deterministic failure-loop diagnoses",
   "concurrency_restart": "target durable failure episode must not reset on automatic new Execution or minor head churn",
   "bounds": "source issue379 proposes threshold2 and phase30min; do not adopt these as target defaults",
   "human_action": "repair spec/ops before blind redispatch; owner chooses equivalence and recovery thresholds",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tracker issue379 read2026-11-07 stateOPEN updated2026-11-01; motivating four patch_embeds_secret failures and proposal criteria"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_lib.py:7302-7373 confirms narrower per-task environment counters"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/kanban_dev_status_lib.py:3253-4017 full build_attention body; pinned grep for #379/deterministic5/repeated/consecutive/retry.loop in projection and principal tests returned no hits"
    }
   ],
   "evidence_classification": "intended requirement + historical incident recorded in tracker; implementation not established",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Repeated equivalent failure diagnosis remains a separate open requirement",
    "proposed_test": "repeat deterministic failure across two new automatic Executions produces Attention and blocks further blind recovery once Work policy exhausted"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B020",
   "trigger": "optional one-shot or LaunchAgent sample",
   "preconditions": "host commands available; user opted in persistent monitor",
   "inputs_outputs": "vm_stat/memory_pressure/sysctl/ps → partial sample/errors and investigative trend events",
   "owners": {
    "decision": "host sampler only",
    "effect": "host sampler only"
   },
   "durable_state": "bounded rotated JSONL plus launchd captures",
   "identities": "PID+start stamp; sample timestamp; host filesystem",
   "transitions": "sample → append/rotate; missing metric retained as errors; no stop/retry/block effect",
   "failure_semantics": "missing sample means unable to observe, never healthy; summed RSS is not unique memory; historic evening incident causal evidence absent",
   "concurrency_restart": "one short process per sample; file lock/rotation described, implementation excluded as host diagnostics",
   "bounds": "source four commands5s, top40,8MiB logs; values personal",
   "human_action": "persistent monitoring explicit opt-in; inspect actual sample cost before enable",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/memory-monitor.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/memory-incident-2026-10-28.md:entire file"
    }
   ],
   "evidence_classification": "documentation and historical investigation only; monitor code/tests excluded as unrelated host instrumentation",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Host resource sampling is diagnostic and never admission authority",
    "proposed_test": "host metric unavailable cannot authorize capacity/replacement or infer stop; no host monitor service added"
   },
   "proposed_test": null
  },
  {
   "id": "ops-B021",
   "trigger": "hourly sync or bootstrap installer",
   "preconditions": "main checkout clean incl untracked; explicit origin/main fetched; no ahead/divergence; exact current origin SHA",
   "inputs_outputs": "verified tree → atomic self-script refresh then independently rerun safe installers",
   "owners": {
    "decision": "sync runner; feature installer owns own files",
    "effect": "sync runner; feature installer owns own files"
   },
   "durable_state": "deployed copies; sync report/cron; no Work store migration",
   "identities": "explicit origin/main SHA and owned script name",
   "transitions": "behind clean checkout → fast-forward → revalidate → atomic self-update → installers; failures aggregate",
   "failure_semantics": "dirty/wrong branch/ahead/divergent/ref mismatch refuses all deploy; timed-out/failed installer reported but remaining installers continue",
   "concurrency_restart": "no sync-level flock observed; file promotion atomic+fsync; all safe installers run even no new source commits, correcting drift",
   "bounds": "git120s/installer600s source limits; bootstrap active immediately after smoke",
   "human_action": "pause job to stop updates; secret rotation/service restart/per-repo wiring explicitly skipped",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/auto-sync.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/hermes-setup-sync.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-hermes-setup-sync.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_hermes_setup_sync.py:entire file"
    }
   ],
   "evidence_classification": "observed implementation and all tests read; no runtime execution",
   "proposed_target": {
    "owner_issue": "issue52",
    "section_or_ac": "Self-update reconciles deployment only from a verified clean upstream checkout",
    "proposed_test": "repeat installation preserves user decisions and config; wrong source revision/dirty checkout does not silently activate code; bootstrap verification records exact deployed revision"
   },
   "proposed_test": null
  },
  {
   "id": "worker-B001",
   "trigger": "Run starts or an escalation is attempted",
   "preconditions": "Validated job and runtime available",
   "inputs_outputs": "Deadline and optional step budget; cancellation/deadline/step_limit reason",
   "owners": {
    "decision": "Lifecycle",
    "effect": "Runtime and process launcher share abort signal"
   },
   "durable_state": "Run result/evidence per contract; not verified in runner",
   "identities": "job_id and unique run_id per contract",
   "transitions": "active -> first termination reason; timer disposed after settlement",
   "failure_semantics": "first termination wins; already expired terminates immediately Runner disposes lifecycle before manifest repair; each repair has fresh own deadline, so shared execution deadline does not bound finalize repair.",
   "concurrency_restart": "same absolute deadline may be supplied for escalation; restart semantics not inspected",
   "bounds": "deadline and steps; worker contract describes max_steps as best effort",
   "human_action": "operator handles terminal failure",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/lifecycle.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:docs/worker-contract.md §Escalation",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/runner.ts:846-963"
   ],
   "evidence_classification": "observed implementation plus intended contract; runner enforcement unverified",
   "proposed_target": "issue50 lifecycle/recovery acceptance: attempts share deadline and no restart resets work-level bound; test elapsed and simultaneous aborts",
   "proposed_test": null
  },
  {
   "id": "worker-B002",
   "trigger": "Sanitized run export requested",
   "preconditions": "Existing run directory",
   "inputs_outputs": "Source run, destination, secrets -> redacted files and export manifest",
   "owners": {
    "decision": "exportSanitized",
    "effect": "local filesystem exporter"
   },
   "durable_state": "export files and export-manifest.json",
   "identities": "run_id from run directory basename",
   "transitions": "walk -> exclude private names and temp -> redact -> manifest",
   "failure_semantics": "I/O failures propagate; atomicity of entire export and symlink behavior not verified",
   "concurrency_restart": "destination itself rejected and nested export omitted; concurrent export not verified",
   "bounds": "private patch, snapshot, impact manifest, plan, refusal detail excluded",
   "human_action": "operator chooses export destination",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/export.ts"
   ],
   "evidence_classification": "observed implementation; operational success unverified",
   "proposed_target": "issue50 evidence/export AC: exclude private content and redact; test nested destination and secret leakage",
   "proposed_test": null
  },
  {
   "id": "worker-B003",
   "trigger": "Worker reports completion",
   "preconditions": "Job snapshot and actual run evidence exist",
   "inputs_outputs": "result, checks, patch and impact manifest -> completion versus PR-ready",
   "owners": {
    "decision": "worker contract (proposed)",
    "effect": "runner, not independently reviewed"
   },
   "durable_state": "run manifest, ordered events, result, hashes and partial evidence required by contract",
   "identities": "job_id, run_id, base revision and artifact hash",
   "transitions": "completed is bounded execution, not external acceptance; missing required checks or invalid manifest blocks PR-ready",
   "failure_semantics": "unknown usage stays unknown; partial evidence retained on failure/cancel/timeout; implementation not fully audited",
   "concurrency_restart": "unique run directory prevents overwriting, stated contract only",
   "bounds": "immutable base, required checks, captured diff",
   "human_action": "external assessment remains outside worker",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:docs/worker-contract.md §CodingResult and §Evidence and artifacts"
   ],
   "evidence_classification": "intended requirement; implementation and tests unverified",
   "proposed_target": "issue50 evidence AC and issue51 worker boundary: reject missing or wrong-patch evidence; test forged self-report",
   "proposed_test": null
  },
  {
   "id": "worker-B004",
   "trigger": "Planner malformed output or execution failure during convergence",
   "preconditions": "Historical changes included in pinned ancestry",
   "inputs_outputs": "planner failure versus semantic replan accounting",
   "owners": {
    "decision": "convergence planner",
    "effect": "converge loop"
   },
   "durable_state": "not independently inspected",
   "identities": "run and replan context not independently inspected",
   "transitions": "historical commits describe separating execution failure from semantic replan budget and one bounded formatting retry",
   "failure_semantics": "exact guards and current behavior not established by commit titles",
   "concurrency_restart": "not inspected",
   "bounds": "one format retry stated in commit subject only",
   "human_action": "not established",
   "source_evidence": [
    "3cd18db commit subject and stat (#370)",
    "654b511 commit subject and stat (#371)"
   ],
   "evidence_classification": "historical change, current implementation unverified",
   "proposed_target": "issue50 recovery section: decide failure classes and work-level retry limit; test repeated equivalent failures across Executions",
   "proposed_test": null
  },
  {
   "id": "worker-B005",
   "trigger": "Checks requested",
   "preconditions": "Validated check list and workspace",
   "inputs_outputs": "Commands -> passed/failed/skipped/unavailable, exit code, output refs",
   "owners": {
    "decision": "Check runner",
    "effect": "Process launcher and evidence store"
   },
   "durable_state": "Check events and command output",
   "identities": "run and check name",
   "transitions": "serial by default; adjacent named batch concurrent",
   "failure_semantics": "timeout is failed; missing exit unavailable; pre-abort skipped; evidence error propagates",
   "concurrency_restart": "at most two; waits for active peers even when evidence write fails",
   "bounds": "minimum run deadline, check timeout and command timeout; sensitive environment names withheld",
   "human_action": "respond to failures",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/checks/run.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/checks-parallel.test.ts"
   ],
   "evidence_classification": "source observed, tests inspected not run",
   "proposed_target": "issue50 verification AC: timeouts and unavailable distinct; drain peers before patch capture; tests assert timeout/peer wait",
   "proposed_test": null
  },
  {
   "id": "worker-B006",
   "trigger": "Job submitted",
   "preconditions": "untrusted JSON",
   "inputs_outputs": "Versioned job -> validated frozen job or field errors Prepared task maps issue snapshot, bounds, constraints and commands into worker job and validates again.",
   "owners": {
    "decision": "job validator",
    "effect": "caller dispatch after validation"
   },
   "durable_state": "canonical hashes of job and issue snapshot",
   "identities": "job id, 40-hex base, issue group and ordinal criterion labels",
   "transitions": "parse -> validate -> freeze -> hash",
   "failure_semantics": "unknown job fields rejected; result validation is structural and shallow, not proof of evidence",
   "concurrency_restart": "group members must have unique issue numbers",
   "bounds": "positive time and numeric limits; opaque credential reference syntax",
   "human_action": "correct rejected input",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/types.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/validate.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/contract.test.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/task.ts"
   ],
   "evidence_classification": "source and test assertions observed, tests not run",
   "proposed_target": "issue50/51 admission and worker contract AC: immutable snapshot bound to job; reject malformed inputs and structurally valid forged outcomes",
   "proposed_test": null
  },
  {
   "id": "worker-B007",
   "trigger": "Worker supplies impact manifest",
   "preconditions": "acceptance criteria supplied",
   "inputs_outputs": "Delimited JSON -> structural validation and criterion coverage",
   "owners": {
    "decision": "manifest validator; readiness owner downstream",
    "effect": "runner capture (test evidence only)"
   },
   "durable_state": "manifest and hash described in tests",
   "identities": "criterion exact string, grouped member number and ordinal",
   "transitions": "missing/invalid or complete mapping",
   "failure_semantics": "validator verifies nonempty references, not existence or truth; bounded repair fixture tests not independently run",
   "concurrency_restart": "repair/restart implementation not reviewed",
   "bounds": "six dimensions; every nonblank criterion maps to code or tests",
   "human_action": "review unresolved risks",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/impact-manifest.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/design-dimensions.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/impact-manifest.test.ts"
   ],
   "evidence_classification": "source observed, selected fixture test assertions, runner not audited",
   "proposed_target": "issue50 evidence gates AC: self report is not independent verification; missing evidence fails where required; exact artifact test",
   "proposed_test": null
  },
  {
   "id": "worker-B008",
   "trigger": "Optional planner returns response",
   "preconditions": "planner enabled",
   "inputs_outputs": "final answer -> missing, invalid or plan text",
   "owners": {
    "decision": "plan parser",
    "effect": "planner caller (not inspected)"
   },
   "durable_state": "plan artifact per contract",
   "identities": "run/phase identity outside parser",
   "transitions": "one marker pair with nonempty content -> present",
   "failure_semantics": "missing, duplicate, misplaced or empty block invalid; no effect decision here",
   "concurrency_restart": "caller concurrency and retries not inspected",
   "bounds": "exactly one pair",
   "human_action": "resolve failed planning",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/contract/plan.ts"
   ],
   "evidence_classification": "observed parser; caller not audited",
   "proposed_target": "issue51 worker boundary: optional planner may not imply hard gate; malformed output test if adopted",
   "proposed_test": null
  },
  {
   "id": "worker-B009",
   "trigger": "Evidence captured",
   "preconditions": "per-run directory",
   "inputs_outputs": "events/artifacts/results -> redacted records and hashes; verbatim outputs refuse detected secrets",
   "owners": {
    "decision": "evidence store and detector",
    "effect": "filesystem writes"
   },
   "durable_state": "manifest with job, issue, base hashes; events; results",
   "identities": "run basename, job hash, artifact path and digest",
   "transitions": "capture -> redaction or refusal -> registered artifact",
   "failure_semantics": "pattern and literal detection incomplete by nature; deterministic refusal returns classes not value; partial event file remains",
   "concurrency_restart": "sequence scoped to store instance; cross-writer/restart safety unverified",
   "bounds": "event capped at 256 KiB by truncation marker; literal strings shorter than four not detected",
   "human_action": "inspect failures and retention",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/store.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/redact.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/evidence.test.ts"
   ],
   "evidence_classification": "source observed, selected test assertions not run",
   "proposed_target": "issue50 evidence/retention AC: no secrets in persistent or export surfaces, exact bytes hash, partial event recovery test",
   "proposed_test": null
  },
  {
   "id": "worker-B010",
   "trigger": "Operator invokes CLI",
   "preconditions": "Node and built binary installed",
   "inputs_outputs": "run/export/eval/devflow/shadow/review commands and flags -> exit/status",
   "owners": {
    "decision": "CLI dispatch",
    "effect": "worker and integration functions"
   },
   "durable_state": "run and output directories",
   "identities": "job/run IDs and exit status",
   "transitions": "validated run -> structured output and mapped status",
   "failure_semantics": "missing args or malformed job refuse; completed but not ready exits 10; module import does not invoke CLI",
   "concurrency_restart": "signals propagate abort; other command paths not reviewed",
   "bounds": "per-job limits, runtime validation in called modules",
   "human_action": "operator starts and interprets CLI",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:package.json",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli.test.ts"
   ],
   "evidence_classification": "source entrypoint inspected; CLI remainder and test assertions pending",
   "proposed_target": "issue50/55 CLI setup and contracts: invalid input, structured outcomes, no effect on import; exercise real CLI",
   "proposed_test": null
  },
  {
   "id": "worker-B011",
   "trigger": "Standalone sanitized export requested",
   "preconditions": "run manifest exists",
   "inputs_outputs": "run directory, destination, optional resolved credential -> export",
   "owners": {
    "decision": "CLI cmdExport",
    "effect": "exporter"
   },
   "durable_state": "export files and manifest",
   "identities": "run directory",
   "transitions": "credential resolve -> redact/export",
   "failure_semantics": "Standalone export warns and continues generic-only when credential resolution fails; retained-submission export explicitly refuses when resolver fails. Snapshot parse failure falls back to caller ref. Neither path proves comprehensive redaction.",
   "concurrency_restart": "concurrent export unverified",
   "bounds": "pattern detection cannot cover arbitrary unknown secrets",
   "human_action": "operator sees warning",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/export.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli.test.ts"
   ],
   "evidence_classification": "both CLI paths observed; live operations unverified",
   "proposed_target": "issue50/55 export AC: unknown secret context cannot silently produce unsafe export; test unavailable resolver and arbitrary secret",
   "proposed_test": null
  },
  {
   "id": "worker-B012",
   "trigger": "Preflight reads integration availability",
   "preconditions": "Probe is requested with selected repo and credential identity",
   "inputs_outputs": "gh/git/server responses -> ok/fail/unknown with diagnostic",
   "owners": {
    "decision": "probe classifier (test evidence)",
    "effect": "adapter read operations"
   },
   "durable_state": "probe record; exact persistence caller pending",
   "identities": "repository host/owner/name, actor identity and credential context",
   "transitions": "explicit denial -> fail; timeout, DNS, rate limit, 5xx or unknown error -> unknown",
   "failure_semantics": "unknown must not be treated as positive permission or definite absence; no live network proof",
   "concurrency_restart": "credential env resolved afresh for each fake gh invocation; concurrency unverified",
   "bounds": "per-probe timeout in test fixture; actual limits not audited",
   "human_action": "resolve denied or uncertain prerequisites",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli-gh-deps.test.ts"
   ],
   "evidence_classification": "test assertions inspected, fake commands; implementation and live operations remain pending",
   "proposed_target": "issue50/55 preflight AC: distinguish transient unknown from permission denial, verify scoped credentials; tests simulate each class",
   "proposed_test": null
  },
  {
   "id": "worker-B013",
   "trigger": "Operator requests resume",
   "preconditions": "Validated retained task and resolver return matching leaf",
   "inputs_outputs": "task, root, resume selector -> retained completed submission or resumable link",
   "owners": {
    "decision": "resolveResume (implementation pending)",
    "effect": "CLI submit or read-only resume inventory"
   },
   "durable_state": "prior task, run evidence and optional ship report",
   "identities": "task hash, run, effective adapter, grouped issue IDs and retained overrides",
   "transitions": "completed leaf -> adopt without duplicate; resumable leaf -> submit with link; inventory itself does not dispatch",
   "failure_semantics": "Resolver follows newest child by start timestamp with cycle guards and rejects missing/mismatched workspace; completed leaf with unusable submission evidence refuses. Task-binding and evidence-validation dependencies pending.",
   "concurrency_restart": "CLI explicitly avoids lookup by exact current hash after resolver selected an earlier-decision leaf; races unverified",
   "bounds": "existing run limits; no assertion that budget survives new runs",
   "human_action": "operator selects resume or abandons",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts:3692-3819",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts:4881-4973",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli.test.ts:1516-1805",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/resume.ts"
   ],
   "evidence_classification": "CLI body and fixture assertions observed; resolver and real restart operations unverified",
   "proposed_target": "issue50 recovery AC: recover known completion without redispatch and never infer unknown effects; restart replay test",
   "proposed_test": null
  },
  {
   "id": "worker-B014",
   "trigger": "Execution returns a failure or provider refuses a request",
   "preconditions": "Optional planner escalation or provider fallback is configured",
   "inputs_outputs": "typed result/provider status and shared deadline -> trigger, candidate selection or no further attempt",
   "owners": {
    "decision": "escalation wrapper and provider error classifier",
    "effect": "worker runtime adapter; fallback dispatch caller pending"
   },
   "durable_state": "escalation record with first attempt identity, cost and run directory",
   "identities": "requested job hash, actual attempt run IDs and configured model candidates",
   "transitions": "at most one plan-enabled escalation after eligible failure; cancellation or no time -> not run; resumed attempt stays single run",
   "failure_semantics": "timeout, auth, quota or model availability classified provider unavailable at classifier; actual adapter guards/fallback propagation unreviewed; do not equate all failures to retryable",
   "concurrency_restart": "attempts sequential; same absolute deadline shared; no assurance work-level recovery budget survives separate execution",
   "bounds": "one escalation; deadline shared; provider retry at/after deadline can be refused",
   "human_action": "handle exhaustion or refused recovery",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/escalation.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/provider-errors.ts"
   ],
   "evidence_classification": "source bodies observed; tests and real provider unverified",
   "proposed_target": "issue50 recovery failure classes and cross-execution bounds AC: provider refusal differs from cancellation and deterministic refusal; no new attempt after deadline; tests span restart",
   "proposed_test": null
  },
  {
   "id": "worker-B015",
   "trigger": "Worker adapter finishes or fails",
   "preconditions": "Credentials resolved, adapter capabilities checked, pinned checkout available",
   "inputs_outputs": "adapter result plus checks, policy events and captured diff -> result and run evidence",
   "owners": {
    "decision": "worker runner; downstream acceptance separate",
    "effect": "adapter, check runner, capture and evidence filesystem"
   },
   "durable_state": "run events, patch when safe, result, manifest and findings; retained workspace on failure",
   "identities": "job hash, base revision, parent run when resumed, run ID and patch ref",
   "transitions": "checkout -> adapter -> checks or deferred -> final patch -> policy -> self-check -> readiness",
   "failure_semantics": "capture runs even on timeout/cancellation to preserve partial evidence; excluded/missing files make delivery non-ready; secret-bearing patch is deterministically refused and workspace cleanup attempted; cleanup errors suppressed",
   "concurrency_restart": "resumed workspace exists and HEAD matches base; broader resolver/concurrent ownership still unverified",
   "bounds": "normal phases share deadline; finalize repair uses up to two fresh 180-second lifecycles with 24 steps each after original lifecycle disposal",
   "human_action": "inspect structured failure, correct deterministic refusal or explicitly recover",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/runner.ts"
   ],
   "evidence_classification": "observed source, corresponding fixture assertions inspected but not executed; dependent implementation not yet audited",
   "proposed_target": "issue50 lifecycle/evidence AC: timeout/cancel preserve safe partial evidence without readiness, reject incomplete patch; enforce budget in finalize and across Execution; tests simulate deadline during capture/repair",
   "proposed_test": null
  },
  {
   "id": "worker-B016",
   "trigger": "Execution needs checkout",
   "preconditions": "Validated pinned commit and available repository",
   "inputs_outputs": "repository location and SHA -> detached isolated checkout at verified HEAD",
   "owners": {
    "decision": "checkout adapter",
    "effect": "git and local filesystem"
   },
   "durable_state": "disposable workspace, separate evidence outside it",
   "identities": "repository location, exact base SHA and destination workspace",
   "transitions": "resolve exact commit -> clone -> detach -> verify HEAD -> install scanner exclusions",
   "failure_semantics": "failure cleans partial checkout best-effort; inability to determine tracked ignore or hide scanner rules fails closed; symlink not followed",
   "concurrency_restart": "scanner exclude merge is idempotent; shared git common-directory concurrent updates not tested",
   "bounds": "git subprocesses clamped by remaining deadline and cancellation",
   "human_action": "fix repository/credential/checkout prerequisites",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/workspace/checkout.ts"
   ],
   "evidence_classification": "source observed; tests and live operations unverified",
   "proposed_target": "issue50/51 checkout adapter AC: verify exact repository and base SHA and isolate evidence; test unknown revision, interrupt and cleanup",
   "proposed_test": null
  },
  {
   "id": "worker-B017",
   "trigger": "Capture final workspace changes",
   "preconditions": "Checkout and Git available after checks",
   "inputs_outputs": "tracked and untracked files -> patch, file names, excluded, scope violations, reference gaps",
   "owners": {
    "decision": "patch capture; runner judges ready",
    "effect": "Git reads and intent-to-add"
   },
   "durable_state": "patch bytes stored separately by caller if safe",
   "identities": "exact HEAD and workspace, literal changed paths",
   "transitions": "enumerate -> filter secret/forbidden/out-of-scope -> diff -> detect missing relative references",
   "failure_semantics": "referenceGaps returns [] if base listing fails, conflating skipped with no gaps; heuristic detects only supported syntactic forms and is not complete verification",
   "concurrency_restart": "Git add -N mutates index; concurrent workspace writers are not protected here",
   "bounds": "optional deadline/signal; caller capture deliberately ignores lifecycle for partial preservation",
   "human_action": "review exclusions, scope violations and reported missing references",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/workspace/patch.ts"
   ],
   "evidence_classification": "source observed; tests/live behavior unverified",
   "proposed_target": "issue50 exact artifact evidence and unknown AC: listing failure is unknown, never clean; tests for excluded tracked/untracked files and interrupted tree read",
   "proposed_test": null
  },
  {
   "id": "worker-B018",
   "trigger": "Read-only reviewer or planner is dispatched",
   "preconditions": "Server agent definition and declared capabilities available",
   "inputs_outputs": "resolved permission rules, optional exact readonly tool allowlist -> violation or proof",
   "owners": {
    "decision": "adapter pre-dispatch validation",
    "effect": "runtime server enforces actual permissions; event auditor pending"
   },
   "durable_state": "caller may record refusal; actual caller evidence pending",
   "identities": "specific server agent, workspace, exact tool and permission rule ordering",
   "transitions": "unavailable, malformed, ambiguous, default allow or later write grant -> refusal; explicit allowed read and default deny -> pass",
   "failure_semantics": "self-declared read_only alone insufficient for server-enforced proof; no live server test",
   "concurrency_restart": "per-workspace runtime isolation implementation pending",
   "bounds": "only exact declared readonly MCP names except narrow explicit exemptions",
   "human_action": "fix server permissions when refusal is surfaced",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/adapter.ts"
   ],
   "evidence_classification": "source validation observed; caller, tests and live enforcement pending",
   "proposed_target": "issue50/51 adapter-boundaries AC: verify reviewer permissions fail closed and scope exact workspace; tests later wildcard regrant and malformed rules",
   "proposed_test": null
  },
  {
   "id": "worker-B019",
   "trigger": "Review/convergence reads current PR signals or writes response",
   "preconditions": "Scoped repo and PR; identity resolved where available",
   "inputs_outputs": "check runs, statuses, reviews, comments, threads -> normalized observations; replies and optional thread resolution -> remote effects",
   "owners": {
    "decision": "convergence consumer; CLI adapter performs reads and writes",
    "effect": "GitHub API via gh"
   },
   "durable_state": "request/response files; separate finding ledger pending audit",
   "identities": "repository, PR, exact head SHA, actor login and optional actor ID",
   "transitions": "read head signals -> classify by actor and head -> potential reply -> optional resolution",
   "failure_semantics": "unknown viewer keeps markers untrusted; threads query first 100 threads and 50 comments only without pagination; no completeness proof; converge transport writes unredacted request/response while review transport redacts Drain catches failed current-head read and substitutes the reviewed head in post planning; this does not prove safe current-head binding on unknown.",
   "concurrency_restart": "uncertain post replay safety and effect reconciliation belong to unreviewed ledger/converge; do not infer idempotence from markers alone",
   "bounds": "finite GitHub call timeout and bounded initial GraphQL window",
   "human_action": "resolve incomplete observations or failed effects",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts:1919-2160",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts:2590-2951",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/cli-gh-deps.test.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/projection-drain.ts:506-850"
   ],
   "evidence_classification": "CLI source and fake-transport test assertions observed; live APIs and convergence policy unverified",
   "proposed_target": "issue50 evidence/reconciliation AC: incomplete pagination yields unknown, not empty; pending write receipts survive retries; ensure every request/response is structurally secret-safe",
   "proposed_test": null
  },
  {
   "id": "worker-B020",
   "trigger": "A stage starts, completes, stops or process is killed",
   "preconditions": "Ship owns its legacy directory",
   "inputs_outputs": "stage completion and references -> atomic checkpoint, next command and optional prior outcome",
   "owners": {
    "decision": "ship caller constructs checkpoint; stage evidence is authority",
    "effect": "atomic JSON writer"
   },
   "durable_state": "checkpoint, stage evidence, lock, previous outcome",
   "identities": "legacy ship dir, issue group, task, profile digest, invocation and PID",
   "transitions": "in progress -> completed/terminal; absent/corrupt checkpoint -> no authority; evidence determines resume",
   "failure_semantics": "kill leaves stale running marker; lock liveness must be checked; report alone cannot prove effect completion",
   "concurrency_restart": "checkpoint may survive kill; command intentionally omits old outer deadline and gets fresh envelope",
   "bounds": "checkpoint stores no global cross-invocation retry bound",
   "human_action": "inspect refs and run continuation where safe",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/ship-checkpoint.ts"
   ],
   "evidence_classification": "source contract observed; caller and tests pending",
   "proposed_target": "issue50 lifecycle/recovery AC: persist cross-Execution limit and unresolved effects; crash at each stage cannot falsely complete or reset budget",
   "proposed_test": null
  },
  {
   "id": "worker-B021",
   "trigger": "Consumer loads retained execution evidence or considers verification retry",
   "preconditions": "Run manifest, result and job snapshot exist",
   "inputs_outputs": "run directory -> validated bundle including exact patch bytes, artifact checks or errors",
   "owners": {
    "decision": "run evidence loader and infrastructureIncompleteRun classifier",
    "effect": "filesystem reads; caller decides whether to verify"
   },
   "durable_state": "manifest, snapshot, result, patch and artifact hashes",
   "identities": "run_id, job_id, base revision, job/issue hashes, parent link, patch digest",
   "transitions": "all hashes and identity match -> bundle; missing/mismatch -> refusal; only harness-class required checks may be reverified",
   "failure_semantics": "timed out/skipped/unavailable differs from actual test failure; unknown finding codes or execution policy violations block supersession; result validator is shallow and does not prove truth",
   "concurrency_restart": "retained evidence loaded anew; immutable ownership and concurrent writing not established here",
   "bounds": "all declared artifacts checked; gzip patch may be decompressed in memory, no bound visible",
   "human_action": "investigate corrupted evidence or substantive failure",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/run-evidence.ts"
   ],
   "evidence_classification": "source body observed; test and real I/O unverified",
   "proposed_target": "issue50 evidence gate AC: mismatched hashes and identities reject, only independently rechecked transient harness failures supersede; unknown remains unknown",
   "proposed_test": null
  },
  {
   "id": "worker-B022",
   "trigger": "Verification requested for a run",
   "preconditions": "Retained bundle has matching job/base/artifact hashes and patch is present",
   "inputs_outputs": "patch, declared/supplemental checks -> applied patch, check evidence, supplemental check digest and verdict",
   "owners": {
    "decision": "verifyRun",
    "effect": "fresh checkout, Git apply and check runner"
   },
   "durable_state": "verification record, event stream, exact patch, command output and manifest",
   "identities": "source run, repository, base revision, patch digest and canonical supplemental-check specs",
   "transitions": "load -> refuse invalid/missing evidence -> checkout pinned base -> apply exact patch -> run checks -> pass or fail",
   "failure_semantics": "missing checks fails; missing credential emits warning and falls back to generic redaction; only required nonpassing checks determine verdict; no task verification is fabricated",
   "concurrency_restart": "fixed verification workspace reused only if base matches and clean including ignored files; no cross-process lock proven here",
   "bounds": "overall verification deadline covers clone/apply/checks; per-command limits also apply",
   "human_action": "resolve check failure or credential warning",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/verify.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/devflow-verify.test.ts"
   ],
   "evidence_classification": "source body observed; tests not yet reviewed and operations not executed",
   "proposed_target": "issue50 evidence gate AC: verify exact artifact/check digest, no silent secret-redaction downgrade, explicit empty-command behavior; tests for changed command and interrupted check",
   "proposed_test": null
  },
  {
   "id": "worker-B023",
   "trigger": "A pipeline seeks or releases issue ownership",
   "preconditions": "Scoped repo/issue and claim record; remote label/event API available",
   "inputs_outputs": "local claim and remote current label/events -> applied/resumed/conflict/refused or safe release",
   "owners": {
    "decision": "claim protocol",
    "effect": "local lock filesystem and remote label writes"
   },
   "durable_state": "local claim record, lock and GitHub label event",
   "identities": "repo/issue, claim ID, remote event ID, optional actor, local host/PID",
   "transitions": "local intent -> add label -> pin event; resume verifies current event; release only own current event unless explicit force",
   "failure_semantics": "unreadable labels/events refuse; unpinned event may be adopted; remote add is not compare-and-swap and concurrent identical adds may both think they acquired; local lock alone scoped to one directory",
   "concurrency_restart": "exclusive local lock and dead-PID takeover; pin survives interrupted add, but local PID liveness does not prove identity across reuse or all hosts",
   "bounds": "limited clock-skew adoption heuristic and two local lock attempts",
   "human_action": "resolve conflict; force release is explicit operator action",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/claim.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/cli.ts:2952-3053"
   ],
   "evidence_classification": "source implementation observed; tests and live race not verified",
   "proposed_target": "issue50 identity/admission AC: atomic scoped unfinished Work overlap and uncertainty holds; tests two concurrent admissions and lost claim acknowledgement",
   "proposed_test": null
  },
  {
   "id": "worker-B024",
   "trigger": "A caller accounts for prior dispatches or adopts a completed run",
   "preconditions": "Run directories and manifests under task scope",
   "inputs_outputs": "task variants and retained evidence -> ordered bound runs, optional submission/result",
   "owners": {
    "decision": "bound run scanner",
    "effect": "read-only filesystem and evidence loader"
   },
   "durable_state": "manifest job/requested hashes, snapshots, result and optional submission",
   "identities": "task ID, exact job hash, normalized identity, base revision and run ID",
   "transitions": "exact binding selects automatic adoption; normalized identity counts attempts across spec-note/planner variants; completed evidence may reconstruct missing submission",
   "failure_semantics": "manifest-only interrupted run counts against budget if bound; contradictory snapshot is ignored; unreadable/missing manifest cannot be bound and is not counted",
   "concurrency_restart": "scan supports crash recovery; read/dispatch atomicity and concurrent writers unverified",
   "bounds": "counts prior attempts but policy enforcement owner ship pending",
   "human_action": "inspect missing or contradictory evidence",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/bound-runs.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/spec-constraints.ts"
   ],
   "evidence_classification": "source observed; tests and integration pending",
   "proposed_target": "issue50 recovery AC: persist consumed automatic attempts transactionally before dispatch; repeated equivalent failures across Executions do not reset, unreadable record becomes unknown",
   "proposed_test": null
  },
  {
   "id": "worker-B025",
   "trigger": "New review verdict is reconciled with prior findings",
   "preconditions": "Prior finding and head-to-head diff available where needed",
   "inputs_outputs": "new findings, prior comments and diff hunks -> matches or verified-fixed/unknown classification",
   "owners": {
    "decision": "deterministic reconciliation",
    "effect": "caller determines comment/thread effects"
   },
   "durable_state": "prior marker identity, head, line and comment evidence",
   "identities": "PR/head, path/category/line and finding key; semantic fallback uses shared distinctive token",
   "transitions": "match prior -> carry forward; unmatched plus changed/deleted anchor -> verified_fixed; otherwise unknown",
   "failure_semantics": "unmapped head, no thread or no diff stays unknown; line change plus no rereport is source heuristic rather than independent proof of semantic fix; one shared token can weakly match",
   "concurrency_restart": "head-bound snapshot comparison; actual effects and persisted reconciliation pending",
   "bounds": "depends on finite available review/diff inputs; limits handled elsewhere",
   "human_action": "review unknown or disputed finding resolution",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/reconcile.ts"
   ],
   "evidence_classification": "source observed; tests and dependent identity mapping pending",
   "proposed_target": "issue50 immutable finding identities/dispositions AC: missing or failed comparison never resolves; require positive exact-head fix evidence and tests for moved findings",
   "proposed_test": null
  },
  {
   "id": "worker-B026",
   "trigger": "Review findings are matched across heads or authorship surfaces",
   "preconditions": "Repo/PR scoped observation and prior criterion set",
   "inputs_outputs": "normalized claim, path, category and actor observations -> stable criterion ID or explicit existing-match; canonical actor and optional ID",
   "owners": {
    "decision": "criterion resolver and actor normalizer",
    "effect": "ledger/reconciliation consumer"
   },
   "durable_state": "content fingerprint and criterion ID; original actor evidence",
   "identities": "repo, PR, path, category, normalized claim; actor raw/canonical/type/id",
   "transitions": "exact content -> existing; otherwise matched anchor plus same-concern heuristic -> existing; else new",
   "failure_semantics": "prose-only same anchor can match heuristically; actor ID mismatch disqualifies only when both IDs known, missing ID falls back to login",
   "concurrency_restart": "head mapping uses available previous head and hunks; durable transition control is separate pending",
   "bounds": "claim group comparison truncates at 400 characters",
   "human_action": "resolve ambiguous findings or actor attribution",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/criterion.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/review-actor.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/remote-meta.ts"
   ],
   "evidence_classification": "source observed; tests and live API behavior not verified",
   "proposed_target": "issue50 exact finding-set AC: stable opaque criterion identity with explicit ambiguity, head binding and actor evidence; tests for same anchor/different claims and missing IDs",
   "proposed_test": null
  },
  {
   "id": "worker-B027",
   "trigger": "Review status is projected for a head or head signal is published",
   "preconditions": "Scoped review evidence or prior own-authored signal",
   "inputs_outputs": "exact head plus ledger revision and coverage -> pending/pass/fail; desired signal -> create/update/unchanged/stale",
   "owners": {
    "decision": "quality projection and head signal publisher",
    "effect": "ledger reads and remote comment adapter"
   },
   "durable_state": "immutable head review, criterion transitions, signal marker",
   "identities": "repo, PR, exact head SHA, ledger revision, trusted actor and comment ID",
   "transitions": "missing review -> pending; partial/degraded -> reviewed but cannot pass; blockers -> fail; complete/no blocker -> pass",
   "failure_semantics": "live-head read failure in differing-head update is caught and update continues; thus stale protection not proven under unknown; projection is advisory source behavior and not an owner-approved native merge gate Quality test expects blocker omission on a successful new-head review to resolve, while an omitted advisory stays open; this differs from deterministic reconciliation’s omission rule.",
   "concurrency_restart": "identical signal no-op; current-head read avoids some overlapping stale writes, but read/write not atomic",
   "bounds": "head marker reason bounded, remote page completeness separate",
   "human_action": "resolve blockers or unknown evidence",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/head-signal.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/quality.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:test/review-quality-verdict.test.ts:137-174"
   ],
   "evidence_classification": "source observed; ledger implementation, tests and live races pending",
   "proposed_target": "issue50 gate AC: exact head and finding-set binding, partial is unknown, failed authoritative read cannot authorize write; concurrent head change test",
   "proposed_test": null
  },
  {
   "id": "worker-B028",
   "trigger": "A review ledger is opened or mutated",
   "preconditions": "Scoped SQLite ledger file with supported schema",
   "inputs_outputs": "database -> schema migration and transaction boundary; review/criterion/transition/publication/projection rows",
   "owners": {
    "decision": "ledger database and higher-level transaction caller",
    "effect": "SQLite local database"
   },
   "durable_state": "immutable head-review rows, scoped criteria and transitions, publication state, projection outbox",
   "identities": "repository, PR, head SHA, ledger revision, projection kind and remote identity",
   "transitions": "migration transaction commits one version; projection pending -> published/failed/superseded in higher-level caller",
   "failure_semantics": "newer schema and non-SQLite file refused; transaction rolls back on error; no network call inside transaction per source contract but caller needs audit Caller commit requires run/snapshot/verdict head agreement and completed verdict; partial coverage requires explained gaps; stage projection jobs in same transaction. Remaining commit body pending. Drain writes attempt evidence before terminal settlement and leaves failed/capped outcomes visible; remote read fallback and remaining effect branches pending.",
   "concurrency_restart": "BEGIN IMMEDIATE single-writer, WAL and full sync; lease semantics and crash tests pending Projection drain leases are acquired transactionally and explicitly renewed; jobs increment attempts before effects and have a cap, while ambiguous remote reconciliation remains pending.",
   "bounds": "busy timeout and ordered migrations",
   "human_action": "investigate unsupported/corrupt store or retry pending projection",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/ledger-db.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/ledger.ts:995-1215,1530-1675,2115-2318",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/projection-drain.ts:506-850"
   ],
   "evidence_classification": "source schema and transaction helpers observed; higher-level commit, tests, and remote effects pending",
   "proposed_target": "issue50 evidence/storage AC: transactional judgment and projection intent, immutable snapshots, restart with unresolved effects; crash tests at commit boundary",
   "proposed_test": null
  },
  {
   "id": "worker-B029",
   "trigger": "Caller submits validated task or exports a retained submission",
   "preconditions": "Task parses and maps to worker job; retained export requires run and result",
   "inputs_outputs": "raw task -> rejected errors or executed structured result + artifact refs; optional sanitized export",
   "owners": {
    "decision": "submit adapter",
    "effect": "worker, filesystem and export boundary"
   },
   "durable_state": "run evidence and submission record, optional export with corrected result digest",
   "identities": "task ID, run directory, artifact references and export destination",
   "transitions": "validate -> map -> execute -> persist submission -> export; retained export restamps copied records only",
   "failure_semantics": "invalid task rejected before worker; in-memory result written via writeJsonAtomic to submission without visible redaction although result.json uses redacted copy, so secret leakage is possible if result contains one; not operationally reproduced",
   "concurrency_restart": "retained completed run can be exported without rerun; ownership/exact-once is caller responsibility",
   "bounds": "delegated worker lifecycle and export boundaries",
   "human_action": "fix rejected task or investigate redaction failure",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/devflow/submit.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/evidence/store.ts:writeResult"
   ],
   "evidence_classification": "source path observed; leak is a plausible unverified defect, not an observed incident",
   "proposed_target": "issue50/55 evidence AC: arbitrary secret in every result field never appears in result, wrapper or export; inject literal secret and inspect all persisted bytes",
   "proposed_test": null
  },
  {
   "id": "SEL-B01",
   "trigger": "Operator invokes ready-issues or equivalent CLI/MCP query.",
   "preconditions": "Repository policy and scoped issue snapshot are available; caller supplies exclusions/limit.",
   "inputs_outputs": {
    "inputs": "Snapshot issues, milestones, dependencies, labels, repo policy and limit.",
    "outputs": "Classified issue/group result and stable fingerprint; unresolved reasons preserved."
   },
   "owners": "Pure selector decides classification/group/rank; tracker adapter reads only.",
   "durable_state": "None in selector; source tracker remains authoritative.",
   "identities": "Repository plus issue number; qualified cross-repository references are not local dependencies.",
   "transitions": "unresolved/excluded/in_progress/blocked/ready; ready groups ranked; over-limit groups excluded atomically.",
   "failure_semantics": "Malformed/ambiguous milestone or broken internal invariant fails closed; tracker read failure is not an empty result.",
   "concurrency_restart": "Pure deterministic selection for same snapshot/policy/time; fresh read can change answer.",
   "bounds": "Group max and requested group limit; groups never split.",
   "human_action": "Resolve ambiguity, blockers, or policy before acting.",
   "source_evidence": [
    "f89c35b:src/devflow/core/select.py",
    "f89c35b:src/devflow/core/deps.py",
    "f89c35b:tests/test_select.py",
    "f89c35b:tests/test_deps.py",
    "f89c35b:tests/test_tracker.py"
   ],
   "evidence_classification": "observed implementation and asserted fixtures; live tracker operation unverified",
   "proposed_target": "#50 identity-storage.md admission; #51 implementation-plan.md Selector boundary; add deterministic snapshot/classification tests.",
   "proposed_test": null
  },
  {
   "id": "SEL-B02",
   "trigger": "Selector detects unresolved dependency, quarantine/human marker, exclusion, or policy limit.",
   "preconditions": "Issue snapshot has enough fields to classify candidate.",
   "inputs_outputs": {
    "inputs": "Issue state, dependency graph, policy, exclusions.",
    "outputs": "Blocked/excluded/unresolved classification with reason; no automatic issue mutation."
   },
   "owners": "Selector computes result; operator owns source issue resolution.",
   "durable_state": "Reasons exist in response only; issue state is tracker-owned.",
   "identities": "Repository-qualified issue references and issue-number exclusions.",
   "transitions": "Candidate becomes ready only after blockers/ambiguities resolve on a later authoritative snapshot.",
   "failure_semantics": "Incomplete/invalid graph data must not be treated as ready; exact behavior for all tracker field omission cases remains implementation-specific.",
   "concurrency_restart": "Reread produces updated classification; no reservation is implied.",
   "bounds": "Configured dependency/group limits and max group size.",
   "human_action": "Edit issue criteria or explicitly remove quarantine after body is self-consistent.",
   "source_evidence": [
    "f89c35b:src/devflow/core/deps.py",
    "f89c35b:src/devflow/core/decisions.py",
    "f89c35b:src/devflow/core/refine.py",
    "f89c35b:tests/test_triage.py",
    "f89c35b:tests/test_refine.py"
   ],
   "evidence_classification": "implementation/tested contract; issue live status not claimed",
   "proposed_target": "#50 identity-storage.md admission; #51 source-version boundary; test unresolved and cross-repo dependency cases.",
   "proposed_test": null
  },
  {
   "id": "SEL-B03",
   "trigger": "Operator submits plan validate/preview.",
   "preconditions": "Plan input and repository snapshot are readable.",
   "inputs_outputs": {
    "inputs": "Plan JSON, schemas, issue/PR snapshots, policy and explicit open decisions.",
    "outputs": "Validation findings, preview/fingerprint and local artifact."
   },
   "owners": "Pure validation/core decides; CLI/MCP orchestrates reads and local artifact IO.",
   "durable_state": "Preview artifact at requested path; no remote issue write.",
   "identities": "Repository and stable plan item IDs.",
   "transitions": "draft -> validated/needs-decision -> previewed; invalid plans stop before publish.",
   "failure_semantics": "Invalid path/serialization/write errors surface; output write is bounded/confined where specified.",
   "concurrency_restart": "Repeat validation against fresh snapshot; preview content fingerprint detects change.",
   "bounds": "Schema, plan size and file-path constraints.",
   "human_action": "Resolve required decisions and inspect preview.",
   "source_evidence": [
    "f89c35b:src/devflow/core/plan.py",
    "f89c35b:src/devflow/preview.py",
    "f89c35b:src/devflow/schema/plan-0.1.json",
    "f89c35b:tests/test_plan.py",
    "f89c35b:tests/test_preview.py"
   ],
   "evidence_classification": "implementation and fixture tests; no live publication claim",
   "proposed_target": "#50 identity-storage.md; #51 lifecycle/evidence gates; test preview fingerprint and invalid-plan rejection.",
   "proposed_test": null
  },
  {
   "id": "SEL-B04",
   "trigger": "Operator runs plan publish or resumes prior publication.",
   "preconditions": "Validated preview and durable receipt exist; required user action is explicit.",
   "inputs_outputs": {
    "inputs": "Plan preview, receipt, remote listing, adopt/reset decisions.",
    "outputs": "Per-item receipt, result state, next-step instructions."
   },
   "owners": "Publisher protocol manages receipt; tracker adapter performs IO; operator resolves unknown outcomes.",
   "durable_state": "Receipt item statuses including sent/unknown/adopted and remote identifiers.",
   "identities": "Plan ID, item ID and remote marker per target repository.",
   "transitions": "pending -> sent/adopted/unknown; unknown requires verify then adopt or reset; never blindly resend.",
   "failure_semantics": "Lost create acknowledgement remains unknown; zero/multiple marker matches and content mismatch stop for manual resolution.",
   "concurrency_restart": "Same receipt resumes; serialized writer required to avoid duplicate create races (external concurrent invocation not proven).",
   "bounds": "Per-item progress; no broad automatic retry of uncertain write.",
   "human_action": "Verify remote object and explicitly adopt or confirm absence/reset.",
   "source_evidence": [
    "f89c35b:src/devflow/publish.py",
    "f89c35b:tests/test_publish.py",
    "f89c35b:tests/fixtures/plan/remote.json"
   ],
   "evidence_classification": "implementation and tests; remote live behavior not rerun",
   "proposed_target": "#51 lifecycle.md and adapter-boundaries.md; test unknown write reconciliation and idempotent replay.",
   "proposed_test": null
  },
  {
   "id": "SEL-B05",
   "trigger": "Operator invokes refine preview/apply or PR triage/sweep.",
   "preconditions": "Current issue/PR snapshot available; policy defines required checks and review signals.",
   "inputs_outputs": {
    "inputs": "Issue body, approved decisions, CI/review/marker signals, PR snapshots.",
    "outputs": "Proposed issue edits/findings or PR disposition; only approved refinement is written."
   },
   "owners": "Core refinement/triage logic classifies; tracker adapter writes approved edits; operator approves content.",
   "durable_state": "Issue/PR on remote tracker; local preview and audit result.",
   "identities": "Issue number/repository; PR head revision and declared policy.",
   "transitions": "finding -> preview -> human-approved apply; PR pending/blocked/ready/conflicting; review-fix repeats bounded by explicit sweep policy.",
   "failure_semantics": "Unknown check/review signal does not equal green; apply/write errors need reconciliation.",
   "concurrency_restart": "Fresh snapshot needed before apply/retriage; head changes invalidate prior evidence.",
   "bounds": "Policy-defined checks, sweep limits and batch size.",
   "human_action": "Answer ambiguity, approve changes, resolve conflicting or unknown signals.",
   "source_evidence": [
    "f89c35b:src/devflow/core/refine.py",
    "f89c35b:src/devflow/core/pr_triage.py",
    "f89c35b:src/devflow/core/pr_policy.py",
    "f89c35b:tests/test_refine.py",
    "f89c35b:tests/test_pr_triage.py",
    "f89c35b:bindings/devin-pr-sweep/CONTRACT.md"
   ],
   "evidence_classification": "implementation/contracts/tests; external review and CI service behavior unverified",
   "proposed_target": "#51 evidence-gates.md; test changed-head invalidation and bounded fix/review cycle.",
   "proposed_test": null
  },
  {
   "id": "INF-B01",
   "trigger": "Core authorizes a new execution and supervisor/host starts a worker.",
   "preconditions": "Work is admitted, lease/capacity held, unique execution identity allocated.",
   "inputs_outputs": {
    "inputs": "Work/Execution identity, immutable spec, runtime profile, host request.",
    "outputs": "Durable lifecycle record, execution outcome or unknown state."
   },
   "owners": "Core authorizes; supervisor/host executes; record layer persists.",
   "durable_state": "Execution identity, phase, timestamps, start acknowledgement and records.",
   "identities": "Work -> Execution -> Invocation/run; host/container IDs are adapter-local.",
   "transitions": "queued/admitted -> provisioning -> starting -> running; lost ack -> start_unknown, then reconcile.",
   "failure_semantics": "Transport loss leaves outcome unknown; slot remains held until lookup resolves.",
   "concurrency_restart": "Atomic durable records plus reconcile after restart; same identity queried before retry.",
   "bounds": "Existing per-Execution and profile time/attempt bounds; no invented retry defaults.",
   "human_action": "Resolve persistent unknown state; explicit retry after confirmed absence.",
   "source_evidence": [
    "7430304:src/lifecycle.rs",
    "7430304:src/record.rs",
    "7430304:scripts/worker/lifecycle.sh",
    "7430304:tests/lifecycle.rs",
    "7430304:test/lifecycle-test.sh"
   ],
   "evidence_classification": "implementation/tests at pinned revision; no live host success inferred",
   "proposed_target": "#51 lifecycle.md; #52 implementation plan; test lost-ack/restart/reconcile on stable Execution identity.",
   "proposed_test": null
  },
  {
   "id": "INF-B02",
   "trigger": "Cancellation requested or worker status transport is uncertain.",
   "preconditions": "An execution is active or its last state is uncertain.",
   "inputs_outputs": {
    "inputs": "Scoped execution identity and host status query.",
    "outputs": "Terminal/active/unknown observation and admission decision."
   },
   "owners": "Core chooses stop/release; adapter observes and issues cancellation; supervisor handles process.",
   "durable_state": "Cancel requested/uncertain, status, active-slot ownership.",
   "identities": "Execution plus adapter host instance.",
   "transitions": "running -> cancel_requested -> canceled/terminal or cancel_unknown; confirmed absence stops without implicit restart.",
   "failure_semantics": "Lookup failure retains slot; lost kill acknowledgement requires reconcile.",
   "concurrency_restart": "Restart reuses durable identity and reconciles; no second task takes uncertain slot.",
   "bounds": "Cancellation and observation deadlines inherited from profile; exact escalation thresholds are unresolved.",
   "human_action": "Investigate unavailable host or explicitly retry after confirmed terminal state.",
   "source_evidence": [
    "7430304:scripts/worker/lifecycle.sh",
    "7430304:src/lifecycle.rs",
    "7430304:tests/lifecycle.rs"
   ],
   "evidence_classification": "source and tests; physical host failure behavior unverified",
   "proposed_target": "#51 lifecycle.md; #52 recovery section; test lookup failure retains lease and no implicit restart.",
   "proposed_test": null
  },
  {
   "id": "INF-B03",
   "trigger": "Execution reports completion and operator/system advances output handling.",
   "preconditions": "Worker outcome and artifacts are available for verification.",
   "inputs_outputs": {
    "inputs": "Commit, immutable diff/evidence, checks, publication request, archive destination.",
    "outputs": "Verification result, separate push/PR action receipts, archived evidence, cleanup outcome."
   },
   "owners": "Core verifies/authorizes; adapter performs remote effects; worker supplies evidence.",
   "durable_state": "Checks and artifact digests/references, action receipts, PR mapping, archive status.",
   "identities": "Work/Execution/Invocation, commit/head, effect action ID, PR; artifact content digest.",
   "transitions": "completed -> verified/attention -> push -> PR -> archived -> cleanup; publication is explicit and separate.",
   "failure_semantics": "Push/PR partial success remains separately recorded; cleanup refuses missing/wrong-run/nonterminal evidence and reports credential/container cleanup failure.",
   "concurrency_restart": "Receipts support resume; artifacts/workspace retained until durable handoff and retention allows reclamation.",
   "bounds": "Profile-specific verification timeout, size/evidence limits; cleanup retention bounded by policy.",
   "human_action": "Review evidence, authorize publication, resolve partial remote effect, repair cleanup.",
   "source_evidence": [
    "7430304:src/lifecycle.rs",
    "7430304:src/record.rs",
    "7430304:scripts/worker/lifecycle.sh",
    "7430304:scripts/host/publication-validation.sh",
    "7430304:tests/publication.rs",
    "7430304:test/publication-validation-test.sh"
   ],
   "evidence_classification": "implementation/tests; recorded unvalidated image evidence is not validation success",
   "proposed_target": "#51 evidence-gates.md and adapter-boundaries.md; test partial push/PR and evidence-bound cleanup.",
   "proposed_test": null
  },
  {
   "id": "INF-B04",
   "trigger": "An execution/check/recovery attempt fails or makes no progress.",
   "preconditions": "Failure is classified and scoped to Work/Execution; durable history is readable.",
   "inputs_outputs": {
    "inputs": "Failure class, attempt history, elapsed time, confirmed process state, artifact availability.",
    "outputs": "Bounded retry/recheck or terminal Attention with preserved WIP and next action."
   },
   "owners": "Core authorizes retry and budget consumption; CLI loop drives timed observation; adapter/supervisor effects.",
   "durable_state": "Failure class, attempt/budget counters, no-progress state, recovery result and WIP.",
   "identities": "Per-Work accepted no-progress signal and per-Execution attempt budget; equivalent failure cannot reset via auto-created E.",
   "transitions": "recoverable -> bounded retry -> verified progress or Attention; irrecoverable/unknown -> hold/Attention.",
   "failure_semantics": "Unknown transport/artifact state not treated as retryable absence; repeated equivalent failures do not reset through a new E.",
   "concurrency_restart": "Restart reads counters/history and cannot reset limits; one authorized invocation/effect at a time.",
   "bounds": "Owner accepted bounded safe recovery/no-progress in v0.1; exact cross-Execution failure classes/defaults require owner consultation. Preserve existing per-E limits.",
   "human_action": "Investigate cause, repair prerequisite or approve new spec/retry after safe state.",
   "source_evidence": [
    "7430304:docs/execution/lifecycle.md",
    "7430304:src/lifecycle.rs",
    "7430304:scripts/worker/supervisor.sh",
    "7430304:test/supervisor-test.sh",
    "7430304:docs/operations/m4-observed.md"
   ],
   "evidence_classification": "source tested mechanics plus observed notes; target defaults/classifications partially unverified",
   "proposed_target": "#51 lifecycle.md and implementation-plan.md; add failure-class matrix, restart budget and no-progress tests.",
   "proposed_test": null
  },
  {
   "id": "INF-B05",
   "trigger": "Install/upgrade/recovery/cleanup or retention maintenance.",
   "preconditions": "Operator has configured host, runtime, auth and storage policy.",
   "inputs_outputs": {
    "inputs": "Validated configuration, credential references, retained evidence/WIP, cleanup policy.",
    "outputs": "Operational readiness status, repair instructions, bounded cleanup and retained audit history."
   },
   "owners": "Operator owns setup/repair; adapter validates capabilities; Core protects active/recovery-required bodies; retention executor reclaims safe bodies.",
   "durable_state": "Configuration/version records, credentials as references, durable history/evidence references, cleanup results.",
   "identities": "Installation/repository/actor and Work/Execution; secrets never become argv/evidence.",
   "transitions": "unconfigured -> validated -> ready; repairable config -> paused; terminal retained -> eligible for bounded reclamation.",
   "failure_semantics": "Insufficient permissions or failed cleanup is visible; unresolved effects, active writers, paused WIP and recovery dependencies prevent deletion.",
   "concurrency_restart": "Install/update must be resumable/idempotent; retention must not race active writer or recovery; exact process verified only by ops gate.",
   "bounds": "Configurable age/storage limits and safe reclamation; exact defaults remain unsettled.",
   "human_action": "Supply credentials/configuration, repair prerequisites, inspect cleanup result.",
   "source_evidence": [
    "7430304:docs/host/bootstrap.md",
    "7430304:docs/host/helper-update.md",
    "7430304:docs/operations/runtime-credential-setup.md",
    "7430304:scripts/host/helper-update.sh",
    "7430304:scripts/host/bootstrap.sh",
    "7430304:docs/decision-register.md"
   ],
   "evidence_classification": "design and operator docs; installation/recovery live runs not asserted",
   "proposed_target": "#51 identity-storage.md and adapter-boundaries.md; test secret redaction, protected retention and idempotent repair.",
   "proposed_test": null
  },
  {
   "id": "INF-B06",
   "trigger": "Operator collects execution evidence or requests cleanup.",
   "preconditions": "Execution has terminal status and adapter can provide same-run archive plus worker evidence.",
   "inputs_outputs": {
    "inputs": "Execution/run identity, terminal status, archive members/digests, destination, export receipt.",
    "outputs": "Verified local evidence copy and durable receipt; cleanup eligibility only after receipt matches run/incarnation and retained archive."
   },
   "owners": "Core checks run identity/terminal evidence and authorizes cleanup; adapter reads archive/deletes remote state; supervisor must be quiescent.",
   "durable_state": "Archive content/digests, copied status, run-bound export receipt, cleanup result.",
   "identities": "Work/Execution/Invocation, host/container incarnation, worker run ID and archive digest.",
   "transitions": "terminal -> export staged -> local durable copy -> attested -> cleanup eligible; any handoff failure leaves remote evidence.",
   "failure_semantics": "Copy failure means no attestation; wrong run/incarnation/status, missing evidence, symlink or oversize file refuses; delete error is visible and does not erase archive.",
   "concurrency_restart": "Same receipt supports idempotent resume; never receipt-before-copy. Active publisher/process and uncertain publication protect container/evidence.",
   "bounds": "Per-file/archive size and bounded read; retention policy controls later reclamation.",
   "human_action": "Repair destination or inspect evidence; explicitly resolve irrecoverable stale publication before cleanup.",
   "source_evidence": [
    "7430304:src/lifecycle.rs",
    "7430304:src/record.rs",
    "7430304:scripts/host/run-evidence.sh",
    "7430304:scripts/host/publication-validation.sh",
    "7430304:test/run-evidence-test.sh",
    "7430304:test/host-lifecycle-test.sh",
    "7430304:tests/lifecycle.rs"
   ],
   "evidence_classification": "implementation and offline contracts; no live archive success claimed",
   "proposed_target": "#51 evidence-gates.md and lifecycle.md; test lost export ack, wrong run/incarnation, partial local copy, retry and delete refusal.",
   "proposed_test": null
  },
  {
   "id": "INF-B07",
   "trigger": "Installation starts a Work or changes runtime profile.",
   "preconditions": "Configured host/runtime adapters expose capability query and bounded profile.",
   "inputs_outputs": {
    "inputs": "Supported operation set, actor/credential capabilities, runtime bounds, timeout and memory/disk/process budgets.",
    "outputs": "Validated capability snapshot or fail-closed refusal before dispatch."
   },
   "owners": "Core requires supported capabilities and enforces Work budget; adapter reports/enforces physical limits; operator configures host.",
   "durable_state": "Versioned config, selected runtime fingerprint, configured and observed bounds, refusal reason.",
   "identities": "Installation/Repository/Actor/Worker runtime and Work binding.",
   "transitions": "configured -> capability-checked -> dispatchable; unsupported/mismatched bounds -> held/Attention.",
   "failure_semantics": "Unreadable capability is unknown, not supported; mutable alias alone does not establish runtime identity; failed checks cannot mark artifact eligible.",
   "concurrency_restart": "Profile/runtime identity is bound per Work/Execution; config update does not mutate an active bound run.",
   "bounds": "Per-E budgets remain authoritative; exact host reserve and multi-host capacity are deferred.",
   "human_action": "Install supported adapter/runtime or adjust approved profile and revalidate.",
   "source_evidence": [
    "7430304:docs/incus/worker-runtime.md",
    "7430304:docs/host/restricted-ssh.md",
    "7430304:scripts/incus/build-validate-worker-image.sh",
    "7430304:scripts/incus/validate-profile.sh",
    "7430304:test/worker-image-validate-test.sh",
    "7430304:test/host-lifecycle-test.sh"
   ],
   "evidence_classification": "design, implementation and offline fake-host checks; live host capability proof not claimed",
   "proposed_target": "#51 adapter-boundaries.md and implementation-plan.md; test unknown capability, limit violation, runtime mismatch, and no dispatch.",
   "proposed_test": null
  },
  {
   "id": "INF-B08",
   "trigger": "Worker or repository-host adapter needs runtime/repository credentials.",
   "preconditions": "Operator config holds credential references and adapter-specific capability grants.",
   "inputs_outputs": {
    "inputs": "Secret reference, bounded job/config, adapter command and log/evidence content.",
    "outputs": "Secret material delivered only through approved channel; redacted or rejected output."
   },
   "owners": "Installation resolver owns secret access; Core defines allowed effects; adapter/runtime enforces process and network isolation.",
   "durable_state": "Credential reference and provenance only; no secret bytes in durable records.",
   "identities": "Actor, installation, repository and Work binding; separate Worker runtime credential from host mutation credential.",
   "transitions": "reference resolved at dispatch -> bounded use -> revoked/cleaned; errors remain visible and redact secret.",
   "failure_semantics": "Missing/denied capability refuses; leaked value detection fails export; inherited agent/git/network authority must be removed or explicitly bound.",
   "concurrency_restart": "Credential refresh is idempotent and run scoped; restart uses current authorized reference without serializing secret.",
   "bounds": "Credential lifetime, output limits and log/evidence redaction bounds; exact resolver deferred to installation design.",
   "human_action": "Provision/revoke correct scoped credential; repair permission and retry through new authorized invocation.",
   "source_evidence": [
    "7430304:docs/operations/runtime-credential-setup.md",
    "7430304:docs/integration/coding-worker-dsh-interface.md",
    "7430304:scripts/worker/entrypoint.sh",
    "7430304:scripts/host/ssh-transport.py",
    "7430304:test/entrypoint-test.sh",
    "7430304:test/ssh-transport-test.py"
   ],
   "evidence_classification": "mechanism/docs and offline canary assertions; target isolation remains unverified",
   "proposed_target": "#51 adapter-boundaries.md; test argv/log/archive absence, credential role separation and denied network/write capability.",
   "proposed_test": null
  },
  {
   "id": "INF-B09",
   "trigger": "Operator installs or updates a local adapter/control component.",
   "preconditions": "Pinned source revision and exact expected file set/modes/digests are available.",
   "inputs_outputs": {
    "inputs": "Revision, allowlisted files, current/previous versions and atomic staging path.",
    "outputs": "Installed version, verified rollback point, or recoverable pending state with operator guidance."
   },
   "owners": "Operator initiates; installation adapter stages/checks/switches; durable update record supports restart.",
   "durable_state": "Source revision, per-file digest/mode, pending transition and previous verified installation.",
   "identities": "Installation and adapter version; update transaction ID/revision.",
   "transitions": "preflight -> stage -> verify -> atomic switch -> record; interrupted switch -> reconcile then resume/rollback.",
   "failure_semantics": "Unexpected file/symlink/mode/digest or incomplete prior install blocks update/rollback; no blind overwrite; failed rollback remains inspectable.",
   "concurrency_restart": "Lock serializes update/rollback; same-filesystem rename; pending record written before mutation; repeated recovery idempotent.",
   "bounds": "Fixed file allowlist, file size ceiling, bounded requests/timeouts; rollout and rollback thresholds require explicit operator action.",
   "human_action": "Review exact diff/revision, apply or confirm rollback; investigate rejected integrity state.",
   "source_evidence": [
    "7430304:scripts/host/ssh-transport.py",
    "7430304:docs/host/helper-update.md",
    "7430304:test/helper-update-test.sh",
    "7430304:test/ssh-transport-test.py"
   ],
   "evidence_classification": "implementation/offline fake filesystem tests; no target installer run",
   "proposed_target": "#51 implementation-plan.md adapter boundaries; test interrupted switch replay and tampered rollback refusal.",
   "proposed_test": null
  },
  {
   "id": "TGT-B01",
   "trigger": "Operator or optional reference Selector proposes work",
   "preconditions": "Resolved source snapshot, repository allowlist, stable policy identity",
   "inputs_outputs": {
    "inputs": "Single source Issue/policy/content revision or direct intent",
    "outputs": "Proposal is not execution permission; accepted Work or bounded rejection"
   },
   "owners": "Selector proposes; host reads; Core admits",
   "durable_state": "Target proposal admission + source/spec snapshots; current Work issue_refs only",
   "identities": "Target host/repository/Issue/selector/proposal; current repo string/number",
   "transitions": "Proposed -> accepted queued E1; later dispatch independent",
   "failure_semantics": "Unknown source is hold; ambiguity remains upstream; no silent partial multi-Issue acceptance",
   "concurrency_restart": "Deterministic semantic proposal excludes mutable running labels; overlap transaction needed",
   "bounds": "Reference singleIssue; rich grouping deferred, emitted group atomic if later added",
   "human_action": "Resolve ambiguous specification upstream; explicitly resubmit after safe release",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/adapter-boundaries.md#Selector contract",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md#Admission, overlap, and source revisions",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_submit"
   ],
   "evidence_classification": "intended requirement; Selector/allowlist/source refresh absent in runtime",
   "proposed_target": "#50 admission/identity + #53 allowlist + #55 proposal CLI",
   "proposed_test": "Same snapshot/policy yields same proposal; manual/Selector overlaps rejected atomically; unknown cannot start"
  },
  {
   "id": "TGT-B02",
   "trigger": "Any state-changing Command or lost response",
   "preconditions": "Stable local actor/origin/key and required versions",
   "inputs_outputs": {
    "inputs": "Exact command payload",
    "outputs": "Saved accepted/rejected decision, changed-payload conflict, query by ID/key"
   },
   "owners": "Core decides; SQLite transaction persists",
   "durable_state": "commands decision/digest + Work/E changes + event log",
   "identities": "origin/actor/key; target adds StoreId and canonical bytes",
   "transitions": "Submit creates E1; replay no new E/version/sequence; retry only failed E -> new E",
   "failure_semantics": "Store fault propagates, not permanently hardened rejection; remote timeout stays unknown",
   "concurrency_restart": "Immediate transaction/savepoint and uniqueness; same replay after restart retains decision",
   "bounds": "Current conflict retry loop bounded; active-store decisions retained",
   "human_action": "Query original identity or replay exact request; never fresh key to evade unknown",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/command.rs:payload_digest",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:handle_command/decide_retry",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_commands.rs:218-399,853-1175,1956-2202",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/tests/store_error_semantics.rs:214-255",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_queries.rs:590-648"
   ],
   "evidence_classification": "observed implementation with passing baseline contract assertions; FNV/serde identity is legacy, not target codec",
   "proposed_target": "#50 canonical versioned byte identity/Store scope; #55 CLI result/skill replay",
   "proposed_test": "All9 commands exact replay and changed-payload; wrongStore; restart; noncommitted disk fault distinguish from unknown commit"
  },
  {
   "id": "TGT-B03",
   "trigger": "CLI tick scans queued E",
   "preconditions": "Queued state, budget/gates, no resource conflict and free slot",
   "inputs_outputs": {
    "inputs": "Saved queue order and supplied facts/clock",
    "outputs": "Ready E + capacity/reservations + durable request in one commit, otherwise deferral"
   },
   "owners": "Core checks; SQLite enforces structure; CLI drives",
   "durable_state": "E enqueue seq, slots, resources, invocation_requests, deferrals",
   "identities": "E/request/workspace/(repository,branch)",
   "transitions": "queued -> ready; blocked head stays queued while independent tail proceeds",
   "failure_semantics": "Constraint/state/capacity conflict rolls back that item; internal store error aborts; new source unavailable must defer",
   "concurrency_restart": "Savepoint per item; uniqueness protects concurrent commits",
   "bounds": "Single-installation fixed capacity; finite full queued scan currently unpaged",
   "human_action": "Inspect capacity holder/conflict diagnostics; no resubmit needed",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:admit_queued_role/dispatch_once",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/src/schema.sql",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_commands.rs:456-583,1538-1593,2862-end",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_queries.rs:531-589,670-741"
   ],
   "evidence_classification": "implemented+baseline asserted for local store; real unavailable/source gates not implemented",
   "proposed_target": "#50 scoped atomic admission; #55 bounded scheduling/source reads",
   "proposed_test": "Concurrent admission one slot/request; unstartable head cannot starve; stale generation prevents dispatch; finite page/checkpoint policy"
  },
  {
   "id": "TGT-B04",
   "trigger": "Dispatch/retry or manual claim/release",
   "preconditions": "Declared branch/workspace; external claim branch-only, positive TTL",
   "inputs_outputs": {
    "inputs": "Actor/resource/TTL and existing owner",
    "outputs": "Exclusive holder or ResourceConflict with owner"
   },
   "owners": "Core authorizes claims; SQLite unique resource key",
   "durable_state": "Reservations retained through paused/uncertain/stopping; safe terminal removes only own",
   "identities": "Current colon-concatenated resource strings; target structured scoped tuple",
   "transitions": "Claim -> held -> renewed/released/expired; expiry forfeits guarantee, never proves death",
   "failure_semantics": "Unknown process/ownership prohibits replacement; arbitrary contract-bypassing writer outside guarantee",
   "concurrency_restart": "Dispatch rechecks claims; replay does not renew; owner-only release",
   "bounds": "External TTL; no perhost/multirepo capacity",
   "human_action": "Manual writer claims before writes; repair ownership conflict without assuming TTL killed writer",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:work_resources/decide_claim/decide_release/decide_retry",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_commands.rs:408-455,604-638,793-852,993-1042",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md#Concrete identity model"
   ],
   "evidence_classification": "implemented local guards; target identity/permission enforcement unverified",
   "proposed_target": "#50 tuples/Issue overlap; #53 managed-root isolation; #52 stop contract",
   "proposed_test": "Colon/unicode IDs cannot collide; wrongholder release no effect; paused reservations remain; TTL is not stop proof"
  },
  {
   "id": "TGT-B05",
   "trigger": "Supervisor consumes approved request or restarts",
   "preconditions": "Approved durable request; fresh Ready required to launch",
   "inputs_outputs": {
    "inputs": "Saved request, worker argv, cwd, process evidence",
    "outputs": "Prepared/Running/Abandoned/Unknown Invocation + typed observations"
   },
   "owners": "Core transition authority; supervisor process I/O",
   "durable_state": "Request consumption + Invocation before spawn; process identity after spawn",
   "identities": "Request/I/E and PID/startstamp/group",
   "transitions": "Ready -> running; definite refusal -> repair pause; lost spawn -> uncertain",
   "failure_semantics": "Prepared/no identity cannot authorize second start; done claim not complete Outcome",
   "concurrency_restart": "Unique request and unresolved writer indexes; perrequest local mutex; restart reconciles existing identity",
   "bounds": "Invocation budget counted at request; runtime firstdispatch anchor",
   "human_action": "Repair definite refusal; investigate unknown; no blind retry",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:reconcile_or_start/poll_invocations",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:assess_process",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/tests/supervisor_contract.rs:361-661,981-1147,1752-1837"
   ],
   "evidence_classification": "observed POSIX fixture implementation and baseline assertions; real Worker task/report absent",
   "proposed_target": "#52 typed real Worker protocol; #50 immutable request/spec; #55 loop restart",
   "proposed_test": "Crash each request/prepare/spawn/identity/report boundary; no duplicate writer; Worker summary cannot settle success"
  },
  {
   "id": "TGT-B06",
   "trigger": "Cancel/runtime failure or recovery requests stop",
   "preconditions": "Correlated known process identity; complete descendant snapshot",
   "inputs_outputs": {
    "inputs": "PID/start/group and saved desired action",
    "outputs": "Durable StopIntent before signal; Requested != Stopped; settle only after proof"
   },
   "owners": "Core desired state; supervisor snapshots/signals/inspects; store persists",
   "durable_state": "stopping/uncertain plus stop_intents identities",
   "identities": "I and all recorded descendant identities including setsid escape",
   "transitions": "running -> stopping -> uncertain or safely terminal; unknown holds resources",
   "failure_semantics": "Persistence/snapshot failure sends no signal; failed lookup unknown; reusedPID never signaled; zombie stopped",
   "concurrency_restart": "Restart uses durable snapshot even after leader reparent/exit; reap own children",
   "bounds": "Current SIGKILL immediate; uncertainty threshold surfaces Attention, never proves death",
   "human_action": "Investigate unresolved identity; acknowledgment does not free resources",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/ports.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:persist_stop_snapshot/stop_unless_persisted/drive_stops",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/tests/supervisor_contract.rs:169-360,1148-1313,1412-1751,1838-2008"
   ],
   "evidence_classification": "implemented local POSIX contract; no general runtime/provider guarantee",
   "proposed_target": "#52 process contract + #55 stop/reconcile scheduling; #51 terminal proof",
   "proposed_test": "Fail durable intent before any signal; escapee survives groupkill and blocks release; restart at firstsignal; unknown preserves slot"
  },
  {
   "id": "TGT-B07",
   "trigger": "Worker chooses planner phase",
   "preconditions": "Optional adapter capability",
   "inputs_outputs": {
    "inputs": "Task and read-only planner tools",
    "outputs": "Plan evidence, not remote mutation or lifecycle decision"
   },
   "owners": "Worker adapter internal",
   "durable_state": "Plan/checkpoint evidence reference if used",
   "identities": "I/task/spec",
   "transitions": "Optional plan -> code within bounded task",
   "failure_semantics": "Planner writes violate configured policy; current target does not enforce planner tools",
   "concurrency_restart": "Adapter restart contract must preserve outer request identity",
   "bounds": "All elapsed work within Invocation limits",
   "human_action": "Configure optional hardened Worker; no mandatory v0.1 planner service",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/adapter-boundaries.md#Worker pipeline disposition",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/source-workflow-audit.md#Execution"
   ],
   "evidence_classification": "deferred intended hardening; not current implementation",
   "proposed_target": "#52 optional worker internal hardening",
   "proposed_test": "If claimed, write tool refused and no host mutation credentials"
  },
  {
   "id": "TGT-B08",
   "trigger": "Writer exits or artifact/check evidence arrives",
   "preconditions": "Exact produced change plus stopped writers",
   "inputs_outputs": {
    "inputs": "Artifact/gitobjects/digest, declared command requirements and actual result",
    "outputs": "Validated exact evidence; valid negative vs invalid vs absent vs unavailable"
   },
   "owners": "Worker produces; supervisor transports; Core evaluates",
   "durable_state": "Target immutable envelope/PreparedChange/VerificationResult; current nonempty filename facts",
   "identities": "Store/W/E/I/spec/artifact/repo/base/produced/argv",
   "transitions": "Exit -> validation -> conformance or repair -> publication eligibility",
   "failure_semantics": "Current artifacts_complete accepts any historical Present; fs validation only nonempty; no digest/schema/commit binding. These are defects/gaps, not intended validity",
   "concurrency_restart": "Each new artifact gets fresh proof; prior I/root files cannot be relabeled current",
   "bounds": "Invocation cap bounds repairs; risk-manifest extra gate deferred",
   "human_action": "Provide missing evidence/repair negative results; do not relabel corrupt as valid",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/projection.rs:artifacts_complete",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:finish_invocation/find_artifact/artifact_present",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/publication_recovery.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Evidence schema and validity"
   ],
   "evidence_classification": "legacy presence implementation + intended stronger requirement; no exact-byte real verification proof",
   "proposed_target": "#51 evidence + #52 transport + #54 dependency retention",
   "proposed_test": "WrongI/spec/repo/commit/digest rejected; later corrupt invalidates earlier Present; actual commands on retained git objects; valid nonzero is negative"
  },
  {
   "id": "TGT-B09",
   "trigger": "Confirmed exit with recoverable missing evidence",
   "preconditions": "All writers proven safe; recoverable source exists; no permanent invalid claim",
   "inputs_outputs": {
    "inputs": "Workspace/checkpoint/runtime artifact source and remaining attempt/deadline",
    "outputs": "Recorded recovery attempt/result or failure with WIP preserved"
   },
   "owners": "Supervisor collection; Core decides exhaustion/repair eligibility",
   "durable_state": "E recovery_attempts/deadline; observations; preserved WIP",
   "identities": "E/I/artifact and dependency references",
   "transitions": "WaitingArtifact -> found evidence -> reassess; exhausted -> safe fail",
   "failure_semantics": "Current Missing and Corrupt conflated; run_recovery scans every I by filename, allowing old artifact attribution. Preserve as known gap; no permanent-refusal class yet",
   "concurrency_restart": "Attempt/deadline durable, restart does not reset; unresolved effects/writers must retain safety holds",
   "bounds": "Current3attempts/30min perE separate from I; no newly invented recovery defaults",
   "human_action": "Repair/retry only after explicit preconditions and safe ownership",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:assess_artifact/sweep/fail_terminal/decide_resume",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:run_recovery",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/tests/supervisor_contract.rs:662-873",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Outcome, counters, and repair"
   ],
   "evidence_classification": "implemented bounded filename collection; broader safety/permanent classification missing",
   "proposed_target": "#51 typed repair; #52 artifact source; #54 retained bodies; #55 bounded recovery scheduler",
   "proposed_test": "Missing may recollect; wrongsubject/corrupt cannot promote; no recovery while writer live; original I identity and all unresolved effects guard terminal"
  },
  {
   "id": "TGT-B10",
   "trigger": "Validated prepublication gates authorize effects",
   "preconditions": "Accepted immutable target/actor/capability; exact artifact; no cancel/budget prohibition",
   "inputs_outputs": {
    "inputs": "Separate guarded push/create/update/comment/reply/thread/ready intents",
    "outputs": "Per-effect applied/failed/unknown receipt, no inferred stage success"
   },
   "owners": "Core authorizes; host adapter only remote writer; Worker prepares",
   "durable_state": "Outbox intent before I/O and append-only matching receipts in target",
   "identities": "Action/payload/original actor/binding/target revision",
   "transitions": "Prepared -> push applied -> create -> review/fix; unknown -> read-only reconciliation",
   "failure_semantics": "Current fixture combined publish_pr synthetic head, no real push/marker/ready receipt; classifier only produced/pushed equal-or-ancestor",
   "concurrency_restart": "Unknown must not resend; applied partial success survives restart/config drift",
   "bounds": "Each transport/reconciliation separately bounded; no exactly-once remote claim",
   "human_action": "Investigate ambiguous effects; retain original binding/config; no manual blind push",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:maybe_publish",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/src/fixture.rs:apply_outbox/attempt",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/stage.rs:ConvergeRecord::classify",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/tests/stage_contract.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/tests/fixture_adapter.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Stage effects and reconciliation"
   ],
   "evidence_classification": "fixture local implementation/classifier baseline; real host operations absent",
   "proposed_target": "#51 effect facts; #50 binding FKs; #53 credentials; #55 reconciliation",
   "proposed_test": "Push succeeds/create unknown survives restart without duplicate push/create; mismatch actor/payload rejected; no forcepush/merge; cancel prevents dependent new write"
  },
  {
   "id": "TGT-B11",
   "trigger": "Input/adapter/process failure or result/evidence export",
   "preconditions": "Boundary parser and credential isolation",
   "inputs_outputs": {
    "inputs": "Potentially untrusted reasons, remote output, credential refs",
    "outputs": "Bounded codes/stable safe references; secret values absent"
   },
   "owners": "Adapters redact/normalize; config isolates; Core uses typed reasons",
   "durable_state": "Target retained safe reasons and protected content; current freeform String reasons",
   "identities": "CredentialRef != actor identity; exact evidence provenance",
   "transitions": "Raw failure -> classified bounded report; secret bodies not exported",
   "failure_semantics": "Current ps/spawn/store errors/free-text intent may reach logs/evidence; inherited env/stdout unchecked. Docs do not prove isolation",
   "concurrency_restart": "Restart uses saved refs, never secrets-on-argv; exports cannot leak prior source engagement",
   "bounds": "Reason/payload/body/log bounds and retention require #53/#54 choices",
   "human_action": "Configure scoped credentials and inspect denied setup; no generic permission bypass",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/observation.rs:LaunchRefused",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/store.rs:StoreError",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:spawn/checked_ps_output",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/architecture.md#Installation boundary",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/implementation-plan.md:R24/R38"
   ],
   "evidence_classification": "known unimplemented security/export requirement; no production proof",
   "proposed_target": "#53 configuration/isolation + #52 Worker env + #54 storage + #55 output",
   "proposed_test": "Secrets in argv/env/error/stdout/source body never appear in public result/export; bounded reason codec rejects arbitrary remote text"
  },
  {
   "id": "TGT-B12",
   "trigger": "Current review arrives; operator judges/releases; next fix dispatch",
   "preconditions": "Complete current source snapshot and exact head/base/spec/generation",
   "inputs_outputs": {
    "inputs": "Immutable round/occurrences and per-occurrence decisions/rationale",
    "outputs": "Exact-set judgment and durable disposition history; stale rejection"
   },
   "owners": "Core validates in acceptance transaction, TUI/skill only collects",
   "durable_state": "Current judgments keyed(W,head); target immutable round/source/occurrence ledger",
   "identities": "W/spec/target head/base/round/source/occurrence/generation",
   "transitions": "New material evidence -> new round/generation; old judgment historical",
   "failure_semantics": "Current Core validates nonempty/unique only, not membership/coverage; samehead growth can omit B; stringified reviewer verdicts are not stable findings",
   "concurrency_restart": "Two-connection judgment/release/dispatch races must invalidate stale generation; same content duplicate no churn",
   "bounds": "Review rounds bound active remediation; no automatic acceptance on exhaustion",
   "human_action": "Explicit current Dismiss/Accept rationale or Fix/Wait; reapproval cannot be inferred",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_judge/decide_release_judgment/admit_queued_role",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/projection.rs:work_detail",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/judgment.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/tests/tui_surface.rs:830-893",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Immutable review snapshots and finding identity"
   ],
   "evidence_classification": "known #24 runtime defect; local TUI protections baseline only; target design not implemented",
   "proposed_target": "#51 immutable rounds and exactset AC; #50 source/spec; #55 CLI guard",
   "proposed_test": "Samehead A->A+B between inspect/submit, judgment/release, release/dispatch rejects old set even guessed version; omitted/extra/foreign/duplicate all reject"
  },
  {
   "id": "TGT-B13",
   "trigger": "Exact artifact mechanically verified, then optional checks/review finish",
   "preconditions": "Approved policy/spec/invariants and current exact diff",
   "inputs_outputs": {
    "inputs": "Mechanical results; defaultON invariant lane BEFORE publication; independently optional code review AFTER; explicit CI names",
    "outputs": "Typed CompletionProof or bounded fix/Attention; disabled lanes recorded unperformed"
   },
   "owners": "Core gates; configured Workers assess; host facts supply CI",
   "durable_state": "Target lane policy + immutable assessments + outcome proof; runtime none",
   "identities": "Spec/invariantset/artifact/head/base/check producer/run and effect IDs",
   "transitions": "Code -> mechanical -> invariant if enabled -> publish ready -> review if enabled -> bounded fix -> proof",
   "failure_semantics": "Current assess_completion unconditionally requires aggregate CI Success+native Approve, uses SHA only and empty evidence_refs; obsolete defects, not target policy",
   "concurrency_restart": "New head/base/spec requires fresh applicable proof; historical Outcome immutable",
   "bounds": "Existing perE I/review/runtime limits retained; no refund/reset on sameE",
   "human_action": "Resolve remaining finding/inconclusive requirement; explicit invariant/policy reapproval required to change requirement",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:assess_completion",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/domain.rs:ExecutionBudget",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Owner-approved lanes and caller flow",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#CI identity and requirement snapshot",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Outcome, counters, and repair"
   ],
   "evidence_classification": "intended owner policy versus incompatible old runtime; real lane orchestration absent",
   "proposed_target": "#51 lane proof/outcome; #52 task purpose; #53 defaults; #55 scheduling",
   "proposed_test": "All4 lane combinations and CIempty/namedproducer/rerun; no native Approve/RequestChanges/draft barrier; missing proofbody cannot complete"
  },
  {
   "id": "TGT-B14",
   "trigger": "CLI or TUI/installed skill submits/inspects",
   "preconditions": "Same installation config and shared Command/Query ports",
   "inputs_outputs": {
    "inputs": "Typed actions, version/head/replay identity",
    "outputs": "Machine results, current detail/capacity/Attention and explicit failure/unknown"
   },
   "owners": "CLI/skill translates; Core decides; projection reads",
   "durable_state": "Store durable truth; TUI draft/staged/pending only local selection",
   "identities": "Command origin/actor/key and exact seen Work version/head",
   "transitions": "Inspect -> stage -> advisory preview -> submit -> query/replay recovery",
   "failure_semantics": "Missing review source != clean; TUI local error and landedbutlost response distinct; unrelated commandID cannot clear pending",
   "concurrency_restart": "Conversation/UI closure never owns/cancels worker; no fresh key while unknown",
   "bounds": "CLI loop ticks/interval configurable; Query pagination not implemented despite design",
   "human_action": "Use direct CLI fallback; install supported named-agent skill when delivered",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-cli/src/main.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/src/session.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-tui/tests/tui_surface.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_preview.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/agent-interface.md"
   ],
   "evidence_classification": "implemented CLI/TUI fixtures; required skill/install absent, MCP deferred",
   "proposed_target": "#55 CLI/skill schema; #53 installation; TUI delivery deferred",
   "proposed_test": "Installedskill and CLI same contracts; malformed/config/store distinguish accepted/rejected/error/unknown; closing session leaves Work queryable"
  },
  {
   "id": "TGT-B15",
   "trigger": "Paused/failure or continuous uncertainty exceeds threshold",
   "preconditions": "Saved current evidence and supplied clock",
   "inputs_outputs": {
    "inputs": "CurrentE/observations/outcome/ack",
    "outputs": "Current actionable next actor/action, source availability"
   },
   "owners": "Projection derives; Core validates acknowledge",
   "durable_state": "Acknowledgement only; no separate mutable alert truth",
   "identities": "Attention version tied state/evidence occurrence",
   "transitions": "Current action -> acknowledged hidden -> material new action resurfaces",
   "failure_semantics": "Current occurrence counts all observations, so unrelated polls can churn Attention; target material generation avoids this. Queue/ordinaryCI wait is not automatic Attention",
   "concurrency_restart": "Restart derives same action; ack never releases gate/resource",
   "bounds": "Current uncertainty15min, distinct from new no-progress threshold",
   "human_action": "Investigate/decide current action; acknowledge only means seen",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/projection.rs:attention_views/derive_attention",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_acknowledge",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/contract_queries.rs:257-530",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Same-head growth and exact-set guards"
   ],
   "evidence_classification": "implemented+baseline assertions; new material-generation semantics target only",
   "proposed_target": "#51 material evidence generations; #55 operator action presentation",
   "proposed_test": "Duplicate identical polls no new Attention; new failure/required action resurfaces; ack cannot approve spec or free unknown writer"
  },
  {
   "id": "TGT-B16",
   "trigger": "First CLI/TUI use or explicit config load",
   "preconditions": "Writable local config parent and valid existing file",
   "inputs_outputs": {
    "inputs": "Actor/repo/capacity/workerargv/workspace/allowedEffects/budget",
    "outputs": "Stable shared actor or explicit malformed config failure"
   },
   "owners": "Config boundary resolves, Core consumes validated data",
   "durable_state": "Atomic scratch synced content + hardlink/flock/rename fallback",
   "identities": "Current actor nanos/PID; target Store/host/repo/actor/credential refs",
   "transitions": "Absent -> complete published config; existing adopts",
   "failure_semantics": "Empty/malformed foreign file never deleted; unknown actor kind currently defaults Human; no full role/capability schema",
   "concurrency_restart": "Concurrent firstwriters share winner; OS releases file lock on crash",
   "bounds": "Missing actual installer/skill and resource pressure bounds; setup.sh installs mutable stable toolchain without external timeout",
   "human_action": "Repair malformed installation explicitly; choose defaults where required",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-config/src/lib.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-config/tests/installation.rs",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:scripts/worker/setup.sh",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:README.md"
   ],
   "evidence_classification": "implemented config atomicity with baseline tests; complete secure setup missing",
   "proposed_target": "#53 config/credentials/installer + #54 pressure + #55 skill walkthrough",
   "proposed_test": "Concurrent bootstrap winner; wrongschema/unknown actor fail; no overwrite foreign config; clean install and actual skill discovery"
  },
  {
   "id": "TGT-B17",
   "trigger": "Admission/firstdispatch authoritative read detects changed/unavailable source",
   "preconditions": "Source membership fixed and binding sealed",
   "inputs_outputs": {
    "inputs": "Actual source revision/content/read observation plus approved spec/invariantset",
    "outputs": "Versioned hold or exact matching approved request; changed source new spec sameWork"
   },
   "owners": "Core requires/judges, adapter reads, CLI drives",
   "durable_state": "Target immutable Work binding/spec snapshots/approvals/activeIssueClaims; absent runtime",
   "identities": "Store/host/repo/Issue/spec/read generation",
   "transitions": "ApprovedV1 -> changehold -> immutableV2 -> renewed approval; no live request rewrite",
   "failure_semantics": "Unknown cannot mean unchanged; postfinal-read remote race accepted boundary; replan cannot enlarge actor/permissions/budget",
   "concurrency_restart": "Local newer mismatch invalidates earlier match atomically; overlap claim held through pause/unknown; retry reacquires",
   "bounds": "SameE counters/runtime unchanged; optional future auto regeneration deferred",
   "human_action": "Approve exact new spec/invariants; new binding requires separately authorized new Work",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md#Admission, overlap, and source revisions",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_submit/work_resources/decide_retry",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Owner-approved lanes and caller flow"
   ],
   "evidence_classification": "owner-approved target design; runtime missing and config defaults currently reused",
   "proposed_target": "#50 identity/overlap/refresh + #51 approval evidence + #55 commands",
   "proposed_test": "Changed/unavailable beforedispatch holds; afterlastread race documented; manual duplicate Issue keys reject; retry after newer owner rejected"
  },
  {
   "id": "TGT-B18",
   "trigger": "Operator upgrades unversioned legacy runtime",
   "preconditions": "All supported launch/effect routes stopped; writers/effects safe; storage preflight",
   "inputs_outputs": {
    "inputs": "SQLite oldstore+WAL/WIP, explicit archive/new paths and new config",
    "outputs": "Read-only consistent archive + fresh StoreId/newWork; no executable import"
   },
   "owners": "Operator maintenance; setup preflight; new store refuses unknown formats",
   "durable_state": "External archive checksum/counts/sequence manifest; new transactional schema bootstrap",
   "identities": "Archive identity distinct StoreId, fresh W/E/I/branch/workspace namespace",
   "transitions": "Maintenance -> quiesce -> archive -> init new -> validate -> activate",
   "failure_semantics": "Unknown writer/effect blocks overlapping cutover; no copy mainDB ignoringWAL; diskfull no prune; no fallback old executable after new activity",
   "concurrency_restart": "Crash retains maintenance, incomplete neverused store may reinit; accepted new activity resumes that Store",
   "bounds": "Archive/WAL/temp/WIP capacity accounted; retention default owner review",
   "human_action": "Schedule maintenance; resolve old unknown through evidence; explicit new submissions",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md#Maintenance cutover and legacy reference",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/src/sqlite.rs:init",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/repository-host-contract.md#Verification, release scope, and migration"
   ],
   "evidence_classification": "intended owner design; current init mutates/seeds unversioned stores without metadata check",
   "proposed_target": "#50 cutover/version/Store identity + #53 retired routes + #54 archive pressure",
   "proposed_test": "WALconsistent checksum/counts/rawdecisions; unknown/newer refused without writes; no oldkey replay/import; restartbefore/after activation"
  },
  {
   "id": "TGT-B19",
   "trigger": "Retention/storagepressure cleanup or evidence read",
   "preconditions": "Dependency graph of activeWIP/approval/replay/recovery/unknown effects",
   "inputs_outputs": {
    "inputs": "Artifact/gitobjects/source bodies, immutable metadata and configured age/size",
    "outputs": "Only eligible bodies reclaimed, result recorded; missingbody unavailable"
   },
   "owners": "Retention boundary manages bodies; Core evidence guards depend on availability",
   "durable_state": "Stable metadata/refs/reasons; required bodies pinned; archive history preserved",
   "identities": "Evidence/contentdigest/Store/W/E/I/action dependency",
   "transitions": "Available -> eligible compact/move/reclaim -> explicit unavailable; no synthetic body",
   "failure_semantics": "Digest alone cannot replace reconstructable required bytes; no deleting recovery evidence to satisfy pressure",
   "concurrency_restart": "Crashsafe catalog/reclamation work; no deletion under active writer/unknown effect",
   "bounds": "Configurable age/storage/history bounded; defaults require owner decision",
   "human_action": "Choose pressure/retention policy; blocked cleanup surfaces actionable tradeoff",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/identity-storage.md:215-221,247-287",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md:101-123,267-267,354-362",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/v0.1.md#Isolated environment, rerun, and cleanup"
   ],
   "evidence_classification": "intended requirement; no runtime retention implementation",
   "proposed_target": "#54 retention/history/catalog and #51 body-dependent evidence",
   "proposed_test": "Pressure cannot delete pinned bodies; reclaimed optional body stays unavailable not valid; archive metadata identity intact"
  },
  {
   "id": "TGT-B20",
   "trigger": "Active Work repeats activity or waits without material advancement",
   "preconditions": "Saved semantic progress facts and supplied clock; never infer process death",
   "inputs_outputs": {
    "inputs": "Last material progress generation/attribution and wait classification",
    "outputs": "Versioned no-progress diagnostic/current Attention per chosen policy"
   },
   "owners": "Core defines progress predicate; CLI schedules reads; adapters report facts",
   "durable_state": "New perWork progress episode/state required; absent current E fields",
   "identities": "W/spec/stage/episode and material evidence generation",
   "transitions": "Progress resets episode; repeated polls/identicalfailure do not; no-progress != deadline",
   "failure_semantics": "Existing15min uncertainty timer covers unknown process only, not CI/review/recovery stalls; docs deferred text now obsolete",
   "concurrency_restart": "Restart persists lastprogress; late/duplicate facts cannot erase stalled episode",
   "bounds": "No new numerical defaults chosen; owner must select duration by wait/stage",
   "human_action": "Investigate cause; no forced terminal/kill from diagnostic alone",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/domain.rs:ExecutionBudget/Execution",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/projection.rs:attention_views",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/implementation-plan.md:104,131,230",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/v0.1.md:208",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/source-workflow-audit.md#Stalled convergence"
   ],
   "evidence_classification": "new owner-accepted v0.1 requirement; missing runtime/design details; old deferral is conflict",
   "proposed_target": "#50 durable progress episode + #51 material evidence + #55 bounded observation/no-progress policy",
   "proposed_test": "Long-running liveworker/CIpending/repeatedrecovery/unknownoutbox each classified; identicalpoll no progress; restart no reset; threshold does not free resources"
  },
  {
   "id": "TGT-B21",
   "trigger": "Transient failure/timeout becomes eligible for safe automatic recovery",
   "preconditions": "Classified retryability; writers/effects safe; approved policy; crossE recovery budget",
   "inputs_outputs": {
    "inputs": "Failure fingerprint/cause/prerequisite proof + prior automatic attempts",
    "outputs": "Recorded bounded retry/recheck/park/Attention, never unbounded fresh E"
   },
   "owners": "Core decides; supervisor/host performs; CLI schedules",
   "durable_state": "New perWork recovery episode/fingerprint/attempt ledger, absent current state",
   "identities": "W/recoveryEpisode/failureClass/E chain/action/I",
   "transitions": "Definite transient -> reconcile/repair -> eligible automatic newE if authorized; repeated equivalent failure -> halt",
   "failure_semantics": "Unknown effects/start never definite failure; permanent refusal/invalid artifact requires intervention; no-progress alone not stop permission",
   "concurrency_restart": "Restart/reapproval/newE does not reset equivalentfailure automatic budget; manual retry remains explicit existing behavior",
   "bounds": "Keep10I/5review/6h perE etc; owner chooses new failure classes/crossE retry ceilings, no random defaults",
   "human_action": "Choose recoverable classes/ceilings/backoff; explicit operator override recorded if allowed",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_retry/decide_resume/sweep",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-exec/src/supervisor.rs:run_recovery",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/source-workflow-audit.md#Stalled convergence",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md#Outcome, counters, and repair"
   ],
   "evidence_classification": "new owner-accepted v0.1 requirement; current retry resets E budgets by explicitCommand only, no automatic ledger",
   "proposed_target": "#50 durable recovery episodes + #51 typed failure/prerequisite evidence + #53 defaults + #55 recovery loop",
   "proposed_test": "Equivalentfailure E1->E2 keeps crossE count; max stops; timeout stop proof before requeue; permanent/unknown never blind retry; restart/duplicate tick no duplicate E"
  },
  {
   "id": "TGT-B22",
   "trigger": "Cancel/budget exhaustion during launch/effect/repair",
   "preconditions": "Saved desiredaction; all possible writers/actions accounted",
   "inputs_outputs": {
    "inputs": "Actual stopproof, pending/unknown/applied effect identities",
    "outputs": "Stopping/uncertain hold until safe; no new dependent writes; immutable partial receipts"
   },
   "owners": "Core gates terminal/new actions; supervisor proves stop; host read-only reconciles",
   "durable_state": "Desiredaction + remaining effects + proof + preserved WIP",
   "identities": "Exact E/I/action/original actor and receipt subjects",
   "transitions": "Cancel/limit -> stop/reconcile -> safe cancelled/failed transaction",
   "failure_semantics": "Known legacy gaps: fixture applies pending publish after cancel; paused/queued directcancel and exhaustedresume lack complete outstanding-effect/writer checks; fail_terminal checks actions but not unresolvedI. Do not retain those as intended",
   "concurrency_restart": "Cancel races request/spawn/effect/result safely; observation/result must not bypass proof; restart retains holds",
   "bounds": "Exhausted limits prohibit newtrials even while reads reconcile; uncertain cannot time out into freedom",
   "human_action": "Resolve unknown facts, no forcedrelease on elapsed time",
   "source_evidence": [
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-core/src/work.rs:decide_cancel/decide_resume/fail_terminal/settle_after_writer_gone",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/src/fixture.rs:attempt",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-github/tests/fixture_adapter.rs:887-1076",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:crates/workloop-store/tests/review_reconciliation.rs:241-272",
    "dd6b344ed4e5ada5b5bdf6df0e3ae76ceac9df90:docs/evidence-gates.md:251-253,360-364"
   ],
   "evidence_classification": "legacy local assertions include obsolete publication-on-cancel behavior; target safety gaps must be fixed",
   "proposed_target": "#51 proof closure + #50 structural guards + #55 cancellation/recovery scheduling",
   "proposed_test": "Every state plus pendingrequest/livechild/unknownaction matrix; cancel prevents fresh apply, read-only lookup resolves original effect; budgetfailure cannot release live writer"
  },
  {
   "id": "ops-B022",
   "trigger": "scheduled mint or lane before/during publication",
   "preconditions": "canonical allowlist/config interpreters; private key readable owner-only; configured token name; intended repo scope known",
   "inputs_outputs": "App credential ref + allowlist/live-record scope → JWT/API token then atomic owner-only env replacement; worker credential file refs only",
   "owners": {
    "decision": "credential integration mints; coordinator stages; Worker does not own renewal policy",
    "effect": "credential integration mints; coordinator stages; Worker does not own renewal policy"
   },
   "durable_state": "protected env file; source run phase/publication status; original actor identity must remain target durable authority",
   "identities": "installation+repo names+configured env name+run publication phase; target binds actor/credential reference per effect",
   "transitions": "mint→file promotion→stage/refresh; fail preserves prior env; live/retained run keeps scope; source final/settled record or terminal untouched past stall drops scope",
   "failure_semantics": "unknown/corrupt run record ignored in source scope union; no scope means refuse; deterministic4xx not retried,5xx/429/transport gets one retry; unexpected errors type-only; rewrite atomic but no cross-writer lock observed",
   "concurrency_restart": "concurrent cron/lane mint possible; os.replace prevents torn file but does not serialize read-modify-write or guarantee freshest token wins; environment value shadows rotated file",
   "bounds": "source API30s,2attempts with60s backoff; terminal publication scope horizon3600s; these are not approved target defaults",
   "human_action": "credential expiry/permission failure is repair prerequisite; revoke/rotate through adapter without replaying unknown effects or reattributing actor",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "docs/cw-github-token-refresh.md:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/cw-github-token-refresh.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/install-cw-github-token-refresh.sh:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "tests/test_cw_github_token_refresh.py:entire file"
    },
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_remote_lib.py:338-414,477-503,938-951"
    }
   ],
   "evidence_classification": "observed code and complete isolated credential tests read; no live secrets/mint performed",
   "proposed_target": {
    "owner_issue": "issue52",
    "section_or_ac": "Credential renewal preserves actor/scope and never retires unresolved effects by age",
    "proposed_test": "rotation keeps original actor and scope for outstanding action; expired/unknown renewal holds; time alone never retires unknown publication; concurrent renewal cannot lose unrelated credential config"
   },
   "proposed_test": "rotation keeps original actor and scope for outstanding action; expired/unknown renewal holds; time alone never retires unknown publication; concurrent renewal cannot lose unrelated credential config"
  },
  {
   "id": "ops-B023",
   "trigger": "remote launch/poll/collect",
   "preconditions": "caller already persisted run identity; immutable validated base; owned job/run; typed operation outcome",
   "inputs_outputs": "job+run+attempt/base key → admit/provision/start,status/reconcile,collect response",
   "owners": {
    "decision": "coordinator lifecycle; infrastructure adapters effects; caller persists each phase",
    "effect": "coordinator lifecycle; infrastructure adapters effects; caller persists each phase"
   },
   "durable_state": "remote record phase,start_acked,uncertain,status,termination_reason,evidence_dir,exported; coordinator mirror",
   "identities": "repo-qualified job; generated durable run before launch; key job:attempt:base",
   "transitions": "created→admit→provision→start→running/uncertain→terminal→collect; proven no-record may bounded relaunch; unknown stays same identity",
   "failure_semantics": "missing must bind exact job/run+refusal code; status outage reconciles then mirror; permanent collect-refusal differs from transient; raw stderr detail retained up to200 in some library returns",
   "concurrency_restart": "new run cannot replace uncertain bound identity; store scan exceptions in review_fix_attempts/live_remote_runs currently become empty list, unlike strict quarantine fence",
   "bounds": "configured remote iterations and collect attempt/time bounds; source review_fix_attempts counts attempts even parked record; target Work budget required",
   "human_action": "permanent refusal/unknown effects require diagnostic/operator action; no blind retry after lost ack",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_remote_lib.py:entire file"
    }
   ],
   "evidence_classification": "observed complete library read; caller orchestration/tests pending",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Uncertain remote admission and collection use stable identity before any replacement",
    "proposed_test": "lost start ACK never spawns replacement; scan error cannot reset counted failures/capacity; permanent collection refusal surfaces bounded Attention"
   },
   "proposed_test": "lost start ACK never spawns replacement; scan error cannot reset counted failures/capacity; permanent collection refusal surfaces bounded Attention"
  },
  {
   "id": "ops-B024",
   "trigger": "bounded worker complete or no-op converge",
   "preconditions": "exact run identity; preserved artifact; approved scope; required configured checks; separate publication state",
   "inputs_outputs": "worker result+worker-evidence mapping+publication summary→posted/not_ready/failed/unknown and stage classification",
   "owners": {
    "decision": "coordinator accepts; source remote worker publisher effects; target Core authorizes host adapter effects",
    "effect": "coordinator accepts; source remote worker publisher effects; target Core authorizes host adapter effects"
   },
   "durable_state": "publication attempts/status/prURL/error/in-progress-since; collected evidence refs",
   "identities": "coordinator run↔worker run; repository,branch,PR URL; dispatched check names",
   "transitions": "completed/pr_ready→bounded propose→readback→collect summary validation; unresolved/contradictory never accepted; no-op can satisfy empty explicitly dispatched check set vacuously",
   "failure_semantics": "posted requires same adopted URL+repo and completed/pr_ready; not_ready iff completed not ready and noPR; missing/malformed/foreign run blocks; marker prose alone is not publication proof",
   "concurrency_restart": "propose reads durable in-flight/resolved record rather than replaying; source max2 confirmed failure attempts and stall horizon; target must preserve applied effects independently",
   "bounds": "source publication max2; source review-fix cap counts all remote attempts; configured checks may empty, unknown set needs positive required-passed evidence",
   "human_action": "refused is operator decision; no merge/force-push authority inferred; changed live head reassessment target required",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_remote_lib.py:613-642,1162-1668"
    }
   ],
   "evidence_classification": "observed full library; integration production unverified and caller receipts pending",
   "proposed_target": {
    "owner_issue": "issue51",
    "section_or_ac": "Publication success requires independently matched run/head/check effects",
    "proposed_test": "same output claim without pushed-head/receipt fails; wrong repo/run/branch summary fails; explicitly empty optional CI/check set distinct from unavailable set; partial push retained when PR create fails"
   },
   "proposed_test": "same output claim without pushed-head/receipt fails; wrong repo/run/branch summary fails; explicitly empty optional CI/check set distinct from unavailable set; partial push retained when PR create fails"
  },
  {
   "id": "ops-B025",
   "trigger": "operator quarantine/recollect or dispatcher observes quarantine",
   "preconditions": "retained terminal run with proven exact identity; no active/uncertain worker/publisher; readback field present",
   "inputs_outputs": "same-run disposition/export proof from status,reconcile,mirror→quarantine/recovery fence",
   "owners": {
    "decision": "operator requests; infrastructure applies; coordinator adopts only matching readback",
    "effect": "operator requests; infrastructure applies; coordinator adopts only matching readback"
   },
   "durable_state": "quarantined disposition,verified exported flag,retained artifacts; same-job fence",
   "identities": "job/run and record; foreign run cannot bind; fieldless old reply cannot prove fence lifted",
   "transitions": "retained→quarantine readback→fenced; explicit lifted fence+recovery proof permits recollect; unknown holds",
   "failure_semantics": "wire/mirror multiple-source precedence distinguishes stale fieldless from current fielded source; quarantine action nonzero can still be adopted via matching readback; unknown cannot lift fence",
   "concurrency_restart": "strict iter_cw_runs_strict fence scan returns None on failure; phase quarantine does not consume live slot but blocks same job until operator decision",
   "bounds": "bounded API probes; retained credential/evidence cleanup separate from capacity",
   "human_action": "explicit quarantine/recollect; never reinterpret human wait or delete retained recovery data",
   "source_evidence": [
    {
     "revision": "2059618991823c3470ef2a1f94aefcfa063144c5",
     "location": "scripts/coding_worker_remote_lib.py:956-1159,1698-1735"
    }
   ],
   "evidence_classification": "observed complete library; remote-specific operational mechanism/tests pending",
   "proposed_target": {
    "owner_issue": "issue55",
    "section_or_ac": "Operator quarantine retains evidence and fences work without pretending completion",
    "proposed_test": "fieldless/foreign/readfailed observation never releases fence; quarantine preserves exact evidence and unknown effects; operator recovery does not invent success"
   },
   "proposed_test": "fieldless/foreign/readfailed observation never releases fence; quarantine preserves exact evidence and unknown effects; operator recovery does not invent success"
  },
  {
   "id": "worker-B030",
   "trigger": "Operator snapshots review database",
   "preconditions": "Existing SQLite source and distinct target paths",
   "inputs_outputs": "live database -> self-contained integrity-checked snapshot and sha256 manifest",
   "owners": {
    "decision": "snapshot workflow",
    "effect": "SQLite online backup and local filesystem"
   },
   "durable_state": "snapshot, manifest, previous pair during swap and locks",
   "identities": "source path, canonicalized target paths, schema revision and snapshot digest",
   "transitions": "lock targets -> backup temp -> integrity/hash -> stage -> publish pair; failure restores previous pair",
   "failure_semantics": "interrupt leaves no newly asserted success; previous verified pair restored on next run after mid-swap when recognized; exact crash coverage and lock races not verified here",
   "concurrency_restart": "two paths locked in stable order; recovery recognizes .previous files; PID-based stale lock caveat",
   "bounds": "bounded lock wait and optional cancellation; optional page transfer rate",
   "human_action": "retain and verify archive; resolve corruption or lock timeout",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/review/ledger-backup.ts"
   ],
   "evidence_classification": "source observed; tests and live database operations unverified",
   "proposed_target": "issue50 identity/storage maintenance cutover AC: archive old database read-only, verify consistency/digest and no replay; tests interrupted snapshot and no source modification",
   "proposed_test": null
  },
  {
   "id": "worker-B031",
   "trigger": "Worker emits tool events and final answer",
   "preconditions": "Normalized adapter events and machine-observed checks/model data",
   "inputs_outputs": "pre-redaction observed commands, declared command policy and final response -> audit and violations",
   "owners": {
    "decision": "execution policy auditor; runner classifies result",
    "effect": "event observer and result capture"
   },
   "durable_state": "observed commands and violation records through result/evidence",
   "identities": "run, tool call ID or inferred name+command, model provider/ID and checks",
   "transitions": "collect -> deduplicate -> compare commands and explicit self-report -> policy violation or audit",
   "failure_semantics": "shell parsing is explicitly best effort; events without call ID collapse identical commands, so audit is not a complete enforcement proof; regex catches explicit contradictions only",
   "concurrency_restart": "event order and sequence local to run; lost stream/unknown provenance not proven",
   "bounds": "declared-only policy optional; otherwise extra commands recorded",
   "human_action": "inspect violations and provide correct declaration",
   "source_evidence": [
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/policy.ts",
    "c8c3e2b44080d678c162131d53b790b66f20072f:src/runtime/runner.ts"
   ],
   "evidence_classification": "source observed; tests and actual runtime signal completeness pending",
   "proposed_target": "issue50 worker boundary AC: missing event coverage remains unknown, self-report never overrides recorded facts; test conflicting claims and missing event identifiers",
   "proposed_test": null
  },
  {
   "id": "SEL-B06",
   "trigger": "CLI/MCP/query/preview needs effective selection, PR or refinement policy.",
   "preconditions": "Explicit target snapshot.repository and local checkout provenance; explicit flag may override discovery.",
   "inputs_outputs": {
    "inputs": "Policy path, git remotes, target repository and surface-specific overrides.",
    "outputs": "Effective validated policy with fingerprint and adoption/skipped diagnostic, or typed error."
   },
   "owners": {
    "decision": "Shared boundary resolves/config parses; pure selection consumes policy; entrypoints do not reimplement defaults.",
    "effect": "Shared boundary resolves/config parses; pure selection consumes policy; entrypoints do not reimplement defaults."
   },
   "durable_state": "Source policy file plus caller-owned snapshot/result; target saves immutable accepted policy/binding.",
   "identities": "Source currently owner/repo casefolded only; target must include host-instance and stable repository identity.",
   "transitions": "Explicit flag -> selected; matching checkout remote -> adopted; unknown/foreign/no remotes -> defaults or skipped diagnostic; malformed selected policy fails.",
   "failure_semantics": "Source checkout URL parser discards hostname and intermediate path segments. tests/test_policy.py intentionally allows cross-host same owner/repo adoption. This shortcut is incompatible with target #50 identity; do not retain as safe cross-host behavior.",
   "concurrency_restart": "Policy changes affect new snapshots/fingerprint; approved Work must retain saved policy and never silently weaken on new defaults.",
   "bounds": "Read-only git queries timeout10seconds; selection max-group8 default and one repo-specific3 are policy data, not mandated target recovery defaults.",
   "human_action": "Use explicit intended scoped policy; resolve unknown/malformed configuration; do not infer repository identity from a display path.",
   "source_evidence": [
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/boundary.py:115-271",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/checkout.py:20-83",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_policy.py:362-657",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:policies/hermes-setup.yaml"
   ],
   "evidence_classification": "observed implementation and explicit static/fixture assertions, cross-host shortcut retained in source only",
   "proposed_target": "#50 host/repository/actor immutable binding; #53 policy resolution; #55 diagnostics. Test same owner/repo on distinct host never adopts the other policy.",
   "proposed_test": null
  },
  {
   "id": "SEL-B07",
   "trigger": "CLI/MCP/binding/query or acceptance harness reads source state.",
   "preconditions": "Read-only registered operation, bounded complete data or explicit failure.",
   "inputs_outputs": {
    "inputs": "Snapshot/tracker args, constrained policies, GraphQL reads, declared schema and untrusted tracker strings.",
    "outputs": "Typed schema/status/reasons/fingerprint, no secret leakage; partial/truncated sources fail instead of presenting complete."
   },
   "owners": {
    "decision": "Tracker adapter reads/redacts; core classifies; binding adopts selection/order verbatim; acceptance harness checks fixture propositions without becoming execution authority.",
    "effect": "Tracker adapter reads/redacts; core classifies; binding adopts selection/order verbatim; acceptance harness checks fixture propositions without becoming execution authority."
   },
   "durable_state": "Caller snapshot/preview/receipt; acceptance logs test evidence only.",
   "identities": "Source schema versions and source-specific repository/number; target Store/host/source/revision/source-completeness.",
   "transitions": "Read -> validated complete snapshot or unresolved/error; named write command distinct and approved. MCP contains only6 read/preview/validate tools.",
   "failure_semantics": "Issue1000/PR200/comment-page limits fail loudly; GraphQL error/null/missing/truncated connections not clean. Text-regex binding tests prove required phrases, not actual orchestration safety. Fakegh applied-then-lost scenarios are synthetic, not live success.",
   "concurrency_restart": "Same snapshot/policy yields CLI/MCP parity and stable fingerprint; recovery does not create new key after unknown; no lifecycle state in skill.",
   "bounds": "Acceptance MCP30s request timeout and bounded child cleanup; source output/schema limits do not define target cross-E recovery defaults.",
   "human_action": "Inspect source unavailable/truncated/error result; directCLI fallback; install/discover actual skill when delivered.",
   "source_evidence": [
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:AGENTS.md",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:pyproject.toml",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:src/devflow/acceptance.py:212-355,358-662,1544-1746,1848-2159,2307-2333",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_acceptance.py",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_binding.py",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_pr_snapshot.py:172-405",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/test_refine_snapshot.py:97-242",
    "f89c35bbb7819ad3da0c1b4eebeed321df87aa8c:tests/fixtures/plan/bin/gh"
   ],
   "evidence_classification": "source and fixture/assertion-region review; no live host/runtime/skill proof",
   "proposed_target": "#50 source snapshot identity; #51 complete/partial/unavailable review evidence; #53 credential isolation; #55 requiredCLI/skill and explicit errors. MCP remains deferred.",
   "proposed_test": null
  }
 ],
 "acceptance": [
  {
   "id": "I50-AC1",
   "issue": 50,
   "text": "Identity/storage specification defines equality/scoping, serialization, uniqueness/transaction boundaries and Query representation, including repository rename, overlapping IDs/SHAs across hosts/repositories, actor renewal/replacement and config changes for new versus existing Work.",
   "text_sha256": "aa296924e6a6f648c85aeb93f7f5254905a52ffe48088ac3dfe982303569ec80",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC2",
   "issue": 50,
   "text": "Admission/selector specification defines accepted immutable binding and retry/resume behavior, minimum installation policy, stale/duplicate proposals, unknown or denied capability evidence and the single-issue overlap policy; richer grouping remains deferred.",
   "text_sha256": "5783e24f38aba793dd9c3db15ef412c038aa3df8e2d64bf2f1edc11771f3a749",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC3",
   "issue": 50,
   "text": "Migration specification maps current Core/store/CLI/fixture callers and historical keys to targets, provides literal before/after records and planned tests for interrupted migration, historical command replay, collisions and refusal; no live migration is performed.",
   "text_sha256": "142124ac548e2df0f3eaed251541d4da2750c3a9f31dcfe0070e68656bc820dd",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC4",
   "issue": 50,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC5",
   "issue": 50,
   "text": "Comprehensive inventory records repositories/revisions and an enumerable file/module/entry-point/test manifest covering intake, admission, execution, verification, publication, review/fix, reconciliation, recovery, operator intervention, installation and retention. Every manifest entry is reviewed, explicitly excluded with rationale, or recorded as inaccessible and blocking the affected completeness claim; representative incidents alone do not establish coverage.",
   "text_sha256": "820981b99f3059e1b84768c3a2a85d5b1a15f2c98cc07ac022ac1321b5ae8051",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC6",
   "issue": 50,
   "text": "Each behavior receives a stable requirement ID with trigger, preconditions, inputs/outputs, decision/effect owner, durable state, identities, transitions, failure/partial/unknown semantics, concurrency, restart behavior, bounds, human action and planned proof. Distinguish observed implementation, intended requirement, known defect, historical fix and unverified operational claim; use code/tests/history/issues together without promoting a bug into the specification.",
   "text_sha256": "c360c2fb6a24b7d000651a71519b4ce304fa174abce2d0c0043a016a228cb519",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC7",
   "issue": 50,
   "text": "Every inventoried behavior is classified retain/re-model/omit/defer with rationale and linked to R01-R38 or a new requirement ID, a target specification section, responsible design issue #50-#55 and acceptance criterion. For re-model/omit distinguish the source-specific mechanism from the underlying safety/recovery requirement; deferred requirements require explicit owner disposition. No retained behavior may disappear because its original mechanism is omitted.",
   "text_sha256": "b50ceb77fee1296f17d586f6efe5163f810d5e79781b6f02aa16b24e42967084",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I50-AC8",
   "issue": 50,
   "text": "Compare source-derived requirements against current taisan code/tests, documenting implemented+verified, implemented+unverified, missing or deferred with revision-scoped evidence. Produce a rerunnable coverage check over the manifest and mapping that detects unclassified entries, missing target/acceptance links and unresolved gaps; it proves mapping completeness, not runtime correctness. Keep private source names/paths/symbols only in the owner-controlled audit record under AGENTS.md.",
   "text_sha256": "ad8d38c040759eb42fb339c8068361a708e972316ba922a00ec07af4ac1f0a55",
   "snapshot_updated_at": "2026-11-07T01:41:03Z",
   "url": "https://github.com/shimpeiws/taisan/issues/50"
  },
  {
   "id": "I51-AC1",
   "issue": 51,
   "text": "Evidence/gates schema and assessment matrix covers absent, present-and-valid and present-and-invalid artifact evidence and effect receipts with source, verifier, exact revision and failure reason; trace local change through separate push/create/reply/thread/ready effects to stage and Outcome.",
   "text_sha256": "b665ec897940c8828bd2b22bdd0ac9fda53ade4bb6affae854a3ba82efe7c6dc",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC2",
   "issue": 51,
   "text": "Findings/review specification covers same-head finding growth between inspection/submission/release (#24), stale source head and changed diff base, cross-attempt dispositions, exact required checks/review sources, eligible reviewer identity and invocation/review budget consumption; show literal accepted/rejected examples without claiming #24 fixed.",
   "text_sha256": "a98de8ef95238448574721220fdd3f398cfd447d95543857e473e2ac493fe23d",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC3",
   "issue": 51,
   "text": "Outcome/repair specification defines current-head success requirements, retained stop/publication/check/review references, unknown-effect terminal-release guards, missing versus invalid evidence, repair/resume conditions and planned contract tests. Reuse #21/PR #31 classifier evidence only for its narrow assertion.",
   "text_sha256": "64f0fdfd6fd1bd644fb717fe6956a9d4f8718b5e54b4000b0fa0c6dab2328e6f",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC4",
   "issue": 51,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC5",
   "issue": 51,
   "text": "Failure/recovery specification distinguishes transient/retryable, permanent, environment-repair, human-decision and unknown-effect outcomes. For each class define permitted reconciliation/recollection/stop/re-execution, required evidence, unchanged-condition refusal, persisted reason and Attention handoff. Unknown is never treated as an ordinary failed attempt eligible for a blind retry.",
   "text_sha256": "c3b06648504955d0f9040dea7dd0c2eaaf10bdd8a499346e066f1f6fc2bc7cff",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC6",
   "issue": 51,
   "text": "Specify durable counters and identity for repeated equivalent failures across Invocations and automatically created Executions, preserving overall Work recovery limits without letting a fresh Execution reset them. Separate invocation, review, artifact-recovery, automatic-retry and absolute deadline budgets; define pause/resume, operator retry and changed-evidence semantics with accepted/rejected examples. Numeric defaults and exceptions require recorded owner decisions.",
   "text_sha256": "942ba21899ab8079cae686defc74ec983b0de0c1a8e8724042cfffdd7d0689b2",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I51-AC7",
   "issue": 51,
   "text": "Map publication-without-push, push-success/report-interruption, stale-head and same-head finding growth incidents to exact stage/disposition/reconciliation contracts and planned regression tests; source fixes are evidence of a failure class, not substitutes for target acceptance.",
   "text_sha256": "5ada92f9c2a797d7db37f2421a01f419e94679fd0bce74b9e40097e10de45820",
   "snapshot_updated_at": "2026-11-07T01:23:12Z",
   "url": "https://github.com/shimpeiws/taisan/issues/51"
  },
  {
   "id": "I52-AC1",
   "issue": 52,
   "text": "Worker protocol specification names the reference runtime/entry point, concrete input/result schemas and optional capabilities; worker-runtime authentication/session details remain adapter-owned, with no outer retry/review/publication/settlement authority or running-Invocation handoff.",
   "text_sha256": "ac93669d6ee44792eaadb2c8f521de95b1003b38ce52efaa79a6cbea3cb84b18",
   "snapshot_updated_at": "2026-11-07T01:23:14Z",
   "url": "https://github.com/shimpeiws/taisan/issues/52"
  },
  {
   "id": "I52-AC2",
   "issue": 52,
   "text": "Artifact handoff specification defines producer/consumer ownership, digests/revisions, durable transfer acknowledgement and crash recovery before/after export; absent, present-and-valid and present-and-invalid artifact evidence are distinguished, and SHA-only or ephemeral-path-only output cannot establish durable handoff.",
   "text_sha256": "cde123fdff2c611ab0ff8a2d0890dcc5f27934f557254cb21b891ba196a438d9",
   "snapshot_updated_at": "2026-11-07T01:23:14Z",
   "url": "https://github.com/shimpeiws/taisan/issues/52"
  },
  {
   "id": "I52-AC3",
   "issue": 52,
   "text": "Worker isolation specification compares viable single-host mechanisms, records threat assumptions/limitations and an owner choice; flags alone are not enforcement. The shared-suite plan covers both adapters for duplicate/lost start, unknown liveness, process escape/stop proof, missing/corrupt/stale artifacts, denied writes and config-only new-Invocation swap.",
   "text_sha256": "e1ff6b469f3046ab765e110bc4c0b389d9c6cb8c010717f6f42068c803584a83",
   "snapshot_updated_at": "2026-11-07T01:23:14Z",
   "url": "https://github.com/shimpeiws/taisan/issues/52"
  },
  {
   "id": "I52-AC4",
   "issue": 52,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:14Z",
   "url": "https://github.com/shimpeiws/taisan/issues/52"
  },
  {
   "id": "I52-AC5",
   "issue": 52,
   "text": "For every Worker-owned behavior assigned by #50, map its requirement ID to an explicit protocol/capability clause and shared-suite test or justified owner-approved exclusion. Cover provider refusal/limits, definite not-started versus unknown launch, missing/corrupt terminal reports, lost finish reports, checkpoint availability and interrupted artifact promotion; adapter-local retries must be declared and bounded and cannot hide outer-loop attempts or effects.",
   "text_sha256": "ce46f0b47e5fc4e4efcef876618df1b505be556abdaa72d7ee4a931216d647f5",
   "snapshot_updated_at": "2026-11-07T01:23:14Z",
   "url": "https://github.com/shimpeiws/taisan/issues/52"
  },
  {
   "id": "I53-AC1",
   "issue": 53,
   "text": "Configuration/auth specification gives concrete schema/versioning and sanitized examples with user-token-first GitHub setup and a retained token option after App support; define credential-reference storage/resolution and prevent secret values in examples/intents/argv/logs/export.",
   "text_sha256": "ed7eb57d70bc37a394cc41e65b2a1a93953e7757f95579e82b92493ec4945205",
   "snapshot_updated_at": "2026-11-07T01:23:16Z",
   "url": "https://github.com/shimpeiws/taisan/issues/53"
  },
  {
   "id": "I53-AC2",
   "issue": 53,
   "text": "Permissions/rotation matrix defines checks at config/acceptance/dispatch/effect boundaries, unknown/denied/unsupported outcomes and per-role read/write requirements. Examples cover same-actor renewal, actor replacement, lost read permission, removed adapter and changed defaults while original Work/unknown effects retain binding and receipts.",
   "text_sha256": "472fababf0255f35bca0706cd945cd6b4efbc267bc4e77871d1500012de06f19",
   "snapshot_updated_at": "2026-11-07T01:23:16Z",
   "url": "https://github.com/shimpeiws/taisan/issues/53"
  },
  {
   "id": "I53-AC3",
   "issue": 53,
   "text": "Installation/setup design specifies isolated state/workspace, allowlist, chosen Worker/independent-reviewer setup, no unnecessary cloud/container/webhook service, and verification cases for absent/malformed config, missing/expired credentials, secret exposure and Worker isolation. Document expected structured errors without installing credentials or running live checks.",
   "text_sha256": "033180aba6cde6a76ca2b548bb9f18a9b3d3dcc3a52c5db2f9147a0c5cb22668",
   "snapshot_updated_at": "2026-11-07T01:23:16Z",
   "url": "https://github.com/shimpeiws/taisan/issues/53"
  },
  {
   "id": "I53-AC4",
   "issue": 53,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:16Z",
   "url": "https://github.com/shimpeiws/taisan/issues/53"
  },
  {
   "id": "I53-AC5",
   "issue": 53,
   "text": "For each setup/auth behavior assigned by #50, link the requirement ID to configuration validation, failure code, repair actor/action and evidence required before resume. Cover absent runtime/tool, expired credentials, permission denial, source unavailability and changed actor/config bindings; unchanged broken configuration cannot create an automatic retry loop. Specify planned boundary tests and retain original unresolved-effect bindings.",
   "text_sha256": "bf43f8ba72bb6aea32fe92b69bc25a2b37cea2e0cd2f487ffd092772539f045e",
   "snapshot_updated_at": "2026-11-07T01:23:16Z",
   "url": "https://github.com/shimpeiws/taisan/issues/53"
  },
  {
   "id": "I54-AC1",
   "issue": 54,
   "text": "Retention policy specification distinguishes durable identities/decisions/reference metadata from reclaimable bodies, compares age/size/default/history-compaction alternatives and records owner-approved settings, defaults, disabled/invalid semantics and scope; preserve replay/provenance without silently assuming unlimited history.",
   "text_sha256": "a478664d6dbe9381967907d98cb479b242fdbc033ed6a59b0802569a26d1227f",
   "snapshot_updated_at": "2026-11-07T01:23:18Z",
   "url": "https://github.com/shimpeiws/taisan/issues/54"
  },
  {
   "id": "I54-AC2",
   "issue": 54,
   "text": "Cleanup safety eligibility/failure table covers active writers, paused WIP, artifact recovery, unknown effects, pending judgment, terminal Work, expired external claims and shared git objects; specify concurrent access, partial deletion, restart/idempotency and missing bodies. Define cleanup scheduling outside Core filesystem I/O while preserving Core lifecycle/release guards.",
   "text_sha256": "7f56a901db0fd50878839cf9651a421c6e3479fde82fba7acb76ab2e7878affd",
   "snapshot_updated_at": "2026-11-07T01:23:18Z",
   "url": "https://github.com/shimpeiws/taisan/issues/54"
  },
  {
   "id": "I54-AC3",
   "issue": 54,
   "text": "Retention observability specification defines accounting/report limits and owner-approved behavior when protected data alone exceeds configured limits; reclaimed bodies become explicitly unavailable. Literal before/after examples and planned checks cover thresholds, pins, replay after compaction, absent/present-and-valid/present-and-invalid evidence, pressure and interrupted cleanup; no cleanup executes here.",
   "text_sha256": "df54e453159deb8f11fddfec8b5d712f3bf3073deeaadefefbaffbe9be1d313e",
   "snapshot_updated_at": "2026-11-07T01:23:18Z",
   "url": "https://github.com/shimpeiws/taisan/issues/54"
  },
  {
   "id": "I54-AC4",
   "issue": 54,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:18Z",
   "url": "https://github.com/shimpeiws/taisan/issues/54"
  },
  {
   "id": "I54-AC5",
   "issue": 54,
   "text": "Map every retention/preservation behavior assigned by #50 to eligibility guards and planned tests, including timeout recovery, pending automatic retry, repeated-failure history, interrupted publication/disposition and evidence recollection. Cleanup cannot erase recovery counters or reset retry limits, remove pinned WIP, or make an unknown effect appear absent. Explicitly distinguish omitted source pruning mechanisms from retained preservation requirements.",
   "text_sha256": "662055703fccf2c9e6d8f602c72089e941a5fa451b8897f4581f35d8bf432d03",
   "snapshot_updated_at": "2026-11-07T01:23:18Z",
   "url": "https://github.com/shimpeiws/taisan/issues/54"
  },
  {
   "id": "I55-AC1",
   "issue": 55,
   "text": "CLI runtime sequences and failure tables cover ready/spawn/stop/partial effect/artifact recovery/deadline/budget/prolonged uncertainty, scheduling bounds and shutdown. CLI drives I/O/timing, Core judges supplied facts, supervisor/host adapter perform effects; cross-task doctor diagnosis stays deferred; per-Work no-progress and bounded recovery are in scope under this issue.",
   "text_sha256": "e0ad06f74ebde5d114050669c14110183e14cba06da11187531efc4810884f4d",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC2",
   "issue": 55,
   "text": "CLI contract specification defines literal structured accepted/rejected/parse/config/store/source-unavailable/unknown examples, schema version, required IDs, stdout/stderr and exits. Recovery retains original key/payload/actor/preconditions, prohibits silent rewrite/fresh-key retry and includes same-head finding coverage; process exit zero alone cannot prove accepted/completed Work.",
   "text_sha256": "346883f7281ba94ac920141017ee33e99ad8c4da226a31dd8fe5960b6fbb15ed",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC3",
   "issue": 55,
   "text": "Skill/setup workflow design names the supported agent and packaging/install/invocation deliverables, maps every docs/agent-interface.md row and S1 operator step to commands/results/planned evidence, covers eligible/ineligible interventions, concurrency and conversation closure, and classifies absent/present-and-valid/present-and-invalid evidence. No skill implementation or live walkthrough occurs.",
   "text_sha256": "a0369b73c3ff86b02982c38131cbbd6db833f53828e888b440855f5b63dd7cfa",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC4",
   "issue": 55,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC5",
   "issue": 55,
   "text": "Provide a failure-scenario matrix with requirement IDs covering duplicate/manual dispatch, lost launch response, uncertain stop, orphan/escaped writer, missing/corrupt artifacts, publish-before-collect loss, partial/unknown publication, interrupted finding disposition, stale head, unavailable source, stalled execution, repeated equivalent failure and terminal pending intents. Each row specifies trigger/detection evidence, durable transition, ownership hold/release, permitted automatic action, backoff/attempt/deadline bounds, restart behavior, Attention escalation and planned fault-injection proof.",
   "text_sha256": "d65cc2d3994b5a146fbfc884283ea9f1e6c5c6548f8ba9a72277311473bffac8",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC6",
   "issue": 55,
   "text": "Specify per-Work no-progress detection separately from runtime deadline and prolonged uncertainty. Define observable progress and its persisted clock, missing/stale signal behavior and notification/escalation; absence of progress alone never proves death or authorizes a replacement writer. Choose thresholds and policy with the owner. Cross-task doctor diagnosis remains deferred.",
   "text_sha256": "3eed7b0ccfb174b41119cfc54f02976fb1863b1935bb2ed6772ae422322d1455",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC7",
   "issue": 55,
   "text": "Specify timeout recovery as durable stop intent, proven stop of all recorded writers, reconciliation of unknown external effects and bounded requeue/re-execution only when policy permits. Define same versus new Execution identity, retained WIP, #51's cross-Execution recovery budget, and explicit human handoff on permanent refusal/exhaustion. A transition to failed with a manual retry command does not alone satisfy automatic-recovery acceptance.",
   "text_sha256": "3058582329b4be09caad152089894bb935482d8674e4c88629f1acb569369fb5",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC8",
   "issue": 55,
   "text": "Specify persisted retry scheduling/backoff and repeated-equivalent-failure suppression, including crash before/after scheduling, concurrent manual retry/resume, source restoration and unchanged environment. Once terminal remote/local evidence invalidates a pending retry/resume, retire that intent durably and idempotently; restart cannot resurrect obsolete work or reset bounds.",
   "text_sha256": "03bea09ac3c069536698464e3a49ce56cd5a9b73a6e55c9e1d719f8b0f18fb4f",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I55-AC9",
   "issue": 55,
   "text": "Update implementation-plan/lifecycle/decision documents to reconcile the earlier no-progress deferral with this approved per-Work recovery scope. Keep Core transition authorization, CLI timing/I/O and adapter effects separate. Link every assigned #50 behavior to a concrete specification and acceptance criterion; list unresolved choices as blockers rather than claiming recovery complete.",
   "text_sha256": "15adcfce3f3a41d88dd38c277edbaa16ff359509ae97074050ec8d2bf06924a9",
   "snapshot_updated_at": "2026-11-07T01:23:20Z",
   "url": "https://github.com/shimpeiws/taisan/issues/55"
  },
  {
   "id": "I56-AC1",
   "issue": 56,
   "text": "Design closure consistency review connects admission through code/artifact/separate publication/review/fix/Outcome/Attention/restart with one owner per decision/effect/record and compatible schemas/guards. Record owner decisions for all implementation-shaping tradeoffs; unresolved items explicitly block the affected implementation or have owner-approved deferral.",
   "text_sha256": "9c5c951fdc6441ee921e61ee165e2302111b33326a127428a4cdc0ab28144e72",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC2",
   "issue": 56,
   "text": "Acceptance planning maps every R01-R38 and S1/general/agent-interface criterion to concrete contract/schema, implementation unit, dependency and planned proof, preserving finding coverage and configurable history/body retention. Classify evidence as absent, present-and-valid for exact revision/criterion or present-and-invalid because stale/mismatched; fixtures do not prove live acceptance.",
   "text_sha256": "3f11323a9184bd72c20f76410c9dec42b3cabde4cb5b86815e46eee231ef8e52",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC3",
   "issue": 56,
   "text": "Acceptance planning keeps GitHub production S1, two-Worker shared-suite/config swap and host fixture/narrow live GitLab proof as separate obligations; no production GitLab, cross-task doctor, TUI or extra infrastructure scope is added; per-Work bounded recovery remains required.",
   "text_sha256": "d394c5913d51317dc5b507b7ecbed5a03126b93fc1ae1d9553fd4080a55a9e52",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC4",
   "issue": 56,
   "text": "Implementation gate proposes reviewable dependent implementation units using the approved contracts and existing #17/#24/#28, without creating/updating those issues. Record what explicit later authorization is required for backlog publication, runtime implementation and live tests; design closure alone authorizes none of them.",
   "text_sha256": "f579f79f2207bc8fb098f34405b498504a5b26bddd3a7f96e927aabcb24055e7",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC5",
   "issue": 56,
   "text": "Design review documents applicability/rationale/open questions for all six design aspects, explicit owner decisions for implementation-shaping tradeoffs, and implementation-plan mapping. For each required evidence item, artifact, receipt or record in scope, specify behavior when absent, valid for the exact schema/identity/revision/criterion, or invalid because malformed/stale/mismatched; retain the reason. No design decision claims runtime or live completion.",
   "text_sha256": "f607b50785e7cbc242bc9a728fd309de9ddcb180fe83731b88560fa5a20b4af9",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC6",
   "issue": 56,
   "text": "Review #50's manifest and rerunnable mapping check: every source/current-target behavior has a retain/re-model/omit/defer disposition and every retained/re-modeled requirement has a target normative specification, design owner, implementation unit and planned proof. Extend R01-R38 where inventory discovers additional requirements; document inaccessible sources and stale revisions as gaps, not exhaustive coverage.",
   "text_sha256": "7b8fda4163b7ecb1dc586f90ee794c44312c4bb79d606630f19952407c59dba2",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC7",
   "issue": 56,
   "text": "Require a reviewed recovery acceptance matrix distinguishing reconciliation, recollection, safe stop and re-execution. Include bounded automatic timeout recovery, equivalent-failure suppression across attempts, backoff/restart, unchanged-repair refusal, terminal-intent retirement and Attention escalation. A green happy path or safe failure/manual retry alone cannot establish recovery delivery; map each branch to fault injection and later isolated live evidence where appropriate.",
   "text_sha256": "8e3f1668aa949e8b5b6cf5adad143b2996128b45c416ebd98fbe77845f11d4c8",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  },
  {
   "id": "I56-AC8",
   "issue": 56,
   "text": "Record owner-approved mechanism omissions and behavioral deferrals individually. Do not classify generic single-Work recovery as deferred doctor functionality; keep cross-task diagnosis deferred. The implementation start gate remains blocked by unmapped retained requirements or unresolved recovery policy/schema decisions, and update the implementation plan and release acceptance accordingly.",
   "text_sha256": "cac18d131087296bfdbfc82674062c229db867e3d6dd9d8d6c52bf32ae1b941f",
   "snapshot_updated_at": "2026-11-07T01:23:22Z",
   "url": "https://github.com/shimpeiws/taisan/issues/56"
  }
 ]
}
```
<!-- extraction-catalog:v1:end -->
