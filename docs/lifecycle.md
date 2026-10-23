# Work lifecycle and built-in queue

This document fixes the v0.1 lifecycle semantics behind the [module contracts](module-contracts.md). It is a design contract, not an implemented API. The initial installation has one host and one state store. Work, Execution, and Invocation have different lifetimes; the queue and TUI columns are views of those records, not extra card states.

## Caller behavior

```text
submit(intent, issues, key=A)       → Work W1, Execution E1 queued
query(W1)                           → queued; E1 has no Invocation
dispatch(E1)                        → reserve capacity, persist request R1
start(R1)                           → Invocation I1 under E1
observe(I1, recoverable stop)       → E1 paused; capacity released
resume(W1, execution=E1, key=B)     → E1 queued again, budget intact
dispatch(E1), start(R2)             → Invocation I2 under E1
observe(I2, confirmed failure)      → E1 failed; capacity released
retry(W1, expected=E1, key=C)       → Execution E2 queued under W1
```

An accepted `submit`, `retry`, or `resume` records the requested work once. It does not promise immediate worker start. Capacity shortage leaves the Execution queued; the operator does not have to resubmit it. The CLI and TUI read the same Work list and use the same Commands.

## Authoritative records

The notation is illustrative and does not select a programming language or database schema.

```ts
type RequestId = string & { readonly tag: "invocation_request" };
type ImmutableIntent = { goal: string; issues: GitHubIssueRef[];
  constraints: string[]; verification_argv: string[][] };
type ExecutionInput = { intent_digest: string; selected_base_revision?: string };
type ExecutionBudget = { deadline?: string; max_invocations?: number;
  max_review_rounds?: number; runtime_limit_seconds?: number;
  first_admitted_at?: string };
type ProcessIdentity = { host_id: string; pid: number;
  started_at: string; process_group_id: number; invocation_id: InvocationId };
type ContinuationRef = { evidence: EvidenceRef[]; checkpoint?: EvidenceRef };
type ExternalWait = { kind: "checks" | "review" | "github_action" | "artifact_recovery"; ref: string };
type PauseReason = { kind: "human_judgment" | "recoverable_stop" | "repair";
  evidence: EvidenceRef[] };
type VerifiedCompletion = { pr: GitHubPrRef; head: PrHeadSha;
  checks: EvidenceRef[]; review: EvidenceRef[]; writer_stop: EvidenceRef[] };
type ConfirmedFailure = { code: string; evidence: EvidenceRef[] };

type WriteResource =
  | { kind: "workspace"; workspace_id: string }
  | { kind: "branch"; repository: string; branch: string };
type ResourceReservation = {
  resources: WriteResource[];
  holder: { kind: "execution"; execution_id: ExecutionId }
        | { kind: "external"; actor_id: string };
  expires_at?: string;   // external claims only; an execution row ends with its Execution
};

type Work = {
  id: WorkId;
  version: WorkVersion;
  intent: ImmutableIntent;
  current_execution: ExecutionId;
};

type Execution = {
  id: ExecutionId;
  work_id: WorkId;
  ordinal: number;
  input: ExecutionInput;
  budget: ExecutionBudget;
  state: ExecutionState;
};

type ExecutionState =
  | { kind: "queued"; enqueued_at: EventSequence }
  | { kind: "ready"; request_id: RequestId }
  | { kind: "running"; invocation_id: InvocationId; stage: "coding" | "review" | "fix" }
  | { kind: "waiting"; reason: ExternalWait; head?: PrHeadSha }
  | { kind: "paused"; reason: PauseReason; continuation: ContinuationRef }
  | { kind: "uncertain"; request_id: RequestId; reason: string; desired: "continue" | "cancel" | "fail" }
  | { kind: "stopping"; request_id: RequestId; desired: "cancel" | "fail" }
  | { kind: "succeeded"; outcome: VerifiedCompletion }
  | { kind: "failed"; outcome: ConfirmedFailure }
  | { kind: "cancelled"; stop_evidence: EvidenceRef[] };

type Invocation = {
  id: InvocationId;
  request_id: RequestId;
  execution_id: ExecutionId;
  role: "coding" | "review";
  state: "prepared" | "running" | "unknown" | "exited" | "not_started";
  process_identity?: ProcessIdentity;
  evidence: EvidenceRef[];
};
```

`Work` has no separately updated phase. Its current phase, Outcome, and Attention are derived from its current Execution and saved Observations. E1 is created with W1 in the accepted `submit` transaction, before capacity is reserved. A retry creates E2 in the accepted `retry` transaction. Resume requeues the same Execution; it never creates another Execution or resets that Execution's budget.

`queued` means accepted but without a capacity reservation or approved Invocation request. `ready`, `running`, `waiting`, `uncertain`, and `stopping` hold one Execution capacity slot. `paused` and the three terminal states hold none. v0.1 keeps the slot during automatic CI or review waiting, even if the last process has exited. This avoids a second capacity acquisition path between stages. A later version can change that policy without adding a card state.

`paused` requires durable continuation input and proof that earlier writers have stopped or cannot write. A pause for `repair` records the failed precondition, the required repair, and evidence that the condition was revalidated before `resume`; repeatedly resuming unchanged configuration is not an eligible transition. `uncertain` is evidence of an unresolved start or process identity, not a synonym for failure. It retains capacity until reconciliation establishes a safe transition. A worker report claiming completion is evidence; only `work` can record `succeeded` or `failed` after checking it.

## Queue admission

The pending queue consists of current Executions in `queued`, ordered by their committed `enqueued_at` sequence and then Execution ID. A duplicate Command replay keeps the original sequence. Dispatch scans in that order for the oldest currently eligible Execution; a blocked or unavailable source does not prevent later independent work from being considered. It records a retryable diagnostic for the blocked item and does not silently change its input or target. v0.1 has no manual ordering, priority lanes, or separate queue table.

The internal dispatch loop reads a queued Execution and its observed Work version. Selection grants no permission. `work` rechecks the current state, input revision, human gates, ownership, and capacity. One state-store transaction moves `queued → ready`, reserves one slot, and saves the first `InvocationRequest`. The request ID is derived from the Execution ID and its next durable Invocation sequence; the store enforces uniqueness. If the Work version, capacity, or request ID conflicts, the loop rereads and decides again. `execution` consumes only an approved request and records an Invocation before asking a worker to start.

After coding, `work` may request review or fix within the same Execution. When that Execution already holds a slot, `ready` for the next Invocation saves a new request without reserving a second slot. An Invocation is never started merely because a TUI row says `running`.

The input revision and PR head used for an Invocation are checked when its request is saved. A judgment accepted while E waited in the queue is not applied to a newer head. A changed head that needs a new human judgment moves E to `paused` without reserving capacity. An unavailable source leaves E `queued` with a retryable diagnostic; it never silently substitutes a different review target.

Eligibility includes writer-resource exclusion across Work records and external actors. v0.1 declares each Execution's write resources as a workspace record plus a `(repository, branch)` pair. Both keys derive from the Work ID — the workspace under a managed root and the branch fixed as `work/<work_id>` — so `retry` can pre-check them and a retried Execution can resume the same workspace and branch. The same `queued → ready` transaction reserves the capacity slot, writes the resource rows, and saves the first Invocation request. The reservation is held while the Execution is nonterminal — including `paused`, `uncertain`, and `stopping`, so a pause never exposes its WIP — and is released in the terminal commit, after writer-stop evidence and outstanding-effect reconciliation. Releasing the reservation does not delete the workspace; retained files remain evidence.

A Work-level rule prevents two active Executions under one Work, but it is not sufficient to protect shared branches or workspaces. `submit` cannot conflict because its branch derives from a fresh Work ID. `retry` checks the `work/<work_id>` reservation at acceptance and is rejected with `resource_conflict` when another holder owns it. Dispatch performs the same uniqueness check atomically; a conflict appearing between acceptance and admission leaves the Execution `queued` with a retryable diagnostic. Issue overlap is a separate policy: shared Issue references alone do not imply a shared branch, while a shared branch does imply a writer conflict.

Every supported writer entry point for a managed resource uses this ownership path. A manual or unmanaged writer registers an external claim on the same reservation store before writing to a managed branch; the claim carries only `(repository, branch)` because the managed workspace root is reserved for `execution`. An external claim is bounded by a TTL and renewable by the claiming actor. A `queued` Execution holds no reservation, so a claim that arrives before its admission wins; dispatch then defers the Execution until the claim is released or expires. Claim expiry forfeits the exclusion guarantee but is not evidence that the writer stopped: a still-active writer is detected through later Observations such as an unexpected head change and handled by normal reconciliation. A claim conflicting with a held reservation is rejected with the current holder; the claiming actor releases its claim, and an operator may force-release with the override recorded. The v0.1 deployment does not claim protection from arbitrary external processes that bypass this contract.

## Transition rules

| Current state | Trigger and guard | Next state and effect |
| --- | --- | --- |
| No Work | Valid `submit` | Create W and E1 `queued`, with Command result in one commit. |
| `queued` | Capacity and all gates permit internal dispatch | `ready`; reserve one slot, claim the Execution's write resources (a resumed Execution already holds them), and save an Invocation request. |
| `queued` | Capacity unavailable | Stay `queued`; no request, reservation, or retry count. |
| `queued` | Current head invalidates a required human judgment | `paused`; record the stale head and needed decision, without reserving capacity. |
| `queued` | `cancel` wins before dispatch | `cancelled`; no worker may start. |
| `queued` or `paused` | Absolute Work deadline or Execution budget exhausted; no writer/effect outstanding | `failed`; record the exhausted limit without reserving capacity and release any held write-resource reservation. |
| `paused` | `cancel`; no writer/effect/request can still act | `cancelled` directly, with no slot to release; release the write-resource reservation. |
| `ready` | Correlated worker launch and process evidence | `running` with the same request and Invocation. |
| `ready` | Launch definitely refused before any process exists | `paused` with a repair action, or `failed` if the failure is terminal; release slot; retain the reservation if paused, release it if failed. |
| `ready` or `running` | Launch or liveness cannot be established | `uncertain`; retain slot and prohibit replacement start. |
| `uncertain` | Correlated old process confirmed alive | `running`, or `stopping` if stop was requested; keep the same request and slot. |
| `uncertain` | Old process confirmed stopped and no effect remains unknown | `paused`, `failed`, or `cancelled` according to the saved cause and desired action; release slot, and release the write-resource reservation unless pausing. |
| `running` | Worker exits; external check/action is pending | `waiting`; keep the Execution slot. |
| `running` | Worker exits; required artifact/evidence is missing but a durable source may recover it | `waiting` for `artifact_recovery`; retain ownership until recovery is complete or bounded recovery is exhausted. |
| `waiting` | Fresh evidence permits the next coding/review/fix step | `ready` with a new request under the same Execution and reservation. |
| `waiting` for `artifact_recovery` | Bounded recovery attempts or deadline exhausted | `failed` with an `artifact_recovery_exhausted` code; record the bound, attempts, and preserved evidence; release slot and the write-resource reservation. |
| `running` or `waiting` | Human decision or recoverable interruption; writers safe | `paused`; record continuation, release slot, and retain the write-resource reservation. |
| `paused` | Valid `resume` or `release_judgment` | Same Execution `queued` with a new enqueue sequence and unchanged budget. |
| `running` or `waiting` | Failure confirmed and writers safe | `failed`; record reason, release slot, and release the write-resource reservation. |
| `ready`, `running`, or `waiting` | Runtime budget or deadline exhausted; writer/request/effect may still act | `stopping` with desired `fail`, or `uncertain` with desired `fail`; retain slot until safe. |
| `failed` | Valid `retry` naming that Execution | Create next Execution `queued` under the same Work; E1 remains historical. |
| `ready`, `running`, or `waiting` | `cancel`; launch/stop or effect still possible | `stopping` with desired `cancel`, or `uncertain` with desired `cancel`; retain slot. |
| `uncertain` with desired `continue` | `cancel` | Stay `uncertain` with desired `cancel`; retain slot and request stop of the correlated Invocation. |
| `uncertain` or `stopping` with desired `fail` | `cancel` while failure stop is already in progress | Accept an idempotent no-op; retain the failure cause and continue stopping. |
| `waiting` | `cancel`; no writer, launch, or external effect can still act | `cancelled` directly; release slot and the write-resource reservation. |
| `stopping` | Liveness becomes unknown | `uncertain` with the same desired action; retain slot. |
| `stopping` or `uncertain` | Old writer stopped or writes prevented; external effects reconciled | `cancelled` if desired `cancel`, or `failed` if desired `fail`; release slot and the write-resource reservation. |
| Active state | Required PR-head evidence verified, no unresolved writer/effect/gate | `succeeded`; record the verified head, release slot, and release the write-resource reservation. |

The table names allowed transitions, not all policy checks. A Command also checks actor, Work version, idempotency key, target Execution, and budget. `retry` requires a confirmed `failed` current Execution; `resume` requires `paused`. Neither is accepted from `uncertain`, `stopping`, `succeeded`, or `cancelled`. Budget exhaustion is an internal timed decision that checks the stored limit against the observed clock in the commit; it is not a reason to reject `cancel`. `cancel` acceptance may mean that stopping is requested; it is not a claim that the process or external action has stopped. A second cancellation with a different key against an already cancelled Work is an accepted no-op with no new version.

Execution budget counts every durably recorded Invocation attempt, including a definite `not_started` result, and review/fix rounds. v0.1 defaults per Execution: `max_invocations` = 10 across both roles, `max_review_rounds` = 5 where each review Invocation consumes one round — including a re-review driven by a head change — and `runtime_limit_seconds` = 6 hours. The first dispatch starts its runtime budget; queue time does not consume that budget. Resume retains the original counters and deadline. Retry creates a new Execution with the policy's next budget; it cannot change the accepted Work intent. An absolute Work deadline, if configured, is separate and continues while queued. Artifact recovery has its own bounded attempt/deadline policy — 3 attempts within 30 minutes by default — and does not count as a new Invocation. `execution` performs each attempt, collecting from the workspace, recorded checkpoints, and the worker runtime's artifact store, and durably records the attempt count, deadline, and result before reporting an Observation. Recovery runs only after a confirmed worker exit; it does not start while the writer may still be alive. If the bound is exhausted the Execution transitions to `failed` with an `artifact_recovery_exhausted` code: required evidence could not be verified, so success cannot be claimed. Available workspace/WIP evidence is preserved, and the failure is eligible for a recorded repair or retry decision — a retried Execution can claim the same workspace and branch. Missing artifacts alone do not prove that a writer is still alive or that it stopped.

## Completion, review, and Attention

For v0.1, completion means that the change has been published as one PR, the required verification and review evidence covers its current head, and no writer, human gate, or outgoing GitHub action remains unresolved. Merge is a separate human or future policy action. `succeeded` stores the PR and verified head. A later head change does not rewrite that historical Outcome or automatically reopen the Execution. The current PR-readiness Projection must show that the newer head has no valid review; follow-up work requires a new Work item.

Attention is derived from a human decision that can be made now: a paused judgment, a confirmed failure with an eligible retry or repair decision, or an uncertain condition that has exceeded its investigation threshold — 15 minutes of continuously unresolved uncertainty by default. The Projection exposes the threshold and the next responsible actor/action. Acknowledgement records that the operator saw the current Attention version; it does not release a gate or hide a newly actionable condition, and Attention resurfaces only when its version changes — new evidence, an escalation, or a different required decision. Queue/capacity wait, automatic CI wait, and ordinary reconciliation are not Attention. Findings and checks are always tied to a PR head. A delayed webhook for an older head remains evidence for that head and cannot move the current head backward.

## Crash and concurrency rules

1. Command identity, payload digest, result, Work version, and E creation/requeue are saved together. The same key and payload replay the original decision; a changed payload under that key is rejected.
2. Capacity and write-resource reservations change with the corresponding state transition in the same transaction. A caller cannot supply an arbitrary capacity delta. At most one nonterminal Execution exists per Work, including `queued` and `paused`; at most one Invocation may be an unresolved writer for an Execution. Write-resource exclusion spans Work records, external claims, and all supported entry points; each declared workspace ID and `(repository, branch)` pair has at most one holder. The store enforces the structural part of this rule as constraints inside the same transaction — uniqueness, partial unique indexes, and fixed slot rows — while `work` evaluates transition legality (see [State store model](module-contracts.md#state-store-model)).
3. A crash after `ready` commits leaves an approved request to replay. A crash after Invocation preparation but near process spawn requires `execution` to use its process-control port to inspect the stable Invocation identity. Merely finding no response does not prove that no process started. An uncertain start blocks another writer.
4. A stop/cancel/fail intent is durably recorded before sending a signal. If persisting that intent fails, the supervisor must not signal and then report a durable stopping state. Start and stop are serialized for the request. If a cancelled request could still launch, the Execution remains `stopping` or `uncertain` until its process is ruled out or stopped. The recorded intent includes a snapshot of the writer's known write-capable descendants; once the leader exits, an orphaned child is reparented and can no longer be found through the process tree, so the snapshot cannot be reconstructed after the fact.
5. Heartbeat expiry, a missing PID alone, and a worker's own `done` claim do not release capacity. Stop proof requires inspecting every recorded identity — the process group plus the snapshotted descendants — and reading process state, not mere presence: an unreaped zombie still lists in the process table but cannot write, and a process that escaped the process group survives a group signal. The supervisor signals each recorded identity only after re-verifying its recorded process start time, so a reused PID is never signaled, and reaps the processes it spawned so inspection is not polluted by zombies. Unknown inspection is not proof of stopped. The old process must be confirmed stopped before a replacement Invocation or Execution may write; v0.1 specifies no separate write fence, since a replacement simply does not start while a writer is unproven.
6. An unknown GitHub write remains an outstanding effect. Cancellation does not erase it or undo a published PR. Reconcile the action ID with GitHub before recording a terminal cancellation or completion that depends on it.
7. Artifact recovery and writer liveness are separate facts. A missing export cannot cause unbounded collection retries, and successful artifact retrieval cannot by itself prove the writer stopped. Recovery attempts and deadlines are durable and bounded.

## Deliberately deferred

Host-level enforcement of workspace and branch claims must still be proven by the vertical slice. Also deferred are multiple hosts, multiple concurrent Executors for one Work, arbitrary Kanban cards, automatic priority changes, history import from Hermes, and completion across multiple PRs. None is required to define the v0.1 lifecycle above.
