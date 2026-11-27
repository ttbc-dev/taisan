# AGENTS.md

## Source references

- Publication-facing docs and code describe the private source
  repositories this project was distilled from by role ("the ops repo",
  "the worker repo", "the selector repo", "the infra repo" — see
  `docs/source-workflow-audit.md` for the role legend). Issue numbers
  and abbreviated revision SHAs may be cited as owner-verifiable
  evidence locators.
- By owner decision (#438) the private repository names and the owner's
  identity are acceptable to disclose: where a source or scenario name
  does appear — including the private acceptance repository
  `shimpeiws/coding-worker-smoke` — it is retained verbatim, and links
  or `#N` citations to private repositories resolve only for the
  owner. Documents carrying such references mark them as
  private/owner-only rather than presenting them as working links.
- `docs/source-workflow-audit.md` is the owner-controlled audit record:
  it retains source-internal names, issue/PR numbers, and revision SHAs
  so each design mapping can be re-verified against the sources. It
  stays in the published tree as the provenance record.
- `docs/release-audit.md` is the disclosure audit for issue #368: it
  maps every leak location by file:line or commit reference, classified
  by leak class, and records in §7 the owner disposition under which
  this tree is published.

## Pull requests

- PR titles and descriptions are written in English.

## Verification

```bash
cargo fmt --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` contract tests spawn `target/debug/workloop-fixture-worker`, so
run `cargo build --workspace` first — `cargo test` alone does not produce the
fixture-worker binary.

## Trust boundary

taisan brokers credentials into Worker processes that execute coding-agent
work over staged inputs. That makes the boundary sharper than for a typical
CLI:

**Trusted:**

- The operator and installation configuration — endpoint config, coordinator
  store, and bootstrap-staged credential files.
- The coordinator↔endpoint authenticated session (mutual MAC-authenticated
  handshake, `docs/executor-transport-contract.md` §3). Once authenticated,
  the coordinator's commands and credential deliveries are trusted peer
  input — still digested, bounded, and bound to identity, but not untrusted.
- The closed approved-isolation-profile registry
  (`crates/workloop-core/src/worker.rs`) and the containment plan the
  profile resolves to.
- Credential providers (`env:`/`file:`/`command:`) — their *outputs* at rest
  are trusted secret material; the reference grammar itself is operator
  configuration.

**Not trusted:**

- **The Worker process and everything it produces** — `out/` contents,
  workspace state, `handoff.json`, stdout/stderr. Everything read back from
  Worker-writable space is parsed under whitelist, symlink-refusal, and byte
  bounds (`crates/workloop-executor/src/runner.rs`).
- **Wire data pre-authentication** — framed envelopes, handshakes, and
  commands; bounded before allocation and digested against the durable
  binding before any effect.
- **Staged inputs** — coordinator-supplied request/workspace items. Their
  digests and paths are checked, but their *content* drives a Worker.
- **`credential_ref` values** — a reference may be a mispasted secret; parse
  refusals never echo it, and provider-resolution refusals are held to the
  same rule: name only the provider rule that failed
  (`crates/workloop-core/src/credential.rs`,
  `crates/workloop-exec/src/credentials.rs`,
  `crates/workloop-github/src/credential.rs`).
  `docs/security/audit-v0.3.md` F10 records residual echoes of slot fields
  on provider paths as a defect against this rule, tracked for follow-up.

## Invariants

These are guarantees the implementation is expected to uphold. A change that
breaks one is a defect, not a tradeoff. The normative wording lives in
`docs/executor-transport-contract.md` (§2–§8), `docs/installation-config.md`,
and `docs/worker-protocol.md`; when prose and contract differ, the contract
is right.

- `credential_ref` names a provider slot (`env:`/`file:`/`command:`), never
  a value. Refusals never echo the reference (audit v0.3 F10 records
  provider-resolution refusals echoing slot fields on some sides — a
  tracked defect).
- Resolved credentials never appear in argv, env, request bodies, receipts,
  evidence, logs, or error messages. `ResolvedCredential` is unformattable
  or redacted at each boundary: `workloop-exec`'s has no `Display` and a
  redacted `Debug`; `workloop-github`'s `Display`/`Debug` render
  `<redacted>`; `ExecutorService` has no `Debug` at all (audit v0.3 F2
  records `CoordinatorEndpoint` deriving `Debug` over raw credentials,
  and F4 records the github-side `command:` provider interpolating
  bounded helper stderr into the typed error — tracked defects).
- `file:` providers require an absolute path resolving to a regular file
  owned by the effective user with `mode & 0o077 == 0`, checked after
  symlink resolution (`crates/workloop-exec/src/credentials.rs`,
  `crates/workloop-github/src/credential.rs`).
- `command:` providers run argv directly — no shell — with a bounded wall
  clock, process-group kill on timeout, and bounded output
  (`crates/workloop-exec/src/credentials.rs`; audit v0.3 F4 records the
  github-side provider missing all three bounds — a tracked defect).
- The session handshake is mutual: fresh nonces per session, directional
  MAC tags, constant-time MAC comparison, and nonce/identity echo-binding
  on both sides (`crates/workloop-executor/src/auth.rs`,
  `crates/workloop-executor/src/channel.rs`).
- Frames are bounded before allocation by class: handshake 64 KiB, command
  1 MiB, receipt-bound collection
  (`crates/workloop-core/src/executor.rs`).
- Every command is bound to installation, endpoint identity, incarnation,
  request, Invocation, and request digest before dispatch; a replay returns
  the durable receipt byte-for-byte, a conflicting operation id is refused
  (`crates/workloop-executor/src/service.rs`).
- Identity before effect: operation records commit before dispatch, start
  intent before spawn, stop intent before signal, acknowledgement tombstone
  before body reclaim.
- The staged runtime credential is written mode-0600 from creation
  (private tmp + rename), bound to `launch_config_digest`, at most 4096
  bytes, only while the record is `prepared`; the start gate refuses
  `runtime_auth_mount` without a staged file; the file is removed after
  stop proof and at acknowledge reclaim
  (`crates/workloop-executor/src/runner.rs`,
  `crates/workloop-executor/src/state.rs`; audit v0.3 F3 records
  `runtime-auth.tmp-partial` residue surviving both removal points, and
  F5 records an abandoned `prepared` request having no removal path —
  tracked defects).
- Worker environment is an allowlist, never inheritance: `env_clear` at
  spawn plus `ENV_PASSTHROUGH`/`SELECTOR_ENV_PASSTHROUGH` containing no
  credential-bearing names (`crates/workloop-exec/src/supervisor.rs`,
  `crates/workloop-exec/src/bounded.rs`).
- Contained launches unshare user/pid/ipc/net, bind system dirs read-only,
  tmpfs `/tmp` `/run` `/home`, and shadow masked roots/files; a mount plan
  that cannot be shadowed refuses before launch
  (`crates/workloop-exec/src/containment.rs`).
- The isolation-profile registry is closed — an unapproved profile id fails
  closed at config validation and at prepare
  (`crates/workloop-core/src/worker.rs`,
  `crates/workloop-executor/src/service.rs`).
- Worker output is read via `symlink_metadata` plus regular-file check and
  byte cap; `copy_tree` refuses symlinks and unsafe paths — a planted
  symlink, FIFO, or device node is a typed refusal, never a follow.
- Deadline enforcement is monotonic and conservative: measured clock
  offset, uncertainty, and one-way delay are subtracted — skew can consume
  authorized runtime, never extend it. The sweep runs on the endpoint's
  clock independent of coordinator liveness (audit v0.3 F6 records one
  corrupt binding record silently disabling the sweep, and F8 records a
  service restart orphaning enforcement for still-running workers —
  tracked defects).
- Declared capabilities are honest: contained capabilities are only
  advertised when the configured profile carries a sandbox primitive;
  a `read_only` workspace grant without enforceable containment refuses at
  prepare (audit v0.3 F9 records `runtime_auth_delivery` declared
  unconditionally — a tracked defect against this honesty rule).
- Errors name typed classes (`credential.missing`/`unresolvable`,
  `capability_missing`, `incarnation_mismatch`, …) and never carry secret
  material.
- Sealed state records verify a tagged digest on read — corruption is a
  `Corrupt` error, never a silent absence
  (`crates/workloop-executor/src/state.rs`).

## Out of scope

v0.3 deliberately does not defend against the following. Do not raise them
as findings in a security review; where relevant, name them as accepted
risks.

- Malicious code running inside the Worker beyond what containment already
  bounds — sandbox escapes (bubblewrap/kernel vulnerabilities) are an
  upstream problem, and the fixture profile
  (`fixture-worker-v0.1`, `sandbox_primitive: None`) provides no isolation
  by design.
- Vulnerabilities in the spawned tools themselves: `git`, `gh`, the worker
  runtime binaries.
- Transport confidentiality/integrity below the framed channel — the
  session layer assumes a private transport (SSH forced-command bridge or
  a local socket); the MAC authenticates, it does not encrypt.
- A same-UID attacker who can write the endpoint's state root or bootstrap
  credential files — sealed records detect corruption, not a hostile local
  writer; `runtime-auth` and the state tree rely on the host account
  boundary.
- Multi-tenant endpoint hosts and ambient-environment attacks on the
  supervisor (hostile `GH_CONFIG_DIR`, proxies, `PATH`) — the operator
  environment is trusted input.
- Resource exhaustion inside declared per-Invocation limits, and
  platform-level denial of service.

## Security review

Repo-wide audits run the checklist at `docs/security/audit-checklist.md`
read-only against a pinned commit; reports persist verbatim as
`docs/security/audit-v*.md`. Per-change review follows the trigger policy
in `docs/security/review-policy.md`; records for changes that merit one
land in `docs/security/reviews/`. Vulnerability reports go through the
private advisory flow in `SECURITY.md`.
