# Release versioning and tag policy — proposal

Status: **proposal, pending owner sign-off** (issue #370). This document
proposes the version/tag policy and the procedures the release freeze
will follow. It is a draft prepared before the final candidate is
frozen — the evidence issues behind the acceptance record are complete
(#270 manual acceptance owner-approved, #271 pilot and #367 clean
install closed, #368 dispositioned by the #438 ruling, #369 license
applied), while #365 itself stays open until the owner dispositions
the flagged defects. What remains is those dispositions plus the
owner's version choice and candidate sign-off. **No tag exists, no
release exists, and no
commit named here is a release candidate.** Nothing in this document
authorizes a tag, a GitHub release, a version-metadata change, a
visibility change, or publication. The two route decisions the draft
left open are now made: the **publication route** is in-place
publication with history kept (owner ruling under issue #438,
[release audit](release-audit.md) §7 — no history rewrite, no squash)
and the **distribution route** is prebuilt binaries via cargo-dist on
GitHub Releases plus a personal Homebrew tap and cargo-binstall
metadata (owner decision on #370, 2026-11-25 — crates.io is not a
channel).

Per `AGENTS.md` no private source repository is named; private
counterparts are described by role only. Issue/PR numbers and revision
SHAs appear as owner-verifiable evidence locators.

## 1. Proposed release identity

The milestone
["Initial public release — validated v0.3"](https://github.com/ttbc-dev/taisan/milestone/8)
names the delivered unit **v0.3** — the same ordinal the design
documents use for the v0.1/v0.3 requirement slices. The proposal keeps
that ordinal visible in the public identity:

- **Version:** `0.3.0` — semver shape so that fixes on the frozen
  release can ship as `0.3.1`, `0.3.2`, … without reusing the milestone
  ordinal. Pre-1.0 semantics apply: minor bumps may change behavior;
  patch bumps are for fixes on a frozen release line only.
- **Tag:** `v0.3.0` — annotated tag (`git tag -a`) on the frozen
  candidate commit once the freeze criteria below are met.
- **Release title:** "taisan 0.3.0 — initial public release (validated
  v0.3)" or owner-adjusted equivalent; the title names the validated
  unit so release names stay consistent with the milestone and the
  v0.1–v0.3 requirement documents.

Rationale for `0.3.0` over `1.0.0`: the release's own scope statement
limits it to one Linux host, CLI operation, and the manually-driven
workflow with known limitations ([release
notes](release-notes-v0.3.md)); a 1.0.0 claim would overstate the
verified surface. The choice is the owner's; the alternative the owner
may prefer — tagging `v1.0.0` for the first public artifact — is
recorded here so the decision is explicit either way.

## 2. Tag scheme

- **Annotated, not lightweight.** The tag object carries the release
  identity, the tagger, and a message citing the frozen commit, the
  acceptance record ([release-acceptance-v0.3.md](release-acceptance-v0.3.md)
  §1), and the milestone — so `git show v0.3.0` is itself a small
  provenance record.
- **Tag target is the recorded candidate commit**, not "current main"
  at tag time. The candidate is recorded before tagging (§3); the tag
  is applied with `git tag -a v0.3.0 <recorded-sha>` and verified with
  `git rev-list -n 1 v0.3.0` against the record.
- **Patch releases** on the line reuse the procedure: `v0.3.1` tags the
  recorded fix commit after the same check record.
- **Tag placement is direct under the decided publication route.** The
  #438 ruling is in-place publication with the existing history kept —
  the published `main` *is* this repository's `main`, so the tag names
  the recorded candidate commit itself, with no rewritten-history
  duality to carry. Publication must still precede the release's
  public announcement steps, but the tag object, the candidate record,
  and the published history all name the same SHA.

## 3. Candidate-commit recording procedure

The acceptance record already models the pattern: its §1 records the
exact `origin/main` HEAD (`git log -1`, full SHA, merge subject) and
the merges the evidence depends on. The freeze extends it:

1. After the owner's candidate sign-off — the acceptance evidence is
   met and the gating fixes (#344/#363/#364, #447–#450, #466) have
   landed; what remains open is the disposition call on each flagged
   defect and the open repair PRs, tracked in
   [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §6–§7 and
   the [freeze checklist](release-freeze-checklist.md) — record the
   candidate:

   ```text
   git fetch origin
   git rev-parse origin/main        # full SHA = candidate
   git log -1 --format='%H%n%ad%n%s' --date=iso-strict origin/main
   ```

2. Record the SHA, its `git log -1` output, and the retrieval date in
   the release notes' verification section (drafted at
   [release-notes-v0.3.md](release-notes-v0.3.md)) alongside the check
   results run **on that exact commit** — not on whatever main happens
   to point at later.
3. Any merge to `main` after recording invalidates the record: the
   candidate is re-recorded and the checks re-run. A later merge does
   not silently extend the candidate.
4. The tag is applied only against the recorded SHA (§2). Under the
   decided in-place publication route the recorded SHA and the
   published history's commit are the same object — no pre/post-rewrite
   pair needs to be carried.

## 4. Version metadata alignment

Today the workspace carries almost no version metadata: every crate
except `workloop-evidence-protocol` (0.1.0) declares no `version`
field and resolves to Cargo's `0.0.0` default; there is no
`workspace.package.version`. This matters beyond cosmetics —
`taisan-worker-claude`'s `describe` reports
`adapter_version: env!("CARGO_PKG_VERSION")` into the durable
declaration the supervisor digests, so released binaries currently
self-report `0.0.0`.

Proposed alignment **at freeze time** (not in this draft change, and
not before the owner signs off):

- Set `workspace.package.version = "0.3.0"` and have member crates
  inherit it (`version.workspace = true`), so `taisan`,
  `taisan-worker-claude`, `public-issue-selector`, and the fixture
  binaries all report the release version.
- `workloop-evidence-protocol` moves from its standalone `0.1.0` to
  the workspace version — its path-dependency consumers take the same
  release line; crates.io publication is out of scope for this
  release, so the version is an identity marker, not a registry
  contract.
- The same commit updates `crates/workloop-cli/Cargo.toml`'s
  `workloop-evidence-protocol = { version = "0.1.0", path = ... }`
  requirement to the new version and regenerates `Cargo.lock` — a
  `^0.1.0` requirement left against a `0.3.0` path crate fails Cargo
  resolution and the build gate. (`workloop-store`'s path-only
  dependency declares no version requirement and needs no edit.)
- The metadata bump lands in the freeze commit (or a commit the freeze
  records), so the tagged tree self-reports `0.3.0` end to end. Note
  that the bump also changes the `adapter_version` the released
  `taisan-worker-claude` self-reports into declarations
  (`env!("CARGO_PKG_VERSION")`); the describe captures retained from
  the pre-freeze live runs still report the pre-bump value — the
  freeze records both so the notes' tested-identity values stay
  honest.

This draft intentionally changes no `Cargo.toml`: the version bump is
part of the freeze, and landing it early would make builds claim a
version that has no candidate behind it.

## 5. Changelog generation policy

The release notes ([release-notes-v0.3.md](release-notes-v0.3.md)) are
the public-facing changelog for the release (0.3.0 as proposed,
pending the §7 decision). Proposed policy:

- **Source of truth is the tracker plus merged history**: the notes
  are compiled from milestone-#8 issue titles/states and the merge
  subjects of PRs merged into the candidate's first-parent history —
  the same enumeration style as
  [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §1. `git
  log` output is evidence input, not published verbatim.
- **Every entry traces to a locator**: an issue/PR number or a
  revision SHA the owner can re-check. A claim with no locator does
  not ship.
- **Evidence-class discipline is preserved**: fixture/contract-tested
  behavior is described as such; only owner-run live evidence
  authorizes "verified live" wording. Unexercised scope (e.g. remote
  execution and remote review) appears under known limitations, never
  as a capability.
- **Public-artifact rules apply**: English, standalone,
  provider-neutral, no private source names, no personal schedule —
  per the milestone-#8 boundary and `AGENTS.md`.
- **Regeneration is deterministic**: re-deriving the notes from the
  recorded candidate SHA and the tracker state produces the same
  claims; a hand-edited claim that no longer matches evidence is a
  release-note defect, fixed or removed.

## 6. Distribution route — decided

Owner decision on #370 (2026-11-25): taisan ships as **prebuilt
binaries**, not as a registry package.

- **Primary channel: cargo-dist on GitHub Releases.** One cargo-dist
  configuration produces the release matrix (per-platform archives),
  per-artifact checksums, shell and PowerShell installers, and the
  generated release notes. The artifact checksums cargo-dist emits
  are the packaged-artifact evidence the release notes' verification
  section records (the AC asks for checksums "if binaries are
  distributed" — they are).
- **Secondary channels from the same setup:** a personal Homebrew
  tap (`brew tap` against the owner's tap repository; homebrew-core
  deferred until notability justifies it) and **cargo-binstall**
  metadata so `cargo binstall` fetches the prebuilt binary instead
  of compiling.
- **crates.io is not a release channel.** taisan is a CLI tool, not
  a library; no registry publishing. The `0.3.0` workspace version
  (§4) is an identity marker, not a registry contract — unchanged by
  this decision.
- **Source build stays supported** (`cargo install --path
  crates/workloop-cli --bin taisan`, per the release notes'
  requirements section); the dist artifacts are a convenience route
  over the same binaries.
- **Lane boundary:** the cargo-dist workflow definition and release
  automation live under #441; this document records the route
  decision and the steps that follow it
  ([freeze checklist](release-freeze-checklist.md) §5) but does not
  create or modify `.github/workflows/`.

## 7. What remains for the owner

| Decision | Options | This draft |
| --- | --- | --- |
| Release version | `0.3.0` (proposed) / `1.0.0` | **owner-pending** — the only undecided *decision* row in this table; owner acts outside it (defect dispositions, candidate sign-off) remain — status paragraph, freeze checklist |
| Candidate commit | recorded `origin/main` SHA at sign-off (§3) | procedure only; **not named** — the freeze records it |
| Publication route | in-place, history kept | **decided** — #438 ruling ([release audit](release-audit.md) §7) |
| Distribution route | cargo-dist → GitHub Releases + Homebrew tap + cargo-binstall; no crates.io | **decided** — #370 comment, 2026-11-25 (§6); workflow is #441's lane |
| Crate version alignment | workspace `0.3.0` inheritance (proposed) / leave `0.0.0` | proposed, not applied — a metadata step that follows the version choice, not an independent decision |
| Tag creation | annotated `v<version>` on the recorded candidate | procedure only; no tag exists |
