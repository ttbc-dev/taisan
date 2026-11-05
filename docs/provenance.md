# Provenance

This document records where the workloop design comes from, which external
systems are design evidence rather than runtime dependencies, and the audit
of documentation links and status descriptions performed under issue #32.

v0.1 extracts and reorganizes an existing personal development workflow into
an open source project. That workflow centers on the `hermes-setup` Kanban
extensions and `coding-worker`, with related behavior in `devflow` and
`coding-worker-infra`. None of these systems is required to build, run, test,
or operate this repository. They are the evidence the design was distilled
from; every normative rule in this repository's docs must stand on its own.

## Source systems

| Repository | Role in the original workflow | What the workloop took from it |
| --- | --- | --- |
| `shimpeiws/hermes-setup` (private) | Personal agent-operations repository hosting the Hermes Kanban extensions: the devin board and console, `devin-milestone` loop, `cw-dispatch` lanes, the devflow scheduler, reconciliation and attention scripts, and the coding-worker integration. | Lifecycle and dispatch failure modes (ghost cards, stranded retries, wedged converge loops), the operator judgment interaction pattern, attention/acknowledgement semantics, and the decision to re-model rather than port the Kanban state machine. |
| `shimpeiws/coding-worker` (private) | Autonomous coding worker for issue-driven development; owns execution — understand a task, inspect a repository, plan, edit, test, and return artifacts and evidence. | Worker-side evidence: a worker report is a claim, not an Outcome; bounded review/fix orchestration (`ship`/`converge`) must not supersede core settlement authority. |
| `shimpeiws/devflow` (private) | Harness-agnostic issue selection: answers "which issues can be started now, and in what order" as a deterministic function of a tracker snapshot. | Work Selector / Candidate behavior: stable proposal identity, grouping, allowlist scoping, and the boundary between proposing Work and accepting it. |
| `shimpeiws/coding-worker-infra` (private) | Remote execution infrastructure running bounded coding-worker jobs in isolated containers, coordinated from a single host. | Execution-environment evidence: bounded runs, idempotent lifecycle operations, and the observation that an SSH disconnect or lost acknowledgement is not proof of failure. |

All four repositories are private. Their names and roles are recorded here so
the design remains traceable; the repositories themselves, their histories,
and their contents are not distributed with this project and are not
consulted at build or runtime.

## Provenance versus dependency

A **provenance reference** explains why a rule or boundary exists. It is
evidence for reviewers and future maintainers, never an import, a build
input, or a runtime requirement. A **runtime dependency** would be something
the code links against, shells out to, or reads at run time; this project has
none of the four source systems in that role.

Concretely:

- The built-in queue, state store, supervisor, CLI, and TUI are implemented
  in this repository's own Rust workspace. They do not call hermes-setup,
  devflow, coding-worker, or coding-worker-infra.
- The GitHub integration ships as a fixture adapter that performs no network
  I/O; a real adapter is future work (see issue #17).
- The Worker port is exercised by `workloop-fixture-worker`, a real local
  process with scripted failure modes — not by coding-worker.
- The Work Selector port is specified in
  [adapter boundaries](adapter-boundaries.md) but has no implementation yet;
  devflow is the reference behavior, not a dependency.

## Scrubbed-term mapping

The design docs were originally written against the private source systems
and later generalized for publication. The generic terms below are the
scrubbed forms; this table restores the mapping so readers can trace each
claim back to its evidence.

| Term used in the docs | Source system |
| --- | --- |
| "external Kanban system", "external board records", "board card" | The hermes-setup Kanban (devin board and console) |
| "external workflow implementation" | hermes-setup (this was a scrubbing artifact; see the broken link below) |
| "selector", "Work Selector", "selector implementation" | devflow |
| "worker implementation", "a worker implementation" | coding-worker |
| "Auditor" (implementation-plan deferred list) | Unresolved — no matching repository or component was identified; treated as a generic automation concept, not a source-system reference |

## Reference verification

Each external or cross-document reference found in the documentation was
classified as **valid** (verified against its cited source), **invalid**
(malformed or pointing at an unrelated target), **absent** (target does not
exist), or **private/unresolved** (target exists but is not accessible to
readers of this repository).

| Reference | Cited at | Classification | Evidence |
| --- | --- | --- | --- |
| `https://github.com/shimpeiws/external workflow implementation/pull/386` | `docs/module-contracts.md`, TUI review judgment section | **Invalid** — a scrubbing artifact; the path segment is a placeholder, not a repository | Recovered from git history: the original text linked `shimpeiws/hermes-setup` PR 386 |
| `shimpeiws/hermes-setup` PR 386 — "feat: expose Kanban console overview and work detail projections (#372)" | Same section, now cited by name | **Valid target, private access** — merged 2026-10-23; adds `kanban-dev-console-rust` with `preflight → submit`, snapshot refresh, and lost-response replay, matching the described interaction | `gh api repos/shimpeiws/hermes-setup/pulls/386` (owner-verifiable only; the repository is private) |
| `shimpeiws/hermes-setup`, `shimpeiws/coding-worker`, `shimpeiws/devflow`, `shimpeiws/coding-worker-infra` | This document | **Valid targets, private access** — all four exist and are private | `gh repo view` on each name |
| Internal links between `README.md` and `docs/*.md` | All documents | **Valid** — every relative link and heading anchor resolves | Scripted link check over the repository's Markdown files |
| "Auditor" | `docs/implementation-plan.md` deferred list | **Unresolved** — no repository or component by that name was found under the project account; documented here rather than inventing a target | `gh repo view`/`gh search repos` returned no match |

Unresolved or inaccessible targets are recorded explicitly; no replacement
URLs were invented.

## Implementation status

This is a point-in-time audit of what the repository actually contains,
distinguishing implemented foundations, fixtures, and missing integrations.

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
- `workloop-tui` — management TUI over the same Command/Query contracts.
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
- A Work Selector implementation (devflow adapter or reference selector);
  the port is specified but unimplemented.
- GitHub comment command adapter and Projects projection.
- CI pipeline (tracked by issue #28) and the end-to-end v0.1 validation
  against a real GitHub repository.

## Document audit

### Documents checked

`README.md` and every file in `docs/`: `principles.md`, `architecture.md`,
`adapter-boundaries.md`, `implementation-plan.md`, `module-contracts.md`,
`lifecycle.md`, `glossary.md`, `v0.1.md`, `decisions.md`.

### Corrections made

- `README.md` — replaced "design proposal. There is no installable
  implementation yet." with a status that distinguishes the implemented
  slice-0 foundations, the fixtures, and the missing integrations; added
  this document to the design-documents list.
- `docs/module-contracts.md` — replaced the malformed
  `external workflow implementation/pull/386` link with the verified
  hermes-setup PR 386 reference, marked private, and pointed readers here.
- `docs/adapter-boundaries.md` — status line updated to reflect that the
  Worker role is exercised by the fixture worker while the Selector port
  remains unimplemented.
- `docs/implementation-plan.md` — repository baseline updated to the
  current crate inventory (added `workloop-config` and
  `workloop-fixture-worker`, confirmed `workloop-tui` exists).
- `docs/architecture.md` — module-map preamble reworded from future tense
  ("will implement") to scope language now that the TUI exists.

The remaining documents describe target contracts and milestone criteria.
Their "design proposal / not an implemented API" phrasing remains accurate
at the contract level — the documents specify intended semantics, and the
implementation-status section above records how much of that is built.

## Design aspects: applicability, rationale, and open questions

The six aspects below record how provenance from the source systems bears on
each, without settling concrete rules — those remain open for the design
issues that follow (#33–#36).

### State transitions

- **Applicable.** The hermes-setup Kanban and run-record model is the direct
  ancestor of the Work/Execution/Invocation split; its observed transitions
  (card vs run-record disagreement, ghost cards) motivated the single
  authoritative store.
- **Rationale.** The scrubbed docs deliberately avoid prescribing concrete
  transitions; the lifecycle contract in `docs/lifecycle.md` is the
  re-modeled result, not a port.
- **Open questions.** Which hermes-setup transition rules were retained
  versus re-modeled is the subject of the source-workflow audit (issue #33).

### Failure semantics

- **Applicable.** coding-worker-infra's operational records (lost SSH
  acknowledgements, disconnect survival, idempotent recovery) and
  hermes-setup's incident history are the primary failure-semantics evidence.
- **Rationale.** Rules like "an expired claim is not stop evidence" and
  "unknown is a first-class state" come from observed failures, not
  speculation.
- **Open questions.** Which incident classes still lack contract-test
  coverage is tracked in `docs/implementation-plan.md` and issue #33.

### Trust boundaries

- **Applicable.** The worker/core boundary (worker reports are evidence, not
  outcomes) and the GitHub boundary (Observations, never authority) both
  derive from the source systems' authority confusions.
- **Rationale.** coding-worker and coding-worker-infra define execution
  authority outside the control plane; the workloop keeps settlement inside
  the core.
- **Open questions.** The exact credential and permission scope each adapter
  needs is unsettled (issue #35).

### Identity invariants

- **Applicable.** `work_id`/`execution_id`/`invocation_id` exist precisely
  because issue numbers, card IDs, and process IDs in the source systems
  were not stable identities.
- **Rationale.** devflow's stable proposal keys and coding-worker's run IDs
  are the precedents for durable request identity.
- **Open questions.** How Work identity should relate to multi-issue
  groups and external card IDs remains open (issue #34, decisions.md).

### Information preservation

- **Applicable.** hermes-setup's retention and pruning scripts
  (`cw-round-prune`, `cw-run-reclaim`) are the counter-evidence: aggressive
  cleanup destroyed evidence the workloop now requires.
- **Rationale.** The append-only `event_log`, evidence references, and
  preserved WIP on failure are direct responses to that history.
- **Open questions.** What evidence must be retained versus pruned, and how
  exports stay safe, is an open spike (decisions.md Q4).

### Resource bounds

- **Applicable.** coding-worker-infra's bounded containers and hermes-setup's
  concurrency limits informed capacity slots, budgets, and TTL claims.
- **Rationale.** v0.1 defaults (`max_invocations`, `max_review_rounds`,
  `runtime_limit_seconds`, claim TTLs) are re-modeled from operational
  limits that were previously implicit.
- **Open questions.** Whether capacity should be per-host, per-repository,
  or global is unresolved until multi-host scope is considered.
