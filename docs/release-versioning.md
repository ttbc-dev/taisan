# Release versioning and tag policy — proposal

Status: **proposal, pending owner sign-off** (issue #370). This document
proposes the version/tag policy and the procedures the release freeze
will follow. It is a draft prepared before the final candidate is
frozen — the freeze itself stays blocked on the release-acceptance
(#365), clean-install reproduction (#367), public-content audit (#368),
and license (#369) gates. **No tag exists, no release exists, and no
commit named here is a release candidate.** Nothing in this document
authorizes a tag, a GitHub release, a version-metadata change, a
visibility change, or publication. The public-release route (routes
A/B/C in [release audit](release-audit.md#6-options-for-owner-decision))
is an open owner decision this document does not make.

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
  `git rev-list -n 1 v0.3.0` against the record. Where the route
  rewrites history, `<recorded-sha>` is the candidate's commit in the
  published history — see the placement rule below and §3.4.
- **Patch releases** on the line reuse the procedure: `v0.3.1` tags the
  recorded fix commit after the same check record.
- **Tag placement depends on the owner's publication route** and this
  proposal does not choose one, but under every route the published
  history can differ from this repository's private `main`: route A
  (publish in place) still rewrites all 1,017 commits to strip names
  and identities
  ([release audit](release-audit.md#6-options-for-owner-decision) §6),
  and routes B (fresh publication) and C (truncated history) publish a
  sanitized/squashed tree. So under every route the tag names the
  recorded candidate's commit *in the published history* — never a
  private pre-rewrite commit — and tagging waits for the published
  tree to exist. The candidate record carries both the private
  pre-rewrite SHA and the published SHA so either side of the rewrite
  can be re-verified (§3.4).

## 3. Candidate-commit recording procedure

The acceptance record already models the pattern: its §1 records the
exact `origin/main` HEAD (`git log -1`, full SHA, merge subject) and
the merges the evidence depends on. The freeze extends it:

1. After the acceptance gate (#365, which consumes the #270 manual
   and #271 scheduled evidence) and the release-blocking fixes
   (#344, #364 per
   [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §6) have
   landed, record the candidate:

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
4. The tag is applied only against the recorded SHA (§2). If the
   publication route rewrites history, the record names both the
   pre-rewrite and published SHAs so either side of the rewrite can be
   re-verified.

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
pending the §6 decision). Proposed policy:

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
  authorizes "verified live" wording. Unexercised scope (e.g.
  scheduled intake, #271) appears under known limitations, never as a
  capability.
- **Public-artifact rules apply**: English, standalone,
  provider-neutral, no private source names, no personal schedule —
  per the milestone-#8 boundary and `AGENTS.md`.
- **Regeneration is deterministic**: re-deriving the notes from the
  recorded candidate SHA and the tracker state produces the same
  claims; a hand-edited claim that no longer matches evidence is a
  release-note defect, fixed or removed.

## 6. What remains for the owner

| Decision | Options | This draft |
| --- | --- | --- |
| Release version | `0.3.0` (proposed) / `1.0.0` | no choice made |
| Publication route | A / B / C ([release audit](release-audit.md#6-options-for-owner-decision)) | no choice made; tag placement follows it |
| Crate version alignment | workspace `0.3.0` inheritance (proposed) / leave `0.0.0` | proposed, not applied |
| Tag creation | annotated `v0.3.0` on the recorded candidate | procedure only; no tag exists |
