# Recovery and configuration policy

Status: owner-approved target design from the #50 review. This document changes no runtime behavior or installed configuration. #51 specifies evidence and counters, #53 specifies configuration validation, and #55 specifies the CLI protocol and execution loop.

## Configurable policy and defaults

Every operational limit, interval, retry schedule, classifier, progress rule, and escalation threshold has a documented default and a configuration override. Keep safety invariants separate: configuration cannot authorize duplicate writers, infer success from unavailable evidence, or erase an unresolved effect.

This is a project-wide policy, not a recovery-only exception. Apply it to admission, execution, verification, publication, retention, and operator intervention. Documented configuration may be extensive; coding agents must be able to discover and validate it without relying on hidden constants. The [project principles](principles.md) require this policy for subsequent designs.

The following names are target configuration keys, not implemented options. All counters and elapsed time are durable across process restart and successor Executions of the same Work. Limits compose: reaching any applicable limit prohibits another automatic attempt.

| Target key | Default | Meaning |
| --- | --- | --- |
| `recovery.transport.max_retries` | 3 | Retries after the initial transient-failure attempt |
| `recovery.transport.backoff_seconds` | `[10, 30, 90]` | Waiting schedule, with configurable jitter; respect a valid server retry instruction without exceeding the remaining deadline |
| `recovery.effect_reconcile_seconds` | 600 | Automatic reconciliation window for an unknown host effect; expiry requests human action, not a new write or a failed receipt |
| `recovery.max_fix_invocations` | 3 | Work-wide automatic correction dispatches after initial implementation, including verification and review fixes |
| `recovery.equivalent_failure_limit` | 2 | Consecutive equivalent failed validations after correction before human attention |
| `recovery.no_progress_seconds` | 1800 | Warning and diagnostic threshold; this threshold alone never authorizes a forced stop |
| `recovery.automatic_time_limit_seconds` | 14400 | Cumulative time from first dispatch in automatic execution or automatic waiting; exclude human-decision waiting |

The Work-wide four-hour limit supplements the existing per-Execution Invocation, review, runtime, and artifact-recovery bounds. It does not replace them. Human waiting cannot replenish either budget. A time limit prohibits new attempts and initiates safe stopping; it does not establish termination.

The [installation configuration](installation-config.md#configuration-schema) documents each key's type, unit, default, accepted range, zero and disable semantics, precedence, cross-field constraints, and examples under one TOML schema. Define the jitter policy and how a changed retry count extends or truncates the configured schedule. Reject unknown keys and invalid combinations at configuration load. Do not silently interpret zero as unlimited. Explicit disabling of an operational threshold must not disable ownership, approval, evidence, or deduplication guards. Provide a complete reference usable by coding agents.

## Failure and progress classification

| Evidence | Automatic action |
| --- | --- |
| Transient connection or service failure | Retry within the configured count, backoff, and deadlines |
| Lost response after a write may have occurred | Reconcile the original action and exact authorized payload; do not issue a new action to bypass uncertainty |
| Authentication, permission, or configuration failure | Hold for repair or human action; require changed and revalidated evidence before resume |
| Failed check or actionable review finding | Use the bounded correction loop, not a transport retry |
| Repeated equivalent failure | Accumulate under Work identity across Executions; request human action at the configured limit |

Equivalent failures use stage, stable check identifier, error category, and normalized diagnostics. Ignore volatile timestamps and temporary paths. Normalization rules are configurable; an unclassifiable failure is unknown, not evidence that the cause changed. Preserve raw evidence and the classifier version used. #51 completes the deterministic classification schema and literal unknown cases before implementation.

Progress means accepted new artifacts, a previously failing required check becoming valid and successful, or an unknown action acquiring a verified result. Log volume or a new commit alone is not progress. Progress rules are configurable and versioned; they cannot accept malformed or mismatched evidence. Progress rearms the no-progress warning but never restores consumed time or attempts.

## Request and external-effect ownership

`work` decides and persists an approved `InvocationRequest`. `execution` consumes it, persists the Invocation before launch, and starts the bounded CodingWorker or ReviewWorker. The shared store enforces request uniqueness. Re-reading a request reconciles its existing Invocation instead of launching another writer. A lost start receipt remains process uncertainty until `execution` establishes the recorded writer's state. This is not a requirement for a new remote worker query service.

Repository-host effects use a separate path. `work` saves a `HostActionIntent`; the host adapter applies it and reports facts. Workers only return local changes and evidence. An unknown effect is reconciled against its saved action, actor, target, payload, and postcondition. Ref existence alone or a similar comment is insufficient. The adapter cannot assume the external service implements idempotency by internal action ID. A repeated wake is permission to inspect the existing action, not permission to repeat its write. Any later authorized write must satisfy fresh guards.

## Human waiting and capacity

A lane means an execution capacity slot, not a branch or workspace reservation. Moving responsibility to a human does not itself prove safe release.

Across the lifecycle, minimize lane blocking during human-decision and configuration-repair waits. Prevent new dependent dispatches and publication, request a safe stop of an active worker, and release the slot promptly when stop or write-prevention evidence is established. Persist the continuation before release. If no worker has started, hold the Work without acquiring a slot, or release a prepared slot only after proving its saved request cannot still launch. Keep conflicting-resource reservations independently; do not retain compute capacity solely because a human has not answered.

For example, detecting that a source Issue was closed holds new dispatch and publication for an explicit human decision; it does not automatically cancel Work. Before execution, do not occupy a lane while waiting. During execution, preserve the issued task and WIP, safely stop the worker, and release the lane once writer safety is proven. Do not silently replace the live task with edited Issue content. A later human decision resumes through the queue and fresh guards.

| State at handoff | Capacity and reservations |
| --- | --- |
| Writer stopped or proven unable to write, no unknown host effect | Pause with durable continuation; release the slot, retain reservations needed for WIP and resume |
| Writer live or liveness/stop uncertain | Retain the slot and reservations; prohibit a replacement writer |
| Writer proven stopped, host effect alone unknown | Release the slot; retain the original action and affected-resource reservations; prevent dependent new writes and terminal settlement |

Other eligible Work may use a released slot only when its resources do not conflict. An unresolved host effect is not process uncertainty and does not require a dead worker's compute slot. Human-wait projection retains both the original automatic-wait elapsed time and unresolved effect identity. A later result does not silently restart execution while human approval is required.

After a human decision, the same paused Execution re-enters the queue and reacquires capacity before dispatch. Recheck source, revision, permissions, resource ownership, and unresolved effects. Preserve Work identity, WIP, evidence, and all consumed budgets. No timer, acknowledgment, or configuration change alone releases reservations or constitutes approval.

## Configuration changes for accepted Work

Resolve and save a versioned effective policy with each accepted Work. Configuration edits apply to new Work by default. Existing Work retains its accepted policy, including after restart or same-Work specification reapproval.

An explicit authorized policy-change operation may update existing Work. Record the actor, old and new policy, rationale, Work-version precondition, and decision. Preserve consumed counters and elapsed time. Require explicit approval for a budget expansion. Tightening a limit below consumed usage triggers the limit path; it does not discard evidence or prove stop. An already-issued Invocation retains its immutable input; enforce changed Work-level limits through supervision and safe stopping rather than rewriting the request. The [installation configuration](installation-config.md) defines the schema; #55 defines the Command before exposing this operation.

## Planned verification

Test defaults and every override through configuration loading and effective Work policy. Cover invalid keys, ranges, schedules, disabled thresholds, and policy updates. Fault-inject lost start receipts, lost host-write responses, restart during backoff, and successor Executions; no case may reset budgets or create a duplicate writer/effect. Verify that two equivalent post-correction failures request human action and material progress only rearms its diagnostic timer.

With capacity one, pause W1 after proven worker stop and show independent W2 can run while W1 retains its reservations. Repeat with an unknown writer: W2 cannot take that slot. Repeat with a stopped writer and unknown host effect: independent W2 can run, conflicting Work cannot, and W1 cannot settle or perform a dependent write. Human release requeues W1 and reacquires capacity without resetting policy usage. These are future contract tests, not runtime completion claims.

Repeat the capacity-one scenario for source-Issue closure and configuration-repair waits, both before and after worker launch. Prove that a prepared request cannot launch after its slot is released, and that independent Work proceeds after confirmed stop while conflicting Work remains excluded.
