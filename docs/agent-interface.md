# CLI-backed agent interface requirements

## Status and release scope

This document specifies the v0.1 interface deliverables agreed in issue #47. It does not ship a skill, add a CLI option, change core behavior, or establish live E2E evidence. The CLI exists and emits JSON against fixtures. Agent skills, their installation guidance, and the complete agent-facing CLI verification are future delivery work planned through #36.

v0.1 requires direct CLI operation and skills usable from the user's existing coding agent. TUI delivery is deferred without removing its crate, binaries, or tests. MCP is optional future work. A second independent control interface, GitHub comment command adapter, or Projects projection is not a release gate. Real GitHub integration for publication and observations remains required.

## Workflow and authority

The skill collects intent, issue references, constraints, and verification requirements, invokes `taisan`, and explains the recorded result. It uses the configured installation, actor, state store, and workspace root. It does not maintain its own queue or authoritative state, launch a managed worker outside the core, or invent retry and recovery policy. The core and supervisor own durable execution independently of the conversation; closing an agent session is not cancellation or proof that a worker stopped.

The skill exposes these operator workflows through the CLI:

| Operator goal | Existing CLI commands to reuse | Required interpretation |
| --- | --- | --- |
| Request work | `submit` | Preserve intent, constraints, verification input, and replay identity. Accepted means durably queued, not completed or necessarily started. |
| Inspect progress and evidence | `work-list`, `work-detail`, `attention`, `capacity` | Show canonical Work/Execution identity, current recorded state, evidence, unavailable sources, and blockers. Agent inference is not recorded evidence. |
| Intervene | `retry`, `resume`, `cancel`, `acknowledge` | Use observed versions and eligible target identities. Retry creates a new Execution after confirmed failure; resume continues a paused one; cancel acceptance is not stop proof; acknowledgement applies only to the current Attention version. |
| Judge review findings | `judge-findings`, `release-judgment` | Preserve head and finding identities; require current finding-set coverage and core acceptance, not conversational approval alone. The #24 coverage gap is unresolved. |
| Recover a lost command response | `command-result`, or identical command replay | Reuse the original identity and payload. An unknown result never justifies a fresh key or duplicate effect. |

These command names exist today; the table defines target workflow coverage, not a claim that the skill or every required safeguard is implemented. Direct CLI operation remains available for diagnosis.

## CLI result and failure requirements

The CLI must provide structured machine-readable results that preserve the core Command and Query contracts. A command result identifies accepted or rejected status, command identity, canonical Work/version and Execution identity where applicable, and rejection reason/evidence. Queries preserve source availability and distinguish a recorded failure, unknown state, not-found result, and inability to read the store.

Exit status, diagnostics, and structured output must have documented, consistent meanings. An agent must be able to distinguish a domain rejection from a transport, parsing, configuration, or store error without scraping prose. Diagnostics must not corrupt machine-readable output. A zero process exit alone is never evidence that a Command was accepted or Work completed. The exact output/exit-status contract is specified in the [execution loop](execution-loop.md#cli-contract); verification of the implementation against it remains release work.

Before mutation, the caller retains the installation/actor identity, idempotency key, payload, and observed preconditions needed to recover a lost response. The core still enforces preconditions at acceptance. The skill must not silently change a rejected request's expected version, head, finding decisions, or key to make it pass. A failed or unavailable query is surfaced with the missing evidence; it is not translated into a successful or empty workload.

## Installation and invocation deliverables

The eventual release must include runnable instructions for installing the CLI from the checkout, configuring one isolated installation, installing and discovering the provided skill in a supported coding agent, and invoking it with a sample issue, intent, constraints, and verification command. The instructions must name the supported agent and actual skill location/invocation once implemented. This scope decision makes no unsupported portability claim and provides no placeholder installation command for a nonexistent skill.

The walkthrough must explain which installation and actor the skill uses, how credentials are supplied through installation configuration, how the execution loop runs independently of the conversation, and how to inspect or stop work directly through the CLI. It must cover a rejected action and recovery after a lost response. TUI and MCP installation must not be prerequisites.

## Acceptance and evidence

These are planned release checks. No skill run or live GitHub E2E is verified by #47. Record each later result with revision, installation/config identity, invoked interface, command identity, expected result, observed structured output, and relevant persisted evidence. Redact credentials. An agent's prose summary alone is insufficient.

| Check | Required evidence |
| --- | --- |
| Clean setup and invocation | A supported agent discovers and invokes the shipped skill using documented setup, without TUI or MCP; the CLI can also be invoked directly against the same installation. |
| Submit | Direct CLI and skill paths each submit Work in isolated runs. Accepted results correlate with exactly one queued Execution per request; acceptance is not reported as Work completion. |
| Inspect | Work list/detail, evidence, Attention, and capacity agree with recorded state; unavailable sources and queue blockers remain visible. |
| Eligible interventions | Retry, resume, cancel, and acknowledge pass their documented eligibility checks through direct CLI and skill paths. Correlate Execution identities, stop proof, and Attention versions; include ineligible-action rejection. |
| Review judgment safety | A stale head and findings added on the same head between inspection and acceptance cannot silently authorize an incomplete judgment. Verify via the shared Command handler/CLI; initial TUI draft tests do not satisfy this check. Track the unimplemented coverage guard under #24/#36. |
| Duplicate and concurrent commands | Identical replay returns the original decision; a changed payload conflicts; concurrent direct CLI and skill actions cannot create duplicate Executions or writers. |
| Lost response | Query by command identity or replay identity, or replay the identical request, recovers the original decision. If recovery is unavailable, preserve unknown status and do not create a fresh request. |
| Explicit failure | Domain rejection, invalid input/configuration, store failure, and unavailable evidence produce distinguishable documented results; none is reported as accepted or completed. |
| Conversation ends | Recorded work and execution ownership remain inspectable through the CLI after the invoking agent session ends. No duplicate launch, inferred cancellation, or loss of execution authority results from closing the conversation. |

TUI fixture tests remain in the workspace and continue to run in normal repository verification. Their presence does not require a TUI walkthrough for release. Shared invariants demonstrated there, including stale-head rejection, incomplete judgments, and unknown-result recovery, must remain covered at the core/CLI boundary when validating agent skills.
