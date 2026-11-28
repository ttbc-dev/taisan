# Executor transport contract (v0.4)

Status: design contract for #374. This document defines a provider-neutral
protocol between one coordinator and one configured executor endpoint. It does
not select the reference transport. Restricted SSH with bounded framed messages
remains a candidate until the reference environment decision in #373 is
complete. Therefore, the #374 criterion to select one reference transport
remains open. The ownership boundary and accepted execution modes are in
[remote executor boundary](remote-executor.md).

## 1. Contract boundary

The coordinator allocates identities, authorizes each operation, and records
the result. The executor accepts only operations bound to a previously
authorized installation, request, and Invocation. The executor runs one
existing Worker request and returns process facts and collected bytes. It does
not choose work, change budgets, retry an Invocation, publish repository-host
effects, or decide an Outcome.

The executor transport moves immutable, content-addressed payloads. A Worker
request, report, or declaration may be one payload, but the executor envelope
does not parse or redefine those formats. Worker protocol negotiation and
validation stay at the Worker adapter boundary. Executor transport versions
are negotiated independently. A transport fault never becomes a Worker report
status.

Core types remain provider-neutral. They contain no SSH key, VM, container,
cloud, transport-library, or OS process type. The transport adapter parses
wire messages, checks their bounds and digests, and maps them to domain results
before Core records those results. Endpoint-specific process identifiers stay
inside the executor. Core stores an opaque process-evidence reference and
digest with the normalized `running`, `exited`, or `unknown` fact.

## 2. Identities and immutable binding

The coordinator authenticates the configured endpoint principal before
handshake. The hello response supplies the current incarnation. The
coordinator then persists the endpoint binding, including that incarnation,
before sending `prepare` or any other Invocation operation. Every command
carries the request identity and bound incarnation. Every receipt echoes
those fields and adds the incarnation that produced the receipt.

```text
installation_id
endpoint_id
endpoint_identity_digest
bound_incarnation_id
request_id
invocation_id
request_digest
operation_id
operation_kind
payload_digest
```

`endpoint_id` names the configured endpoint. `endpoint_identity_digest`
identifies the authenticated endpoint principal, such as the pinned host
identity. The transport adapter computes it as SHA-256 of the canonical
authenticated-principal bytes with the
`taisan.executor.endpoint-identity.v1\0` domain tag. The principal bytes stay
inside the adapter. The authenticated identity must match the configured
binding before the endpoint accepts a command.

`bound_incarnation_id` is the executor incarnation to which the Invocation
was bound. It is a fresh, unpredictable identifier created at executor
process start and does not survive a restart. The endpoint reports its current
incarnation during handshake. Each receipt also records
`responder_incarnation_id`, which identifies the incarnation that produced
the receipt. Reconciliation may query an older bound incarnation, but the
endpoint cannot use a new incarnation's process facts as proof about a
previous process. A changed incarnation makes any unproven process `unknown`;
it does not prove that the process stopped.

The first binding for a request records the endpoint identity, executor
incarnation, selected protocol version, opaque task-payload manifest digest,
capability set digest, launch configuration digest, request digest, and
transfer limits. The task payload manifest lists each item's stable ID,
content type, size, digest, and exact source revision when applicable. Its
format is owned by the executor contract. Payload bodies remain opaque to the
executor protocol. That binding is immutable. Configuration changes apply
only to requests that have not yet been bound. A running or uncertain
Invocation cannot move to another endpoint, incarnation, payload, or launch
configuration.

The immutable launch configuration contains an installation-configured
launch-profile ID and digest, an opaque Worker binding digest, the
read-only or writable workspace grant, sandbox-profile digest, mount grants,
runtime-credential delivery reference, resource limits, data bounds, and
deadline. The local Worker adapter configuration holds the Worker argv,
declaration, and runtime identity. Their digest binds them without adding
runtime-specific types to Core or the executor transport. The configuration
excludes secret values. The executor rejects a launch whose configuration
digest differs from the coordinator's binding.

The coordinator computes `launch_config_digest` as SHA-256 over the canonical
configuration with the domain tag `taisan.executor.launch-config.v1\0`.
Changing any field creates a different binding. A remote retry is a new
request, not an edited launch configuration.

## 3. Handshake and capability negotiation

Before preparing work, the coordinator authenticates the configured endpoint
identity and sends a bounded `taisan.executor.hello/1` request. The executor
returns its incarnation, supported executor protocol versions, capability
declarations, and a digest over the canonical capability declaration.

The coordinator selects the highest mutually supported protocol version
allowed by the installation's configured range. It binds that version and the
capability digest to the request before dispatch. Unknown required capabilities,
an unsupported version, a changed identity, or an unavailable declaration
refuses dispatch. The coordinator does not infer capabilities from an endpoint
name or silently drop a requested capability.

Capabilities describe observable contract support, such as bounded object
transfer, process control, read-only workspace enforcement, and supported
resource limits. A capability declaration is not evidence that enforcement
works. The reference environment and contract fixtures must verify the
declared behavior.

## 4. Wire envelope and replay rules

Each command uses canonical UTF-8 JSON. Canonicalization sorts object keys by
UTF-8 byte order, emits no insignificant whitespace, rejects duplicate keys
and floating-point values, and encodes integers in base 10 without leading
zeroes. Strings escape only quotation marks, backslashes, and control
characters. Control characters use lowercase `\u00xx` escapes. Other Unicode
code points are emitted as UTF-8 without normalization. Integers are limited
to signed 64-bit values. SHA-256 digests use these domain tags:
`taisan.executor.command.v1\0`
for a command, `taisan.executor.receipt.v1\0` for a receipt,
`taisan.executor.capabilities.v1\0` for capabilities,
`taisan.executor.payload.v1\0` for a command payload, and
`taisan.executor.result.v1\0` for an operation result. Each envelope digest
covers the canonical envelope with its own digest field omitted.
`payload_digest` and `result_digest` cover only their canonical payload or
result values with their respective tags. Binary payloads travel as bounded
data frames referenced by digest. The transport frames each
message with a fixed-width unsigned 32-bit big-endian length before the body.
The receiver checks the length before allocating or parsing the body. Unknown
envelope fields and unsupported major versions are refused.

The hard wire limits are 64 KiB for handshake messages, 1 MiB for commands
and receipts, 1 MiB for `handoff.json`, and 4 MiB for one data frame. Each
Invocation also carries finite `max_input_bytes`, `max_output_bytes`,
`max_item_count`, and operation-timeout values. The coordinator selects those
values from the installation's versioned limits before binding the request.
The executor streams payloads and must not buffer the full transfer in memory.
A receiver rejects a frame or transfer before exceeding any hard or
Invocation limit. Retries use the same operation deadline and do not reset it.

```json
{
  "format_version": "taisan.executor.command/1",
  "installation_id": "install-a",
  "endpoint_id": "executor-a",
  "endpoint_identity_digest": "<64 lowercase hex>",
  "bound_incarnation_id": "boot-7f1c",
  "request_id": "e-7:inv:2",
  "invocation_id": "inv-41",
  "request_digest": "<64 lowercase hex>",
  "operation_id": "op-4f2a",
  "operation_kind": "start",
  "payload_digest": "<64 lowercase hex>",
  "command_digest": "<64 lowercase hex>",
  "payload": {
    "prepared_id": "prep-41",
    "launch_config_digest": "<64 lowercase hex>"
  }
}
```

The executor returns a `taisan.executor.receipt/1` envelope with the same
request identity, `bound_incarnation_id`, and `responder_incarnation_id`, plus
the `command_digest`, operation result, `result_digest`, and `receipt_digest`.
It adds a bounded diagnostic code when needed. A receipt is evidence of that
operation only. A `started` receipt does not prove that the process is still live, and a
`stop_requested` receipt does not prove that it stopped.

The coordinator allocates and commits each mutating `operation_id` and full
`command_digest` before sending. The executor keys the durable operation
record by installation, endpoint principal, bound incarnation, request,
Invocation, and operation ID. It stores the full canonical command digest
before performing the effect. A replay returns the stored receipt only when
the full command digest matches. Reusing an operation ID with a different
command digest returns `conflicting_replay` and performs no effect.

The executor also keeps one start slot keyed by installation, request, and
Invocation. A second start ID for that Invocation returns the recorded start
state or a conflict. It never creates another process. Start and stop intents
serialize on that slot. Once stop intent is durable, a delayed start is
refused. The coordinator retains operation IDs and receipts for as long as the
Invocation or an unresolved recovery decision can refer to them. The executor
retains operation receipts and acknowledgement tombstones as durable metadata
for the lifetime of their installation records. It retains WIP and body data
until durable collection acknowledgement and the executor retention policy
permit cleanup.

Read-only observation requests also carry durable operation IDs. Replaying an
observation ID returns its recorded observation. A later observation uses a
new operation ID. An observation of a prior bound incarnation reports both
the bound and responder incarnation IDs. If the endpoint cannot inspect the
prior process, the result is `unknown`. This preserves an auditable sequence
without treating transport retries as new observations.

## 5. Operations

All six operations carry the identity tuple and a bounded payload. The result
vocabulary is closed. Free-form endpoint text is limited to retained
diagnostics and never controls state transitions.

The coordinator-facing port translates to domain results and keeps the wire
envelopes private:

```rust
trait ExecutorPort {
    fn prepare(&mut self, request: PrepareRequest) -> PrepareResult;
    fn start(&mut self, request: StartRequest) -> StartResult;
    fn observe(&mut self, request: ObserveRequest) -> Observation;
    fn request_stop(&mut self, request: StopRequest) -> StopResult;
    fn collect(&mut self, request: CollectRequest) -> CollectionResult;
    fn acknowledge(&mut self, request: AcknowledgeRequest) -> AckResult;
}

enum StartResult {
    Started { process: ProcessEvidenceRef, receipt: ReceiptId },
    KnownNotStarted { reason: RefusalCode, receipt: ReceiptId },
    Unknown { reason: UnknownReason, receipt: Option<ReceiptId> },
}

enum Observation {
    Running { process: ProcessEvidenceRef, sequence: u64 },
    Exited { process: ProcessEvidenceRef, sequence: u64 },
    Stopped { proof: StopProofRef, sequence: u64 },
    Unknown { reason: UnknownReason, sequence: Option<u64> },
}
```

These signatures sketch the boundary. `OperationId` and `PayloadDigest` are
durable fields on each request. The port returns typed domain facts; it does
not expose JSON values or transport errors to Core.

| Operation | Required behavior | Main results |
| --- | --- | --- |
| `prepare` | Validate the request, immutable bodies, object closure, launch configuration, limits, and required capabilities. Stage inputs atomically before any Worker process exists. | `prepared`, `pending`, `refused`, `unknown` |
| `start` | Start at most one process tree for the bound request. Persist start intent and process identity before returning `started`. | `started`, `known_not_started`, `unknown`, `refused` |
| `observe` | Return recorded process facts, latest bounded progress sequence, and receipt identities for the same Invocation. It may query a prior bound incarnation after reconnect or reboot. | `running`, `exited`, `stopped`, `unknown`, `unavailable` |
| `request_stop` | Persist stop intent, snapshot descendants, signal the recorded process tree, apply the bound grace period and escalation, then inspect process state. | `stop_requested`, `stopped`, `unknown`, `unavailable` |
| `collect` | After stop proof, transfer the bounded report and log. For producing roles, also transfer the handoff manifest and required object closure into coordinator quarantine. Bind every item to the request and Invocation. | `collected`, `pending`, `incomplete`, `invalid`, `unknown`, `unavailable` |
| `acknowledge` | Confirm that the coordinator durably stored the validated collection. Release executor retention only for the matching collection digest and Invocation. | `acknowledged`, `conflict`, `refused`, `unknown` |

`known_not_started` is valid only when durable executor state proves no process
ever started for that request. Spawn refusal before process creation qualifies.
Lost responses, receipt-store failure after a possible spawn, identity capture
failure after spawn, and endpoint reboot before process inspection return
`unknown`. The coordinator must not start a replacement writer while start or
stop remains unknown.

`observe` reports `unknown` when the executor cannot inspect the recorded
process identity. A closed transport connection, elapsed heartbeat, expired
lease, or quiet log does not prove exit. `request_stop` may return `stopped`
only with a receipt bound to the stop operation, process identity, descendant
snapshot digest, and bound incarnation, after inspection proves those
processes are gone. If inspection fails or the endpoint incarnation changed,
the result is `unknown`. A later responder incarnation may replay a durable
stop proof or completed collection receipt from the bound incarnation. It may
not create a new start or infer process exit from reboot.

`exited` means the leader process exited. It does not prove that descendants
have stopped and does not authorize collection. After natural exit, the
executor inspects the recorded descendant set. It returns `stopped` with a
stop-proof receipt only when that set is gone. If `request_stop` first returns
`stop_requested`, the coordinator later issues a new `observe` operation to
obtain the final stop proof. Replaying the stop operation returns its original
receipt and does not change its result. `collect` names the stop-proof receipt
that authorizes output collection.

Cancellation remains a Core decision. Core records cancellation before it
sends `request_stop`. If no start was authorized, Core refuses later starts
under that request and records `known_not_started` only after the executor
confirms no start occurred. If a start may have occurred, Core keeps the
reservation until stop proof and effect reconciliation satisfy the existing
terminal guards. Cancellation never deletes remote WIP or clears an unknown
process state.

## 6. Source and artifact custody

The coordinator acquires source material through its trusted repository and
installation adapters. It transfers only immutable request bodies and
content-addressed objects to the configured executor. The executor never
fetches a repository host, reads the coordinator filesystem, or resolves a
filesystem path from the wire. A local path is not an artifact identity.

Each transfer declares a stable `transfer_id`, total bytes, item count, chunk
size, and per-item digests before data frames arrive. Each chunk carries a
`collection_id`, `item_id`, offset, byte count, and chunk digest. A chunk
replay with the same transfer, collection, item, offset, and digest is
idempotent. A different digest at an occupied offset is
`conflicting_replay`. The receiver enforces the request's
immutable limits before accepting each chunk. It rejects traversal paths,
symlinks, duplicate item identities, missing chunks, over-limit bodies, and
digest mismatches. It stages partial transfers separately and exposes them to
the Worker only after the full request digest and object closure validate. A
`pending` receipt returns the transfer ID and next expected offset. Replaying
the same operation resumes that transfer. The final `prepared` or `collected`
receipt is immutable and replays byte-for-byte.

For a producing Invocation, the Worker writes `handoff.json` beside
`report.json` in a fresh out directory assigned to that Invocation. The
manifest is outside the workspace and produced commit. It binds
`request_id`, `invocation_id`, produced commit, and bounded descriptors for
the request's declared workspace outputs. The coordinator verifies the
manifest and objects in quarantine. The descriptor set must cover every
`required_artifacts` entry. An omitted required output is `Missing`. The
coordinator derives expected artifact digests from files in the exact verified
produced commit. A Worker digest is a claim. A mismatch against the commit or
a request-pinned `expected_digest` is `Corrupt`. The coordinator rejects a
foreign or stale identity, malformed or oversized manifest, unsafe path,
missing object, or invalid closure. `required_artifacts` and
`artifact_digest(workspace, path)` remain the evidence path for workspace
outputs. The handoff manifest is not itself a required artifact. The
coordinator derives the canonical Git object manifest separately from imported
objects. See [handoff manifest custody](handoff-manifest-custody.md).

`handoff.json` uses `taisan.remote.handoff/1` and contains exactly
`format_version`, `request_id`, `invocation_id`, `repository`, `base_commit`,
`produced_commit`, and `artifacts`. `artifacts` is sorted by UTF-8 path bytes.
Each entry contains exactly `path`, `bytes`, and `claimed_sha256`. Unknown
fields are refused.
The manifest digest is SHA-256 over canonical JSON with the
`taisan.remote.handoff.v1\0` domain tag. The manifest is capped at 1 MiB and
the entry count is capped by the Invocation's `max_item_count`. A path is a
UTF-8 slash-separated relative path. The coordinator rejects an empty path,
an absolute path, a backslash, NUL, an empty component, `.` or `..` component,
a duplicate path, a symlink, or a path outside the exact required-artifact
set. The descriptor's `bytes` must equal the size in the produced commit.
The coordinator computes the content digest from that commit and compares it
with the Worker claim and any request-pinned expected digest.

```json
{
  "format_version": "taisan.remote.handoff/1",
  "request_id": "e-7:inv:2",
  "invocation_id": "inv-41",
  "repository": {"host": "host-1", "repository": "repo-a"},
  "base_commit": "<40 lowercase hex>",
  "produced_commit": "<40 lowercase hex>",
  "artifacts": [
    {
      "path": "dist/result.json",
      "bytes": 128,
      "claimed_sha256": "<64 lowercase hex>"
    }
  ]
}
```

The collection digest covers sorted entries with `item_id`, byte count,
content digest, and source object identity. It uses the
`taisan.executor.collection.v1\0` domain tag. It covers the report, log,
`handoff.json`, object closure, and any supervisor-issued verification
evidence required by the request. It excludes transport frame boundaries and
temporary staging paths.

Remote out-directory contents have an explicit whitelist. `report.json`,
`runtime.log`, and `checkpoint.json` keep their v0.3 meanings. Remote
`handoff.json` is an additional transfer-metadata member. It is not a Worker
report, required workspace artifact, or evidence of durable collection. The
local v0.3 out-directory whitelist remains unchanged. Retention treats
`handoff.json` as part of the remote Invocation out-directory unit.

Remote verification evidence is a separate supervisor-authorized collection
item, never a Worker claim. It binds `verification_id`, `artifact_id`, the
exact repository and produced commit, the verification command-set digest,
environment digest, result digest, and bounded per-command outputs and logs.
Each command result records its argv digest, exit or signal result, elapsed
time, output byte count, and output digest. Output and log byte counts obey
the immutable Verification limits. The coordinator accepts `Passed` only for
the exact collected artifact and exact command and environment bindings. A
Worker report's claimed checks remain claims and cannot substitute for this
evidence.

For producing roles, the coordinator commits the validated artifact record
and verified handoff ref before it sends `acknowledge`. For report-only roles,
the coordinator durably stores the validated report and required logs before
it acknowledges the collection. The executor retains the workspace, out
directory, staged bodies, and receipts until an `acknowledge` operation
matches the coordinator's durable collection ID and digest. A lost
acknowledgement response does not require another collection. The executor
keeps a small acknowledgement tombstone and replays the same receipt after
body cleanup. If the endpoint restarts, it accepts a first acknowledgement
for the old bound incarnation only when the same authenticated principal has
a durable complete collection and matching stop proof. It returns a receipt
that names both the old bound and new responder incarnation. Operation
identity and acknowledgement tombstones are durable metadata and are not
reclaimed while they may be replayed. A different
collection digest for one Invocation is a conflict. Cleanup after
acknowledgement is scoped, bounded, restartable, and subject to retention
policy. Invalid or incomplete collection never becomes `Present` evidence.

## 7. Credential and enforcement boundary

The coordinator authenticates the endpoint using its configured endpoint
credential reference. The endpoint authenticates the coordinator against its
configured principal. Neither credential enters the Worker environment.

The coordinator resolves the Worker's runtime credential through the existing
credential provider and delivers it through a one-Invocation encrypted secret
channel bound to the authenticated endpoint and immutable launch
configuration. The executor stages it in a mode-0600 file and exposes only
that selected runtime credential to the Worker through the existing
read-only runtime-auth mount. The executor retains the file only while the
Invocation may run and removes it after stop proof. Secret values never appear
in argv, request bodies, operation receipts, reports, or logs. Endpoint
administration credentials, SSH agent sockets, keyrings, and host credentials
are unavailable inside the Worker.

The executor receives no coordinator store handle and no repository-host
read/write or GitHub mutation credential. Repository-host reads and writes
remain with the coordinator's host adapter. The endpoint cannot publish a
branch, PR, review, comment, thread action, or ready transition.

The executor applies the v0.3 mount, PID, network, and credential controls,
including the mandatory CONNECT proxy that allows only the configured
model-API hosts. No remote profile may weaken those controls. Review and
invariant-check workspaces are enforced read-only at the executor filesystem
boundary. A Worker declaration or request flag alone does not prove that a
workspace is read-only. Preflight refuses dispatch if the selected
containment profile cannot enforce the role's grant or model-API egress rule.
Coding and Fix may write only within their assigned workspace and bounded out
directory.

## 8. Deadlines, resource limits, and replacement fencing

The coordinator binds an absolute deadline and a resource-limit set before
dispatch. The set includes maximum total input bytes, maximum total output
bytes, maximum item count, maximum chunk size, maximum report and log bytes,
process/runtime limits, and the stop grace period. Each field is explicit in
the launch configuration digest. A missing, zero, negative, or unsupported
required limit refuses preparation. Installation defaults must be finite and
versioned; later config edits do not change a bound request.

The executor command carries the absolute deadline, coordinator send time, and
remaining runtime measured against the coordinator's monotonic clock. These
are executor transport fields. They do not change the Worker request body.
The handshake records estimated clock offset, uncertainty, round-trip time,
measurement time, and the configured maximum drift rate. The measurement is
valid only for the configured handshake freshness interval. If preparation
takes longer, the coordinator repeats the measurement before `start`. The
executor applies the conservative offset edge, subtracts uncertainty, bounds
clock drift through the authorized deadline, and subtracts the configured
maximum one-way transport delay. It converts the conservative deadline into a
monotonic cutoff at start. It refuses a start when the measurement is stale,
the offset uncertainty exceeds installation tolerance, or it cannot prove a
positive remaining interval. The coordinator also enforces its own deadline
and may request stop. Clock skew, drift, or transport delay can consume budget
but cannot extend it.

The coordinator holds the existing writer reservation while the remote
Invocation is live or its stop is unproven. Attention records the unresolved
condition but does not release the reservation. A new writer requires the
existing stop proof and unresolved-effect guards. A replacement endpoint or
new incarnation cannot adopt the old request, clear its reservation, or fence
the old process by declaration alone. Recovery uses the normal Core path and
a new request identity only after the old writer is proven stopped and
unresolved effects satisfy their existing release guards.

## 9. Refusal and failure vocabulary

The executor protocol uses these stable codes. The transport adapter maps them
to the existing Core evidence and Attention model; it does not add Worker
report statuses or Outcome kinds.

| Code | Meaning |
| --- | --- |
| `endpoint_identity_mismatch` | Authenticated principal differs from the configured endpoint binding |
| `incarnation_mismatch` | Command names a different executor boot incarnation |
| `protocol_unsupported` | No common protocol version satisfies the configured range |
| `capability_missing` | A required capability is absent or changed |
| `configuration_mismatch` | Immutable launch configuration digest differs |
| `request_identity_mismatch` | Installation, request, Invocation, or request digest does not match the prepared record |
| `conflicting_replay` | An operation ID was reused with a different payload digest |
| `input_invalid` | Input body, object closure, path, schema, or digest failed validation |
| `limit_exceeded` | A declared byte, count, time, process, or output limit was exceeded |
| `known_not_started` | Durable evidence proves no Worker process was created |
| `process_identity_unavailable` | A process may have started, but the executor could not durably capture its identity |
| `unknown` | The operation may have taken effect, but the executor cannot prove its result |
| `unavailable` | The endpoint or required receipt cannot be reached within the operation bound |
| `collection_incomplete` | One or more required collection members are absent |
| `collection_invalid` | Collection identity, manifest, object closure, or digest validation failed |
| `acknowledgement_conflict` | Acknowledgement does not match the durable collection identity and digest |

Diagnostics are size-limited and redacted. Unknown codes are protocol errors;
the coordinator retains the raw bounded receipt for diagnosis and fails closed.

## 10. Fault matrix

| Fault point | Durable evidence | Coordinator behavior |
| --- | --- | --- |
| Authentication or handshake fails | No prepared request; typed handshake refusal | Refuse dispatch; do not transfer source or start a Worker |
| Prepare reply is lost | Prepared operation may exist; same operation ID and digest | Query or replay the same prepare; do not start until preparation is confirmed |
| Start reply is lost | Start intent and possibly process identity | Reconcile the same request; hold reservation while `unknown`; never issue another start |
| Executor reboots during a run | Prior-incarnation receipts plus new incarnation ID | Mark process state `unknown`; retain reservation and WIP; do not infer stop |
| Coordinator loses connection while running | Last observation sequence and bound deadline | Reconnect to the same endpoint and Invocation; do not restart or fall back locally |
| Stop reply is lost | Durable stop intent and prior process identity | Replay/query the same stop operation; release only after stop proof |
| Collection is interrupted | Partial transfer IDs and verified chunk digests | Resume the same collection within recovery bounds; quarantine remains invisible |
| Collected report or manifest is stale or invalid | Rejected identity/digest and raw bounded body | Preserve as invalid evidence; do not acknowledge or classify artifacts as present |
| Acknowledgement command is lost before it arrives | Coordinator durable collection record and digest; no executor acknowledgement tombstone | Replay the same acknowledgement; executor retains output |
| Acknowledgement commits but its reply is lost | Coordinator durable collection record and digest; executor acknowledgement tombstone | Replay the same acknowledgement receipt; output may be reclaimed under retention policy |
| Endpoint is unavailable past recovery bounds | Last receipts, attempt count, and reason | Surface unavailable/Attention; preserve ownership uncertainty and retained WIP |
| Same operation ID has different bytes | Existing operation digest and conflicting replay digest | Refuse without side effect; retain both digests for diagnosis |

## 11. Compatibility and cutover

The executor protocol is additive to the v0.3 Worker protocol. Existing local
requests keep their recorded request format, digest, adapter binding, launch
configuration, and evidence. The local-only default remains unchanged and
requires no executor endpoint configuration.

Remote placement is opt-in and binds the executor protocol version, endpoint,
incarnation, capabilities, immutable launch configuration, and limits before
the first remote operation. There is no dual dispatch and no automatic
local/remote fallback. A remote request that becomes unavailable remains bound
to that endpoint and identity. Any later attempt is a new Core-authorized
request with a new request and Invocation identity after ownership and stop
guards permit it.

The implementation cutover adds transport-specific records and evidence
without rewriting historical Worker request/report digests. Contract fixtures
must cover the versioned contract, replay rules, limits, custody, and fault
matrix. They do not prove a live remote endpoint. Live transport and runtime
claims require the separately authorized acceptance work in the v0.4
milestone.

## 12. Design choice and alternatives

The contract keeps the six operations in one provider-neutral executor port.
The operation ledger gives each effect a stable identity, while the per-
Invocation start slot prevents duplicate starts even if a caller allocates a
second operation ID. This keeps retry policy in the coordinator and process
state in the executor.

An alternative is to put SSH command construction, PID handling, and artifact
paths directly in Core. That would make Core depend on one transport and one
executor's process model. The transport adapter instead authenticates the
endpoint, validates wire data, and returns opaque process evidence.

A second alternative is a resident endpoint service that accepts desired state
and independently retries lifecycle steps. That gives the endpoint an outer
controller and makes start, cancellation, and acknowledgement authority harder
to audit. The coordinator instead issues each bounded operation and records
the decision before sending it.

Restricted SSH with bounded framed messages remains the leading reference
transport candidate. The contract does not select it until #373 confirms the
environment can run the helper without changing v0.3 isolation or custody
guarantees.

## 13. Wire examples

### Handshake

The transport authenticates the endpoint principal before it accepts this
hello. The nonce binds the response to this handshake and prevents reuse of an
old capability declaration.

```json
{
  "format_version": "taisan.executor.hello/1",
  "installation_id": "install-a",
  "endpoint_id": "executor-a",
  "coordinator_nonce": "<32 lowercase hex>",
  "supported_protocols": ["taisan.executor/1"]
}
```

```json
{
  "format_version": "taisan.executor.hello-reply/1",
  "installation_id": "install-a",
  "endpoint_id": "executor-a",
  "coordinator_nonce": "<32 lowercase hex>",
  "endpoint_identity_digest": "<64 lowercase hex>",
  "responder_incarnation_id": "boot-7f1c",
  "supported_protocols": ["taisan.executor/1"],
  "capabilities": ["bounded_transfer", "process_control", "workspace_read_only"],
  "capabilities_digest": "<64 lowercase hex>",
  "clock": {
    "offset_ms": 2,
    "uncertainty_ms": 4,
    "round_trip_ms": 8,
    "measured_at_unix_ms": 1800000000000
  }
}
```

### Start receipt

```json
{
  "format_version": "taisan.executor.receipt/1",
  "installation_id": "install-a",
  "endpoint_id": "executor-a",
  "endpoint_identity_digest": "<64 lowercase hex>",
  "bound_incarnation_id": "boot-7f1c",
  "responder_incarnation_id": "boot-7f1c",
  "request_id": "e-7:inv:2",
  "invocation_id": "inv-41",
  "request_digest": "<64 lowercase hex>",
  "operation_id": "op-4f2a",
  "operation_kind": "start",
  "payload_digest": "<64 lowercase hex>",
  "command_digest": "<64 lowercase hex>",
  "result_digest": "<64 lowercase hex>",
  "receipt_digest": "<64 lowercase hex>",
  "result": {
    "status": "started",
    "process": {
      "evidence_ref": "proc-evidence-41",
      "identity_digest": "<64 lowercase hex>"
    }
  }
}
```

### Unknown start

```json
{
  "format_version": "taisan.executor.receipt/1",
  "installation_id": "install-a",
  "endpoint_id": "executor-a",
  "endpoint_identity_digest": "<64 lowercase hex>",
  "bound_incarnation_id": "boot-7f1c",
  "responder_incarnation_id": "boot-7f1c",
  "request_id": "e-7:inv:2",
  "invocation_id": "inv-41",
  "request_digest": "<64 lowercase hex>",
  "operation_id": "op-4f2a",
  "operation_kind": "start",
  "payload_digest": "<64 lowercase hex>",
  "command_digest": "<64 lowercase hex>",
  "result_digest": "<64 lowercase hex>",
  "receipt_digest": "<64 lowercase hex>",
  "result": {
    "status": "unknown",
    "reason": "process_identity_unavailable"
  }
}
```

The second example does not authorize a new start. The coordinator keeps the
writer reservation and reconciles or stops the possible process through the
same Invocation identity.

## 14. Coordinator lifecycle reconciliation

The coordinator keeps a durable operation ledger (`executor_operations`) and a
reconcile cursor (`executor_reconcile`) per bound request. A restart replays
this ledger against the endpoint's own durable request/operation records;
neither side invents work the other cannot verify. The following invariants
hold for every bound request:

- **Single writer.** One `start` command identity is admitted per request; the
  `executor_one_start` index makes a second start row a structural conflict.
  An unanswered or `unknown` start is queried under the recorded identity —
  never re-dispatched, never minted under a fresh operation id.
- **Persist-before-send.** Every command the coordinator sends is committed to
  the operation ledger before the wire sees it; a coordinator restart replays
  the same canonical bytes. Receipts attach only to their recorded command.
- **Durable stop intent.** A recorded `request_stop` command is the stop
  intent — durable evidence, not session state. Reconciliation re-drives
  intent → signal → inspect until a `stopped` receipt carries a proof bound
  to `bound_incarnation_id`.
- **No release without proof.** Writer release requires a `stopped` receipt
  whose `proof.incarnation_id` equals the binding's `bound_incarnation_id`.
  `running`, `exited` (leader gone, descendants unproven), `unknown`,
  `unavailable`, and transport faults never release the reservation.
- **Idempotent start.** Re-issuing the recorded start identity returns the
  recorded result; it never mints a second start row.
- **Bounded retry.** Consecutive unproductive reconcile steps persist in the
  reconcile cursor; `transport_max_retries` bounds them and the persisted
  `transport_backoff_seconds` schedule gates re-drives. A fault that never
  reached an operation still counts.
- **Deadline carry.** The absolute deadline recorded in the binding bounds
  every minted operation's remaining budget; replayed commands carry their
  original fields — a restart can never refund runtime.

### Fencing decision table

A possibly-live writer is an unreceipted/`unknown`/`started` start, or a stop
intent without a bound proof. Fences are fail-closed: the `RemoteFence` fact
marks the Invocation `unknown` (never released), moves the Execution to its
stop/pause hold, and derives the named Attention `remote_fence` carrying the
class. A fenced request mints and sends nothing; only a durable terminal
record minted by the bound incarnation is still adopted under a fence.

| Evidence at reconcile time | Fence class | Coordinator behavior |
| --- | --- | --- |
| Responder/session incarnation ≠ `bound_incarnation_id`, writer possibly live, probe under the recorded binding cannot prove the stop | `incarnation_changed` | Fence; reservation held; the durable stop intent stays re-driveable only through durable records |
| Same mismatch, but a recorded or freshly answered `stopped` receipt binds `bound_incarnation_id` | none | Adopt the terminal record; settle the writer through the normal stop-settle path |
| Consecutive transport faults (connect, IO, or foreign/unverifiable receipts) reach `transport_max_retries` | `transport_exhausted` | Fence; backoff schedule was persisted per step |
| Receipts answer `unknown`/`unavailable`, or `stop_requested`/still-live answers persist past the bound | `unanswerable` | Fence; unproven liveness is bounded like a fault |
| The endpoint refuses a recorded operation as `conflicting_replay` | `conflicting_replay` | Fence; the two ledgers disagree — nothing proceeds |
| A durable binding or operation row fails read-side verification | `record_corrupt` | Fence that request; other bound requests reconcile normally |
| A reconcile write loses the cursor CAS or a local replay conflicts | `ledger_conflict` | Fence; an orchestration disagreement the pass cannot resolve alone |
| `start` answered `known_not_started`/`refused` (non-`conflicting_replay`) | none | `LaunchRefused`; the writer is proven never to have existed |
| Binding recorded, no `start` ever issued (crash between bind and start) | none | `awaiting_dispatch`; dispatch owns the next step |
| `running`/`exited` answer under the bound incarnation | none | Adopt/keep observing; the reservation holds either way |

### Failure taxonomy

The closed `RemoteFenceClass` vocabulary — every reconciliation failure class
names one of these so the Attention a fenced writer produces is named by its
class:

| Class | Meaning |
| --- | --- |
| `incarnation_changed` | Session responder incarnation differs from the recorded `bound_incarnation_id` while a writer may still live |
| `transport_exhausted` | Connect/IO faults exceeded `transport_max_retries` |
| `conflicting_replay` | The endpoint's replay fence refused a recorded command — the ledgers disagree |
| `unanswerable` | `unknown`/`unavailable`/unproven answers persisted past the bounded budget |
| `record_corrupt` | A durable binding or operation row failed verification — reconciliation never acts on corrupt evidence |
| `ledger_conflict` | A local store write refused a reconcile step (CAS loss or conflicting replay) |

Local lifecycle loops (poll, signal/inspect, deferred report collection,
request consume) never touch a remote-bound invocation: presence of a durable
`executor_bindings` row routes the request to the reconcile driver, and an
unresolved remote writer blocks the local stop-settle without ever receiving
a local signal.
