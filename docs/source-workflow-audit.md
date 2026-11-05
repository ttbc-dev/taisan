# Source Workflow Audit

This document audits the original personal development workflow that
workloop v0.1 distills into an open-source project (issue #33). It records
which existing operational behavior belongs in v0.1 — classified as
**retain**, **re-model**, or **omit** — and what evidence backs each
decision. It does not authorize implementation; it ends at scope definition
and implementation planning.

The four source repositories are private. Their names, revisions, file and
symbol references, and issue/PR numbers are recorded so the design stays
traceable; the repositories themselves are not distributed with this
project and are not consulted at build or runtime. Links into them are
verifiable only by the repository owner.

## Inspected revisions and scope

Audit performed 2026-11-05. Inspection was source reads at the recorded
revisions plus authenticated `gh` issue/PR state queries — no live host,
no installed copies under `~/.hermes/`, and no running processes were
inspected.

| Repository | Inspected revision | Scope inspected |
| --- | --- | --- |
| `shimpeiws/hermes-setup` | `6371cf99042a9eacdc23a7ac867ff23d50abd070` (`origin/main`) | `scripts/` (all `*_lib.py` and hyphenated CLI entrypoints), `docs/` including `docs/design/converge-state-transitions.md` and `docs/automation-inventory.md`, `skills/`, installer scripts |
| `shimpeiws/coding-worker` | `66daef534e8de9e6d9489d40b384716db73c7927` (`origin/main`) | `src/` (`contract/`, `devflow/`, `dsh/`, `checks/`, `cli.ts`), `docs/` including `worker-contract.md` and `dsh-supervision.md` |
| `shimpeiws/devflow` | `f89c35bbb7819ad3da0c1b4eebeed321df87aa8c` (`origin/main`) | `src/devflow/` (`core/`, `cli/`, `tracker/`, `publish.py`, `mcp_server.py`), `docs/`, `policies/*.yaml`, `bindings/` |
| `shimpeiws/coding-worker-infra` | `3522605ad832f1cd1d31f0554d35a332a5f4d1e0` (`origin/main`) | `src/` (Rust `cwctl`), `scripts/host/`, `scripts/worker/`, `scripts/incus/`, `docs/`, `test/` |

Notes on inspection fidelity:

- The local checkout of coding-worker was 231 commits behind `origin/main`;
  all coding-worker claims below were verified against `origin/main` via
  `git show`, not the stale working tree.
- The local checkouts of devflow and coding-worker-infra were not on
  `main` (one ahead of `origin/main`, one on a pinned lane branch); claims
  were likewise verified against `origin/main`.
- Symbol references cite names and approximate line numbers at the
  recorded revision; they are locators, not pinned permalinks.
- File paths in evidence columns (e.g. `scripts/devin-milestone.py`,
  `docs/shadow-review.md`, `src/devflow/converge.ts`) are relative to the
  source repository named by the surrounding row or section — not to this
  repository.
- hermes-setup `docs/design/converge-state-transitions.md` is itself a
  design audit pinned to hermes-setup `839094bb` and coding-worker
  `e6c16686`; where this document cites it as evidence, the underlying
  claims were re-verified at this audit's own recorded revisions.
- Unverified/inaccessible areas: live host configuration and installed
  script state; Telegram-facing runtime behavior; any actual production
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
| Ready-issue selection | caller invocation (CLI, MCP, scheduler tick) | `devflow.core.select.ready_issues` — pure function of snapshot + policy | none in devflow (snapshot and result are caller-owned) | tracker `Snapshot` via read-only `gh` (`tracker/github.py`) | pure; staleness is the caller's problem | `src/devflow/core/select.py` (status precedence `excluded`>`unresolved`>`in_progress`>`blocked`>`ready`; `sha256:` fingerprint; `SelectionInvariantError` → exit 6) |
| Grouping | inside selection | `group_issues` — milestone + ordered label-set key | none | issue labels/milestone in snapshot | deterministic; oversized groups split into deterministic chunks | `src/devflow/core/grouping.py` (unlabeled issues get unique `issue:<n>` key; `max_size` split) |
| Per-repo policy scoping | config load | `RepoPolicy` from `policies/<repo>.yaml` | policy file (repo config) | none | unknown keys rejected at parse | `src/devflow/core/policy.py`; `policies/hermes-setup.yaml` (`exclusion_labels`, `require_labels`, `max_group_size: 3` justified by lane audit of 3h deadline vs 4+ issue groups) |
| Scheduler candidate intake | cron tick (`devflow-scheduler-halfhourly` */30) | hermes scheduler picks first ready group; never ranks/splits/re-filters | run state, parked selectors, run history | devflow output | triage errors → park with diagnostic | `scripts/devflow_scheduler_lib.py` (`run_lock` ~L631, `load_state` ~L639, `_parked_file` ~L860, `append_run_record` ~L673); `docs/devflow-scheduler.md` |

**Classification — Selection**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Pure snapshot+policy selection with stable fingerprint | **retain** (semantics) | the selection-is-a-pure-function boundary is what made Hermes/CLI agreement a code property; workloop keeps the identical boundary | Work Selector port in `adapter-boundaries.md` — Candidates carry stable proposal identity | no implementation yet; contract tests needed for determinism and stable identity | port specified but unimplemented; snapshot schema for workloop undefined |
| Grouping by milestone+label-set, group as atomic batch | **re-model** | grouping is selection detail, not a core invariant; keep "group is atomic — never split mid-group" but leave key derivation to the selector impl | Work Selector output shape (group `execution_order`) | fixture selector for contract tests | real selector deferred; group identity vs `work_id` mapping is issue #34 |
| Per-repo YAML policy / allowlist | **re-model** | the need (scope which repos/issues are eligible) is generic; the file format and label namespaces are personal config | installation/adapter config | `workloop-config` fields | none beyond config schema |
| Scheduler auto-intake of selected groups | **omit** | automated issue discovery is a personal operations feature, not lifecycle semantics; submission into workloop is via `submit` | none — out of scope per `architecture.md` | n/a | if a selector adapter is later added, it must go through `submit`, never around admission |

### 2. Admission — turning a request into tracked work

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Enqueue request → card | `devin-milestone.py enqueue` / scheduler intake | `cmd_enqueue` + `devin_kanban_lib` | request file (atomic write), Kanban card | repo allowlist, issue data | dedup onto existing card; bounded re-key (`-r2/-r3`), `error_kind: terminal_card` | `devin-milestone.py:15971` (`cmd_enqueue`); `devin_kanban_lib.py` `kanban_create` (~L352), `save_request` (~L726, mkstemp+`os.replace`) |
| Content-hash idempotency | every enqueue | `default_idempotency_key` — hash excluding `selection` snapshot and post-create bookkeeping | idempotency key → request path | none | re-run dedups rather than duplicating; #185 (fingerprint break on bookkeeping update) fixed by excluding bookkeeping from the hash | `devin_kanban_lib.py:690` (`default_idempotency_key`), `request_fingerprint` (~L710); issue #185 CLOSED 2026-10-05 |
| Terminal-card dedup guard | dispatch scan | scheduler / dispatch | card status | card status | dedup onto a terminal card is never started | `devin_kanban_lib.py:60` (`TERMINAL_CARD_STATUSES = done/blocked/archived`); AGENTS.md "Kanban dedup will fight you" (#163) |
| Issue triage before enqueue | `plan`/`start`/`enqueue` | `cmd_plan` triage via devflow; `spec:ambiguous` filtering | triage diagnostic files | issue body | `TriageEmptyError` (retry later) vs `TriagePermissionError` (deterministic) | `devin-milestone.py:647` (`cmd_plan`), `:1023` (`cmd_triage_diagnose`) |

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
| Claim card for dispatch | dispatch tick | `kanban_claim` with TTL | card claim record | card assignee/status | claim expiry ≠ process death (#266) | `devin_kanban_lib.py:627` (`kanban_claim`), `kanban_heartbeat` (~L517); issue #266 CLOSED 2026-10-16 |
| Equivalent-active exclusion | scheduler tick, per candidate | scheduler scans active cards/runs for same repo+issue overlap | scan outcome informs enqueue/skip | all lanes' cards and run records | scan failure → `unknown` hold, not "free" (three-way distinction from #307) | `devflow_scheduler_lib.py` equivalent-active scan (~L1449 at audit rev `74f45454`; re-verified present at `6371cf9`); issues #300, #307, #346 |
| Parked selectors | scheduler tick on triage failure | `_park_selector` / `_selector_parked` | parked registry file | — | parked ≠ blocked; re-evaluated per tick | `devflow_scheduler_lib.py` ~L860–1109 |
| Global/per-repo capacity | every dispatch decision | scheduler config `repo_limit` | incidents/state | live process counts per lane | lane-blind counting overcounted other lanes (#346) | `devflow_scheduler_lib.py:574` (`repo_limit`); issue #346 CLOSED 2026-10-21 |

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
| `cw-dispatch` lane execution | claimed card | `cmd_cw_dispatch` → lane by `executor:` config (devin / coding-worker / coding-worker-remote) | cw run record (`cw-runs/<task>.json`), launch attempts | lane config, card binding | `stop.action` classifies; retry → `pending_retry` + `next_attempt_at` | `devin-milestone.py:15197` (`cmd_cw_dispatch`), `_cw_launch` (~L12466), `_cw_settle` (~L6786)/`_cw_settle_terminal` (~L7307); `docs/coding-worker.md` |
| Remote job launch | remote-lane dispatch | `coding_worker_remote_lib.launch` builds `CodingJob`, pushes job file, starts container run | `new_remote_state` record; `idempotency_key(job_id, attempt, base_revision)` | remote host admission | `uncertain` ack → reconcile, never drop or re-key | `coding_worker_remote_lib.py:528` (`build_coding_job`), `:613` (`new_remote_state`), `:326` (`idempotency_key`), `:662` (`launch`); `devin-milestone.py` remote wait outcome handling ~L9340 |
| Admission slot contention | remote launch | host-side admission (exit 8 = conflict) | `pending_since` episode budget accumulates | remote slot state | `admission_conflict` → bounded deferred retry, reason code only (no remote text on card) | `coding_worker_remote_lib.py:718` (`admission_status`); `devin-milestone.py` `_cw_remote_slot_wait`; `src/lifecycle.rs` exit-code table (issue #460) |
| Duplicate dispatch guard | first writing stage of `devflow ship` | worker-side claim: `ship.lock` (O_EXCL, pid takeover) + `agent:running` issue label pinned to the `labeled` event id | `claim.json` under ship dir; GitHub issue label | issue event log | second run refuses; crash between steps resumes idempotently | `src/devflow/claim.ts` (RUNNING_LABEL, ordering claim.json → label → pin) |

**Classification — Dispatch**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Dispatch = atomic (transition + reservation + saved request) | **retain** | the atomicity is what hermes approximates across card/claim/launch | dispatcher transition `queued→ready/running` reserves slot and saves request in one commit | lifecycle.md dispatch section; store tests | — |
| Lost acknowledgement is not failure | **retain** | remote `uncertain` handling: identity persisted, next tick reconciles | `uncertain` Invocation state; reconcile, never re-key | lifecycle.md; `workloop-exec` stop proof | — |
| Two-layer duplicate guard (local lock + remote label) | **re-model** | the *requirement* is single-writer ownership of a workspace/branch; the mechanism (label+lock) is GitHub-and-host specific | `SafeOwnership`/writer fencing before start | module-contracts ownership; `workloop-exec` | ownership scope for foreign writers unresolved (see incident audit) |
| Remote slot admission, lane executor matrix | **omit** | single-host v0.1; the executor dimension is config, not lifecycle | n/a | n/a | revived only if remote execution enters scope |

### 5. Execution — the worker attempt

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Bounded `coding-worker run` | ship/converge stage dispatch | worker runs a `CodingJob`: `job_id`, `base_revision` (immutable, validated pre-exec), `timeout_seconds`, `limits`, `checks`, issue snapshot | run dir + `status.json` + evidence manifest | job file; model runtime | status ∈ completed/failed/timed_out/cancelled/blocked + `termination_reason` | `docs/worker-contract.md` CodingJob/CodingResult; `src/contract/types.ts`, `validate.ts` (`jobHash`) |
| Plan/execute split | job with `runtime.planner` | separate planner phase; read-only tool surface enforced by audit | `plan.md` artifact ref, phase accounting | none | write-tool call in plan phase fails the run | `src/dsh/adapter.ts` `PLANNER_AGENT`, `PLANNER_READONLY_MCP_TOOLS`; `src/contract/plan.ts` |
| Container supervisor (remote) | coordinator launch | `run.sh` entrypoint: non-root drop, `setsid`, `CW_HOOK`/`CW_REPO_HOOK`, atomic `status.json`/`worker.pid`/`times.json` | run dir under `/runs/<id>`; `state` file | container filesystem | worker exit without terminal record → `uncertain` (never unlocks retries) | `scripts/worker/supervisor.sh` (`write_terminal_state`, signal trap); `docs/execution/lifecycle.md` §1 |
| Managed model runtime | every run | dsh adapter supervises pinned OpenCode (managed version gate 1.18.34) | — | opencode HTTP | provider refusal classification; step-limit is post-hoc best-effort | `docs/dsh-supervision.md`; `src/dsh/adapter.ts`, `normalize.ts` |
| Process stop | timeout/judgment halt | `stop_record_process`: verified pid identity + start-stamp gate, process-group SIGTERM→grace→SIGKILL, stop proof = zero live group members | durable stop-intent marker via `on_signal_attempt` | `ps` output | `unverified` (no signal delivered) vs `unknown` (signal delivered, outcome unproven) → safety hold | `coding_worker_lib.py:6752` (`stop_record_process`), `pid_alive_state`; issue #337; `devin-milestone.py:13772` (marker + swallowed `OSError`, see incidents) |

**Classification — Execution**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Bounded run contract (immutable base, timeouts, budgets) | **retain** | resource bounds were the load-bearing safety shape | `CodingJob`-equivalent Invocation request: input, budgets, limits | lifecycle.md Invocation; `workloop-config` worker argv | — |
| Worker report is evidence, not outcome | **retain** | `stop.action` over exit code; `uncertain` is first-class | `WorkerReport.status` never settles; core decides Outcome | module-contracts Worker port; fixture worker tests | — |
| Stop semantics: identity gates, group-wide stop proof, unverified/unknown distinction | **retain** | #337's fixes encode exactly the right contract | `ProcessControlPort` stop proof (descendant snapshots, PID-reuse safety, reaping) | `workloop-exec` implements this | — |
| Plan/execute split with read-only planner | **re-model** | a useful hardening, but the agent/runtime boundary is worker-internal | worker implementation detail | fixture worker | not required by port contract |
| Pinned OpenCode version, DSH specifics, incus container details | **omit** | adapter/runtime internals | n/a | n/a | — |

### 6. Verification — did the attempt produce a sound result

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Declared checks run | after implementation, before propose | worker executes `checks` serially; bounded `parallel_group` batches (cap 2, write-surface independence documented) | per-check record (cmd, exit, timing, `timed_out`) | none | failing required check → run fails; `checks_failed` | `docs/worker-contract.md` checks/policy; `src/checks/run.ts`; `repo-setup-check` mechanism in `docs/execution/lifecycle.md` |
| Verify stage + remediation | `devflow verify` post-run | `verify.ts` produces verification record bound to exact patch bytes | verification record | patch + base | code-class failure seeds bounded remediation (`MAX_VERIFY_REMEDIATIONS = 2`); environment/human classes halt | `src/devflow/verify.ts`, `verify-remediate.ts` (issue #140) |
| High-risk review gate | before propose | `assessRunRiskFromRun` on the run's impact manifest — `failure_semantics`/`trust_boundaries`/`identity_invariants` dimensions | cleared-review manifest | manifest | missing reviewer *stops* the pipeline (never silently ships) | `src/devflow/review-gate.ts` (issue #115) |
| Settle classification | run end | `classify_result` prefers structured `stop.action` (done/retry/human/fix_env) over exit code | `last_result` on run record | record file | missing/corrupt classification is never success | `coding_worker_lib.py:1974` (`classify_result`); `docs/coding-worker.md` "終端判定" |

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
| `devflow propose` | verified run | the pipeline's only GitHub write path: branch, commit, push, PR | `propose-record.json` with bounded refusal codes | git remote, GitHub API | refusal codes are the only persisted detail (sanitized export can't leak); `publish_failed` covers partial publish | `src/devflow/propose.ts` (issue #41/#317): never merges, never force-pushes, marker + provenance on every body |
| `devflow converge` fix publication | converge round with fixes | converge pushes fix commits to head branch, posts marked replies with fix SHA | `findings.json` ledger, `converge.json` rounds | PR head, review threads | head-signal marks bound to head SHA | `src/devflow/converge.ts`; `src/review/head-signal.ts` |
| Remote-lane publication | remote ship/converge settle | coordinator `cwctl publish` + job-contract bounded publish stage (#447) | run record `publication_status`, `pushed_head` | remote container, GitHub | publish-before-collect ordering (#389); fail-closed adoption check | `coding_worker_remote_lib.py:905` (`publish`); `devin-milestone.py:8986` (`_cw_remote_converge_pushed_head`); issues #389 CLOSED, #445/#447 |
| Unknown write reconciliation | after any GitHub mutation | action-id/PR-URL reconciliation | proposal/publication records | GitHub reads | adopt matching existing effect; refuse ambiguous | `src/devflow/propose.ts` reconciliation; `scripts/host/publication-validation.sh`; `cwctl` publish exit 9 (second-different-PR conflict) |

**Classification — Publication**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Single publication owner with persisted intent | **retain** | #389: double publication owner → duplicate-PR risk; #445: verdict without push evidence | core-owned `PrActionIntent` outbox is the only mutation entry; unknown responses reconciled by action ID | module-contracts PrActionIntent; `workloop-github` fixture | real adapter is issue #17 |
| Verdict requires publication evidence (fail-closed adoption) | **retain** | #445: `fixed` while head == pre-run head must block, never settle | stage contract: `converge` outcome = fix commits *published* to PR head | lifecycle.md head-bound completion; contract test | — |
| Marked replies, no merge/force-push, provenance markers | **retain** | machine marker + bounded effects are the trust contract | adapter mutation surface | fixture adapter records intents | — |
| Bounded refusal codes as the only persisted failure detail | **re-model** | sanitize-by-code is a good idea; exact code set is implementation detail | failure codes on Command results | lifecycle.md failure classes | — |
| Job-contract embedded publish instructions (remote lane) | **omit** | executor-internal workaround for a stage-boundary bug; the invariant (publication is part of the stage) is what transfers | stage contract via port | n/a | — |

### 8. Review / fix — convergence on a PR head

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Devin Review path | `pull_request_review` webhook on managed PR | `coding-worker-resume-only.py` records resume events; dispatch consumes | `cw-resume-events` ledger, debounce | webhook, managed-PR binding | at-least-once events; replay-safe | webhook route (automation-inventory); `devin-milestone.py` `_cw_drain_resume_events` (~L14567) |
| Lane shadow review | shadow event (webhook or lane-emitted) | `shadow-review-only.py` filter + `shadow-debounce-dispatch.py` | `cw-shadow-events` ledger | PR author/sender, lane binding | bot-author drop; lane-bound exemption post-#447 | `shadow-review-only.py`, `shadow-debounce-dispatch.py`; `docs/shadow-review.md`; issues #234, #323, #446, #447 |
| Converge rounds | review findings on a head | `convergePr`: classify findings (severity × disposition), one batched bounded fix run per round, round cap | `findings.json` (cross-invocation), `converge.json` (per-invocation) | PR checks, review surfaces | `converged`/`awaiting-human`/`capped-*`/`halted`/`budget-exhausted`; security/data/high-severity → human halt | `src/devflow/converge.ts` (`ConvergeStatus`, `FindingsLedger`, `acquireConvergeLock` — out-dir scoped, not repo-wide) |
| Judgment capture/release | operator decision on findings | `cw-judge`/`cw-release` → `record_judgment_wait`/`record_judgment`/`release_judgment_wait`; carryover across generations | `cw-judgment-waits/judgment-*.json`, `judgments.json` handoff | PR head at judgment vs release | head move stales answers — never silently rebind; replay of lost response goes to owner ledger | `coding_worker_lib.py` H3 (`record_judgment_wait` ~L4151, `record_judgment` ~L4613, `mark_judgment_head_change` ~L4713, `release_judgment_wait` ~L4855); `cmd_cw_judge` (`devin-milestone.py:5623`), `cmd_cw_release` (:5695), `cmd_cw_autojudge` (:5881) |
| Console judgment UI | operator action | `kanban-dev-console-rust` builds structured requests; `kanban-dev-action.py` delegates to `cw-judge`/`cw-release`/scheduler reenqueue | projection cache only (snapshot-dir, atomic) | live head, finding IDs | preflight + schema-checked response; exact replay bypasses preflight | `scripts/kanban-dev-console-rust/src/action.rs` (~L183–314), `app.rs`; `docs/kanban-dev-console.md` |
| Finding disposition evidence | settle/reconciliation | `cw-finding-dispositions/finding-*.json` — cross-attempt proof a finding was fixed | disposition ledger | commits on PR | later explicit halt supersedes older fix evidence | `coding_worker_lib.py` H2 (`record_finding_disposition`, `reconcile_converge_findings` ~L3054) |

**Classification — Review / fix**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Head-bound judgment: stale answers never silently rebind | **retain** | head is the unit of review truth | `judge_findings`/`release_judgment` with expected head + version; stale head = explicit rejection | module-contracts judgment commands; contract tests | — |
| Bounded converge loop with severity-routed human halt | **retain** | the review/fix loop is the product; the bound + human-halt boundary is the safety shape | bounded fix rounds → new Invocations; Attention for human-decision halt | lifecycle.md review/fix states | convergence stall detection remains open (incidents §) |
| Judgment waits/decisions as durable ledgers surviving card replacement | **retain** | decisions outlive attempts and cards | Work-level decision/evidence records keyed to head | lifecycle.md evidence | — |
| Finding disposition ledger across attempts | **retain** | proof a *finding* was fixed must outlive the run that fixed it | evidence refs on Execution/Invocation | store event log | — |
| Console (Rust TUI) over versioned read contracts + typed actions | **re-model** | same shape (reads projections, submits Commands, never reclassifies) but workloop TUI is first-party, not a foreign-board adapter | `workloop-tui` over Command/Query | existing crate | — |
| Webhook routes, debounce timers, bot-author filters | **omit** | transport of a specific deployment; workloop ingests Observations through the GitHub adapter instead | GitHub adapter Observations | fixture adapter | reviewer-orthogonality lesson retained via #21 invariants |
| ConvergeLock as out-dir serialization | **omit** | superseded by single-writer ownership | ownership/claim | n/a | — |

### 9. Recovery — returning from failure

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Reconcile sweep | cron `devin-milestone-reconcile` */5 | `_reconcile_pass` advances card-owned runs; settles card/run disagreement | run/card state | process liveness, run records | tri-state liveness; `uncertain` holds | `devin-milestone-reconcile.py`; `_cw_settle_terminal` (`devin-milestone.py` ~L7307) |
| Blocker re-evaluation | independent periodic tick (#358) | `blocker_reeval_lib` — classifies from structured evidence only, recovers only proven-transient causes | evaluation/next-check/attempt records | issue state, timers | fails closed on unknown; never auto-recovers human/spec/manual blockers; deterministic retry key | `scripts/blocker_reeval_lib.py`; `docs/blocker-reeval.md` |
| Remote recollect/discard/quarantine | operator action | `cmd_cw_remote_recollect` / `discard` / `quarantine` | resets `push_check_failures`; quarantine fences unverifiable runs | container/evidence | container+evidence retained until proven; `cwctl collect` exit 10 = permanent refusal (settle, stop retrying) | `devin-milestone.py:11588/11834/12294`; `src/lifecycle.rs` exit-code table (`EXIT_COLLECT_PERMANENT`); issue #377 |
| Resume from retained workspace | operator `devflow submit --resume` | `resolveResume` binds resume chain by job hash (task identity), adopts completed leaf | ship dir workspaces | evidence files | unreadable/mismatched binding → error, never half-trusted | `src/devflow/resume.ts` (issue #105); `bound-runs.ts` (dead-end runs count toward dispatch budget, #139/#124/#221) |
| Timeout replan attribution | reconcile detecting timeout | checkpoint marker/PID/invocation-stamp attribution to a prior run; unreadable → possibly-live → refuse successor | checkpoint/ship_dir evidence | filesystem, PID | kill-then-final-flush can mimic foreign writer — attribution, not freshness | `devin-milestone.py` ~L8931 (attribution heuristic); issue #381 CLOSED 2026-10-23 |
| Retention/pruning | cron | `cw_run_reclaim`, `cw_round_prune`, `cw_orphan_reaper` | — | state dirs | aggressive cleanup destroyed required evidence | `scripts/cw_run_reclaim_lib.py`, `cw_round_prune_lib.py`, `cw_orphan_reaper_lib.py`; `docs/disk-reclaim.md` |

**Classification — Recovery**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Reconciliation reads facts, never invents them | **retain** | every recovery path reconciles persisted evidence + live facts; tri-state liveness | store + supervisor reconcile loop; `uncertain` holds capacity | lifecycle.md; `workloop-cli` run loop | — |
| Idempotent lifecycle ops keyed on stable identities | **retain** | cwctl's repeated-call reconciliation is the right shape | all Commands idempotent by key+digest | store command tests | — |
| Bounded, recorded recovery with permanent-refusal class | **retain** | #377: recovery retry needs durable attempts, budgets, escalation — `max_invocations` alone doesn't bound collect | bounded durable recovery (waiting reason / paused(repair) / failed(evidence_unavailable)); Attention escalation | lifecycle.md; contract tests | artifact-disposition state still unmodeled in current contract (open) |
| Deterministic blocker re-evaluation from structured evidence | **re-model** | the *requirement* (resume only when provable conditions changed; never auto-expand permissions, #215) transfers; the periodic-job mechanism doesn't | `paused` continuation conditions + resume validation | lifecycle.md resume contract | recording "what must change to resume" is a required field — currently implicit |
| Attribution heuristic for foreign writers (checkpoint markers) | **omit as mechanism / retain as requirement** | the heuristic is ship_dir-specific; the requirement — a writer whose provenance can't be proven is treated as live — transfers | `SafeOwnership` evidence | module-contracts | ownership scope decision open (incident audit) |
| Prune/reclaim scripts | **omit** | they destroyed evidence the workloop requires; retention policy is a deliberate non-port | append-only `event_log` preserves history | store | retention/export safety is decisions.md Q4 — open |

### 10. Attention — surfacing what needs a human

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Attention projection | status/attention read | `build_attention` — reconciles incidents, parked selectors, blocked cards, judgment waits; `unknown` reason/source/id is surfaced, not hidden | projection only (no authority) | board, requests, runs, incidents | bounded GitHub lookups (#315); incomplete evidence → `unknown` item | `kanban_dev_status_lib.py:3080` (`build_attention`, schema `kanban-dev-attention/0.1`), `_cw_blocked_attention` (~L2637 reads ship.json/converge.json for fresh cause) |
| Stale-attention cleanup | scheduled job (#295) | `kanban_attention_cleanup_lib` — archives only when every linked issue/PR is closed/merged, or a provably-newer duplicate covers all identity | archive ledger (auditable) | GitHub issue/PR state | fails closed on uncertainty | `kanban_attention_cleanup_lib.py` (`_covers`, `_newer_covering`, `_active_reason`); `docs/kanban-attention-cleanup.md` |
| Change-aware notice + acknowledge | notify tick (#309) | `coding-worker-attention-hourly`; acknowledge marks seen without releasing any gate | notice state | — | repeated identical blocker → notify once | `coding-worker-attention-run.py`; issue #309 CLOSED |

**Classification — Attention**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Attention = actionable-now decisions, derived not stored | **retain** | the projection has no authority; uncertainty is displayed | workloop Attention: paused judgment, confirmed failure with eligible retry, prolonged uncertain | lifecycle.md Attention rules; TUI/CLI queries | "prolonged" threshold and acknowledge-visibility semantics undecided |
| Unknown-surfaced-over-hidden | **retain** | an incomplete-evidence item is shown as unknown, never silently dropped | projection exposes provenance/uncertainty | query layer | — |
| Archive/cleanup job with fail-closed covering checks | **omit** | needed only because Attention was persisted state; derived Attention makes stale items impossible by construction | n/a | n/a | if historical terminal evidence ever counts as non-actionable needs an explicit rule |
| Change-aware notification + Telegram delivery | **omit** | delivery mechanism is personal infra | n/a | n/a | acknowledgment-as-seen (not gate release) retained as semantic |

### 11. Environment setup — making a host able to run work

| Behavior | Trigger | Decision authority | Persisted state | External facts | Failure / recovery | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Installer scripts | operator run | `install-<feature>.sh` — copy `scripts/`/`skills/` into `~/.hermes/`; idempotent, mostly no uninstall | installed copies, manifest | host fs | `--check` detects drift | `scripts/install-*.sh`; AGENTS.md "Deploying changes" |
| Packaged component install | operator | `install-kanban-dev-console.sh` — manifest + per-file SHA-256, atomic promote, `.previous` rollback, `--disable`/`--rollback` | console-owned root `~/.hermes/kanban-dev-console/` | backend/schema compatibility | missing/incompatible backend → hard fail, no degraded view | `docs/kanban-dev-console.md`; `tests/test_kanban_dev_console_package.py` |
| Worker host bootstrap | one-time host setup | `scripts/host/bootstrap.sh` + restricted-SSH helper; image build pins worker artifact sha256 (`component-manifest.json`) | host state, image | incus, SSH | documented runbook | `scripts/host/bootstrap.sh`, `helper.sh`, `ssh-transport.*`; `docs/host/bootstrap.md`, `restricted-ssh.md`; `scripts/incus/build-worker-image.sh` |
| Credential handling | runtime | `credentials.ts` (worker); `credential_files`/`redact`/`_argv_has_no_secrets` (remote lane) | credential refs, never values in job | local auth files | secrets stay out of argv/`ps` | `src/credentials.ts`; `coding_worker_remote_lib.py:351–508` |

**Classification — Environment setup**

| Behavior | Class | Rationale | Destination | Impl / test mapping | Verification gaps |
| --- | --- | --- | --- | --- | --- |
| Declarative installation config | **re-model** | the requirement — actor identity, worker argv, capacity, workspace root, allowed effects — is generic; file layout is personal | `workloop-config` | existing crate | — |
| Secrets never in argv/logs/evidence | **retain** | credential refs not values; redaction is structural | secrets policy on adapters; `credentials_ref` opaque | Worker port input contract | — |
| Hermes deploy model (repo→`~/.hermes` copy, skill wiring, manifest/rollback packaging) | **omit** | personal host ops | n/a | n/a | — |

## Incident audit

Six failure classes the issue names explicitly. "Incident" = a recorded
past failure; "residual defect" = behavior that can still fail today.
CLOSED status alone is not proof of non-recurrence — it means a fix was
deployed; each class states what the fix was and what remains.

### Card / run disagreement

- **Incidents**: #337 (CLOSED 2026-10-21 — timed-out ship runs left cards
  `running`), #343 (CLOSED 2026-10-21 — human-judgment halt left converge
  run/card `running`), #266 (CLOSED 2026-10-16 — claim-TTL reclaim marked
  live successful ships `gave_up`/`blocked` while the process succeeded).
- **Fixes deployed**: reconcile sweep (`_reconcile_pass`,
  `_cw_settle_terminal`); durable stop-intent marker before signal;
  tri-state `pid_alive_state` (unknown ≠ dead); verified pid identity
  + start stamp before kill; heartbeat/claim semantics clarified.
- **Residual defect**: the structural cause persists in hermes — authority
  is split across card + request + run record + live process, so
  disagreement is reconciled rather than impossible. In workloop this is
  re-modeled: one authoritative store, `Work`/`Execution`/`Invocation`
  separation, `uncertain` as first-class. **Disposition: re-model.**
- **Remaining hermes defect found in this audit**: the durable stop-intent
  marker save swallows `OSError` (`devin-milestone.py:13777–13784` —
  `except OSError: pass`), so the signal is still delivered when the
  marker write fails. The docstring contract says refusal must not leave
  a marker, but nothing says a marker failure must abort the signal —
  a crash-consistent write order violation that remains live today in
  `_stamp_stop_attempt`.

### Duplicate dispatch

- **Incidents**: #185 (CLOSED — retry bookkeeping broke the request
  fingerprint), #307 (CLOSED — three-way ambiguity caused wrong human
  action), #381 (CLOSED — lane-blind equivalent-active scan collided with
  manual `devflow ship --resume` in the same `ship_dir`).
- **Fixes deployed**: fingerprint excludes bookkeeping; three-way
  exclusion classification; checkpoint attribution (marker + PID +
  invocation stamp + `updated_at`) with unreadable→possibly-live→refuse;
  worker-side `agent:running` label + `ship.lock` claim.
- **Residual defect**: the manual-writer boundary — a foreign process
  (unmanaged CLI, manual salvage) writing to a managed workspace is only
  defended by heuristic attribution inside ship_dir, and only on the
  paths that check it. Workloop must decide `SafeOwnership` scope:
  per-Work writer uniqueness is specified; cross-Work / external-writer
  exclusion is not. **Disposition: re-model (ownership scope is an open
  question, not a ported mechanism).**

### Uncertain stops

- **Incidents**: #337 (CLOSED — see above), #266 (CLOSED).
- **Fixes deployed**: `stop_record_process` contract — verified start
  stamp + group-member count as stop proof, `unverified` vs `unknown`;
  remote supervisor `uncertain` state on missing terminal record;
  stop-intent marker ordering (inside pre-signal gate).
- **Residual defect**: the `OSError` swallow above — a stop-intent
  persistence failure does not abort the signal. Also, `unknown` after
  delivered signal relies on the next sweep reading group death "as
  ours"; there is no independent deadline for how long `stopping` may
  hold before escalation. **Disposition: retain the contract, fix the
  swallow in re-modeling (persist stop intent as part of the `stopping`
  commit *before* signaling — structurally guaranteed by the single
  store).**

### Missing PR publication

- **Incidents**: #445 (CLOSED 2026-10-28 — remote converge reported
  `review-fixed`, never pushed; container destroyed with the commits;
  real loss on coding-worker PRs #329/#330), #389 (CLOSED 2026-10-23 —
  collect-before-publish doubled the publication owner).
- **Fixes deployed**: #447 (MERGED) — bounded publish stage in the remote
  job contract (`git push origin HEAD:<head-ref>`, marked replies,
  `fixed` only after push); fail-closed adoption check
  (`_cw_remote_converge_pushed_head`) covering *all* produced-commit
  completes, `push-uncertain`/`push-failed` classes; `cw-remote-recollect`
  resets `push_check_failures`.
- **Residual defect**: none observed in code at `6371cf9`; live-operation
  verification is absent (see Evidence classification). The generalized
  defect class — a stage whose definition varies by executor — is the
  subject of agent-workloop #21. **Disposition: retain the invariant
  (publication evidence required for success classification), re-model
  as executor-independent stage contract.**

### Stale-head reviews

- **Incidents**: #446 (CLOSED 2026-10-28 — webhook filter dropped
  bot-authored PRs; remote-lane PRs structurally never received shadow
  review; `reviewer: shadow|both` silently degraded to `devin`),
  #234 (CLOSED — the original shadow route), #246 in coding-worker
  (CLOSED — skip PRs already merged at review start), #382 (CLOSED —
  decoupled reviewer firing from executor).
- **Fixes deployed**: lane-bound exemption (`cw_managed_prs` /
  `cw_pushed_heads`, not the author-controlled PR body marker, fail-closed
  on unreadable ledger); lane emits `origin="lane"` shadow events on
  settle against the *live* head; head-signal markers bind replies to
  head SHA; `mark_judgment_head_change` stales pending answers.
- **Residual defect**: the reviewer dimension is now deliverable on
  remote lanes, but only because the lane synthesizes the event the
  webhook would have sent — a second, config-dependent firing path.
  Judgments bound to a stale head are still rejected correctly.
  **Disposition: retain head-bound judgment and reviewer-orthogonality
  invariants (#21); omit the webhook/debounce mechanism.**

### Stalled convergence

- **Incidents**: #377 (CLOSED 2026-10-22 — terminal remote failure known,
  collect retried forever, card `running`), #326 (OPEN — converge 30-min
  stall vs 120-min deadline vs pending-retry expiry vs settle-lodging
  proposal), #379 (OPEN — same failure kind 4× ≈30 min each), #278/#279
  (OPEN — bounded recovery-first parking proposals).
- **Fixes deployed**: bounded collect with `push_check_failures` budget
  (max 3), `push-uncertain`/`push-failed` blocking classes, container +
  evidence retention, `cw-remote-recollect`; `cwctl` exit 10 permanent
  collect refusal.
- **Residual defects**: **stall detection, repeated-identical-failure
  detection, and bounded requeue after timeout are all still OPEN
  proposals in hermes** — this is the largest class that is *not* fixed
  at the recorded revision. Workloop's re-model must include: distinguish
  "no progress" (stall — notify only) from "deadline exceeded" (stop
  intent → proven group stop → bounded requeue); repeated identical
  failure kinds surface to Attention; recovery loops never run unbounded.
  **Disposition: retain as open requirements; the specific thresholds
  (30/120 min) are policy values, not invariants.**

## Design evidence issues

| Item | Status (2026-11-05) | What it contributes | Contract conflicts / missing evidence |
| --- | --- | --- | --- |
| `agent-workloop` issue #21 | OPEN | The cross-cutting invariant discovered by #445/#446: **stage contracts are executor-independent** — `converge` means "bounded fix commits published to the PR head, marked replies, draft→ready exactly once" regardless of where the stage runs; **success classification requires publication evidence**; reviewer dimension is orthogonal to executor | Its invariants are the audit's own re-model targets; no conflict. Evidence: issue body records two verified runs (coding-worker PRs #329/#330) |
| `hermes-setup` #445 | CLOSED 2026-10-28 | Missing-publication incident; exact reproduction (run id, card id, 4 unresolved threads, head still `38805f36`) | Conflict it exposed: remote job contract ended at "make the bounded fix + CONVERGE_RESULT" — no publication stage; resolved by #447 |
| `hermes-setup` #446 | CLOSED 2026-10-28 | Silent-reviewer incident; evidence: 97 shadow events all human-authored, zero for lane PRs | Exposed that one matrix dimension (reviewer) was silently overridden by another (executor) — the orthogonality invariant |
| `hermes-setup` #447 | MERGED | The fix: bounded publish stage inside the job contract + fail-closed adoption check + lane-bound webhook exemption + lane-emitted shadow events | Contract change: `CONVERGE_RESULT: fixed` now means "fix pushed to head", not "fix produced"; verification of live post-merge behavior is absent — the code paths were read, not exercised |

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
- **Personal-environment (omit)**: Hermes Kanban board/card/request/
  run-record triangulation; claim TTLs and heartbeats; parked-selector
  registry; lane executor matrix and per-repo YAML policies as formats;
  Telegram delivery and cron cadence; webhook filters and debounce;
  `install-*.sh` repo→`~/.hermes` deploy model; pinned OpenCode runtime;
  incus containers and restricted-SSH transport; prune/reclaim scripts;
  ConvergeLock; checkpoint-attribution heuristics.

## Design aspects

Per the issue, no concrete rule is settled here — each aspect records
applicability, rationale, and open questions.

### State transitions

- **Applicable.** The card/request/run-record triangulation and its
  disagreement class are the direct ancestors of the
  Work/Execution/Invocation split and the single authoritative store.
- **Rationale.** The audit shows the source system *reconciles*
  disagreement continuously (reconcile sweep, `_cw_settle_terminal`,
  judgment-wait ledgers); workloop instead makes most disagreement
  unrepresentable — which is exactly why the port is a re-model.
- **Open questions.** Whether queued-admission ownership checks treat a
  foreign writer as `queued+blocked-diagnostic` or `uncertain` (research
  notes suggest both readings); whether artifact-collection pending
  becomes a `waiting` reason or a `paused(repair)` substate.

### Failure semantics

- **Applicable.** Tri-state liveness, `unverified` vs `unknown` stop
  results, `uncertain` remote acks, permanent vs transient collect
  refusal (exit 10), `stop.action` over exit code.
- **Rationale.** These distinctions each exist because conflating them
  caused a specific incident (#266, #307, #337, #377).
- **Open questions.** The swallowed stop-intent `OSError` shows "persist
  intent before signal" is asserted but not enforced in the source;
  workloop must decide the commit ordering (intent inside the `stopping`
  transition) rather than port the callback. Whether every launch attempt
  counts against `max_invocations` — v0.1 may deliberately simplify, but
  it must say so.

### Trust boundaries

- **Applicable.** Worker reports are claims; GitHub mutations only via
  persisted intent; judgments need explicit release; remote-adapter text
  never reaches card-facing strings unchecked (bounded reason codes).
- **Rationale.** Each boundary exists because authority confusion caused
  incidents (#389 double publication; #445 verdict-without-effect;
  card-facing strings carrying remote detail).
- **Open questions.** Credential/permission scope per adapter (issue
  #35); whether a declared-high-risk manifest gate like `review-gate.ts`
  belongs in v0.1 scope.

### Identity invariants

- **Applicable.** `idempotency_key(job_id, attempt, base_revision)`,
  `jobHash` binding of runs to tasks, request fingerprint excluding
  bookkeeping, claim pinned to GitHub `labeled` event id, plan keys
  binding head+findings+decisions, `run_id` vs coordinator vs worker
  ids deliberately distinct.
- **Rationale.** Workloop's `work_id`/`execution_id`/`invocation_id`
  exist precisely because issue numbers, card IDs, and PIDs were not
  stable identities.
- **Open questions.** How Work identity relates to multi-issue groups
  and external card IDs (issue #34); whether finding IDs are
  review-surface identities or durable defect identities.

### Information preservation

- **Applicable.** `findings.json` surviving invocations, judgment ledgers
  surviving card replacement, disposition ledgers surviving attempts,
  retention failures (prune scripts) destroying required evidence,
  sanitized export via bounded refusal codes.
- **Rationale.** The append-only `event_log`, evidence refs, and
  preserved-WIP-on-failure are direct responses.
- **Open questions.** Retention vs export safety (decisions.md Q4);
  artifact digest/retention minimums; whether an "evidence unavailable"
  terminal class is needed.

### Resource bounds

- **Applicable.** Global + per-repo capacity, claim TTLs, dispatch
  budgets (dead-end runs count), converge round caps, bounded
  remediation (`MAX_VERIFY_REMEDIATIONS`), `push_check_failures` budget,
  job `limits.max_steps`/`max_cost_usd`, ship 3h deadline vs
  `max_group_size: 3` (capacity-derived policy bound).
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
| Issue/PR states and titles (`gh`-queried 2026-11-05) | **valid** | All statuses re-queried for this audit |
| hermes-setup source symbols at `6371cf9` | **valid** | Verified by direct source reads (this audit) |
| coding-worker source at `66daef53`, devflow at `f89c35b`, coding-worker-infra at `3522605` | **valid** | Read via `git show`/`ls-tree` on `origin/main` |
| Research notes in `agent-workloop-audit-hermes-lifecycle` worktree (intake/review-scheduler/worker-recovery) | **valid-after-verification** | Used as pointers; every claim carried into this document was re-verified at this audit's revisions; their 2026-10-23 issue states were refreshed |
| hermes-setup design docs (`converge-state-transitions.md`, `devin-kanban.md`, `devflow-scheduler.md`, `blocker-reeval.md`, `coding-worker.md`, `shadow-review.md`) | **valid** as design documentation — not proof of deployed behavior | The converge doc pins `839094bb`/`e6c16686`; re-verified against this audit's revisions |
| `docs/automation-inventory.md` host evidence (cron/webhook listing, 2026-10-17) | **valid-after-verification** for 2026-10-17 only; current host state not re-checked | Treated as a recorded claim, not current fact |
| Live host state, installed `~/.hermes` copies, runtime process behavior, Telegram delivery | **absent** | Not inspected; flagged wherever a claim would have depended on it |
| Whether merged fix #447 works in production | **absent** | Code paths verified; operational outcome unverifiable from here |
| coding-worker-infra's own pinned-commit claims about coding-worker (`coding-worker-dsh-interface.md`) | **valid** as that repo's recorded claims | Independent re-verification against this audit's coding-worker revision where the audit depended on it |
| Prior audit's line numbers (e.g. scheduler ~L1449 at `74f45454`) | **valid-after-verification** | Symbols re-verified to exist at `6371cf9`; exact line numbers drifted |
| Web-search-derived issue history (from prior research notes) | **invalid** | Earlier notes found search could not surface the private repo; this audit used authenticated `gh` only |
