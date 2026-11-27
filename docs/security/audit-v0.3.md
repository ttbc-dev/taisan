# Security audit — v0.3

| | |
| --- | --- |
| Commit | `90fd9d0` (merge of PR #493, `issue-489-security-baseline`) |
| Branch | `issue-491-agent` |
| Date | 2026-11-27 |
| Method | read-only source review per `docs/security/audit-checklist.md` |
| Auditor | agent pass (Devin), first repo-wide audit |

## Audited paths

- `crates/workloop-core/src/credential.rs` — `credential_ref` grammar
- `crates/workloop-core/src/executor.rs` — envelope bounds, digest and
  binding checks (`binding_violations`, `ExecutorHelloReplyV1::parse`,
  `ExecutorCommandV1::parse`, `RemoteHandoffV1::parse`, `path_is_valid`,
  `check_bound`, `strict_json`)
- `crates/workloop-core/src/worker.rs` — isolation-profile registry
- `crates/workloop-exec/src/credentials.rs` — `env:`/`file:`/`command:`
  resolution, `ResolvedCredential`
- `crates/workloop-exec/src/containment.rs`, `isolation.rs`,
  `boundary.rs`, `supervisor.rs`, `bounded.rs` — mount plan, env
  construction, launch gates, spawn env
- `crates/workloop-executor/src/auth.rs` — session MAC, `mac_eq`,
  nonces, envelope types
- `crates/workloop-executor/src/channel.rs` — framed channel,
  `endpoint_handshake`/`coordinator_handshake`
- `crates/workloop-executor/src/config.rs` — `EndpointConfig`,
  `read_credential`, `CoordinatorEndpoint`
- `crates/workloop-executor/src/service.rs` — dispatch, `bound_request`,
  six operations, `handle_credential`, `serve_unix`, capability
  declaration
- `crates/workloop-executor/src/runner.rs` — `stage_credential`,
  `release_credential`, `has_credential`, launch arms, `enforce_deadlines`,
  collect path (`read_bounded_file`, `copy_tree`, `bundle_objects`,
  `run_git`, `git_env`, `read_credential_command` equivalents)
- `crates/workloop-executor/src/state.rs` — `write_atomic`,
  `write_atomic_private`, `requests()`, `reclaim_bodies`, seal/verify,
  `request_dir`/`op_path` tagging
- `crates/workloop-executor/src/adapter.rs` — coordinator-side adapter,
  `binding_violations` use
- `crates/workloop-github/src/credential.rs` — host-side provider
  implementations
- `crates/workloop-github/src/gh_cli.rs` — `GH_TOKEN` transport,
  `scrubbed_git_env`, `push_authorization`
- `crates/workloop-store/src/sqlite.rs` — query parameterization
- `scripts/s1/` — owner-side tooling incl. `mint-app-token.py`

## Invariants verified

| Invariant | Status | Settling code |
| --- | --- | --- |
| `credential_ref` names a slot, never a value; grammar refusals don't echo it | upheld | `workloop-core/src/credential.rs` parse + tests |
| Provider-resolution refusals never echo the reference's identifying fields | violated on some sides | finding F10 |
| `file:` provider: regular file, euid-owned, `mode & 0o077 == 0`, symlink resolved before checks | upheld | `workloop-exec/src/credentials.rs:155-197`; `workloop-github/src/credential.rs:176-204` |
| `command:` provider: argv no-shell | upheld (both impls) | `workloop-exec/src/credentials.rs:206+`; `workloop-github/src/credential.rs:222-246` |
| `command:` provider: bounded wall clock + bounded output | violated on github side | finding F4 |
| `ResolvedCredential` redacted; services holding secrets don't derive `Debug` | violated for `CoordinatorEndpoint` | finding F2 |
| Frames bounded before allocation, per class | upheld | `workloop-executor/src/channel.rs:29-37,85-92`; `workloop-core/src/executor.rs:48-50` |
| Handshake: mutual MAC, fresh nonces, nonce echo-binding, constant-time compare | upheld | `workloop-executor/src/channel.rs:119-192`; `auth.rs:39-85` |
| Command digest verifies over raw body minus covered member; identity members hex64-validated before any slicing | upheld | `workloop-core/src/executor.rs:895-940` |
| Command bound to full identity set before dispatch; replay fence; conflicting op id refused | upheld | `workloop-executor/src/service.rs:217-427,447-491` |
| `runtime-auth` written 0600 atomic-private, bound to launch digest, ≤4 KiB, only while `prepared` | upheld | `runner.rs:472-497`; `state.rs:93-115` |
| `runtime-auth` removed after stop proof / at acknowledge reclaim | partially upheld | findings F3, F5 |
| Worker env is allowlist + `env_clear`, no credential-bearing passthrough names | upheld | `supervisor.rs:606-610`; `bounded.rs:92-228` |
| Containment: unshare, ro binds, tmpfs, masked roots/files, unshadowable plans refused | upheld | `workloop-exec/src/containment.rs` |
| Closed isolation-profile registry; unapproved id fails closed | upheld | `worker.rs:104-128`; `service.rs:564-572` |
| Worker output read: whitelist + `symlink_metadata` + regular-file check + byte cap; `copy_tree` refuses symlinks | upheld | `runner.rs:38-43,~1729,~1782` |
| Deadline: conservative conversion, skew only consumes; sweep independent of liveness | upheld (mechanism) | `runner.rs:520-536,291-298` |
| Deadline sweep keeps enforcing when a record is corrupt | violated | finding F6 |
| Deadline enforcement survives a service restart for still-running processes | violated | finding F8 |
| Capability honesty: declared capabilities reflect enforceable profile | partially upheld | finding F9 |
| Single execution slot; enumeration failure fails closed | upheld | `service.rs:804-828` |
| Collect authorized by recorded stop-proof receipt; ack tombstone before reclaim | upheld | `service.rs:1032-1062,1113-1183` |
| `gh` token via env only; push auth via env-scoped `GIT_CONFIG_KEY_N`; header-corrupting tokens refused | upheld | `gh_cli.rs` (`push_authorization`, `run_plain` env_clear) |
| Sealed records: corrupt → `Corrupt` error, never silent absence | upheld | `state.rs` (`read_record`/`verify`) |
| Store: parameterized queries | upheld | `workloop-store/src/sqlite.rs` |
| bwrap actually enforces the namespaces the plan requests | not-verifiable | flags verified statically; kernel behavior is runtime |

## Findings

F1, F7, and F11 were reclassified into Accepted risks — their concrete
harms require declared non-goals (a multi-tenant endpoint host,
platform-level denial of service, trusted operator configuration).
Numbering of the remaining findings is preserved.

### F2 — MEDIUM: `CoordinatorEndpoint` derives `Debug` over raw session credentials

`crates/workloop-executor/src/config.rs:192-204`.

`#[derive(Debug, Clone)]` on a struct whose `pub` fields
`endpoint_credential`/`coordinator_credential` hold the raw `Vec<u8>`
secrets. `ResolvedCredential` deliberately implements a redacted Debug;
`ExecutorService` has no Debug at all; this struct contradicts both
conventions. No current call site formats it (verified by grep), but the
struct is one `{:?}` — a startup `eprintln!`, an `anyhow` context — away
from printing both session secrets.

**Direction:** hand-write a redacted `Debug` impl, mirroring
`ResolvedCredential`. **Coverage:** none.

### F3 — MEDIUM: `runtime-auth.tmp-partial` residue outlives the declared retention bound

`crates/workloop-executor/src/state.rs:93-115` (`write_atomic_private`
names the tmp `path.with_extension("tmp-partial")` → `runtime-auth.tmp-partial`);
`crates/workloop-executor/src/runner.rs:506-508` (`release_credential`
removes only `runtime-auth`); `crates/workloop-executor/src/state.rs:580-588`
(`reclaim_bodies` removes `runtime-auth` but not the tmp name).

A crash or I/O error between tmp creation and rename leaves
`runtime-auth.tmp-partial` holding partial secret bytes. Neither the
stop-proof release nor the acknowledge reclaim names that file, so the
residue survives both declared removal points indefinitely. The
supervisor-side sibling already learned this lesson —
`unstage_credential` removes both `runtime-auth` and `runtime-auth.tmp`
(`workloop-exec/src/boundary.rs`).

**Scenario:** a credential delivery is retried while the request is
still `prepared` — a lost ack, a coordinator-side re-send
(`stage_credential` refuses any later state, so a retry can only land
before start). The retry's `write_atomic_private` fails after the tmp
file is created but before rename (I/O error, full disk): the first
delivery's `runtime-auth` is intact, `runtime-auth.tmp-partial` now
holds the torn second write, and the retry's refused ack is invisible
to the start gate — `has_credential` checks only the `runtime-auth`
name, so `start` proceeds on the staged credential. The request runs,
stops, is collected and acknowledged — `release_credential` clears
`runtime-auth` at stop proof and `reclaim_bodies` reports bodies
reclaimed, while a credential fragment still sits in the request dir.
(A crash mid-stage leaves the same residue but strands the request on
its dead incarnation — under F8, no stop proof can ever be produced,
so collect and acknowledge never run; only the in-incarnation write
failure reaches a false reclaim report.) The defect is the violated
retention contract — a false reclamation report — independent of who
might read the residue.

**Direction:** add `runtime-auth.tmp-partial` to the `reclaim_bodies`
list and to `release_credential`; or have `stage_credential` sweep the
tmp name first. **Coverage:** none.

### F4 — MEDIUM: `command:` provider on the github side has no timeout, no output bound, and leaks bounded stderr into the error

`crates/workloop-github/src/credential.rs:222-246`
(`run_credential_command`).

`Command::output()` waits indefinitely with no deadline and no
process-group kill, buffers stdout and stderr unbounded in memory, and
on non-zero exit interpolates up to 160 chars of stderr into the
`credential.unresolvable` detail (`:236-241`). Compare
`resolve_command` in `workloop-exec/src/credentials.rs:206+`: 30-second
wall bound, drained pipes, process-group kill, bounded output, and
`stderr` deliberately wired to `Stdio::null()`.

Two defects, one site:

- **Availability:** a wedged credential helper hangs credential
  resolution forever — the supervisor's run loop stalls.
- **Secret-in-error leak:** a helper that prints the secret to stderr on
  failure (a debugging flag, a misbehaving wrapper) puts up to 160 bytes
  of it into the typed error — which flows to diagnostics/evidence. The
  exec side chose `Stdio::null()` exactly for this.

**Direction:** reuse the exec-side bounded spawn (or mirror its bound +
stderr-null choice); if stderr is diagnostically needed, keep it out of
the error detail. **Coverage:** none found for the timeout path.

### F5 — MEDIUM: a staged credential for an abandoned `prepared` request has no removal path

`crates/workloop-executor/src/runner.rs:481-497` (staging requires
`state == "prepared"`), `:1051-1064` (`enforce_deadlines` iterates only
`is_live()` records), `:506-508` (`release_credential` runs only inside
the stop-proof path, `:1296`); `crates/workloop-executor/src/state.rs:578-597`
(`reclaim_bodies` runs only at acknowledge —
`service.rs:1175`), and acknowledge requires a completed collection
(`service.rs:1118-1126`) which requires a stop proof
(`service.rs:1045-1055`) which a never-started request can never
produce.

Contract §7: the executor "retains the file only while the Invocation
may run." A request that is prepared, has its credential staged, and is
then abandoned (coordinator disconnect, crash, deadline lapse) violates
that: the file persists on the endpoint filesystem with no sweep, no
TTL, and no reachable removal operation.

**Scenario:** coordinator prepares + delivers the runtime credential,
then its session drops before `start`. The `runtime-auth` file sits in
`requests/<id>/` forever; every subsequent prepare for a *different*
request re-stages nothing here — the secret just stays.

**Direction:** sweep `prepared` records past their deadline (or add a
staging TTL) and release the credential there; `clear_torn_prepare`
(`runner.rs:~1327`) should also clear `runtime-auth` for parity.
**Coverage:** none.

### F6 — MEDIUM: one corrupt binding record silently disables all deadline enforcement

`crates/workloop-executor/src/runner.rs:1051-1054`:

```rust
let Ok(requests) = self.state.requests() else {
    return;
};
```

`state.requests()` propagates `StateError::Corrupt` on the first
unreadable or seal-failed `binding.json`. The sweep then returns early —
every 200 ms tick and every command's leading sweep — with no error
surfaced. One corrupt record turns deadline enforcement off endpoint-wide
and silently.

The asymmetry is the tell: `op_start` treats the same enumeration error
as a fail-closed refusal and documents why (`service.rs:804-827`); the
sweep fails *open* on the same input for the security mechanism that
bounds authorized runtime.

**Scenario:** a torn write or bit-rot corrupts `requests/<a>/binding.json`.
A worker on request `<b>` runs past its authorized deadline indefinitely;
nothing is logged.

**Direction:** sweep per-record (`requests()` yielding
`Result<record, Corrupt>` items, skip-and-report), or at minimum surface
the failure instead of `let ... else return`. **Coverage:** none.

### F8 — MEDIUM: a service restart orphans deadline enforcement for still-running workers

`crates/workloop-executor/src/runner.rs:1056`:
`record.bound_incarnation_id != current_incarnation → continue`.

`boot` mints a fresh incarnation (`service.rs:65-95`). Prepare and start
refuse commands whose incarnation is not the live one, and stop returns
`incarnation_mismatch` for a record bound to a dead incarnation
(`service.rs:497-505`, `:797-803`, `runner.rs:1126-1133`). Collect and
acknowledge, however, dispatch through `bound_request`
(`service.rs:447-491`, `:1032-1035`, `:1113-1117`), which matches the
command's incarnation to the *stored record's* binding — not the live
incarnation — so an old-incarnation request is still accepted there: a
record whose stop proof predates the restart can still be collected and
acknowledged, while a still-running worker can never be stopped, hence
never produces a proof and never becomes collectable. The deadline
sweep also skips old-incarnation records — but the worker process the
old incarnation spawned may still be running. After a restart, such a
process exceeds its authorized-runtime bound with no enforcement: the
sweep won't touch it, `request_stop` returns `incarnation_mismatch`,
and nothing reaps it.

**Scenario:** endpoint restarts mid-Invocation; the spawned worker's
process group survives; its deadline passes — it runs indefinitely,
still writing to its request dir.

**Direction:** at boot, fence old-incarnation live records — inspect
their process records and run the stop/deadline path (the identities
are durable and inspectable; custody fencing need not preclude stop
enforcement). **Coverage:** none.

### F9 — MEDIUM: `runtime_auth_mount` silently degrades under `Gate::Uncontained`; `runtime_auth_delivery` is declared unconditionally

`crates/workloop-executor/src/runner.rs:711-719` (the `Gate::Uncontained`
launch arm extends argv with `run --request <path> --out <path>` — no
`--runtime-auth` flag and no mount); `:560-565` (`has_credential` still
gates the start); `crates/workloop-executor/src/service.rs:~36`
(`runtime_auth_delivery` sits in `BASE_CAPABILITIES`, always declared —
see `declared_capabilities`, `:157-168`).

Under a profile with `sandbox_primitive: None` (the registry's
`fixture-worker-v0.1`, `worker.rs:115`), a `runtime_auth_mount: true`
prepare is accepted and the credential is staged — but it is never
delivered through the contract's read-only mount, and the secret file
sits at a predictable host path.

The honest-capability convention (`BASE` vs `CONTAINED_CAPABILITIES`)
exists exactly to prevent this overstated claim — `runtime_auth_delivery`
should sit on the contained side, or prepare should refuse
`runtime_auth_mount` when the gate is `Uncontained`.

**Scenario:** `fixture-worker-v0.1` is an approved entry in the closed
profile registry, so an operator running it holds a sanctioned
configuration — the accepted risk below accepts that the deployment
*drops isolation*, not that the endpoint may misreport what it
delivers. A coordinator trusting the declared `runtime_auth_delivery`
prepares with `runtime_auth_mount: true`; prepare succeeds, the
credential is staged, and start passes the `has_credential` gate — then
the uncontained launch arm mounts nothing: the promised §7 read-only
delivery never happens — the worker is launched with no
`--runtime-auth` mount — and nothing reports the degrade. The harm
needs no attacker and no malicious worker: the endpoint advertises a
delivery channel it does not provide for an approved profile. The further
escalation — a same-UID Worker harvesting the staged and endpoint
credentials to impersonate the coordinator — rides on the
uncontained-deployment accepted risk below and is not the finding; the
capability misdeclaration above is.

**Direction:** move `runtime_auth_delivery` into the contained
capability set or refuse the mount under uncontained launch.
**Coverage:** none.

### F10 — LOW: provider errors echo the reference's identifying fields on some sides only

`crates/workloop-exec/src/credentials.rs:129-141` (`resolve_env` echoes
the variable `'{name}'`); `crates/workloop-github/src/credential.rs:179,
184,189,198,202` (`read_credential_file` echoes `path.display()` at five
sites) and `:232,238` (`run_credential_command` echoes `{program}`).

The documented rule is that the reference may be a mispasted secret and
must not be echoed. The exec-side `file:` provider carefully returns
generic strings (`:155-192` — no path in any error) while the github
side echoes the full path; the github-side `command:` provider echoes
`{program}` on spawn and exit failures (`:232,238`) while the exec-side
`resolve_command_bounded` (`:210-308`) never carries argv in a refusal.
Both sides' `env:` refusals echo the variable name (exec `:129-141`,
github `:156`). A `file:ghp_<secret>` paste on the github side, an
`env:ghp_<secret>` paste on either side, or a github-side `command:`
reference whose argv[0] is a pasted secret lands the secret in
`credential.unresolvable` → diagnostics/evidence.

**Direction:** apply the exec-side convention — name the provider kind
and the failed property, never the user-supplied value.
**Coverage:** none.

### F12 — LOW: hand-rolled `hmac_sha256`

`crates/workloop-executor/src/auth.rs:39-63` — a textbook RFC 2104
implementation, covered by RFC 4231 vectors (`auth.rs` tests). Correct
as written; the exposure is future edits to a bespoke primitive.

**Scenario:** a later edit to `hmac_sha256` — key normalization, pad
constants, or block handling — subtly breaks the primitive; session
MACs keep being produced and compared while no longer matching RFC
2104, and only the RFC 4231 vectors catch it, only while they keep
running against the edited code.

**Direction:** adopt the `hmac` crate (already in the `sha2` ecosystem)
or add a comment pointing at the vector tests. **Coverage:** RFC 4231
vectors.

## Needs verification

- **Executor-side `git` in a Worker-controlled repo.** `bundle_objects`
  (`runner.rs:~1608-1654`) runs `git -C <workspace> bundle create` after
  the Worker has had write access to `.git/`. `git_env()`
  (`runner.rs:1664-1669`) seals `GIT_TERMINAL_PROMPT=0`,
  `GIT_ALLOW_PROTOCOL=file`, `GIT_CONFIG_NOSYSTEM=1` — but repo-local
  `.git/config` is still parsed, and `include.path` chains from it.
  Whether `bundle`/`init` reach command-executing config keys
  (`core.fsmonitor`, `core.hooksPath`, `filter.*.clean`) needs a live
  probe or a git-version-specific read. Also `GIT_CONFIG_GLOBAL` is not
  clamped here while `scrubbed_git_env` (`gh_cli.rs`) does clamp it.
- **`CoordinatorEndpoint` production wiring.** The struct is referenced
  only by `config.rs`, `adapter.rs`, and the contract tests — the
  coordinator-side loader that populates it from installation config
  was not located in this pass; confirm whether it exists elsewhere and
  whether it reuses the unchecked `read_credential` (the
  bootstrap-credential accepted-risk entry below).
- **Delivery-vs-incarnation binding.** `handle_credential`
  (`service.rs:1192-1209`) checks `request_id` + `invocation_id` but
  not `bound_incarnation_id` (the delivery vocabulary has no incarnation
  member). A delivery arriving on a post-restart session for an
  old-incarnation prepared request stages a credential that can never
  be consumed or reclaimed — same retention hole as F5, narrower door.
- **`out/`-dir intermediate-component symlink.** `read_bounded_file`
  checks the leaf via `symlink_metadata`; a symlinked `out/` directory
  itself is not checked. In contained mode the mount point cannot be
  replaced, and in uncontained mode the Worker reads the host fs
  anyway, so no capability is gained — verify the containment reasoning
  holds for every profile before closing this out.
- **`gh` ambient environment.** `api`/`api_as` inherit the supervisor's
  environment (no `env_clear` on that path — `gh_cli.rs` ~246-291);
  `GH_TOKEN` is set explicitly and wins for `api.github.com`, but
  ambient `GH_CONFIG_DIR`/`GH_ENTERPRISE_TOKEN`/proxy vars pass through.
  The supervisor env is trusted input (accepted risk), but a
  `GH_CONFIG_DIR` redirect changes which host `GH_TOKEN` is offered to —
  confirm intended precedence.
- **Aggregate state-root quota.** `max_state_bytes` bounds staged bytes
  per Invocation (`staged_bytes`, `runner.rs:~1768`); there is no
  aggregate bound across prepared requests, so an authenticated
  coordinator can fill endpoint disk with unbounded prepares. The
  coordinator is a trusted peer — flag as accepted risk or add a cap.

## Accepted risks

Recorded so future audits do not re-raise them:

- Sandbox escapes (bubblewrap/kernel) and vulnerabilities in spawned
  tools (`git`, `gh`, worker runtimes) are upstream concerns; the
  profile boundary is namespace-level, not a security boundary against
  malicious in-sandbox code beyond what it bounds.
- `fixture-worker-v0.1` (`sandbox_primitive: None`) exists for contract
  tests; deploying it drops isolation entirely — the worker shares the
  host filesystem, so a same-UID worker can read *every* request's
  staged `runtime-auth` and the endpoint's own
  `endpoint_credential_file`/`coordinator_credential_file` (mode 0600 is
  no barrier to the same UID), forge `stop-intent`/`binding`
  records — the sealed digest is an unkeyed self-check, recomputable by
  the writer — or impersonate the coordinator with the harvested
  session credentials (`coordinator_handshake`, `channel.rs:119`)
  (see F9 for the capability-honesty gap this exposes).
- Bootstrap credential files are read without the `file:` providers'
  ownership/mode checks: `read_credential` (`config.rs:154-186`)
  applies only a 4096-byte bound and a whitespace trim — no euid,
  `mode & 0o077`, or regular-file check, despite the module
  docstring's "mode-0600" staging claim. The concrete harm — a
  co-tenant reading a lax-permissioned bootstrap secret and forging
  coordinator sessions (`SessionOpen` MAC, `channel.rs:177-190`) —
  requires a multi-tenant endpoint host, a declared non-goal; the host
  account boundary is the accepted defense. (Raised as F1 in this
  audit's first pass; reclassified. Sharing the provider-side
  euid/`mode & 0o077`/regular-file checks so a mis-staged file is
  refused rather than silently accepted remains cheap fail-closed
  hardening, not a finding.)
- The unix listener trusts the local host account boundary
  (`serve_unix`, `service.rs:1284-1333`): socket permissions follow
  umask with no `SO_PEERCRED`/peer-uid gate, each `accept` spawns a
  thread that blocks in `read_frame` with no handshake deadline and no
  bound on pre-auth connections, and startup unconditionally unlinks
  whatever sits at `socket_path` (`:1289`). The harms — a co-tenant's
  session attempts and unauthenticated thread exhaustion — are
  multi-tenant-host and platform-level-DoS non-goals; the
  `remove_file` hazard rides on trusted operator configuration.
  (Raised as F7 in this audit's first pass; reclassified. Socket
  permissions, a handshake deadline, a bounded connection pool, and a
  socket-type check before unlink remain available hardening.)
- The state tree and non-secret records use umask-default permissions:
  `EndpointState::open` (`state.rs:369-373`, `create_dir_all` on root +
  `requests/`) and `write_atomic` (`:80-86`) tighten nothing, so under
  a permissive umask the state root and request contents (staged
  inputs, request bodies, `out/` bodies) are world-readable —
  work-content disclosure, not a credential leak (`runtime-auth`
  itself stays 0600). The harm requires a co-tenant on the endpoint
  host, a declared non-goal; the host account boundary is the accepted
  defense. (Raised as F11 in this audit's first pass; reclassified.
  Tightening the state root to 0700 at `open` — and refusing or
  warning if it pre-exists laxer — remains cheap hardening, not a
  finding.)
- Transport confidentiality is delegated below the framed channel (SSH
  forced-command bridge / local socket); the MAC authenticates but does
  not encrypt.
- A same-UID writer on the endpoint host can rewrite the state root —
  sealed records detect corruption, not forgery; the state root is
  trusted-local storage.
- Credential `Vec<u8>`s held in process memory are not zeroized; host
  memory disclosure is out of scope.
- The coordinator is an authenticated peer: a coordinator holding valid
  credentials is trusted to issue commands; findings concern code paths
  that weaken what its *peers or co-tenants* can do, not what the
  coordinator itself can.
- Per-Invocation resource limits bound the work, not the platform; host
  OOM/fork-exhaustion inside declared limits is a deployment concern.
