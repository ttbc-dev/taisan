# Draft release notes — taisan (release version pending; 0.3.0 proposed)

Status: **draft release materials, pending the frozen candidate**
(issue #370). The filename follows the v0.3 milestone unit, not a
decided release version — the owner still chooses between `0.3.0`
(proposed) and `1.0.0` per
[release-versioning.md](release-versioning.md) §7, and the final
title/version line is stamped at that decision. These notes are
prepared from pre-freeze source materials only. The final candidate
is not yet frozen — no commit named here is the release candidate and
no tag exists — so every **TODO at freeze** below is discharged at
freeze, on the recorded commit. The upstream evidence these notes
depended on is now in hand: the #270 manual real-Worker acceptance
(runs 1–6, owner-approved at comment `7171973494`), the #271
scheduled-intake pilot, and the #367 clean-Linux-install reproduction
are complete and closed; the #368 public-content audit is
dispositioned by the #438 owner ruling (in-place publication, history
kept); and the #369 license is applied (MIT OR Apache-2.0). The
acceptance issue #365 stays open until the owner dispositions the
flagged defects — none of those dispositions is made here. The
record's fold-in of the pilot/clean evidence lands via PR #472 (open
at writing); the run records cited here are the retained owner-side
evidence, not that PR. Per `AGENTS.md`, the live-run
target is the private isolated repository
`shimpeiws/coding-worker-smoke` — retained verbatim as an
owner-only reference (links and `#N` citations resolve only for the
owner); other private sources are described by role only. Issue/PR
numbers, revision SHAs, and the owner-side evidence locators cited
inline (`RUN-RECORD-*.md`, `acceptance-v03-*/`) are owner-verifiable
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
   reservations. The #344 absence-proof defect (a chdir+argv-change
   escapee mis-proved absent) is fixed on the candidate by PR #428 —
   launch-group clearance plus bounded `absence_unprovable`
   escalation; the residual ambiguous-match convergence gap is #313
   (see known limitations). Restart-clean recovery is evidenced by
   the pilot's two real restarts (below).

10. **Scheduled admission** — exercised live by the authorized #271
   pilot on the clean #367 install (driver built from `81c62cf`;
   owner-side records `acceptance-v03-pilot/RUN-RECORD-2026-11-26-PILOT.md`
   and `acceptance-v03-clean/RUN-RECORD-2026-11-25-CLEAN.md`):
   **7 Works admitted across
   scheduled ticks** (intake-2, -3, -4, -13, -14, -16) — every
   dispatch decision made by the scheduler, none by manual `submit`.
   Two Works ran the full verified path to `succeeded`: **w-6**
   (issue #74 → PR #77 on the isolated repository) and **w-7**
   (issue #75 → PR #78) — coding → independent verification pass →
   invariant check → publish → sealed bound-actor review round →
   assessment `complete:true` → `succeeded`. The other five reached
   honest terminals without claiming success (operator cancels after
   guarded publish refusals and a stuck lane — #469/#470 below).
   Scheduler scenarios exercised with durable rows: `queue_full`
   skips (intake-5..9), replayed candidates, selector outage
   (`exchange_launch` on scheduled and manual intake), a planned
   SIGINT restart (in-flight `inv-4` re-minted as `inv-5`), an
   **unplanned guest VM restart** (orphaned `running` `inv-11`
   re-minted as `inv-12`, `scheduled_startup` intake resumed — also
   the restart-clean store-recovery evidence), and disable →
   `scheduled_reenable` (intake-13). `issue_claims` held single
   ownership of every selected Issue; no Work was dispatched twice.

Also shipped and exercised: the internal intake scheduler (v0.2) and
its Selector port (live-exercised by the pilot above, at small pilot
scale — see known limitations), the `taisan-tui` console (deferred
from release requirements), and the agent-agnostic skill at
`skills/taisan/SKILL.md` with its [walkthrough](skill-walkthrough.md).

## Tested Worker / Selector / runtime identities

Only public reference adapters are named; every other implementation
is an owner-supplied swap behind the same ports
([reference adapters](reference-adapters.md)).

| Component | Identity | Evidence class |
| --- | --- | --- |
| Worker adapter | `taisan-worker-claude` (`crates/taisan-worker-claude`) — argv-v2 transport driving the **Claude Code CLI** (`claude`) headless as the coding-agent runtime; declaration `runtime.product: "claude-code"` | Owner-run live for `coding`, `fix`, and `invariant_check` Invocations under `taisan-reference-v0.1`; the `review` role is declared and contract-tested — the live review finding was operator-posted, not Worker-produced |
| Selector adapter | `public-issue-selector` (`crates/workloop-selector`) — stateless stdin/stdout Selector protocol; ambient `gh` auth, declared `--github-endpoint` scope | Contract tests + interchange evidence; scheduled-intake live path exercised by the #271 pilot (admission of all 7 Works, `select` driven by scheduled ticks) |
| Host adapter | in-process `github` driver inside `taisan run` (`workloop-github`), real reads/writes via the `gh` CLI transport | Owner-run live (push, PR open, review reads, capability reports) |
| Fixture Workers | `workloop-fixture-worker`, `workloop-contract-worker` (independent second implementation of the same wire) | Contract/fixture evidence only — never live claims |

Exact runtime identity, from the retained owner-run `describe`
captures (`describe-run5.json` in `acceptance-v03-run5/`; identical
values in the pilot install): `adapter: "taisan-worker-claude"`,
`adapter_version: "0.0.0"` (pre-version-bump — the §4 metadata
alignment in
[release-versioning.md](release-versioning.md) makes released
binaries self-report the release version), `runtime.product:
"claude-code"`, `runtime.version: "2.1.331 (Claude Code)"`,
`protocol_versions: ["taisan.worker.request/2",
"taisan.worker.report/2"]`, roles `coding`, `fix`,
`invariant_check`, `review`. The exercised runtime required the
declared `--runtime-arg --verbose` knob for stream-json mode
(recorded observation, release-acceptance §7).

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
a freeze never inherits a check record from a different commit.

**Distribution route (owner-decided 2026-11-25, issue #370):**
prebuilt binaries via **cargo-dist** on GitHub Releases — matrix
build, per-artifact checksums, shell/PowerShell installers,
generated release notes — with the same cargo-dist setup feeding a
**personal Homebrew tap** (`brew tap` against the owner's tap;
homebrew-core deferred until notability) and **cargo-binstall**
metadata (`cargo binstall` fetches the prebuilt binary). crates.io is
not a channel: taisan is a CLI tool, not a library. The dist
workflow itself is #441's lane. **TODO at release:** record the
packaged-artifact checksums cargo-dist produces here; the
source-build path above remains supported independently.

## Known limitations

- **Single Linux host** — no multi-host or distributed execution;
  containment needs bubblewrap namespaces.
- **Remote execution shipped but unexercised** — the remote executor
  surface (contract #374, wire types/bindings #375, reference
  endpoint #377, bounded transfer/collection #376) is on the
  candidate as v0.4 groundwork, with one open defect (#437:
  zero-byte required artifact reported `Present` where local reports
  `Missing`). No end-to-end remote run exists; remote execution and
  remote review are follow-ups, not part of this release.
- **No automatic merge or deploy** — publication ends at a reviewed
  change request; merge and deploy remain human acts.
- **Scheduled intake exercised at pilot scale only** — the #271
  pilot admitted 7 Works across scheduled ticks with 2 verified
  `succeeded` completions and 5 honest operator-cancelled terminals;
  this is a tiny-pilot sample on one installation, not a
  capacity/robustness claim.
- **Pilot-discovered defects, dispositions pending owner call** —
  surfaced by the #271 pilot, filed and unfixed at record time:
  - **#468** — `taisan init` emits the numeric repository id, which
    the `public-issue-selector` GitHub backend cannot use; scheduled
    intake fails closed (`adapter_source_unavailable`) on the first
    tick. Workaround: set the `id` in `config.toml` to the
    `owner/name` slug by hand (what the pilot did).
  - **#469** — a `caches/*` entry that is a symlink escaping the
    sandbox root dangles inside containment: every verification
    reports `cache_unreadable` and the assessment item stays
    `inconclusive` with no designed re-drive path (a `waiting/ci`
    execution can only be cancelled). Workaround: replace the
    symlink with a real directory.
  - **#470** — publish branches are named `work/w-N`
    deterministically, so repeated installs against the same
    repository collide with earlier `work/w-N` branches: publish
    refuses `stale_head` (correctly — it never overwrites a foreign
    head) and the Work needs an operator cancel. 5 of 7 pilot Works
    hit this.
  - **#471** — replayed candidates consume the selector's
    `max_candidates` budget each tick, so a permanently-replayed
    issue can starve new proposals. Workaround used in the pilot:
    raise `pending_queue_limit`.
- **Earlier flagged defects, dispositions pending owner call** —
  recorded open in release-acceptance §6: #313 (two unrecorded
  same-signature processes can hold an unidentified launch
  unresolved — honest-uncertain wedge, not a false absence), #398
  and #399 (`file:` credential sources can stay readable inside
  contained verification — same credential-hygiene class #364 gated
  on), #400 (v2 `report.json` full-body retention dropped by a merge
  resolution — bounded evidence survives via the CAS prefix, the
  full report does not).
- **Fixed defects worth noting** — #344 (absence proof), #363
  (spent-deadline ordering), #364 (staged runtime-auth cleanup),
  #447–#450 (refusal budget burn, judged-role resume, runtime
  residue, divergent-head republish), and #466 (fast-exiting output
  flood misclassified `success` — fixed by PR #467) are repaired on
  the candidate line; #419 (`artifact.json` collision) has repair PR
  #431 open.
- **Open operator-facing defects** — recorded in release-acceptance
  §7 and unfixed: #453 (cancel does not withdraw pending outbox
  intents — a cancelled Execution whose intent never reconciles parks
  permanently at `stopping`; run 4 needed a manual claim/slot
  release), #454 (capability reports refresh only on
  unscoped passes — a `run --work`-scoped loop can deadlock
  publication until one unscoped pass runs), #455 (a leftover remote
  `work/w-N` head kills a Work's publish path via transport-budget
  exhaustion), #456 (a Worker report of
  `unavailable`/`provider_unavailable` at exit 0 is lane-terminal —
  no bounded re-request).
- **First-use rough edges, open** — #401 (`check` passes
  `worker_runtime`/`isolation_primitives` vacuously when `init`
  scaffolds no `[worker]` section), #402 (containment-probe refusal
  records only "exited non-zero" — no stderr detail), #403 (no
  `taisan --version`), #422 (preflight does not probe `gh`
  version/features — a distribution-default `gh` 2.45.0 broke
  adapter list calls; the recorded environment ran `gh` 2.105.0).
- **Deferred residuals** — the #359 retained-body edges, #353
  (fixture-worker test orphans), #409 (#405 follow-ups), and
  #302/#306/#139 are recorded open issues with bounded consequences;
  see release-acceptance §6 for the complete ledger.
- **Run-1 publish leg used operator workarounds** — the clean
  publish path is proven by run 3 on post-#421 code (and twice more
  by the pilot), not by run 1's H1.
- **Cost/quality** — nothing in this release claims model cost or
  quality superiority; the runs were single-operator and small.
- **License** — MIT OR Apache-2.0 (owner-selected under #369,
  applied by PR #464); `assets/taisan-hero.jpg` is excepted.
- **Publication posture** — the #368 audit is dispositioned by the
  #438 owner ruling: in-place publication, history kept, private
  names acceptable to disclose; private-repository references in
  this tree are marked owner-only rather than removed.

## Consistency boundary

These notes assert only what README, the v0.3 docs, and the
retained-run walkthrough
([release-walkthrough-v0.3.md](release-walkthrough-v0.3.md))
already claim together: no live evidence beyond the cited owner-run
records, no remote review/execution in this release, no superiority
inference from a small pilot. A claim that drifts from that boundary
before freeze is corrected here, not in each doc separately.
