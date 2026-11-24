# Remote executor reference endpoint (v0.4)

Status: reference implementation of #377, the Linux-side executor endpoint
of the transport contract in
[executor transport contract](executor-transport-contract.md) and the
boundary in [remote executor boundary](remote-executor.md). It is one
static endpoint: one configured coordinator, one execution slot, no
provisioning, no multi-tenancy, no deployment tooling.

The implementation lives in `crates/workloop-executor` and adds exactly
one executable, `taisan-executor`. Every other surface is reused from
`workloop-core` and `workloop-exec`.

## 1. Component inventory

| Component | Kind | Why it exists |
| --- | --- | --- |
| `taisan-executor serve` | foreground/durable helper executable (new) | Holds the incarnation identity, the durable state root, the one execution slot, and the supervisor calls. These must outlive any one transport session; a library cannot own them. |
| `taisan-executor connect` | stdio↔socket bridge in the same executable | The restricted-SSH forced-command shape: the remote command reads a bounded frame from stdin, hands it to the durable service over the unix socket, and writes the framed receipt to stdout. It holds no state, so one executable covers both roles. |
| `ExecutorService` (`service.rs`) | library server | hello plus the six fixed operations, replay fences, one-start slot, incarnation fencing. |
| `RemoteExecutor` (`adapter.rs`) | coordinator-side transport adapter | Session mutual authentication, hello binding check, framed command send, sealed receipt verification. It never emits a shell string. |
| `endpoint-state` (`state.rs`) | durable state root on the local filesystem | Digest-sealed JSON records — one file per record, atomic rename. Local durable receipt files satisfy the contract; no database server is introduced. |
| `Runner` (`runner.rs`) | invocation engine | Staging, launch, observation, stop escalation, and collection through `ProcessControlPort`, `containment`, `isolation`, and the v2 `describe` probe — the existing supervisor surface unchanged. |

Nothing else was added: no daemon set, no broker, no management CLI, no
database, no control plane. The new executable is the small
fixed-operation helper the acceptance criterion permits; a persistent
component is unavoidable because incarnation identity, durable receipts,
and retained failure evidence must survive transport sessions, and only a
resident process can own the live process table across them.

## 2. Slice boundary

The endpoint exposes exactly the fixed operations of the contract —
`hello`, `prepare`, `start`, `observe`, `request_stop`, `collect`,
`acknowledge` — over a bounded framed channel. There is no arbitrary
command capability.

The executor owns: the staged request bodies, the launch pipeline, the
local process evidence, stop proofs, output collection, and the durable
records that answer reconnects and restarts honestly.

The coordinator owns: every identity (`request_id`, `invocation_id`,
`request_digest`, `operation_id`), work selection, budgets, retry,
repository-host effects, and Outcome decisions. The Worker owns the
request/report contract; the executor invokes it through the unchanged
`describe`/`run --request --out` argv surface inside the configured
isolation profile and never redefines its wire formats.

## 3. Sessions and authentication

The transport is a length-prefixed framed channel carrying the #375 wire
envelopes; the reference placement is restricted SSH whose forced command
is `taisan-executor connect`, so the channel is stdio to the durable
service's unix socket — but any reliable bidirectional stream works.

Each session performs a mutual-authentication handshake before any
command:

1. The coordinator sends `taisan.executor.session-challenge/1` — a
   fresh nonce, the installation/endpoint identities, and its
   wall-clock send time so the endpoint can record the clock
   measurement the deadline accounting needs.
2. The endpoint answers `taisan.executor.session-auth/1` carrying a
   fresh endpoint nonce, the `endpoint_identity_digest`, and an
   HMAC-SHA256 (RFC 2104) over the two nonces and the identity under
   the endpoint bootstrap credential, proving the endpoint-side
   principal.
3. The coordinator sends `taisan.executor.session-open/1` — the
   counter-proof MAC over both nonces under the directional
   coordinator credential. The endpoint verifies it; a wrong
   credential yields `taisan.executor.session-refusal/1` and a closed
   channel.

The endpoint's authenticated-principal identity is
`endpoint_identity_digest` — SHA-256 of the canonical principal bytes
under the `taisan.executor.endpoint-identity.v1` domain tag — and the
adapter pins it against configured configuration; a different identity
closes the session. After authentication the adapter sends `hello` and
records the responder's incarnation.

Runtime credentials for a launch declaring `runtime_auth_mount` travel
over the authenticated session as
`taisan.executor.credential/1` / `taisan.executor.credential-ack/1`
(session messages, not executor operations), bound to the
`launch_config_digest`, and are staged at `requests/<id>/runtime-auth`
with owner-only permissions.

## 4. Operations and durable state

`taisan-executor serve --config <endpoint.toml>` binds a unix socket at
`config.socket_path`, mints a fresh unpredictable
`inc-<uuid>` incarnation into `service.json`, and answers connections.
A background sweep enforces bound deadlines executor-side: a request
whose conservative local deadline (coordinator deadline minus clock
uncertainty minus one-way delay) expires is stopped under its own
authority — the receipt names the stop proof, not a coordinator guess.

The state root (`config.state_root`) holds digest-sealed JSON records:

```text
service.json                         service record: incarnation, boot sequence
requests/<request_id>/
  binding.json                       request record: identities, launch config,
                                     digests, durable state word
  ops/<operation_id>-<tag>.json      operation ledger: command bytes, stored
                                     receipt body + digest (replay surface)
  process.json                       process record: leader/descendant identities,
                                     incarnation, evidence ref + digest,
                                     snapshot digest, local deadline
  prepared/                          staged inputs (request.json, inputs/, bundle)
  workspace/                         worker workspace
  out/                               worker output (until acknowledge)
  runtime-auth                       staged runtime credential (mode 0600)
  stop_intent.json                   durable stop intent (fences late starts)
  stop_proof.json                    durable stop proof
  observations.json                  durable observation sequence
  collection.json                    collection manifest + digest
  objects.bundle                     retained produced-object bundle (until ack)
  ack.json                           acknowledge tombstone
```

Every record is sealed by `record_digest` over its canonical bytes; a
seal violation reads as corruption, never silently. The operation ledger
makes each command idempotent: an identical replay returns the stored
receipt bytes, a different body under the same operation id is
`conflicting_replay`, and a committed-but-unreceipted command resumes
from durable state rather than re-running its effect.

`acknowledge` writes the tombstone and releases the retained bodies
(inputs, workspace, output); identity metadata and the operation ledger
stay for audit.

## 5. Launch, containment, and honesty

`prepare` validates the launch configuration (profile match, bounded
limits, declared required capabilities, workspace grant enforceability,
deadline, `work_id`), verifies `launch_config_digest`, and stages
content-addressed items under `prepared/`. The request body must be
well-formed JSON; item digests use the executor's content tag so a
tampered body is refused before staging.

`start` enforces the one-start slot and the single execution slot,
runs negative preflight (`containment::gate`), probes the Worker's v2
`describe` declaration and checks the pinned binding (adapter,
adapter version, runtime version, capability digest), builds the mount
plan masking the state root, and spawns through `ProcessControlPort`.
Uncontained placement (`sandbox_primitive: none`) honestly degrades:
`workspace_read_only` and other containment-dependent enforcement is
refused at prepare, and the hello capability set reports only what the
configured profile enforces.

`observe` inspects the recorded leader and descendant identities —
state, not presence — and answers `running`, `stopped` (with durable
proof), or `unknown`. A bound request answered by a new incarnation can
no longer inspect its process tree, so it answers `unknown` with
`incarnation_mismatch`; a durable stop proof still answers `stopped`.

`request_stop` persists the stop intent first — a late `start` is
refused — then escalates TERM→KILL over the verified group within the
configured grace and writes the stop proof.

`collect` requires the receipt digest of the operation that proved the
stop, gathers output bytes under the bound output limit, and returns a
`collection_digest`-sealed manifest. `acknowledge` requires that exact
collection identity; anything else is `acknowledgement_conflict`.

Capabilities are reported in `hello` from the configured profile, not
claimed. A required capability outside the declared set is refused at
prepare with `capability_missing`; an unsupported required enforcement
never silently runs.

## 6. Configuration

`taisan-executor serve` reads one TOML file (`deny_unknown_fields`):

```toml
installation_id      = "inst-example"
endpoint_id          = "ep-linux-1"
state_root           = "/var/lib/taisan-executor"
socket_path          = "/var/lib/taisan-executor/executor.sock"
endpoint_credential_file   = "/etc/taisan/endpoint.cred"
coordinator_credential_file = "/etc/taisan/coordinator.cred"
worker_argv          = ["taisan-worker-claude"]
isolation_profile_id = "taisan-reference-v0.1"
supported_protocols  = ["taisan.executor/1"]
max_clock_offset_ms  = 30000
max_one_way_delay_ms = 2000
max_state_bytes      = 268435456
```

The credential files hold the directional bootstrap secrets (≤4096
bytes each). Bootstrap is trusted setup; the Worker never sees the
endpoint credentials, the coordinator credential, or any
host-management authority.

## 7. Fixtures and verification

`tests/contract.rs` proves the contract against a scripted process
table (`ScriptedProcessControl`) and a `UnixStream` session pair — no
live remote host, per the milestone's fixture rule. Covered: the full
lifecycle over one session, byte-for-byte replay, conflicting replay,
durable stop-intent fencing, the single execution slot, reconnect
recovery, restart/incarnation change (typed `unknown`, preserved
stop proofs), capability and identity refusals, collect/acknowledge
authorization, session-credential rejection, credential delivery, and
executor-side deadline enforcement.
