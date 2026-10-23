# Decisions and open questions

## Working decisions

- Target individuals and small teams with GitHub repositories.
- Deliver a self-hosted, single-instance deployment first.
- Keep Work identity and execution authority in the core; interfaces translate commands and project state.
- Use a CLI as the required interface and GitHub as the first independent adapter.
- Model one Work to many issues and one Work to many sequential execution attempts.
- Make commands idempotent, version-aware, and auditable.
- Keep planning and prioritization human-led in v0.1.
- Treat a board integration as optional, with no special authority over execution.

## Questions to resolve through a spike

1. Which minimal Work states and transition reasons make recovery clear without encoding an interface's card vocabulary?
2. What is the simplest durable lease and fencing mechanism for a single-instance deployment, including process crashes and delayed writes?
3. Should the first GitHub command surface use issue comments, PR comments, or both? How is a multi-issue Work linked without implying one issue equals one job?
4. What review signals are sufficient to declare a specific PR head ready, and what remains a human decision?
5. What evidence must be retained to diagnose failures while keeping engagement data isolated and exports safe?
6. Which executor contract is minimal enough for a second executor without diluting checkpoint and recovery semantics?

Resolve these with a vertical slice and contract tests. Avoid introducing a general-purpose plugin framework before two real interfaces exercise the boundary.
