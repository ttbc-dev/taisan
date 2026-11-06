# Selector and worker adapter boundaries

Status: design proposal for the adapter contracts. This document refines [architecture](architecture.md), [module contracts](module-contracts.md), and [lifecycle](lifecycle.md). The Worker role is exercised through the execution supervisor and a configurable `worker_argv` (the fixture worker in slice 0); the Work Selector port and the real GitHub adapter are not yet implemented. See [provenance](provenance.md) for the implementation audit.

## Goal

taisan is a **control plane**, not a work planner and not a coding agent. The lifecycle is derived from explicit invariants and failure scenarios. The core defines its own state model and dispatch rules.

Two independently replaceable ports surround the core:

- **Work Selector** decides *what to propose*: which issues belong together and why. Issue selection and grouping are adapter responsibilities, not core dependencies.
- **Worker** decides *how to perform one authorized invocation*: coding, review, or fix. Claude Code, a worker implementation, or another runner may implement the port. Their internal orchestration is not the taisan authority.

Neither adapter writes core state, reserves capacity, creates Executions, retries Work, or declares a Work completed. Core decisions remain deterministic given persisted facts and policy.

## Ownership matrix

| Responsibility | Selector | taisan Core | Worker | GitHub integration |
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

**Reference selector:** list eligible GitHub issues from a configured allowlist, propose **one issue per Work**, no semantic grouping, deterministic order and stable proposal keys. This is intentionally simple. More sophisticated grouping belongs in any third-party adapter satisfying the contract.

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
- optional `resume` capability declared by the adapter; absence means taisan cannot assume an internal session can be resumed.

Worker-internal mechanics stay inside the adapter: how a process is launched, how requests and results are translated to and from the worker's own formats, authentication to the worker runtime, internal session state, and any resume implementation. None of these may surface in Core types, state transitions, or Outcome decisions — the Core contract above is the whole boundary. Capabilities are either required by this contract or explicitly declared optional; an adapter must never silently emulate a capability it does not support, or compensate by altering Command, queue, budget, or Outcome semantics.

The core supervisor persists launch intent, owns resource reservations, and establishes stop proof before authorizing a conflicting writer. Worker exit code, self-reported success, PR URL or a model's summary are evidence, not Work completion. Results must be tied to the Invocation and, for review, the reviewed PR head. Replays must not silently start a second worker.

**Adapter selection and interchange.** The Worker adapter is chosen through installation config — the `worker_argv` entry point today; the exact selection key is an implementation detail — never by changing Core. Any implementation conforming to this contract is interchangeable for *new* Invocation requests. Interchange does not transfer a running Invocation to a different worker, and it requires no migration of adapter-internal state.

The Worker contract test suite below is shared: it runs unchanged against every adapter implementation, covering failure behavior (lost start response, duplicate request delivery, unobservable liveness, stop proof, missing artifacts, stale-head evidence) as well as the happy path. A second, independently implemented adapter that replays scripted results is sufficient to prove interchangeability — it does not need to be a real coding agent.

**Reference worker:** a thin Claude Code CLI process adapter that launches one bounded request in an isolated workspace, captures exit/process identity and output artifacts, and exposes cancellation and observation. No built-in planning pipeline, review framework or proprietary Claude Code-specific state in Core. Other worker adapters are valid implementations; their internal orchestration must not supersede core retry or settlement authority.

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
8. Swap two conforming worker implementations: taisan command, queue, budget and outcome semantics remain unchanged.
9. An adapter attempts to request retry or change a Work state directly: rejected; only Commands through Core may do so.
10. The same Invocation request is delivered twice: reconciled by request identity; no duplicate start or duplicate effect.

## Non-goals

No external Kanban migration or synchronization, no general plugin marketplace or dynamic loader, no automatic semantic grouping in the reference selector, and no model-specific orchestration inside the core. Adapter interchange excludes live handoff of a running Invocation to a different Worker, migration of adapter-internal saved state, and any mechanism for switching workers mid-Invocation.
