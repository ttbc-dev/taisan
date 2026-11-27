# taisan

![taisan hero illustration](assets/taisan-hero.jpg)

taisan is an adapter-based work orchestration loop for coordinating development work across GitHub repositories. A human chooses the work and supplies intent and constraints; taisan coordinates implementation, pull requests, review, bounded fixes, and the points where a human needs to intervene.

**Status:** design plus a working implementation across the v0.1–v0.3 slices. The Rust workspace implements the core contracts, state store, execution supervisor, CLI, and TUI; the production GitHub host adapter runs in-process inside `taisan run` through the `gh` CLI transport; Workers launch through the executable `taisan.worker.request/2` / `taisan.worker.report/2` wire with declaration negotiation, immutable bound requests, filesystem/process/network containment, and reference-based credentials; intake runs the internal scheduler with a configuration-selected Selector. Contract and fixture evidence covers the wire, scheduling, recovery, and fail-closed boundaries; owner-run live acceptance of a real Worker and real host effects is recorded in the [release acceptance record](docs/release-acceptance-v0.3.md) — the manual chain across runs 1–6 plus a scheduled-intake pilot on a clean install — with the release materials under [release notes](docs/release-notes-v0.3.md). The agent-agnostic skill ships at `skills/taisan/SKILL.md` with a [walkthrough](docs/skill-walkthrough.md) covering setup, install, submit, inspect, Attention/decision, and recovery against fixtures. The CLI and CLI-backed agent skills are required for v0.1; the existing TUI is deferred from release requirements. See [provenance](docs/provenance.md) for what is built, what is fixture-only, and where the design came from.

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

Install Rust with support for edition 2024, then build from source. A Linux install additionally needs `bwrap`, `git`, and a C toolchain; the `github` host transport needs the `gh` CLI, and the public reference Worker needs a Node.js runtime for `claude`. The public reference Selector/Worker install the same way per [public reference adapters](docs/reference-adapters.md). The v0.3 Worker path is documented in the [installation and operation guide](docs/v0.3-installation.md); live Worker and host acceptance is owner-run evidence — recorded in [release-acceptance-v0.3.md](docs/release-acceptance-v0.3.md) and the [filled manual-acceptance record](docs/evidence/v0.3-manual-acceptance.md).

```sh
git clone https://github.com/ttbc-dev/taisan.git
cd taisan
cargo build --workspace
cargo run -p workloop-cli --bin taisan -- --help
```

To install the CLI from this checkout:

```sh
cargo install --path crates/workloop-cli --bin taisan
```

Prebuilt binaries ship on GitHub Releases for x86_64 and aarch64
Linux and macOS once a release is published (none exists yet — the
first tag follows the release process in
[docs/distribution.md](docs/distribution.md)). Three binary routes
install the same archives:

```sh
# shell installer -> ~/.cargo/bin
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ttbc-dev/taisan/releases/latest/download/workloop-cli-installer.sh | sh

# Homebrew (owner's personal tap)
brew install shimpeiws/tap/taisan

# cargo-binstall (fetches the prebuilt archive, no compile;
# --bin selects the shipped binary of the multi-bin package)
cargo binstall --git https://github.com/ttbc-dev/taisan workloop-cli --bin taisan
```

Install the agent skill from this checkout with `npx skills add ./skills/taisan` (or from the registry with `npx skills add ttbc-dev/taisan`); the [skill walkthrough](docs/skill-walkthrough.md) documents setup, installation, invocation, and the direct-CLI fallback with captured output. See the [agent interface requirements](docs/agent-interface.md). The existing TUI remains available with `cargo run -p workloop-tui --bin taisan-tui -- --help` and can be installed with `cargo install --path crates/workloop-tui --bin taisan-tui`; it is not required for v0.1.

The Cargo package names retain their existing names for compatibility. See [name migration](docs/name-migration.md) for legacy commands and state paths.

## Design documents

- [Principles](docs/principles.md) — product and operational rules.
- [Agent interface requirements](docs/agent-interface.md) — CLI-backed skills, installation deliverables, and verification gaps.
- [Architecture](docs/architecture.md) — ownership, interfaces, adapters, and consistency.
- [Adapter boundaries](docs/adapter-boundaries.md) — replaceable Work Selector and Worker contracts and reference adapters.
- [Public reference adapters](docs/reference-adapters.md) — the packaged public Selector (`public-issue-selector`) and Worker (`taisan-worker-claude`), their configuration surfaces, limits, and the composed example.
- [Repository host contract](docs/repository-host-contract.md) — common semantics, required capabilities, host identity, and GitHub/GitLab validation scope.
- [Implementation plan](docs/implementation-plan.md) — named phases and v0.1 acceptance gates.
- [Skill walkthrough](docs/skill-walkthrough.md) — clean setup, install, submit, inspect, Attention/decision, and recovery with captured CLI output.
- [Module contracts](docs/module-contracts.md) — typed messages for every module connection and representative flows.
- [Work lifecycle](docs/lifecycle.md) — queue admission, state transitions, retry/resume/cancel, and crash recovery.
- [Glossary](docs/glossary.md) — Work, Execution, Invocation, commands, observations, and views.
- [v0.1 validation](docs/v0.1.md) — the first vertical slice and its acceptance tests.
- [v0.2 installation and operation](docs/v0.2.md) — clean install, one-shot and scheduled intake, configuration-only adapter swap, and the fixture/live evidence boundary.
- [v0.2 owner-run checklist](docs/v0.2-owner-run-checklist.md) — real-Issue acceptance scenarios and expected results.
- [v0.2 evidence template](docs/v0.2-evidence-template.md) — labeled fixture/contract versus owner-run live evidence with redaction.
- [v0.3 installation and operation](docs/v0.3-installation.md) — clean install, the executable Worker wire, pinned delivery, operation with captured non-live output, cutover, drain, and rollback.
- [v0.3 owner-run checklist](docs/v0.3-owner-acceptance-checklist.md) — real-Worker acceptance scenarios and expected results.
- [v0.3 evidence template](docs/v0.3-evidence-template.md) — labeled fixture/contract versus owner-run live evidence for the real-Worker run.
- [Binary distribution](docs/distribution.md) — the cargo-dist release pipeline, shipped packages, binary install routes, and owner-side release prerequisites.
- [Decisions and open questions](docs/decisions.md) — assumptions to test before expanding the design.
- [Provenance](docs/provenance.md) — source-system origins by role, implementation status, and the audit-record pointer.

## v0.1 direction

The first release aims to run one Work item from the built-in queue through a pull request, review, a bounded fix cycle, and a verified outcome. Users must be able to operate it directly through the CLI and through skills in their existing coding agent. Skills call the same CLI contracts; a second independent control interface is not a release gate. TUI delivery and MCP support are deferred. Duplicate commands, stale views, process loss, and adapter outages must not create duplicate executions or false success.

## v0.2 direction

v0.2 adds one configuration-selected Selector adapter, an internal intake scheduler in `taisan run`, durable one-shot `taisan intake`, and intake history Queries. The [installation and operation guide](docs/v0.2.md) covers a clean install, the default 30-minute interval on a 60-second settings tick, the 120-second adapter timeout, the 10-pending-Work limit, and configuration-only adapter replacement without OS timers. The [owner-run checklist](docs/v0.2-owner-run-checklist.md) and [evidence template](docs/v0.2-evidence-template.md) keep automated fixture/contract evidence separate from owner-run live evidence; the implementing documentation task creates no live Issues, enables no normal-installation automation, and launches no real Workers.

## v0.3 direction

v0.3 makes the Worker boundary executable: Workers launch through a versioned wire (`taisan.worker.declaration/1`, `taisan.worker.request/2`, `taisan.worker.report/2`) with immutable per-Invocation requests, declared capabilities, filesystem/process/network containment, and reference-based credentials — never secret values on argv, in requests, or in reports. Reports from Workers stay evidence, not authority: publication, verification, and completion remain Core- and host-adapter-gated. The [installation and operation guide](docs/v0.3-installation.md), [owner-run checklist](docs/v0.3-owner-acceptance-checklist.md), and [evidence template](docs/v0.3-evidence-template.md) prepared real-Worker acceptance — since executed and recorded as owner-run live evidence in the [release acceptance record](docs/release-acceptance-v0.3.md) and the [filled manual-acceptance record](docs/evidence/v0.3-manual-acceptance.md); the implementing documentation itself created no live Issues, installed no credentials, enabled no normal-installation automation, and executed no live models or host effects.

## Proposals

- [Read-only evidence history export for rui](docs/proposal-rui-evidence-export.md). Proposed snapshot, history, and artifact API for an external local search index.
- [rui evidence protocol v1](docs/rui-evidence-v1.md). T1 wire types, generated schema, and synthetic fixtures. The runtime exporter and CLI remain future work.

## License

Except where otherwise noted, this project is licensed under either
the MIT License or the Apache License, Version 2.0, at your option.
See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

The hero illustration at `assets/taisan-hero.jpg` is excluded from
these licenses. See [assets/LICENSE.md](assets/LICENSE.md) for its
usage terms.
