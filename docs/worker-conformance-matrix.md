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
  declared, may not be weakened. `PosixProvider` declares
  `spawn_unknown` — a real spawn whose identity capture is lost
  leaves a genuine process-table entry the `ps`/`lsof` signature scan
  re-binds — but does not declare `liveness_unknown`,
  `signal_survived`, or `signal_audit`: a real process cannot fake an
  uninspectable liveness read or a writer scripted to survive its
  signal, and the POSIX port keeps no signal log — so those rows are
  reported skipped for it and covered by `ScriptedProvider`.

## Start semantics

| Row | Required semantics | Scenario / coverage |
| --- | --- | --- |
| WC-START-01 | A second delivery of the same durable request — the delivered-twice window where a consumer already recorded a writer identity for it — reconciles by request identity: the recorded writer is inspected and re-asserted running, one Invocation, no second spawn. | `duplicate_request_delivery_starts_no_second_writer` (both providers; the first leg spawns through the provider's own port so the recorded identity is real). |
| WC-START-02 | A start whose outcome was lost (`SpawnError::Unknown`) leaves the writer unproven until reconciliation — never a silent not-started that permits a replacement, and never a second start. The orphaned writer still carries the recorded launch signature, so the poll path re-binds its identity through the same durable commit a normal launch uses and the Execution returns to `running`; when attribution is unprovable the launch stays uncertain instead of guessing. | `unknown_start_outcome_blocks_a_second_writer` (both providers — `PosixProvider` spawns a real orphan and loses its identity capture, so the re-bind runs through the real `ps`/`lsof` scan). |
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
`target/u4.6b-records.json` when the checkout yields a faithful
revision stamp. The records differ only in adapter
identity: every case's Outcome signature — execution states, waiting
reasons, terminal codes, invocation states and roles, writer-identity
presence, intent statuses, and spawn-attempt counts — is equal across
the two runs, as are the paired records' `tested_revision`,
`base_revision`, `evidence_refs`, and full `configuration_identity`
once the bound-worker fields are projected out; the skip partition is
identical (the POSIX model surface is binary-independent). Revision
stamping is fail-closed: records are retained only when the run
stamps a pristine tracked tree with both revisions as full commit ids
(a CI run stamps the workflow event's real head/base commits — the
depth-1 checkout keeps no base history to resolve — and a checkout
with no resolvable base ref cites the deepest commit it retains), so
no retained record can carry a placeholder or describe an uncommitted
worktree. A checkout that cannot produce that stamp — uncommitted
tracked changes, no git history — still runs the identical suite and
every record-independent assertion, builds no records, and retains
none, so the prescribed `cargo test --workspace` stays runnable
mid-edit. `u46b_suite_source_has_no_adapter_name_branch`
is the named static check: adapter-name literals may appear only
inside the suite's marked identity boundary and never on a
conditional, comparison, or assertion line, and the
identity-revealing identifiers (the binary getters, the selection
const, the bound-argv construction, the binding fields) appear outside
that boundary only on branch-free data lines — the boundary holds the
only identity comparison, so no case branches on which worker is
bound. The scan is lexical: it catches branches that mention an
identity token, while branches through renamed aliases remain outside
what a token scan can see.

## Swap-window recovery (U4.6c)

`swap_recovery.rs` exercises crash, restart and recovery at the
adapter-selection boundary, reusing the U4.6 record format and the
fail-closed revision stamping of the rerun above. Three named
scenarios each retain one [U4.6 record](worker-protocol.md#u46-evidence-record)
to `target/u4.6c-<scenario>.json` when the checkout yields a faithful
revision stamp — a dirty or non-git checkout runs every scenario and
every record-independent assertion, builds no records, and retains
none, so no retained record can cite a commit that does not contain
the exercised slice:

| Scenario | What it proves |
| --- | --- |
| `u46c_restart_after_worker_argv_change_reconciles_in_flight_on_recorded_bindings` | WS-20/WS-03: two Invocations are in flight under the prior selection when `worker_argv` changes and the supervisor restarts over the reopened durable store. Each in-flight Invocation reconciles against its recorded adapter ID, version and capability digest — the recorded binding and writer identity are unchanged, the recorded argv still re-verifies `Ready` against a fresh `describe`, and the changed argv selects a configuration repair (`adapter.identity_mismatch`, `capability.digest_mismatch`) for those stored bindings rather than a retarget. Spawn count proves no second writer; only a request created after the change binds the new adapter. The follow-up safe stop records a durable stop intent covering every recorded identity, inspects each stopped, and lands the terminal cancel with no further spawns. |
| `u46c_interrupted_export_import_recovers_under_changed_config_without_duplicate_artifact_ids` | WS-11: two real fixture writers finish under the prior selection, the configuration changes, and the restarted supervisor walks the [crash-recovery table](worker-protocol.md#crash-recovery) — no export row, truncated bundle, interrupted import with leftover quarantine, `imported` row without a ref, ref present with the row still `imported` — each window recovering to `acknowledged` with exactly one artifact ID (the repository's `refs/taisan/artifacts/` set is asserted after every window and after a final full pass). A second artifact whose workspace was lost before acknowledgement converges on `recovery_exhausted` at exactly the configured bound, never acknowledged and never minting a ref. Recovery under the changed configuration launches no worker. |
| `u46c_recollection_acknowledges_and_retains_git_objects_under_changed_config` | WS-11/WS-01: a real fixture writer launched under the prior selection finishes in a git workspace and the Execution waits mid-Execution when `worker_argv` changes and the supervisor restarts. The restarted supervisor recollects the artifact to `acknowledged`, records the declared verification `Pass` on the exact handed-off commit, and leaves the workspace `.git`, the managed work branch, and the handoff repository's produced closure intact through the verified handoff — while the request binding never moves. |

Three named checks guard the evidence machinery itself:
`u46c_revision_stamp_is_fail_closed` (a stamp is either two full
commit ids over a pristine checkout or no stamp at all — never a
placeholder), `u46c_retained_record_carries_the_closed_field_set`
(the record carries exactly the closed U4.6 field set, the closed
claim class, and every binding field on each adapter-selection entry),
and `u46c_evidence_record_cites_the_tested_revision` (the evidence
record names full tested/base commit ids, transcribes every retained
scenario record under that tested revision, and verifies the cited
commit contains the exercised suite and is on the branch history to
`HEAD`).

## Executable wire boundary (U4.7)

`worker_wire_contract.rs` extends the shared conformance suite to the
executable `taisan.worker.request/2` / `taisan.worker.report/2`
boundary (`docs/v0.3.md` §4): the identical scenario table runs
unchanged against every supplied conforming Worker implementation —
the reference `workloop-fixture-worker` and the independent
`workloop-contract-worker` — through the real dispatch path. Durable
request materialization, `describe` declaration negotiation, the
`run --request <path> --out <dir>` process launch, bounded report
collection and classification, artifact handoff and declared
verification, stop proof and restart recovery, and the config-only
`worker_argv` swap are all exercised by a real spawned process; no
scenario branches on which implementation is bound.

The contract Worker is an independent implementation of the public
wire: it depends on `serde_json` and `sha2` but not on
`workloop-core`, and it builds canonical JSON, the tagged request and
report digests, the declaration envelope, and the report envelope
itself — the suite therefore proves the contract is implementable
without sharing the host's response construction, not merely that a
second binary echoes the fixture's bytes. Rows a real process cannot
express (a lost spawn identity, an uninspectable liveness read) stay
in the provider-model suite above; every other row here is covered
by a spawned process on both implementations — WW-DECL-01 alone binds
`/bin/false` in both runs, because its row proves the supervisor's
refusal on a worker that never answers `describe`, which is identical
for either implementation.

Outcome equality is asserted on an adapter-neutral signature —
invocation states, writer-identity presence, report classifications,
launch/dispatch refusal codes, handoff states, claim verdicts, spawn
counts, and pending depth — never on worker-authored payloads, so the
two runs differ only where the wire permits them to. The bound
adapter identity is compared in exactly one place inside the suite's
marked identity boundary
(`the_identical_wire_suite_passes_under_each_implementation` plus
`wire_suite_source_has_no_adapter_name_branch`, which scans the
source for adapter-name literals and identity-revealing identifiers
outside that boundary); at least one row must bind a real declaration
in both runs for the comparison to count. When the checkout yields a
faithful revision stamp the run retains one record per
implementation to `target/worker-wire-contract.json`; a dirty or
non-git checkout runs the identical suite and assertions but retains
nothing.

One pass also runs through the normal driver:
`workloop-cli/tests/run_driver.rs::
a_v2_worker_runs_the_complete_wire_through_the_driver` submits Work
through the CLI and drives bounded `run` ticks against an installation
whose `worker_dispatch` section composes the v2 transport — the
durable request body verifies, the declaration probe records the
binding, both the Coding writer and the pre-publication
InvariantCheck launch and resolve `Done` with `Success` reports, the
produced commit hands off `Acknowledged` with the declared
verification `Pass` on the exact commit, and the agreeing
`claimed_head` is recorded as evidence. The Execution then waits in
`reconciliation`: no CapabilityReport ever reaches this in-process
driver, so `push_ref` stays unconfirmed and `record_intent` records no
publish intent at all — the capability gate holding publication, which
is the correct terminal state for this configuration.

These are fixture/contract evidence rows only: they prove the wire
contract is implementable and enforced end-to-end inside the
supervisor and the normal driver. They do not claim live-provider
integration, and no row here substitutes for one.
