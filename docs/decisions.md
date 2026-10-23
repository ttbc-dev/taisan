# Decisions and open questions

## Working decisions

- Target individuals and small teams with GitHub repositories.
- Deliver a self-hosted, single-instance deployment first.
- Keep Work identity and execution authority in the core; interfaces translate commands and project state.
- Use a CLI as the required interface and GitHub as the first independent adapter.
- Model one Work to many issues and one Work to many sequential Executions; a resumed Execution may have multiple Invocations.
- Make commands idempotent, version-aware, and auditable.
- Keep planning and prioritization human-led in v0.1.
- Implement the development queue in this repository. Its Kanban-style view is derived from the Work/Execution store; there is no separately synchronized card state.
- Do not require Hermes Kanban or migrate its historical cards for the initial personal-use deployment.
- Create E1 with accepted `submit`, create a queued E2 with an accepted `retry`, and requeue the same Execution with `resume`. Dispatch reserves capacity only when starting an Invocation request.
- Keep the v0.1 queue in committed enqueue order, dispatching the oldest eligible item so one unavailable source does not block independent work. An automatic wait retains its Execution capacity slot; uncertain ownership retains the slot until the old writer is stopped or prevented from writing.
- Enforce write-resource exclusion across Work records and supported entry points. Work-level single-Execution rules alone do not protect shared workspaces or branches; unmanaged writers must not write to managed resources.
- v0.1 writer-resource key: a workspace ID plus `(repository, branch)`, both derived from the Work ID (workspace under a managed root, branch fixed as `work/<work_id>`) so a retried Execution resumes the same workspace and branch. The reservation is held while the Execution is nonterminal — including `paused` — and released in the terminal commit after writer-stop evidence and outstanding-effect reconciliation. Releasing a reservation does not delete the workspace.
- Conflicts are rejected at acceptance where the branch is knowable (`retry`, `claim_resources`); dispatch rechecks uniqueness atomically and leaves a conflicting Execution `queued` with a diagnostic. `submit` cannot conflict because its branch derives from a fresh Work ID.
- Manual writers register external claims on the same reservation store. A claim names `(repository, branch)` only, carries a TTL renewable by the claiming actor, and forfeits exclusion on expiry; expiry is not stop evidence. The managed workspace root is execution-only.
- Persist stop intent before signaling and require process-group/child-process stop proof or an equivalent write fence before releasing ownership.
- Stop proof, verified on this host: snapshot write-capable descendants at stop-intent time (orphans reparent and leave the process tree once the leader exits), re-verify each recorded `pid + start time` before signaling so a reused PID is never signaled, and inspect process state rather than presence — an unreaped zombie still lists but cannot write. The supervisor reaps processes it spawned. A `setsid`-escaped child survives a process-group signal, so the snapshot is mandatory, not optional.
- v0.1 specifies no separate write fence: a same-UID writer can undo permission changes, and a path-reopening writer defeats a workspace rename unless replacements never reuse the path. Instead, a replacement simply does not start while a writer is unproven.
- The name Lease is not adopted: ownership splits across the reservation, the TTL claim, and stop proof, and no single entity carries validity, renewal, and fencing together.
- Bound artifact recovery separately from Invocation/runtime budgets and from process-liveness reconciliation. `execution` performs each durable attempt from the workspace, checkpoints, and the worker-runtime artifact store, only after a confirmed worker exit; exhaustion transitions the Execution to `failed` with an `artifact_recovery_exhausted` code and preserved WIP, never to `uncertain`.
- v0.1 defaults per Execution: `max_invocations` 10 across both roles, `max_review_rounds` 5 counting every review Invocation (a head-driven re-review also consumes a round), `runtime_limit_seconds` 6 h from first dispatch, artifact recovery 3 attempts / 30 min, uncertain investigation threshold 15 min.
- Acknowledgement is scoped to an Attention version; Attention resurfaces only when the version changes (new evidence, escalation, or a different required decision), never on a timer.
- A dedicated `capacity` Query explains queue stalls: slot holders, held reservations including external claims, and deferred Executions with their diagnostics.
- Slice 0 (core-truth harness): Rust, SQLite store, real fixture worker processes, a fixture GitHub adapter, CLI only. Contract tests covering the crash-recovery conditions are written before the implementation; real GitHub and the TUI come after.
- v0.1 store model: hybrid — current-state SQLite tables enforce every expressible structural invariant (unique command keys, request/action/observation IDs, one nonterminal Execution per Work and one unresolved writer per Execution via partial unique indexes, a normalized resource-key space, N fixed `capacity_slots` rows, optimistic Work versions), while an append-only `event_log` carries audit and ordering (`EventSequence`). The store never evaluates transition legality, budgets, or gates; `work` does.
- Module delivery is in-process: the store is the durable handoff for approved requests, and `execution` reconciles by scanning on wake and after restart. Wakeups are at-least-once signals, not evidence.
- PR actions re-read the head immediately before writing. A mismatch aborts the write, records `pr_action_result: failed` plus a fresh `pr_head` Observation, and returns judgment to `work`; v0.1 never re-issues an action automatically. `publish_pr` carries no expected head and reconciles by branch or PR existence.
- Command keys and decisions are retained indefinitely in v0.1 — a lost response must always be recoverable. Each adapter's actor identity comes from the installation config and stays stable across restarts.
- Slice 0 layout: a Cargo workspace whose crates follow the ownership boundaries — `workloop-core` (domain, `work`, `projection`, port traits; I/O-free), `workloop-store` (SQLite), `workloop-exec` (supervisor, POSIX `ProcessControlPort`), `workloop-github` (fixture adapter), `workloop-cli` (binary, composition root), `workloop-fixture-worker` (real fixture process). Interface crates share the Command/Query contracts so the later TUI adds no new boundary.

## Questions to resolve through a spike

1. ~~What execution-ownership mechanism prevents conflicting work in a single-instance deployment, including process crashes and delayed writes? Does it meet the requirements for a Lease?~~ Resolved: resource reservations, TTL external claims, and stop proof (verified on a single host); no Lease name, no separate fence. Remaining: prove host-level enforcement in the vertical slice.
2. Should the first GitHub command surface use issue comments, PR comments, or both? How is a multi-issue Work linked without implying one issue equals one job?
3. What review signals are sufficient to declare a specific PR head ready, and what remains a human decision?
4. What evidence must be retained to diagnose failures while keeping engagement data isolated and exports safe?
5. Which executor contract is minimal enough for a second executor without diluting checkpoint and recovery semantics?

Resolve these with a vertical slice and contract tests. Avoid introducing a general-purpose plugin framework before two real interfaces exercise the boundary.
