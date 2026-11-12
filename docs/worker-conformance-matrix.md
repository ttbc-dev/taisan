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
  whichever worker binary the provider is bound to: `WorkerProvider::make`
  binds the real `workloop-fixture-worker` binary by default, and
  `PosixProvider::make_bound` binds an arbitrary worker binary (the
  U4.6b rerun below binds the contract Worker) — real process spawn,
  real signals, real liveness reads. `ScriptedProvider` wraps
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
| WC-ART-02 | An artifact that cannot count never completes the required set: an empty file carries no digest and is never recorded `Present`, and content whose digest differs from the required one is never recorded `Present` either — recovery refuses to promote a mismatched payload instead of reporting it. | `invalid_artifact_never_counts` (both providers; after each form the scenario asserts no `Present` observation exists, then that bounded recovery exhausts to `Failed`). |

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

## Adapter selection and swap

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WS-20 | A `worker_argv` swap between the reference adapter and the independent contract Worker changes the selection only for requests bound after the swap. Requests and running Invocations bound before the swap keep their recorded adapter ID, version, capability digest, and argv; no running Invocation is migrated. Command admission, queue order, budget and terminal cancel Outcome are identical under either adapter. | `adapter_swap.rs::ws_20_worker_argv_swap_binds_only_new_requests_in_each_direction` (both swap directions, fixture → contract → fixture). |
| WS-21 | A runtime version that differs from the declaration stored on the request's own binding — the bound argv is probed again at launch — is a configuration repair: no worker start is attempted, the retained evidence records `runtime.version_mismatch`, and the request is terminal for its binding (abandon and re-issue under a new request, which binds the upgraded declaration). It is never a silent runtime upgrade and the stored declaration is never updated in place. | `adapter_swap.rs::ws_21_runtime_version_mismatch_is_configuration_repair_with_no_start` — drifted declaration refuses with no start, then a new request binds `fixture-2.0.0` and starts; unit cases `adapter::tests::a_same_adapter_runtime_change_is_a_configuration_repair`, `mismatch_reasons_name_the_differing_field`, `a_failed_describe_is_not_selectable`, and `a_wedged_describe_is_bounded_and_not_selectable`. |

The durable binding is the request's `adapter_binding`: adapter ID,
adapter version, runtime version, capability digest, and the exact argv
selected. The first binding wins (`Effect::BindRequestAdapter` updates
only an unbound request), so a later configuration edit cannot retarget
it — and a refused request is terminal for it: the work is re-issued
under a new request rather than resumed into the same repair.
`describe` is effect-free but runs through the supervisor's
process-control port (own process group, bounded wait, group signalled
on timeout). It is fail-closed: an executable that does not implement
it, or a probe that is refused, bounded, non-zero, unparseable, or
missing the adapter identity, makes the adapter not selectable as
`adapter.declaration_unavailable` — no synthetic declaration is ever
compared or dispatched on. The capability digest sorts the declared
protocol, role, and capability lists before hashing, so set-order
changes do not masquerade as a capability change.

## Shared suite rerun (U4.6b)

`worker_contract.rs::u46b_identical_suite_under_either_adapter_has_per_case_outcome_equality`
executes the identical scenario table twice through the supervisor and
the public Command/Query path — once bound to the reference worker,
once to the independent contract Worker — and retains one
[U4.6 record](worker-protocol.md#u46-evidence-record) per case per
implementation, persisted by the run itself to
`target/u4.6b-records.json`. The records differ only in adapter
identity: every case's Outcome signature — execution states, waiting
reasons, terminal codes, invocation states and roles, writer-identity
presence, intent statuses, and spawn-attempt counts — is equal across
the two runs, as are the paired records' `tested_revision`,
`base_revision`, `evidence_refs`, and full `configuration_identity`
once the bound-worker fields are projected out; the skip partition is
identical (the POSIX model surface is binary-independent). Revision
stamping is fail-closed: the run requires a pristine tracked tree and
both revisions as full commit ids (a CI pull-request merge ref stamps
its source-branch parent), so no record can carry a placeholder or
describe an uncommitted worktree. `u46b_suite_source_has_no_adapter_name_branch`
is the named static check: adapter-name literals may appear only
inside the suite's marked identity boundary and never on a
conditional, comparison, or assertion line, and the
identity-revealing identifiers (the binary getters, the selection
const, the bound-argv construction, the binding fields) appear outside
that boundary only on branch-free data lines — the boundary holds the
only identity comparison, so no case branches on which worker is
bound.
