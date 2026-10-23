# Autonomous Development Workloop

An early design for a self-hosted development workloop for individuals and small teams using GitHub. A human chooses the work and supplies intent and constraints; the workloop coordinates implementation, pull requests, review, bounded fixes, and the points where a human needs to intervene.

**Status:** design proposal. There is no installable implementation yet.

## Why build this?

Starting a coding agent is easy. Operating several jobs over time is harder: a worker can disappear while its task still appears active, the same job can start twice, a review can become stale when the PR head changes, and a fix loop can continue without converging. The workloop makes these transitions explicit and inspectable.

The initial target is one operator or a small team running a single self-hosted instance across GitHub repositories. It is intended to be portable between independent engagements, with separate credentials, configuration, and state for each installation.

## Core model

```text
GitHub issues / human intent -> Work -> Execution(s) -> PR and review -> Outcome
                                |          |
                                +-- state, ownership, evidence --+
                                      |
                         CLI / TUI / GitHub adapters
```

- **Work** is a durable, meaningful development unit. It may include multiple related issues; an issue is not automatically a job.
- **Execution** is one logical attempt to complete a Work item. An explicit retry creates a new Execution; a resume continues the same one.
- **Invocation** is one worker-process start within an Execution.
- **Outcome** is a verified result or a specific reason to stop and request action.
- **Attention** is a current, evidence-backed need for human intervention, not an accumulated list of old alerts.

The core owns Work identity, state transitions, capacity, retry and resume decisions, evidence, and outcomes. The execution-ownership mechanism is still under study. Interfaces submit commands and project state; they do not become the authority for execution. GitHub remains the source for issues, pull requests, and their revisions. See the [glossary](docs/glossary.md) for the terms and their lifetimes.

## Scope and boundaries

| In the first release | Deferred |
| --- | --- |
| GitHub-based intake, PRs, and review signals | Other source-control hosts |
| Single-instance self-hosted operation | Hosted multi-tenant service |
| Bounded implementation and review-fix cycles | Fully autonomous product planning |
| CLI as the baseline control surface; TUI management console for state inspection and operator actions | A required web dashboard |
| Optional adapters, including a GitHub interface | Broad workflow-platform integrations |

The system can make routine implementation decisions within the supplied intent and constraints. It must expose consequential uncertainty and a clear intervention path. It does not merge or deploy by default.

## Design documents

- [Principles](docs/principles.md) — product and operational rules.
- [Architecture](docs/architecture.md) — ownership, interfaces, adapters, and consistency.
- [Module contracts](docs/module-contracts.md) — typed messages for every module connection and three representative flows.
- [Glossary](docs/glossary.md) — Work, Execution, Invocation, commands, observations, and views.
- [v0.1 validation](docs/v0.1.md) — the first vertical slice and its acceptance tests.
- [Decisions and open questions](docs/decisions.md) — assumptions to test before expanding the design.

## Proposed first milestone

Run one Work item from intake through PR, review, a bounded fix cycle, and completion using only the core and CLI. Add one external control adapter and verify that duplicate commands, stale views, process loss, and adapter outages cannot create duplicate executions or false success. The design is proven only after the same Work can be observed and controlled through two independent interfaces. The TUI management console is an implementation target in this repository after that initial validation slice.

## License

No license has been selected. The repository is private during design; select a license before public distribution.
