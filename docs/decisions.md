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
- Keep the v0.1 queue in committed enqueue order. An automatic wait retains its Execution capacity slot; uncertain ownership retains the slot until the old writer is stopped or prevented from writing.

## Questions to resolve through a spike

1. What execution-ownership mechanism prevents conflicting work in a single-instance deployment, including process crashes and delayed writes? Does it meet the requirements for a Lease?
2. Should the first GitHub command surface use issue comments, PR comments, or both? How is a multi-issue Work linked without implying one issue equals one job?
3. What review signals are sufficient to declare a specific PR head ready, and what remains a human decision?
4. What evidence must be retained to diagnose failures while keeping engagement data isolated and exports safe?
5. Which executor contract is minimal enough for a second executor without diluting checkpoint and recovery semantics?

Resolve these with a vertical slice and contract tests. Avoid introducing a general-purpose plugin framework before two real interfaces exercise the boundary.
