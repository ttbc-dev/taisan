# Provenance

This document records where the workloop design comes from: which
external systems are design evidence rather than runtime dependencies,
and how much of the design is actually implemented. It is deliberately
minimal — it names no private repository, path, symbol, or issue number
from the source systems; references to this repository's own issues are
retained as maintainer locators. The
detailed audit — inspected revisions, per-behavior
retain/re-model/omit decisions, incident analysis, and evidence
classification — lives in
[source-workflow-audit](source-workflow-audit.md) (issue #33), the
audit record, which stays in the published tree under the owner
decision recorded in [release-audit](release-audit.md) §7 (#438).

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
internal identifiers are not reproduced in this project's
publication-facing documents and are not consulted at build or runtime;
the audit document records what was inspected and at which revisions.

## Private references in this repository

Separately from the source systems above, this tree records live
acceptance work executed against `shimpeiws/coding-worker-smoke`, a
private GitHub repository owned by this project's owner. Documents
that cite it — under `docs/s1/`, `docs/evidence/`, and
`fixtures/s1/` — retain the name verbatim: links and `#N` issue/PR
citations to it resolve only for the owner (they 404 publicly), and
each citing document marks those references as private/owner-only.
The same applies to owner-local paths (`~/.config/coding-worker/`,
`~/s1-*` run directories) and owner-recorded runtime identities
(GitHub App, installation, and user ids) inside owner-run records.
Disclosure of these names and the owner identity was accepted by
owner decision under issue #438; no credential values are recorded
anywhere in the tree.

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
- The GitHub integration ships as a fixture adapter for tests and, as
  the production path, `GitHubAdapter` composed in-process inside
  `taisan run` behind the `GhCliApi` transport — every GitHub call goes
  through the `gh` CLI as a bounded subprocess. Bounded reads are scoped
  to served hosts and active executions; writes reconcile by read-back
  before retry. Live-verified behavior against a real repository is
  still pending owner-run acceptance.
- The Worker port is exercised by `workloop-fixture-worker` and the
  independent `workloop-contract-worker`, both real local processes
  implementing the executable
  `taisan.worker.declaration/1` / `request/2` / `report/2` wire — not
  by a source-system worker. A real conforming Worker is owner-supplied
  and validated by the owner-run checklist.
- The Work Selector port has a reference implementation
  (`workloop-selector`, shipped as the `public-issue-selector`
  binary), a fixture selector, and the intake scheduler's
  configuration-selected argv binding; the selector repo remains the
  reference behavior, not a dependency.

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
  `crates/workloop-tui/tests/tui_surface.rs`. TUI delivery is deferred from
  v0.1; these tests do not prove CLI-backed skill delivery or resolve the
  same-head finding-growth gap tracked in issue #24.
- `workloop-config` — installation config: actor identity, worker argv,
  capacity, workspace root, allowed effects, the v0.3 Worker
  credential/egress keys, and the `worker_dispatch` v2 profile.
- `workloop-selector` — the reference Work Selector (pure proposal
  function) shipped as the `public-issue-selector` binary.
- `workloop-intake` — the internal intake scheduler (settings tick,
  interval grid, durable runs) driven by `taisan run`.
- `workloop-contract-worker` — an independent implementation of the
  executable Worker wire that shares no response construction with
  `workloop-core`; the unchanged conformance suite runs against it and
  the fixture worker.

**Fixtures:**

- `workloop-github` fixture adapter — records action intents and emits
  synthetic Observations for contract tests; the production
  `GitHubAdapter`/`GhCliApi` path is separate.
- `workloop-fixture-worker` — a real local process with argv-driven
  failure modes and the v2 wire, used by contract tests and runnable
  non-live installations.
- `workloop-fixture-selector` — deterministic selector fixture for
  intake contract tests.

**Required agent interface work:**

- CLI-backed agent skills, installation and invocation guidance, and
  end-to-end skill verification are not implemented. The CLI already emits
  JSON; its complete agent-facing result and failure contract remains to
  be verified. See [agent interface requirements](agent-interface.md).
- The existing TUI is retained but is not an initial-release requirement.
  An MCP server is optional future work.

**Missing integrations and open gaps:**

- Owner-run live acceptance of the real Worker path and the production
  GitHub driver — the [v0.3 owner-run checklist](v0.3-owner-acceptance-checklist.md)
  defines the scenarios; no live evidence exists yet.
- The `fixture` driver emits no `CapabilityReport` — it is the
  deterministic test double, so a fixture-driven run still parks at
  the host-effect boundary. The in-process `github` driver is a real
  producer: every unscoped source pass re-reads the credential's
  repository permission bits and records a report whose
  `authorization` axis mirrors the read (issue #355, closed). A
  scoped `run --work` pass emits none — it must not drive Work
  outside its scope.
- `check --probe` has no wired adapter probe backend; the stage fails
  closed `capability.unknown` by design.
- A real, owner-supplied conforming Worker implementation — the wire
  and conformance suite exist; the production Worker is not in this
  repository.
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
  speculation; the audit maps each rule to its contract coverage or open
  gap.
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
