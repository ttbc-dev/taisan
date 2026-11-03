# Selector and worker adapter boundaries

Status: design proposal (2026-11-03). This document refines [architecture](architecture.md), [module contracts](module-contracts.md), and [lifecycle](lifecycle.md). It does not claim that these interfaces are implemented.

## Goal

Agent Workloop is a **control plane**, not a work planner and not a coding agent. The existing Hermes Setup loop is the primary source of operational requirements, but its Kanban/card state model and dispatch implementation are **not** dependencies or code to copy. Re-model the lifecycle from invariants and observed failure modes.

Two independently replaceable ports surround the core:

- **Work Selector** decides *what to propose*: which issues belong together and why. The existing devflow selection/grouping is a possible implementation, not a core dependency.
- **Worker** decides *how to perform one authorized invocation*: coding, review, or fix. Claude Code, coding-worker, or another runner may implement the port. Their internal orchestration is not the Workloop authority.

Neither adapter writes core state, reserves capacity, creates Executions, retries Work, or declares a Work completed. Core decisions remain deterministic given persisted facts and policy.

## Ownership matrix

| Responsibility | Selector | Workloop Core | Worker | GitHub integration |
| --- | --- | --- | --- | --- |
| Discover candidate issues / grouping | Owns | Validates submitted proposal | No | Supplies issue facts |
| Accept Work, identity, dedup, queue | Proposes | Owns | No | No |
| Capacity, resource ownership, launch permission | No | Owns | Obeys | No |
| Plan, code, test, review as requested | No | Requests stage | Owns execution | Observes external facts |
| Retry/resume/cancel and stop proof | No | Owns decisions and supervision | Supports stop/observe | No |
| PR head, checks, reviews | No | Evaluates evidence | May produce artifacts | Owns observation/publication |
| Completion, human Attention | No | Owns | Reports evidence only | Supplies facts |

### Selector contract

A selector receives an explicit repository allowlist, selection policy, a snapshot/cursor of available source facts, and a bounded request (e.g. maximum candidates). It returns **proposals**, not commands:

- stable proposal key and source revision/cursor;
- one or more issue references; goal/intent, constraints, optional verification requirements;
- rationale and provenance; optional grouping key;
- declared required permissions/resources; no hidden expansion beyond allowlist.

Core validates proposal shape, repository scope, source freshness, idempotency and overlap/admission policy. Acceptance creates Work and its first queued Execution through the normal Command path. A selector may return zero proposals or fail without stopping existing Work. A proposal does not reserve a slot or authorize a worker.

**Reference selector:** list eligible GitHub issues from a configured allowlist, propose **one issue per Work**, no semantic grouping, deterministic order and stable proposal keys. This is intentionally simple. More sophisticated grouping belongs in devflow or any third-party adapter satisfying the contract.

Open contract question: the exact candidate dedup/overlap policy across independently selected Works; specify before enabling unattended selection. The current manual `submit` remains supported and must not depend on a selector.

### Worker contract

A Worker Adapter receives a durable, uniquely identified **Invocation request** with:

- execution/invocation/request IDs, role (coding/review/fix), immutable goal and constraints;
- workspace and permitted resources, base/head revision when relevant;
- verification instructions, budget/deadline, evidence and continuation references;
- explicit permissions and credential scope.

It provides:

- `start(request)`: acknowledge a stable request identity; distinguish known not-started, started, and **unknown** outcomes;
- `observe(request_id)`: report process identity/liveness, checkpoint, artifacts, completion/failure and evidence provenance;
- `request_stop(request_id)`: request cancellation; **not** proof that writers stopped;
- optional `resume` capability declared by the adapter; absence means Workloop cannot assume an internal session can be resumed.

The core supervisor persists launch intent, owns resource reservations, and establishes stop proof before authorizing a conflicting writer. Worker exit code, self-reported success, PR URL or a model's summary are evidence, not Work completion. Results must be tied to the Invocation and, for review, the reviewed PR head. Replays must not silently start a second worker.

**Reference worker:** a thin Claude Code CLI process adapter that launches one bounded request in an isolated workspace, captures exit/process identity and output artifacts, and exposes cancellation and observation. No built-in planning pipeline, review framework or proprietary Claude Code-specific state in Core. A coding-worker adapter is another valid implementation, not an initial dependency; its own `ship/converge` orchestration must not silently supersede core retry/settlement authority.

## Separation of control surfaces and adapters

CLI, TUI and GitHub comments are **control interfaces** translating human/automation commands. They are not selectors or workers. GitHub issue/PR observation and publication is an **integration adapter**. Keep these ports separate even if a single binary hosts them in v0.1.

## Required contract tests

1. Selector returns duplicate or reordered proposals: stable identities, no duplicate accepted Work.
2. Selector attempts out-of-allowlist or stale issue: rejected with evidence; existing queue continues.
3. Selector unavailable: manual submit and already queued Work remain usable.
4. Worker start response lost: same request reconciled; no second start.
5. Worker reports success without required artifacts/checks: no successful Work outcome.
6. Worker crashes, escapes process group, or fails stop proof: ownership stays uncertain and replacement is blocked.
7. Review result refers to an old head: cannot settle a newer head.
8. Swap two conforming worker implementations: Workloop command, queue, budget and outcome semantics remain unchanged.
9. An adapter attempts to request retry or change a Work state directly: rejected; only Commands through Core may do so.

## Non-goals

No Hermes Kanban migration/synchronization, no devflow requirement, no coding-worker requirement, no general plugin marketplace or dynamic loader, no automatic semantic grouping in the reference selector, and no model-specific orchestration inside the core.
