# taisan

![taisan hero illustration](assets/taisan-hero.jpg)

taisan is an adapter-based work orchestration loop for coordinating development work across GitHub repositories. A human chooses the work and supplies intent and constraints; taisan coordinates implementation, pull requests, review, bounded fixes, and the points where a human needs to intervene.

**Status:** design plus a slice-0 implementation. The Rust workspace implements the core contracts, state store, execution supervisor, CLI, and TUI against fixtures; real GitHub, worker, and selector integrations and agent skills are not yet implemented. The CLI and CLI-backed agent skills are required for v0.1; the existing TUI is deferred from release requirements. See [provenance](docs/provenance.md) for what is built, what is fixture-only, and where the design came from.

## Why build this?

Starting a coding agent is easy. Operating several jobs over time is harder: a worker can disappear while its task still appears active, the same job can start twice, a review can become stale when the PR head changes, and a fix loop can continue without converging. taisan makes these transitions explicit and inspectable.

The initial deployment targets a single operator or small team running one self-hosted instance. Each installation keeps its own credentials, configuration, and state.

## Core model

```text
GitHub issues / human intent -> Work -> Execution(s) -> PR and review -> Outcome
                                |          |
                                +-- one state store --+
                                          |
                              queue / Attention views
                                          |
                              CLI <- agent skills (planned)
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
| GitHub-based intake, PRs, and review signals; a common repository-host adapter contract | Production adapters for other source-control hosts |
| Single-instance self-hosted operation | Hosted multi-tenant service |
| Bounded implementation and review-fix cycles | Fully autonomous product planning |
| CLI and CLI-backed agent skills for submission, inspection, and operator actions | TUI management console, MCP server, and web dashboard |
| Built-in, single-instance development queue and Kanban-style Work view | General-purpose boards, card dependencies, comments, and attachments |
| Optional adapters, including a GitHub interface | Broad workflow-platform integrations |

The system can make routine implementation decisions within the supplied intent and constraints. It must expose consequential uncertainty and a clear intervention path. It does not merge or deploy by default.

## Build and run

Install Rust with support for edition 2024, then build from source. Real integrations are not yet available.

```sh
git clone https://github.com/shimpeiws/taisan.git
cd taisan
cargo build --workspace
cargo run -p workloop-cli --bin taisan -- --help
```

To install the CLI from this checkout:

```sh
cargo install --path crates/workloop-cli --bin taisan
```

Agent skill installation and invocation guidance is a planned v0.1 deliverable, not available in this checkout. See the [agent interface requirements](docs/agent-interface.md). The existing TUI remains available with `cargo run -p workloop-tui --bin taisan-tui -- --help` and can be installed with `cargo install --path crates/workloop-tui --bin taisan-tui`; it is not required for v0.1.

The Cargo package names retain their existing names for compatibility. See [name migration](docs/name-migration.md) for legacy commands and state paths.

## Design documents

- [Principles](docs/principles.md) — product and operational rules.
- [Agent interface requirements](docs/agent-interface.md) — CLI-backed skills, installation deliverables, and verification gaps.
- [Architecture](docs/architecture.md) — ownership, interfaces, adapters, and consistency.
- [Adapter boundaries](docs/adapter-boundaries.md) — replaceable Work Selector and Worker contracts and reference adapters.
- [Repository host contract](docs/repository-host-contract.md) — common semantics, required capabilities, host identity, and GitHub/GitLab validation scope.
- [Implementation plan](docs/implementation-plan.md) — named phases and v0.1 acceptance gates.
- [Module contracts](docs/module-contracts.md) — typed messages for every module connection and representative flows.
- [Work lifecycle](docs/lifecycle.md) — queue admission, state transitions, retry/resume/cancel, and crash recovery.
- [Glossary](docs/glossary.md) — Work, Execution, Invocation, commands, observations, and views.
- [v0.1 validation](docs/v0.1.md) — the first vertical slice and its acceptance tests.
- [Decisions and open questions](docs/decisions.md) — assumptions to test before expanding the design.
- [Provenance](docs/provenance.md) — source-system origins by role, implementation status, and the audit-record pointer.

## v0.1 direction

The first release aims to run one Work item from the built-in queue through a pull request, review, a bounded fix cycle, and a verified outcome. Users must be able to operate it directly through the CLI and through skills in their existing coding agent. Skills call the same CLI contracts; a second independent control interface is not a release gate. TUI delivery and MCP support are deferred. Duplicate commands, stale views, process loss, and adapter outages must not create duplicate executions or false success.

## License

No license has been selected. Choose and add an open source license before distribution.
