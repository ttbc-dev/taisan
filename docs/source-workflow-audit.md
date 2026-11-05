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
and verification gaps. "Workloop responsibility" references the contracts
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
  Workloop's contract now covers this: cross-Work exclusion is enforced
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
  fixed at the recorded revision. Workloop's re-model must include:
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
- **Rationale.** Workloop's `work_id`/`execution_id`/`invocation_id`
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
