# Principles

1. **Human intent is the starting point.** A person chooses the objective, priority, important constraints, and boundaries. Automated selection may propose work, but does not silently redefine intent.
2. **Work follows a meaningful change.** Group related issues when one coherent implementation reduces conflict or preserves an invariant. Avoid a mandatory one-issue/one-PR mapping.
3. **The core owns execution truth.** The built-in queue and Kanban-style view read the same Work/Execution state. An external issue status is only a projection. A visible `running` label is not proof of a live process; a missing label is not proof that work stopped.
4. **Every action has evidence.** Record the command, actor, Work ID, Execution ID, PR head, decision, and resulting state. Distinguish known, disproved, and unknown facts.
5. **Repeatable commands are safe.** Commands carry an idempotency key and preconditions. At-least-once webhook delivery or simultaneous operators must not launch duplicate Executions.
6. **Execution ownership is explicit.** Recovery uses persisted Execution records and process evidence. An expired claim alone does not prove that the old worker stopped. The ownership mechanism and whether to call it a Lease remain open.
7. **Reviews are revision-specific.** A review conclusion applies to a particular PR head. New commits trigger reevaluation; a previous approval or clean review is not silently inherited.
8. **Fix loops have a budget and a stop reason.** A bounded cycle may complete, retry with evidence, or request human action. Repeated activity alone is not progress.
9. **Routine ambiguity is handled locally.** Agents should resolve small, reversible implementation choices and report them with the PR. Missing intent, permissions, security boundaries, and consequential tradeoffs have explicit human gates.
10. **Interfaces are replaceable; semantics are stable.** `retry`, `resume`, `cancel`, and `acknowledge` retain the same meaning whether issued directly from the CLI, by a CLI-backed skill, or by an optional adapter. v0.1 requires the CLI and agent skills; the existing TUI is deferred and MCP is optional future work. An adapter translates input and output; it cannot bypass core preconditions.
11. **Failure stays visible.** Unknown status, missing evidence, failed notifications, and unreconciled external projections do not become success. A temporary adapter outage cannot stop the built-in queue or erase a command result.
12. **Portable by default.** A self-hosted installation has its own configuration, credentials, state, and repository allowlist. No personal paths, accounts, or notification services are required.
13. **Operational policy is configurable.** Across the system, provide defaults and configuration overrides for operational limits, timing, retry and escalation policies, and configurable classifiers. Document every option sufficiently for a coding agent to select and validate it, including types, units, defaults, ranges, disable semantics, precedence, interactions, and examples. Complexity is a documentation responsibility, not a reason to hard-code policy. Configuration cannot bypass identity, approval, evidence, or writer-safety invariants. Accepted Work retains its versioned policy until an explicit authorized update. See [recovery and configuration policy](recovery-policy.md).
14. **Human waiting does not occupy a safe-to-release lane.** Minimize blocked execution capacity throughout the lifecycle. Before handing off for a human decision or configuration repair, stop active workers safely and release the capacity slot as soon as writer safety is proven. Retain only the resource reservations and evidence needed to prevent conflicts and resume safely. An unknown host effect alone does not hold compute capacity after worker stop. A live or unproven writer retains its slot; a timer cannot substitute for stop proof. Resume requeues the Execution and reacquires capacity. See [human waiting and capacity](recovery-policy.md#human-waiting-and-capacity).

## Intended workflow

```text
select and shape Work -> accept -> claim -> implement -> verify -> PR
                                                -> review -> fix -> reassess
                                                -> complete or request attention
```

Human review and merge remain possible at any point. The workloop reports what it knows and what still requires a decision.
