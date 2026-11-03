# Autonomous Development Workloop

An open source control plane for coordinating development work across GitHub repositories. A human chooses the work and supplies intent and constraints; the workloop coordinates implementation, pull requests, review, bounded fixes, and the points where a human needs to intervene.

**Status:** design proposal. There is no installable implementation yet.

## Why build this?

Starting a coding agent is easy. Operating several jobs over time is harder: a worker can disappear while its task still appears active, the same job can start twice, a review can become stale when the PR head changes, and a fix loop can continue without converging. The workloop makes these transitions explicit and inspectable.

The initial deployment targets a single operator or small team running one self-hosted instance. Each installation keeps its own credentials, configuration, and state.

## Core model

```text
GitHub issues / human intent -> Work -> Execution(s) -> PR and review -> Outcome
                                |          |
                                +-- one state store --+
                                          |
                              queue / Attention views
                                          |
                              CLI / TUI / GitHub adapters
```

- **Work** is a durable, meaningful development unit. It may include multiple related issues; an issue is not automatically a job.
- **Execution** is one logical attempt to complete a Work item. An explicit retry creates a new Execution; a resume continues the same one.
- **Invocation** is one worker-process start within an Execution.
- **Outcome** is a verified result or a specific reason to stop and request action.
- **Attention** is a current, evidence-backed need for human intervention, not an accumulated list of old alerts.

The core owns Work identity, state transitions, capacity, retry and resume decisions, evidence, and outcomes. Execution ownership combines store-enforced resource reservations, TTL-bounded external claims for manual writers, and stop proof over recorded process identities. Interfaces submit commands and project state; they do not become the authority for execution. GitHub remains the source for issues, pull requests, and their revisions. See the [glossary](docs/glossary.md) for the terms and their lifetimes.

The repository implements its own development queue. Pending Work, execution requests, and capacity reservations live in the same state store as Work and Execution; the Kanban-style list is a Projection, not a second card database.

## Scope and boundaries

| In the first release | Deferred |
| --- | --- |
| GitHub-based intake, PRs, and review signals | Other source-control hosts |
| Single-instance self-hosted operation | Hosted multi-tenant service |
| Bounded implementation and review-fix cycles | Fully autonomous product planning |
| CLI as the baseline control surface; TUI management console for state inspection and operator actions | A required web dashboard |
| Built-in, single-instance development queue and Kanban-style Work view | General-purpose boards, card dependencies, comments, and attachments |
| Optional adapters, including a GitHub interface | Broad workflow-platform integrations |

The system can make routine implementation decisions within the supplied intent and constraints. It must expose consequential uncertainty and a clear intervention path. It does not merge or deploy by default.

## Design documents

- [Principles](docs/principles.md) — product and operational rules.
- [Architecture](docs/architecture.md) — ownership, interfaces, adapters, and consistency.
- [Module contracts](docs/module-contracts.md) — typed messages for every module connection and representative flows.
- [Work lifecycle](docs/lifecycle.md) — queue admission, state transitions, retry/resume/cancel, and crash recovery.
- [Glossary](docs/glossary.md) — Work, Execution, Invocation, commands, observations, and views.
- [v0.1 validation](docs/v0.1.md) — the first vertical slice and its acceptance tests.
- [Decisions and open questions](docs/decisions.md) — assumptions to test before expanding the design.

## v0.1 direction

The first release aims to run one Work item from the built-in queue through a pull request, review, a bounded fix cycle, and a verified outcome. A second control interface should preserve the same command semantics. Duplicate commands, stale views, process loss, and adapter outages must not create duplicate executions or false success.

## License

No license has been selected. Choose and add an open source license before distribution.
