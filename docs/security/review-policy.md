# Security review policy

This policy defines when taisan code changes require a security review, at
what depth, and where the results are recorded. It complements `SECURITY.md`
(external disclosure) — this document governs internal review.

## Automated scanning

| Layer | Coverage | Enforcement |
| --- | --- | --- |
| `rust.yml` CI | `cargo fmt --check`, `cargo build --workspace`, `cargo test --workspace` (including the `workloop-executor` contract suite under bwrap), `cargo clippy --workspace --all-targets -- -D warnings` | every push and PR |
| Dependency scanning | Dependabot version updates weekly for `cargo` and `github-actions` (`.github/dependabot.yml`, 7-day cooldown) + Dependabot security updates (repo setting); no `cargo audit`/`cargo deny` lockfile vetting | weekly + advisory-driven |
| Secret scanning | GitHub secret scanning with push protection (repo setting); no committed in-CI scanner (Gitleaks/TruffleHog) | every push |

The remaining gaps mean credential-adjacent changes still lean on the
review layers below — Dependabot covers scheduled and advisory bumps,
but nothing runs `cargo audit`-style vetting of the lockfile in CI, and
the two GitHub-side scanners are repo settings, not committed
configuration. If a scanner lands in the tree later, this table is where
it is announced — the trigger tiers stay unchanged.

## Review layers

Two layers, matching the two shapes of change this repo sees:

1. **Repo-wide audit** — a periodic or pre-release pass over the whole trust
   boundary using `audit-checklist.md`. Reports persist verbatim as
   `docs/security/audit-v*.md`.
2. **Per-change review** — a scoped review of a single branch's diff against
   the same checklist, restricted to the surfaces the diff touches. Records
   land in `docs/security/reviews/` when the change merits one.

## Review required

A change MUST get a per-change security review before merge when the diff
touches a trust-boundary surface:

- `crates/workloop-core/src/credential.rs` — the `credential_ref` grammar
  and its refusal vocabulary.
- `crates/workloop-core/src/executor.rs` — envelope formats, bounds,
  digests, and binding checks for the transport contract.
- `crates/workloop-core/src/worker.rs` — the isolation-profile registry.
- `crates/workloop-exec/src/credentials.rs`, `containment.rs`,
  `isolation.rs`, `boundary.rs`, `supervisor.rs`, `bounded.rs`, `egress.rs`
  — provider resolution, mount plans, environment construction, launch,
  and the egress proxy.
- `crates/workloop-executor/src/auth.rs`, `channel.rs`, `config.rs`,
  `service.rs`, `runner.rs`, `state.rs`, `adapter.rs` — session auth,
  framed channel, bootstrap credentials, operation dispatch, staging and
  lifecycle, durable state, and the coordinator-side adapter.
- `crates/workloop-github/src/credential.rs`, `gh_cli.rs` — token
  resolution and the `gh`/`git` transports that carry it.
- `docs/executor-transport-contract.md` §2–§8,
  `docs/installation-config.md`, `docs/worker-protocol.md`,
  `docs/remote-executor-endpoint.md` — normative security text; a wording
  change that weakens a guarantee needs the same scrutiny as a code change.
- Any change that adds a mount destination, an env passthrough name, a new
  `out/` whitelist member, a new frame format, or a new file read/write
  path over Worker-visible or credential-adjacent trees.

## Review recommended

Not blocking, but worth a scoped pass when convenient:

- Changes to `crates/workloop-store` (durable identity/receipt storage),
  `crates/workloop-cli` intake/evidence paths, or `scripts/` (owner-side
  credential tooling).
- Dependency bumps in `Cargo.lock` for `sha2`, `serde_json`, `rusqlite`,
  `uuid`, or anything on a credential or parsing path.
- Release tags — run the repo-wide audit layer instead.

## Review not required

- Tests, fixtures, and contract tests that don't change production
  behavior.
- Docs outside the normative sections listed above.
- Refactors inside a single reviewed surface that preserve behavior
  (verify with the existing test suite; note the diff boundary in the PR).

## Scope of a review

Review against the invariants in `AGENTS.md` and the checklist in
`audit-checklist.md`. In particular:

- Confirm which claimed invariants the diff could weaken, and verify them
  at the settling code, not at the call site.
- Distinguish real findings from declared non-goals — `AGENTS.md`
  "Out of scope" lists what is deliberately unguarded (e.g. sandbox
  escapes, hostile same-UID state-root writers).
- A change that *relaxes* a check needs a concrete justification; a change
  that *adds* a check is reviewed for fail-closed behavior on its own
  error paths.
- A change that denies rather than implements a guarantee is usually
  safer — prefer refusal to best-effort.

## Record-keeping

- Every finding in a review is either resolved in the same change or
  explicitly waived with a reason; neither outcome is silent.
- Changes that trigger "review required" or that produce any finding keep
  a short record at `docs/security/reviews/<slug>-<date>.md`: scope,
  verdict, findings and their dispositions.
- Repo-wide audit reports persist verbatim at `docs/security/audit-v*.md`;
  do not edit a published audit — corrections go in the next audit.

## Running it

This repo keeps no agent-definition directory (`.claude/`, `.agents/` etc.);
the auditor is a documented command over committed checklists rather than a
framework artifact.

- **Repo-wide audit**: run a read-only agent (or a human reviewer) against
  `docs/security/audit-checklist.md` at a pinned commit, e.g. hand the
  agent this brief: "run the security audit at
  `docs/security/audit-checklist.md` on commit `<sha>`; write the report
  to `docs/security/audit-v0.3.md`". The checklist file IS the auditor
  definition — scope, surfaces, method, report shape.
- **Per-change review**: on the branch, review the pending diff against
  the checklist's surfaces and the `AGENTS.md` invariants. Every claim
  must cite a real `file:line` actually read; every finding needs a
  concrete failure scenario. Record the verdict in
  `docs/security/reviews/` when the diff is in the required tier or any
  finding survives.
