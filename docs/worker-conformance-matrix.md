# Worker conformance matrix

The U4.1 contract matrix for Worker providers. Every row states the
provider-neutral required semantics from `docs/adapter-boundaries.md`
(the Worker boundary) and `docs/worker-protocol.md`, and names the
scenario that proves it. The shared suite
(`crates/workloop-exec/tests/worker_contract.rs`) runs each scenario
unchanged against every provider whose declared model surface covers
the row's requirements; a provider that cannot model a row is skipped
for that row, never substituted. Coverage claims are contract-level
modeled behavior — they do not claim production-runtime support.

## Parameterization

The suite parameterizes over two axes:

- **Provider implementation.** `PosixProvider` wraps
  `Supervisor<CountingPosix>` — an observational decorator that
  delegates spawn, inspect, and signal to `PosixProcessControl` and
  records every spawn call and spawned identity — which in turn spawns
  the real `workloop-fixture-worker` binary: real process spawn, real
  signals, real liveness reads. `ScriptedProvider` wraps
  `Supervisor<ScriptedProcessControl>`, an independently implemented
  in-memory process table that can model start/liveness outcomes no
  argv can produce (a spawn whose identity capture was lost, an
  uninspectable process). Both sit behind the same `ProcessControlPort`
  and drive the same `Supervisor` passes; scenario assertions bind the
  durable store — invocations, executions, and host-action intents —
  plus facts observable at the port boundary (spawn count, delivered
  signals, recorded writer liveness), never provider internals beyond
  what the port itself exposes.
- **Declared model surface.** Each provider lists the outcomes it can
  express (`spawn_unknown`, `liveness_unknown`, `stop_signal`, …) and
  each scenario lists the surface it requires. A scenario runs only
  when the declared surface covers all of them, matching the rule that
  a required capability may be absent from a provider but, once
  declared, may not be weakened. `PosixProvider` does not declare
  `spawn_unknown`, `liveness_unknown`, or `signal_audit` — a real
  process cannot fake a lost identity capture or an uninspectable
  liveness read, and the POSIX port keeps no signal log — so those
  rows are reported skipped for it and covered by `ScriptedProvider`.

## Start semantics

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WC-START-01 | A second delivery of the same durable request — the delivered-twice window where a consumer already recorded a writer identity for it — reconciles by request identity: the recorded writer is inspected and re-asserted running, one Invocation, no second spawn. | `duplicate_request_delivery_starts_no_second_writer` (both providers; the first leg spawns through the provider's own port so the recorded identity is real). |
| WC-START-02 | A start whose outcome was lost (`SpawnError::Unknown`) leaves the writer uncertain — never a silent not-started that permits a replacement. The Invocation records no process identity and no second start is attempted. | `unknown_start_outcome_blocks_a_second_writer` (`ScriptedProvider`; skipped on `PosixProvider` — `spawn_unknown` not declared). |
| WC-START-03 | A definitely-refused start (`SpawnError::Refused`) pauses the Execution and produces no writer; the spawn counter observes the refused attempt exactly once and no second attempt follows. Refusal is distinct from uncertainty. | `refused_start_pauses_without_a_writer` (both providers). |

## Liveness and stop

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WC-LIVE-01 | Uninspectable liveness is not death: the recorded writer keeps ownership, no replacement Invocation or spawn is attempted, and recovered liveness re-asserts the same running writer. | `unknown_liveness_is_not_death` (`ScriptedProvider`; skipped on `PosixProvider` — `liveness_unknown` not declared). |
| WC-STOP-01 | A cancel request is not terminal: while the recorded writer is provably alive the Execution sits in `stopping`, and it reaches `cancelled` only after every recorded writer is inspected stopped. | `cancel_waits_for_stop_proof` (both providers); descendant-snapshot coverage in the supervisor suite (`setsid_escapee_is_in_the_snapshot_and_signaled`, `setsid_escapee_survives_the_group_signal_but_blocks_the_terminal_commit`). |
| WC-STOP-02 | A delivered signal is not stop proof: a writer that survived its signal keeps the Execution in `stopping`, a subsequent unverifiable liveness read still cannot settle, and the Execution reaches `cancelled` only after a signal the writer does not survive is inspected stopped. | `a_delivered_signal_is_not_stop_proof` (`ScriptedProvider`; skipped on `PosixProvider` — `signal_survived`/`liveness_unknown`/`signal_audit` not declared). |

## Artifacts

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WC-ART-01 | A confirmed exit without a required artifact runs bounded recovery and then fails `artifact_recovery_exhausted` — never success. | `missing_artifact_runs_bounded_recovery_then_fails` (both providers); `missing_artifact_runs_bounded_recovery_then_fails`, `recovery_artifact_in_the_workspace_root_is_not_truncated` (supervisor suite). |
| WC-ART-02 | An artifact that cannot count — an empty file (no digest → absent, never `Present`) or content whose digest differs from the required one (`Present` but unsatisfying) — never completes the required set. | `invalid_artifact_never_counts` (both providers). |

## Producer identity and effect authorization

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WC-REV-01 | Evidence is bound to its producer Invocation: an artifact recorded under a superseded Invocation's identity cannot satisfy the current Invocation's required set, and no effect is authorized by it. | `stale_revision_evidence_cannot_satisfy_current` (both providers — provider-neutral, store-level row). |
| WC-DENY-01 | With `host_write` denied, a clean worker run produces local evidence and a recorded intent only. Draining the outbox at the authorized application boundary applies nothing and reports no fabricated failure — the intent stays `pending`. | `denied_write_produces_no_effect` (both providers; drains through `FixtureGithub::apply_outbox`, the same boundary the host suite uses). |

## What this slice does not claim

- Provider passes are modeled contract evidence only. A second
  production Worker runtime, migration of a running Invocation, and a
  `resume` capability claim are all out of scope (v0.1 is
  `resume: unsupported`, per `docs/worker-protocol.md`).
- `ScriptedProvider` proves the suite is parameterized over providers,
  not that a second production runtime exists; the POSIX supervisor
  plus the reference worker remains the approved runtime.
- Workspace and git objects are retained until the verified handoff —
  the suite never asserts workspace cleanup, matching the protocol's
  retention rule.
- Skipped rows are reported per provider by the runner, not silently
  absorbed inside a scenario.
