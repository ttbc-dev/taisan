# Draft release notes — taisan (release version pending; 0.3.0 proposed)

Status: **draft release materials, pending the frozen candidate**
(issue #370). The filename follows the v0.3 milestone unit, not a
decided release version — the owner still chooses between `0.3.0`
(proposed) and `1.0.0` per
[release-versioning.md](release-versioning.md) §6, and the final
title/version line is stamped at that decision. These notes are
prepared from pre-freeze source materials only. The final candidate
is not yet frozen — per #370's sequencing it stays blocked on the
release-acceptance (#365), clean-install reproduction (#367),
public-content audit (#368), and license (#369) gates — so no commit
named here is the release candidate, no tag exists, and every TODO
below is discharged at freeze. The acceptance gate (#365) itself
consumes two upstream evidence issues: #270, whose manual-acceptance
runs already produced the owner-side records summarized in
[release-acceptance-v0.3.md](release-acceptance-v0.3.md) (with the
residual gaps that record lists), and #271, whose scheduled-pilot
evidence does not yet exist. Claims marked **TODO** depend either on
that unproduced #271 evidence or on exact values held in owner-side
records the freeze will copy over; nothing else in this document
asserts behavior that is not already verifiable in this repository or
in the recorded owner-run evidence cited inline. Per `AGENTS.md`,
private source repositories and the live-run target are described by
role only; issue/PR numbers and revision SHAs are owner-verifiable
evidence locators.

## What this release is

taisan is an adapter-based work orchestration loop for coordinating
development work on GitHub repositories: a human chooses the work and
supplies intent and constraints; taisan coordinates implementation,
pull requests, review, bounded fixes, and the points where a human
needs to intervene. This release ships the single-host, CLI-operated
workflow validated under the v0.3 unit — see
[README](../README.md) for the model and the scope table.

## Implemented workflow (what the notes may claim)

The workflow evidenced on the candidate line — fixture/contract tests
plus the owner-run live records summarized in
[release-acceptance-v0.3.md](release-acceptance-v0.3.md):

1. **Manual intake** — `taisan submit` admits one durable Work bound
   to a real Issue with constraints and verification argv;
   idempotent replay returns the recorded decision, never a second
   Work.
2. **Coding** — one Invocation per attempt through the executable
   `taisan.worker.declaration/1` / `taisan.worker.request/2` /
   `taisan.worker.report/2` wire: immutable digest-bound request
   materialized before launch, bounded budget, per-Invocation out
   directory.
3. **Independent verification** — the supervisor re-runs the declared
   argv against the exact produced commit in a detached worktree; a
   Worker-claimed check never satisfies a gate.
4. **Publication** — after handoff, verification, and capability
   evidence (`supported` + `authorized`), the in-process GitHub host
   driver applies `push_ref` then `open_change_request` as recorded
   intents with receipts.
5. **Real review finding → judgment** — a human-posted review finding
   on the change request enters as a real review occurrence; a
   recorded `ReviewRound`/`RoundJudgment` binds the exact target head.
6. **Bounded Fix** — a Fix Invocation on the accepted head carrying
   predecessor and finding references produces a new commit.
7. **Republish and fresh reassessment** — the new head is verified,
   re-reviewed, published, and `assess_completion` records the
   terminal Outcome — with no unresolved writer, gate, or effect.
8. **Operator surface** — `work-detail`, `work-list`, `attention`,
   `capacity`, `anomalies`, `evidence` project the durable chain;
   Attention items are evidence-backed and resolve on evidence, not
   timers.
9. **Stop/restart recovery** — `taisan run` is a foreground loop;
   durable identities survive restart, retries stay bounded by
   durable counters, and writers not proven absent keep their
   reservations. The reservation guarantee has an open edge case: a
   chdir+argv-change escapee can be mis-proved absent, enabling a
   second writer on resume — recorded as the release-blocking #344
   absence-proof defect (see known limitations).

Also shipped and contract-tested: the internal intake scheduler
(v0.2) and its Selector port (**live-exercised claim: none — see
known limitations**), the `taisan-tui` console (deferred from release
requirements), and the agent-agnostic skill at
`skills/taisan/SKILL.md` with its [walkthrough](skill-walkthrough.md).

## Tested Worker / Selector / runtime identities

Only public reference adapters are named; every other implementation
is an owner-supplied swap behind the same ports
([reference adapters](reference-adapters.md)).

| Component | Identity | Evidence class |
| --- | --- | --- |
| Worker adapter | `taisan-worker-claude` (`crates/taisan-worker-claude`) — argv-v2 transport driving the **Claude Code CLI** (`claude`) headless as the coding-agent runtime; declaration `runtime.product: "claude-code"` | Owner-run live for `coding`, `fix`, and `invariant_check` Invocations under `taisan-reference-v0.1`; the `review` role is declared and contract-tested — the live review finding was operator-posted, not Worker-produced |
| Selector adapter | `public-issue-selector` (`crates/workloop-selector`) — stateless stdin/stdout Selector protocol; ambient `gh` auth, declared `--github-endpoint` scope | Contract tests + interchange evidence; scheduled-intake live path unexercised |
| Host adapter | in-process `github` driver inside `taisan run` (`workloop-github`), real reads/writes via the `gh` CLI transport | Owner-run live (push, PR open, review reads, capability reports) |
| Fixture Workers | `workloop-fixture-worker`, `workloop-contract-worker` (independent second implementation of the same wire) | Contract/fixture evidence only — never live claims |

Exact runtime identity: the live runs' `describe` declaration binds
`adapter_version` and `runtime.version`; the exercised runtime
required the declared `--runtime-arg --verbose` knob for stream-json
mode (recorded observation, release-acceptance §7). **TODO:** copy the
exact `adapter`/`adapter_version`/`runtime.product`/`runtime.version`
values from the retained owner-run `describe` captures at freeze time.

## Platform and installation requirements

- **One Linux host.** Containment (the `taisan-reference-v0.1`
  profile) requires bubblewrap namespaces; the recorded live
  environment was Ubuntu Noble arm64 with bubblewrap 0.9.0, hosted as
  an on-demand Linux guest on a single macOS machine. macOS cannot
  host containment natively. `taisan check` fails closed at
  `isolation_primitives` when `bwrap` is absent.
- **Rust toolchain** supporting edition 2024 for source builds
  (`cargo build --workspace`; `cargo install --path
  crates/workloop-cli --bin taisan`). The packaged binaries are
  `taisan`, `taisan-tui`, `public-issue-selector`, and
  `taisan-worker-claude` ([licensing inventory](licensing-prep.md) §2).
- **`gh` CLI** for the GitHub host driver and the Selector's live
  source reads — a current release is required: a distribution-default
  `gh` 2.45.0 lacked `gh api --slurp` and broke adapter list calls;
  upgrading to `gh` 2.105.0 unblocked the run (issue #422, recorded
  environment requirement).
- **A real conforming Worker**: the reference `taisan-worker-claude`
  plus its installed runtime (Claude Code CLI) and a runtime
  credential delivered by reference only. Any executable implementing
  the `/2` wire surface is an equivalent conforming Worker.
- **Setup**: follow [v0.3 installation and operation](v0.3-installation.md)
  (profile `config.toml` + installation record, `init` → `check` →
  record → `run`). **Migration**: the repository and binaries were
  renamed from the pre-taisan names — `git remote set-url` and command
  substitutions are listed in [name migration](name-migration.md);
  state paths (`workloop.db`, `workloop.config.json`) are unchanged.

## Verification record

Gates run on `origin/main` HEAD **before** this draft landed (the
draft adds docs only; the recorded commit is the tested commit):

```text
git rev-parse HEAD
119dcb72ce047bdcc4c44fb4a7915ffe2d7fd6ee
git log -1 --format='%h %s' 119dcb72ce047bdcc4c44fb4a7915ffe2d7fd6ee
119dcb7 Merge pull request #435 from shimpeiws/issue-376-agent
```

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Passed — 1753 tests, 0 failures, 6 ignored (the `u36e_live` cases requiring `TAISAN_U36E_*` credentials and owner authorization; fixture-selector/fixture-worker contract tests ran against `target/debug/` binaries produced by the build gate) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |

Build precedes test because contract tests spawn
`target/debug/workloop-fixture-worker`. **TODO at freeze:** re-run all
four gates on the recorded candidate commit and replace this table —
a freeze never inherits a check record from a different commit. The
supported build/install route is the source-build path above; a
cargo-dist binary route is wired per #441
([distribution](distribution.md)) but produces artifacts only once a
tag is cut. **TODO:** record packaged-artifact checksums here once a
release produces them.

## Known limitations

- **Single Linux host** — no multi-host or distributed execution;
  containment needs bubblewrap namespaces.
- **No remote execution, no remote review** — designed (#372/#373
  docs) but not implemented; follow-up, not part of this release.
- **No automatic merge or deploy** — publication ends at a reviewed
  change request; merge and deploy remain human acts.
- **Scheduled intake unexercised live** — the v0.2 scheduler path is
  fixture/contract-tested only; no scheduled live run exists on the
  candidate (#271 open). **TODO:** remove this limitation only if the
  #271 evidence lands before freeze; otherwise it ships as a
  limitation.
- **Release-blocking defect dispositions open** — #344 (absence proof
  edge cases) and #364 (staged runtime-auth cleanup) are assessed
  gating defects in release-acceptance §6; the freeze dispositions
  them, this draft does not.
- **Deferred residuals** — #363 (spent-deadline check order), #422
  (`gh` version preflight), and the #359 deferred edges are recorded
  open issues with bounded consequences.
- **Partial restart/recovery evidence** — the one store-recovery
  attempt was internally inconsistent; the run-1 publish leg used
  recorded operator workarounds (the clean path is proven by run 3).
- **Cost/quality** — nothing in this release claims model cost or
  quality superiority; the pilot runs were single-operator and small.
- **License unselected** — redistribution terms are pending the #369
  owner decision; see [licensing prep](licensing-prep.md).
- **Publication prerequisites** — public-content/history/metadata
  audit (#368) and the disclosure review covering
  `docs/source-workflow-audit.md` and `docs/release-audit.md` gate
  publication, not these notes.

## Consistency boundary

These notes assert only what README, the v0.3 docs, and the
walkthrough outline ([release-walkthrough-v0.3.md](release-walkthrough-v0.3.md))
already claim together: no live evidence beyond the cited owner-run
records, no remote review/execution in this release, no superiority
inference from a small pilot. A claim that drifts from that boundary
before freeze is corrected here, not in each doc separately.
