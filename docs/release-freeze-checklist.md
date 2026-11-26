# Release freeze checklist — mechanical steps

Status: **executed through §4** (issue #370). The owner signed off
2026-11-26 (version `0.3.0`; defect issues closed; #437 deferred to
v0.6); the candidate is pinned at
`00fed690cf2993e220116901f1c2d14584305e18` and the §4 gates re-ran on
it — the record lives in
[release-notes-v0.3.md](release-notes-v0.3.md)'s Verification
record. **No tag exists and no release is published** — §5.2/§5.3 and
the owner acts (live-demo segment, visibility flip) remain.
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

Resolved at freeze (2026-11-26):

- [x] *owner* — defect dispositions are recorded in #365's closing
      comment (2026-11-25): #313/#398/#399/#400 'fixed per §6'
      (owner's disposition wording — the four defects are confirmed
      present on the candidate per
      [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §6;
      the label is not a code-state claim),
      #468/#470/#471 deferred to v0.6 (milestone 11) and the earlier
      deferred set #453–#456 unchanged → v0.6,
      #469 fixed post-pin by #474. Owner acts completed
      2026-11-26: the dispositioned defect issues #313/#358/#359/
      #361/#398/#399/#400 were closed under the owner's 'Fixed on
      the release candidate' wording (a disposition act, not
      evidence a fix landed — #313/#398/#399/#400 remain confirmed
      present), and #437 received its recorded disposition —
      deferred to v0.6 (milestone 11). Ledger:
      [release-acceptance-v0.3.md](release-acceptance-v0.3.md) §6–§7;
      user-facing ones sit in the notes' known-limitations.
- [x] Merge the open repair PR or defer it explicitly:
      #431 (#419) — **merged `e0e31e9` 2026-11-26**, inside the
      pinned candidate. (#472's ledger fold-in merged as `d87d5dc` — the
      notes cite the retained owner-side run records, not that PR —
      and #427 merged as `52a5493`.)
- [x] *owner* — pick the release version: **`0.3.0`** decided
      2026-11-26 ([release-versioning.md](release-versioning.md) §7);
      the version-metadata alignment landed as PR #477, inside the
      pinned candidate.

## 2. Merge order

1. Land any repair PRs the owner dispositioned as blocking.
2. Land the docs updates (these release materials — the #472 ledger
   fold-in already landed).
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

- [x] Re-mark every walkthrough section: "retained — predates
      candidate" where the frozen commit differs from the exercised
      code — done in the freeze record: the only drift is the
      version self-report (`0.0.0` → `0.3.0`), labelled in
      [release-walkthrough-v0.3.md](release-walkthrough-v0.3.md);
      re-recording a live-demo segment stays an owner decision.
- [x] Consistency pass: README, notes, walkthrough tell one story;
      no claim exceeds the ledger — verified in the freeze record
      (disposition wording everywhere reads as disposition labels,
      not code-state claims; every "issue open" stale for the closed
      dispositioned defects was corrected).
- [ ] Publication is in-place per #438 — flipping repository
      visibility is the owner's own act; this checklist does not do it.
