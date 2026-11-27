# Security audit checklist

The repo-wide security auditor for taisan. Run this checklist read-only
against a pinned commit and persist the report verbatim as
`docs/security/audit-v<version>.md`. For per-change review, restrict the
same pass to the diff's touched surfaces per `review-policy.md`.

The auditor does not modify code, run the Workers, or execute credential
providers. Its job is to verify that the claimed invariants hold at the
settling code, and to find places where they don't.

## Step 1 — load the claimed invariants and non-goals first

Read, before touching any code:

- `AGENTS.md` — Trust boundary, Invariants, Out of scope.
- `docs/executor-transport-contract.md` — §2 identities, §3 handshake,
  §4 wire envelope, §7 credential/enforcement boundary, §8 deadlines.
- `docs/installation-config.md` — the `credential_ref` provider grammar.
- `docs/worker-protocol.md`, `docs/remote-executor-endpoint.md` — worker
  and endpoint surfaces.
- `docs/identity-storage.md`, `docs/u3.6e-authorization.md` — identity
  and authorization claims.
- The previous audit report, if one exists — findings must be re-verified
  against the new commit, not carried forward.

The invariants are the false-positive filter: a behavior matching a
declared non-goal is not a finding; a behavior violating a claimed
invariant always is.

## Step 2 — audit each surface

For every surface, verify the check at the settling line — where the
decision is enforced, not where it is declared. Cite `file:line` for every
claim.

### A. Credential reference grammar and resolution

- `crates/workloop-core/src/credential.rs` — parse accepts only the
  closed `env:`/`file:`/`command:` grammar; parse and provider refusals
  never echo the reference text.
- `crates/workloop-exec/src/credentials.rs` — `env:` unset/empty/oversize;
  `file:` absolute, regular, euid-owned, `mode & 0o077 == 0`, symlink
  resolved before checks; `command:` no shell, bounded wall clock,
  process-group kill, bounded stdout; `ResolvedCredential` Debug redacted,
  no Display; provider-config scan bounds (depth, file count, size).
- `crates/workloop-github/src/credential.rs` — the same provider rules
  on the host-adapter side; compare hardening parity with the exec side
  (timeout, output bound, stderr handling, path echoed in errors).
- Failure classes: `credential.missing` (slot undefined) vs
  `credential.unresolvable` (provider failed) — never fabricated, never
  conflated.

### B. Session transport and authentication

- `crates/workloop-executor/src/channel.rs` — 4-byte length prefix gated
  on class bound before allocation; handshake sequence and refusal
  frames; coordinator nonce echo verified.
- `crates/workloop-executor/src/auth.rs` — directional MAC tags,
  `mac_eq` constant-time compare, fresh nonces, canonical-JSON framing,
  closed member sets.
- `crates/workloop-core/src/executor.rs` — `binding_violations` on
  hello-reply and command paths; digest verification over the raw body
  minus the covered member; hex-shape checks before any slicing.
- `crates/workloop-executor/src/service.rs` — `serve_unix`/`serve_stream`:
  socket handling, handshake ordering, per-connection threads, frame
  dispatch, replay fence, `bound_request` identity checks.
- `crates/workloop-executor/src/adapter.rs` — coordinator side: hello
  echo-binding, receipt verification against the issued command.

### C. Bootstrap credentials and endpoint configuration

- `crates/workloop-executor/src/config.rs` — `read_credential` checks
  (size bound only? ownership? mode?); `Debug` derives on structs holding
  credential `Vec<u8>` fields; `CoordinatorEndpoint` handling.
- `crates/workloop-executor/src/bin/` — binary entrypoints, `connect`
  bridge behavior.

### D. Runtime credential lifecycle (contract §7)

- `crates/workloop-executor/src/runner.rs` — `stage_credential`
  (prepared-state gate, launch-config-digest binding, 4 KiB bound),
  `has_credential` start gate, `release_credential` at stop proof;
  every code path that creates, reads, or removes `runtime-auth` — and
  its `*.tmp-partial` sibling.
- `crates/workloop-executor/src/state.rs` — `write_atomic_private`
  (mode 0600 at creation, pre-existing tmp clamp, fsync+rename);
  `reclaim_bodies` and `clear_torn_prepare` coverage of every
  credential-bearing filename.
- Retention: what removes a staged credential when a prepared request is
  never started, never stopped, never acknowledged, or the incarnation
  restarts?

### E. Worker containment and launch

- `crates/workloop-exec/src/containment.rs` — unshare set, read-only
  system binds, tmpfs, `mask_roots`/`masked_files` shadowing,
  `plan_denial` for unshadowable destinations.
- `crates/workloop-exec/src/isolation.rs`, `supervisor.rs` — WorkerEnv
  construction (built, not inherited), `env_clear` at spawn, boundary
  grant → `runtime_auth` wiring, uncontained gate arms.
- `crates/workloop-executor/src/runner.rs` — the `Gate::Uncontained`
  arm: which parts of `launch_config` are silently ignored
  (`runtime_auth_mount`, workspace grant), and whether
  `runtime_auth_delivery` is still declared in capabilities.
- `crates/workloop-core/src/worker.rs` — registry is closed; unapproved
  profile id fails closed at both validation and launch.

### F. Worker output and collection (untrusted read-back)

- `crates/workloop-executor/src/runner.rs` — `OUT_WHITELIST` membership,
  `read_bounded_file` (`symlink_metadata`, regular-file refusal, byte
  cap), `copy_tree` symlink/unsafe-path refusal, `RemoteHandoffV1`
  bound+strict+closed fields, `path_is_valid` grammar.
- `bundle_objects`/`run_git`/`git_env` — `git` invoked inside a
  Worker-controlled repository: which git config surface is still read
  (repo-local `.git/config`, `include.path`, `core.fsmonitor`,
  `core.hooksPath`, filter drivers) despite `GIT_CONFIG_NOSYSTEM`.
- `crates/workloop-executor/src/service.rs` — collect authorization via
  the stop-proof receipt; acknowledge tombstone before reclaim.
- Inline collection fits the receipt bound (`INLINE_COLLECT_BUDGET`).

### G. Deadlines, lifecycle, and durable state

- `crates/workloop-executor/src/runner.rs` — `record_clock_offset`
  clamping, `local_deadline_ms` conservative edge, `enforce_deadlines`
  sweep: error handling on `requests()`, incarnation filtering,
  escalation path.
- `crates/workloop-executor/src/state.rs` — `request_dir`/`op_path`
  sanitize+tag, seal/verify behavior on corrupt records, `requests()`
  error propagation.
- Restart behavior: records bound to a previous incarnation — who
  enforces their deadlines, who can stop their processes, who reclaims
  their staged credentials?
- `crates/workloop-store/` — SQL construction, parameterization.

### H. Host-adapter credential transport

- `crates/workloop-github/src/gh_cli.rs` — `GH_TOKEN` via env only,
  never argv; `run_plain`/`api`/`api_as` env handling; `scrubbed_git_env`
  allowlist; `push_authorization` header-safety checks.
- `crates/workloop-github/src/credential.rs` — provider parity (see A).
- `scripts/` — owner-side credential tooling (e.g. `mint-app-token.py`):
  secrets to stdout only, never to files/argv/logs.

### I. Cross-cutting

- No `Debug`/`Display` impl can print a secret on any struct that holds
  one (`ExecutorService`, `ResolvedCredential`, `CoordinatorEndpoint`).
- Every refusal/error path names a typed class and no secret material.
- New file/dir creations near credentials or staged bodies use
  `write_atomic_private`-equivalent care (creation mode, tmp cleanup).
- Provider/env/config paths that were deliberately hardened on one side
  of the codebase stay hardened on the sibling implementation — the
  dangerous drift is asymmetric, not absent, hardening.

## Step 3 — report

Write the report with this structure; persist verbatim:

1. **Scope table** — commit, branch, date, method (read-only), auditor.
2. **Audited paths** — the surfaces actually read, by crate/file.
3. **Invariants verified** — table: invariant | status
   (`upheld`/`violated`/`not-verifiable`) | settling code location.
4. **Findings** — severity-classified (CRITICAL/HIGH/MEDIUM/LOW), each
   with: title, `file:line`, the violated invariant or missing check, a
   concrete failure scenario (who does what, what leaks or what runs
   past its bound), recommended direction, and existing test coverage.
   No finding without a concrete scenario.
5. **Needs verification** — claims that could not be confirmed by static
   reading alone, with what would settle them.
6. **Accepted risks** — observations inside declared non-goals, recorded
   so the next audit doesn't re-raise them.

## Rules

- Every claim cites a real `file:line` actually read — no findings from
  memory, naming, or inference.
- No finding without a concrete failure scenario.
- Prefer few high-confidence findings over exhaustive speculation; the
  "needs verification" section exists for the rest.
- Read the settling line, not the caller. If an invariant is claimed,
  verify where it is enforced.
- Do not include real secret material in the report.
