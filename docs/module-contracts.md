# Module contracts through three flows

This document turns the [module map](architecture.md#module-map) into three example interactions. It is a design proposal, not an implemented API. Names such as `judge_findings` describe operations; their final spelling and transport are open. The terms and lifetimes come from the [glossary](glossary.md).

## Common boundary

| Message | Producer → consumer | Minimum information | Authority |
| --- | --- | --- | --- |
| Command | CLI, TUI, or another control adapter → `work` | Work ID, operation, actor, origin, idempotency key, and expected state or revision | `work` accepts or rejects the transition. The adapter cannot infer acceptance from a timeout. |
| Command result | `work` → control adapter | Command ID, accepted or rejected decision, reason, and current Work version | The saved result answers a repeated request with the same key. |
| Invocation request | `work` → `execution` | Work ID, Execution ID, requested worker role, bounded task, and budget | `work` chooses the task. `execution` chooses how to invoke the worker safely. |
| Observation | `execution` or GitHub integration → `work` | Subject identity, source, observed time, revision when relevant, result, and evidence reference | The source reports a fact. `work` assesses its effect on Work. |
| Projection | Saved state → `projection` → Query caller | Work version, status, current Attention, evidence references, and freshness or unknown markers | A Projection is a view of recorded facts, not a second state store. |

A Query reads saved state. It does not contact GitHub or start a worker. A monitor or a Command handler may obtain a new Observation and record it before a later Query sees the change. The state store keeps the Command decision, Work transition, and any outgoing request together so a lost response cannot make an accepted operation disappear. A repeated idempotency key returns the saved result only when the payload matches; reuse with a different payload is rejected. An external PR write still needs reconciliation because it cannot share that transaction.

## TUI review judgment

The [hermes-setup console](https://github.com/shimpeiws/hermes-setup/pull/386) shows the intended operator interaction: read a Work detail, choose a decision for each finding, preflight, then submit a typed action. This repository will implement its own TUI against the workloop contracts.

1. The TUI queries the Work detail and current Attention. The Projection includes `work_id`, Work version, PR head SHA, finding IDs, evidence references, and any unavailable source. The TUI keeps selection state, but it does not classify Attention.
2. The operator chooses a decision for each finding. The TUI builds a proposed `judge_findings` Command with the displayed Work version and PR head. A read-only preflight shows whether the saved facts allow that Command. Preflight does not reserve the state or guarantee later acceptance.
3. On submit, `work` first checks whether the idempotency key already has a result. It returns that result for a replay. For a new Command, GitHub integration reads the current PR head and records an Observation. `work` compares that head and the Work version with the Command's expected values. If the GitHub read fails, `work` cannot accept a judgment tied to an unverified head.
4. If the head changed, `work` rejects the old-head judgment and returns the current version and reason. The TUI requeries the findings and clears choices tied to the old head. It does not rewrite the expected head and retry the old choices.
5. If accepted, `work` records the decisions against the finding IDs and head SHA. A later `release_judgment` Command may request more work through `execution` when policy and the recorded decisions permit it. The TUI does not create an Execution or Invocation.
6. If the response is lost, the TUI shows the outcome as unknown. It queries the Command result or replays the identical idempotency key and payload. It does not construct a new release request while the first result is unknown.

This flow requires two separate checks: preflight helps the operator understand the action, while the Command handler enforces the preconditions at acceptance time. A head change after acceptance makes that judgment stale for later decisions; it does not erase the recorded choice.

## Explicit retry after a failed Execution

Assume Work `W1` has Execution `E1` with a confirmed terminal failure. A retry is a new attempt under the same Work intent; it is not another Invocation inside `E1`.

1. The CLI or TUI queries `W1`. The Projection shows `E1`'s Outcome, evidence, current Attention, and whether process ownership is still uncertain.
2. The operator submits `retry` with `work_id=W1`, `expected_execution_id=E1`, the displayed Work version, and an idempotency key.
3. `work` checks the failure evidence, retry budget, current Work version, capacity, and execution ownership. Expiry of a claim alone does not prove that an old process has stopped. If it may still write, `work` leaves the state uncertain and does not authorize `E2`.
4. On acceptance, the state store records the Command result, creates `E2`, and reserves its capacity as one decision. `work` then requests the first Invocation through `execution`. The worker result cannot create `E2` on its own.
5. A concurrent retry with another key sees the changed Work version or active `E2` and receives a rejection. Replaying the accepted key returns the recorded `E2` result without creating `E3`.

`resume` has a different contract: it continues an eligible interrupted Execution and may create another Invocation with the same Execution ID. It never turns a confirmed failed `E1` into `E2` implicitly.

## Worker result and PR review

1. `execution` records an Invocation before starting a CodingWorker or ReviewWorker role. Its request names the Execution, input revision, task, and budget. Devin, Claude Code, or another provider can fill the role.
2. The worker returns artifacts and evidence to `execution`. `execution` records the Invocation result and reports an Observation to `work`. A worker saying "done" is evidence to assess, not a Work Outcome.
3. GitHub integration records separate Observations for the PR head, checks, and review events. Each review conclusion is bound to the head SHA it examined. `work` assesses the worker and GitHub Observations together, then records an Outcome or requests another Invocation within the same Execution.
4. A later PR-head Observation makes a prior head's review conclusion stale. `work` cannot mark the new head reviewed from the old result. `projection` shows the current head and the missing review evidence in the next Query.
5. When `work` requests a PR action, it records the outgoing intent before GitHub integration performs the write. Reconciliation records the actual GitHub result as an Observation. An accepted internal decision alone does not prove that the PR changed.

## Decisions needed before a directory layout

- Define the minimal Work and Execution states, transition reasons, and Outcome codes used by these flows.
- Define how a PR action checks its expected head and how reconciliation handles a head change between judgment acceptance and the external write.
- Define the execution-ownership mechanism that prevents an old process from writing after ownership changes. The [Lease name remains undecided](glossary.md#execution-ownership-terminology-under-study).
- Define the scope and retention of idempotency keys and Command results, including repeated requests after a process restart.

Once these contracts are settled, directories can follow the ownership boundaries: `work`, `execution`, `projection`, shared persistence, and the CLI, TUI, GitHub, and worker adapters. The layout does not decide the contracts.
