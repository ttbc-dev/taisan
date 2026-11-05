# Provenance

This document records where the workloop design comes from: which
external systems are design evidence rather than runtime dependencies,
and how much of the design is actually implemented. It is deliberately
minimal — it names no private repository, path, symbol, or issue. The
detailed audit — inspected revisions, per-behavior
retain/re-model/omit decisions, incident analysis, and evidence
classification — lives in
[source-workflow-audit](source-workflow-audit.md) (issue #33), which is
the audit record and needs its own disclosure review before this
repository is published.

## Source systems

v0.1 extracts and reorganizes an existing personal development workflow.
Four private source systems supplied the design evidence. They are
referred to here by role, using the legend in the audit document:

| Role | What it is | What the workloop took from it |
| --- | --- | --- |
| the ops repo | Personal agent-operations repository hosting the Kanban extensions, dispatch lanes, scheduler, reconciliation and attention jobs, and the operator console | Lifecycle and dispatch failure modes (stale cards, stranded retries, wedged converge loops), the operator judgment interaction pattern, attention/acknowledgement semantics, and the decision to re-model rather than port the external board's state machine |
| the worker repo | Autonomous coding worker for issue-driven development | Worker-side evidence: a worker report is a claim, not an Outcome; bounded review/fix orchestration must not supersede core settlement authority |
| the selector repo | Harness-agnostic issue selection | Work Selector / Candidate behavior: stable proposal identity, grouping, allowlist scoping, and the boundary between proposing Work and accepting it |
| the infra repo | Remote execution infrastructure running bounded worker jobs in isolated containers | Execution-environment evidence: bounded runs, idempotent lifecycle operations, and the observation that a lost acknowledgement is not proof of failure |

All four systems are private. Their names, contents, histories, and
internal identifiers are not distributed with this project and are not
consulted at build or runtime; the audit document records what was
inspected and at which revisions.

## Provenance versus dependency

A **provenance reference** explains why a rule or boundary exists. It is
evidence for reviewers and future maintainers, never an import, a build
input, or a runtime requirement. A **runtime dependency** would be
something the code links against, shells out to, or reads at run time;
this project has none of the four source systems in that role.

Concretely:

- The built-in queue, state store, supervisor, CLI, and TUI are
  implemented in this repository's own Rust workspace. They do not call
  any source system.
- The GitHub integration ships as a fixture adapter that performs no
  network I/O; a real adapter is future work (see issue #17).
- The Worker port is exercised by `workloop-fixture-worker`, a real local
  process with scripted failure modes — not by a source-system worker.
- The Work Selector port is specified in
  [adapter boundaries](adapter-boundaries.md) but has no implementation
  yet; the selector repo is the reference behavior, not a dependency.

## Implementation status

This is a point-in-time audit of what the repository actually contains,
distinguishing implemented foundations, fixtures, and missing
integrations.

**Implemented foundations** (Rust workspace under `crates/`):

- `workloop-core` — domain types, Command/Query contracts, `work` decision
  logic, `projection`, and port traits; I/O-free.
- `workloop-store` — SQLite store implementing the hybrid schema
  (current-state tables plus append-only `event_log`).
- `workloop-exec` — supervisor and POSIX `ProcessControlPort` with stop
  proof: descendant snapshots, PID-reuse-safe signaling, zombie handling,
  and child reaping.
- `workloop-cli` — command/query surface and composition-root run loop
  (submit, retry, resume, cancel, review judgment, resource claims, queries,
  dispatch, and budget sweep).
- `workloop-tui` — management TUI over the same Command/Query contracts;
  the review-judgment flow (head-scoped findings, per-finding decisions,
  advisory preflight, stale-head requery) is covered by
  `judge_findings_flow_end_to_end`, `incomplete_draft_cannot_submit`, and
  `stale_head_rejection_requeries_and_clears_choices` in
  `crates/workloop-tui/tests/tui_surface.rs`.
- `workloop-config` — installation config: actor identity, worker argv,
  capacity, workspace root, and allowed effects.

**Fixtures:**

- `workloop-github` — fixture GitHub adapter only; records `PrActionIntent`
  outbox rows and emits synthetic Observations. No network I/O.
- `workloop-fixture-worker` — a real local process with argv-driven failure
  modes, used by contract tests and runnable installations.

**Missing integrations:**

- Real GitHub adapter (tracked by issue #17).
- A real Worker adapter — e.g. the Claude Code CLI reference worker; the
  port is exercised only by the fixture worker and arbitrary `worker_argv`
  processes.
- A Work Selector implementation (selector adapter or reference
  selector); the port is specified but unimplemented.
- GitHub comment command adapter and Projects projection.
- CI pipeline (tracked by issue #28) and the end-to-end v0.1 validation
  against a real GitHub repository.

## Design aspects: applicability, rationale, and open questions

The six aspects below record how provenance from the source systems bears
on each. Concrete rules live in the lifecycle and module contracts; the
detailed retain/re-model/omit decisions and their evidence are in the
audit document.

### State transitions

- **Applicable.** The ops repo's board and run-record model is the direct
  ancestor of the Work/Execution/Invocation split; its observed
  transitions (card versus run-record disagreement, stale cards) motivated
  the single authoritative store.
- **Rationale.** These docs deliberately avoid prescribing concrete
  transitions; the lifecycle contract in `docs/lifecycle.md` is the
  re-modeled result, not a port.
- **Open questions.** Which source transition rules were retained versus
  re-modeled is settled in the audit's classification tables; how Work
  identity relates to multi-issue groups and external card IDs remains
  open (issue #34, `docs/decisions.md`).

### Failure semantics

- **Applicable.** The infra repo's operational records (lost
  acknowledgements, disconnect survival, idempotent recovery) and the ops
  repo's incident history are the primary failure-semantics evidence.
- **Rationale.** Rules like "an expired claim is not stop evidence" and
  "unknown is a first-class state" come from observed failures, not
  speculation; each is covered by a contract test or scenario in this
  repository.
- **Open questions.** Which incident classes still lack contract-test
  coverage is tracked in `docs/implementation-plan.md` and the audit's
  verification gaps.

### Trust boundaries

- **Applicable.** The worker/core boundary (worker reports are evidence,
  not outcomes) and the GitHub boundary (Observations, never authority)
  both derive from the source systems' authority confusions.
- **Rationale.** The worker and infra repos define execution authority
  outside the control plane; the workloop keeps settlement inside the
  core.
- **Open questions.** The exact credential and permission scope each
  adapter needs is unsettled (issue #35).

### Identity invariants

- **Applicable.** `work_id`/`execution_id`/`invocation_id` exist precisely
  because issue numbers, card IDs, and process IDs in the source systems
  were not stable identities.
- **Rationale.** The selector repo's stable proposal keys and the worker
  repo's run IDs are the precedents for durable request identity.
- **Open questions.** How Work identity should relate to multi-issue
  groups and external card IDs remains open (issue #34,
  `docs/decisions.md`).

### Information preservation

- **Applicable.** The ops repo's retention and pruning jobs are the
  counter-evidence: the audit found they deleted operational records an
  incident review still needed.
- **Rationale.** The append-only `event_log`, evidence references, and
  preserved WIP on failure are direct responses to that history.
- **Open questions.** What evidence must be retained versus pruned, and
  how exports stay safe, is an open spike (`docs/decisions.md` Q4).

### Resource bounds

- **Applicable.** The infra repo's bounded containers and the ops repo's
  concurrency limits informed capacity slots, budgets, and TTL claims.
- **Rationale.** v0.1 defaults (`max_invocations`, `max_review_rounds`,
  `runtime_limit_seconds`, claim TTLs) are re-modeled from operational
  limits that were previously implicit.
- **Open questions.** Whether capacity should be per-host, per-repository,
  or global is unresolved until multi-host scope is considered.
