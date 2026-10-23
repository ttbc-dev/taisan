# Glossary

This document defines the terms used in the workloop design. It describes the intended model, not an implemented API. An ID belongs to the workloop unless the definition says it comes from an external system.

## Units of work

| Term | Meaning and lifetime |
| --- | --- |
| **Candidate** | A proposed change, such as a group selected from GitHub issues. It is not accepted Work and has no execution authority. |
| **Work** | One accepted development objective with intent, constraints, and references to one or more issues. Its `work_id` survives retries. An issue number or board card is not its identity. |
| **Execution** | One logical attempt to complete a Work item under a defined input and budget. It has an `execution_id`. An explicit `retry` creates a new Execution; a `resume` continues the same one. One Work may have several sequential Executions, but at most one may be authoritative and active at a time. |
| **Invocation** | One start of a worker process for an Execution. It has an `invocation_id` and records process identity and evidence. An Execution may have no Invocation yet, or several after interruption or while waiting for CI or review. |

`Attempt` is an ordinary-language synonym for Execution, not a second workloop entity or ID. A worker may use *attempt* for its own internal retries; those do not automatically create a new workloop Execution. Likewise, a process called a *run* is not necessarily a Work or Execution.

```text
Work W1
├─ Execution E1
│  ├─ Invocation I1 → waiting for review
│  └─ Invocation I2 → failure confirmed
└─ Execution E2       → explicit retry
```

Reinvoking a worker after a temporary `pending` result continues E1. Once E1 has a confirmed failure, an accepted `retry` starts E2. A worker's suggestion named `retry` does not decide which case applies: the core must use its stop reason and recorded evidence. `resume` must not silently create E2.

## Requests, facts, and views

| Term | Meaning |
| --- | --- |
| **Command** | A request to change workloop state, such as `submit`, `resume`, `retry`, `cancel`, or `acknowledge`. The core accepts or rejects it using preconditions and an idempotency key. A repeated command with the same key has no second effect. |
| **Query** | A read-only request for recorded information, such as Work status or current Attention. It does not refresh an external service or change workloop state. |
| **Observation** | A fact obtained from a worker, process, or external service, with its source and observation time. It may say that a value is unknown. A PR head SHA, process liveness result, or check status is an Observation. |
| **Projection** | A current view derived from saved state and Observations, such as Work status or an Attention list. It can change when new evidence arrives; it is not an independent execution authority. |
| **Outcome** | An evidence-backed assessment at a Work or Execution decision point. It states what completed, failed, paused, or needs action. A pause or human gate is not Work completion. Later decisions may supersede an earlier assessment without erasing it. |
| **Attention** | A current, evidence-backed action or decision required from a person. It disappears when the underlying need is resolved; acknowledging it only records that someone saw it. Automated waiting and recovery are not human Attention. |
| **Evidence** | Retained records or artifact references supporting an Observation, decision, or Outcome. Evidence includes enough identity and revision information to avoid treating an old PR-head review as current. |

```text
GitHub / process / worker → Observation → saved state and decisions → Projection
                                                                    ↑
                                                                  Query
```

For example, an Observation that a PR head was `abc123` remains in history. If a later Observation reports a different head, the current review Projection must not present a review of `abc123` as current. A Query reads that Projection; a separate monitor or reconciliation step gathers and records new Observations.

## Execution ownership: terminology under study

The workloop needs a way to grant one actor permission to start or continue an Execution and to prevent an old actor from causing a conflicting effect. The exact mechanism and the name **Lease** are not decided.

Existing systems use several different protections:

| Protection | What it establishes |
| --- | --- |
| Time-limited card claim | A card has a temporary assignee. Its expiry alone does not prove that the worker stopped. |
| Process or file lock | Concurrent operations on one host or output directory are serialized while the lock is held. |
| External issue claim | Other hosts or output directories can see that an issue is already in progress; this claim need not have a deadline. |
| Process-liveness check | An Observation about whether the recorded process still exists. It is evidence, not permission. |
| Capacity reservation | A limit on how much work may run, not proof of who owns a particular Execution. |

These protections have different scopes and authorities. Do not call their combination a Lease. Consider that name only after the design specifies one authoritative owner for an Execution, validity or renewal rules, and how a former owner is stopped or prevented from writing before ownership transfers. If termination or write prevention cannot be established, the Execution remains uncertain and conflicting work does not start.

## External identities

GitHub issues and PRs remain external records. Their numbers and head revisions are references and Observations, not substitutes for `work_id` or `execution_id`. CLI, GitHub comments, and optional boards submit Commands or display Projections; none defines a separate Work state.
