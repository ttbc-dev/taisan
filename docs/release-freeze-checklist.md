# Release freeze checklist — mechanical steps

Status: **procedure only** (issue #370). This is the ordered,
mechanical checklist the freeze follows once the owner signs off.
Nothing in it is done yet — **no candidate is pinned, no version
metadata is bumped, no tag exists, and no release is published.**
Steps marked *owner* require an explicit owner decision; everything
else is mechanical. Items are in execution order: each step gates
the next, and any merge to `main` after step 3 invalidates the
recorded candidate.

## 1. Preconditions — all true before step 2

Already met at writing:

- [x] #270 manual real-Worker acceptance — M-1…M-7 exercised,
      owner-approved (comment `7171973494`)
- [x] #271 scheduled-intake pilot — all ACs met (7 admitted, w-6/w-7
      `succeeded`; record `acceptance-v03-pilot/`)
- [x] #367 clean-Linux-install reproduction — runbook verified
      (record `acceptance-v03-clean/`)
- [x] #368 public-content audit — dispositioned by the #438 ruling
      (in-place publication, history kept)
- [x] #369 license — MIT OR Apache-2.0 applied (PR #464)
- [x] Distribution route decided — cargo-dist → GitHub Releases +
      Homebrew tap + cargo-binstall; no crates.io (#370, §6 of
      [release-versioning.md](release-versioning.md))

Still open at writing:

- [ ] *owner* — disposition each flagged defect as blocking-then-fix
      or deferred-documented: #313, #398/#399, #400, #437,
      #453–#456, #468–#471, plus the deferred residuals — ledger:
      [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §6–§7;
      user-facing ones already sit in the notes' known-limitations.
- [ ] Merge open repair/evidence PRs or defer them explicitly:
      #472 (ledger fold-in — merge first, the notes cite its facts),
      #427 (#323), #431 (#419).
- [ ] *owner* — pick the release version: `0.3.0` (proposed) or
      `1.0.0` ([release-versioning.md](release-versioning.md) §7).

## 2. Merge order

1. Land any repair PRs the owner dispositioned as blocking.
2. Land the docs updates (these release materials; the ledger
   fold-in #472).
3. Land the version-metadata alignment (§5.1) as its own commit —
   or fold it into the freeze commit; it must be inside the
   recorded candidate.
4. Nothing else merges between the candidate record (§3) and the
   gates (§4).

## 3. Pin the candidate

```text
git fetch origin
git rev-parse origin/main        # full SHA = candidate
git log -1 --format='%H%n%ad%n%s' --date=iso-strict origin/main
```

Record the SHA, the `git log -1` output, and the retrieval date in
the release notes' verification section
([release-notes-v0.3.md](release-notes-v0.3.md)). Any later merge
invalidates the record — re-record and re-run §4; a merge never
silently extends the candidate.

## 4. Re-run the baseline gates on the pinned commit

The §9 baseline of release-acceptance-v0.3.md, run on the recorded
SHA itself (build before test — contract tests spawn
`target/debug/workloop-fixture-worker`):

```text
cargo fmt --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Replace the notes' verification table with these results (counts
included; the `u36e_live` ignored cases stay listed as ignored —
they need owner authorization). A freeze never inherits a check
record from a different commit.

## 5. Version metadata, tag, and dist — after owner sign-off

### 5.1 Version-metadata alignment (inside the candidate)

Per [release-versioning.md](release-versioning.md) §4:

- Set `workspace.package.version = "<version>"` and switch member
  crates to `version.workspace = true`.
- Move `workloop-evidence-protocol` off its standalone `0.1.0` to
  the workspace version; update `crates/workloop-cli/Cargo.toml`'s
  `workloop-evidence-protocol = { version = "0.1.0", ... }`
  requirement to match; regenerate `Cargo.lock`.
- Re-run §4 on the bumped tree — the bump changes the
  `adapter_version` `taisan-worker-claude` self-reports
  (`env!("CARGO_PKG_VERSION")`), so the notes' tested-identity row
  records both the pre-bump live value (`0.0.0`) and the release
  value.

### 5.2 Tag

```text
git tag -a v<version> <recorded-sha>   # annotated; message cites
                                       # the candidate, the acceptance
                                       # record §1, and the milestone
git rev-list -n 1 v<version>           # must equal <recorded-sha>
```

### 5.3 Distribution artifacts (workflow owned by #441)

- Run the cargo-dist release flow on the tag: matrix archives,
  per-artifact checksums, shell/PowerShell installers, generated
  release notes.
- Record the produced checksums in the release notes' verification
  section ("TODO at release" slot).
- Update the personal Homebrew tap formula to the tag + checksums.
- Verify the cargo-binstall metadata resolves the published
  artifacts.
- crates.io: nothing to do — not a channel.

## 6. Final sweep before the release is announced

- [ ] Re-mark every walkthrough section: "retained — predates
      candidate" where the frozen commit differs from the exercised
      code, or re-record (owner decides on a live-demo segment).
- [ ] Consistency pass: README, notes, walkthrough tell one story;
      no claim exceeds the ledger.
- [ ] Publication is in-place per #438 — flipping repository
      visibility is the owner's own act; this checklist does not do it.
