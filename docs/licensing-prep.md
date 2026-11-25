# License preparation: dependency inventory and options

Status: **decision applied** — the owner selected
`MIT OR Apache-2.0` (dual; licensees pick either) for code and
unmarked project files, with `assets/taisan-hero.jpg` excluded under
its own usage terms in `assets/LICENSE.md`. `LICENSE-MIT`,
`LICENSE-APACHE`, `CONTRIBUTING.md`, `THIRD-PARTY.md`, and `LICENSES/`
are in place; every workspace crate declares
`license.workspace = true`. This document remains the evidence-backed
inventory the selection was made from — see §5 for resolved and
remaining items. ([issue #369](https://github.com/ttbc-dev/taisan/issues/369))

Evidence basis: `cargo metadata --format-version 1 --locked` and
`cargo tree --locked` resolved against `Cargo.lock` in this checkout,
cross-checked against the license files shipped inside each crate
package under `~/.cargo/registry/src/`. `cargo-deny`/`cargo-about` are
not installed on this machine; the inventory was generated from
metadata fields and primary license files instead.

## 1. Dependency inventory

The workspace has **9 direct external dependencies** and **87 external
crates total** in the resolved dependency graph (all targets, normal +
build edges; dev-dependencies add no additional crates). Of those 87:

- **52 crates link into unix release binaries** — the table's
  unmarked rows.
- **18 crates compile only at build time and never link into a
  binary** (19 table rows — `syn` is present at two versions). The
  unix build-time set is: proc-macro crates `clap_derive`,
  `ref-cast-impl`, `schemars_derive`, `serde_derive`; build-script
  helpers `cc`, `find-msvc-tools`, `pkg-config`, `shlex`, `vcpkg`,
  `version_check`; and the proc-macro support crates reachable only
  through them (`proc-macro2`, `quote`, `syn`, `heck`,
  `serde_derive_internals`, `unicode-ident`). The table marks these
  "(build only)". Two further marked rows — `rustversion` and
  `zerocopy-derive` — are also build-time-only, but they resolve only
  through the wasm-bound subtree (`rustversion` via `wasm-bindgen`;
  `zerocopy-derive` via `zerocopy`'s derive feature, off on unix), so
  they never compile for unix either and are counted below.
  (`cargo tree --target aarch64-apple-darwin --edges normal,build`
  resolves 68 of the 87: the 52 linked crates plus the 16 unix
  build-time names; `x86_64-pc-windows-msvc` resolves 72.)
- **16 crates are target-gated** (`(wasm only)`/`(windows only)`/
  `(UEFI only)` rows): `r-efi` (UEFI only),
  the `wasm-bindgen*`/`js-sys`/`bumpalo`/`futures-*`/`slab`/
  `pin-project-lite` cluster (11 crates, wasm32 only, reached via
  `uuid`) and the `windows-sys`/`windows-link`/`anstyle-wincon`/
  `once_cell_polyfill` cluster (4 crates, Windows only). Together with
  `rustversion` and `zerocopy-derive` above, **18 crates never resolve
  for unix targets** (69 unix-resolved rows = 68 unique crates).

`D` = declared directly in a workspace `Cargo.toml`.

| | Crate | Version | Declared license | Reached via |
| --- | --- | --- | --- | --- |
| | ahash | 0.8.12 | MIT OR Apache-2.0 | hashbrown |
| | anstream | 1.0.0 | MIT OR Apache-2.0 | clap_builder |
| | anstyle | 1.0.14 | MIT OR Apache-2.0 | anstream, anstyle-wincon, clap_builder |
| | anstyle-parse | 1.0.0 | MIT OR Apache-2.0 | anstream |
| | anstyle-query | 1.1.5 | MIT OR Apache-2.0 | anstream |
| | anstyle-wincon | 3.0.11 | MIT OR Apache-2.0 | anstream (windows only) |
| | bitflags | 2.13.2 | MIT OR Apache-2.0 | rusqlite |
| | block-buffer | 0.12.1 | MIT OR Apache-2.0 | digest |
| | bumpalo | 3.20.3 | MIT OR Apache-2.0 | wasm-bindgen-macro-support (wasm only) |
| | cc | 1.10.0 | MIT OR Apache-2.0 | libsqlite3-sys (build only) |
| | cfg-if | 1.0.5 | MIT OR Apache-2.0 | ahash, getrandom, js-sys, sha2 |
| D | clap | 4.6.7 | MIT OR Apache-2.0 | workloop-cli, workloop-tui |
| | clap_builder | 4.6.7 | MIT OR Apache-2.0 | clap |
| | clap_derive | 4.6.7 | MIT OR Apache-2.0 | clap (build only) |
| | clap_lex | 1.1.1 | MIT OR Apache-2.0 | clap_builder |
| | colorchoice | 1.0.5 | MIT OR Apache-2.0 | anstream |
| | const-oid | 0.10.2 | Apache-2.0 OR MIT | digest |
| | cpufeatures | 0.3.1 | MIT OR Apache-2.0 | sha2 (x86/aarch64 only) |
| | crypto-common | 0.2.2 | MIT OR Apache-2.0 | digest |
| | digest | 0.11.3 | MIT OR Apache-2.0 | sha2 |
| | dyn-clone | 1.0.20 | MIT OR Apache-2.0 | schemars |
| | equivalent | 1.0.3 | Apache-2.0 OR MIT | indexmap |
| | fallible-iterator | 0.3.0 | MIT/Apache-2.0 | rusqlite |
| | fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | rusqlite |
| | find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 | cc (build only) |
| | futures-core | 0.3.34 | MIT OR Apache-2.0 | futures-util (wasm only) |
| | futures-task | 0.3.34 | MIT OR Apache-2.0 | futures-util (wasm only) |
| | futures-util | 0.3.34 | MIT OR Apache-2.0 | js-sys (wasm only) |
| | getrandom | 0.4.3 | MIT OR Apache-2.0 | uuid |
| | hashbrown | 0.14.5 | MIT OR Apache-2.0 | hashlink |
| | hashbrown | 0.17.1 | MIT OR Apache-2.0 | indexmap |
| | hashlink | 0.9.1 | MIT OR Apache-2.0 | rusqlite |
| | heck | 0.5.0 | MIT OR Apache-2.0 | clap_derive (build only) |
| | hybrid-array | 0.4.15 | MIT OR Apache-2.0 | block-buffer, crypto-common |
| | indexmap | 2.14.2 | Apache-2.0 OR MIT | toml_edit |
| | is_terminal_polyfill | 1.70.2 | MIT OR Apache-2.0 | anstream |
| | itoa | 1.0.18 | MIT OR Apache-2.0 | serde_json |
| | js-sys | 0.3.107 | MIT OR Apache-2.0 | uuid (wasm only) |
| D | libc | 0.2.191 | MIT OR Apache-2.0 | workspace crates (unconditional deps — present in every target's dependency graph, links on unix targets only), cpufeatures, getrandom |
| | libsqlite3-sys | 0.30.1 | MIT | rusqlite (bundled amalgamation — see §2) |
| | memchr | 2.8.3 | Unlicense OR MIT | serde_json, winnow |
| | once_cell | 1.21.4 | MIT OR Apache-2.0 | ahash |
| | once_cell_polyfill | 1.70.2 | MIT OR Apache-2.0 | anstyle-wincon (windows only) |
| | pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | futures-util (wasm only) |
| | pkg-config | 0.3.34 | MIT OR Apache-2.0 | libsqlite3-sys (build only) |
| | proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | clap_derive, quote, syn (build only) |
| | quote | 1.0.47 | MIT OR Apache-2.0 | proc-macro crates (build only) |
| | r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | getrandom (UEFI only — see §5) |
| | ref-cast | 1.0.27 | MIT OR Apache-2.0 | schemars |
| | ref-cast-impl | 1.0.27 | MIT OR Apache-2.0 | ref-cast (build only) |
| D | rusqlite | 0.32.1 | MIT | workloop-cli, workloop-intake, workloop-store |
| | rustversion | 1.0.23 | MIT OR Apache-2.0 | wasm-bindgen (build only) |
| D | schemars | 1.2.3 | MIT | workloop-evidence-protocol |
| | schemars_derive | 1.2.3 | MIT | schemars (build only) |
| D | serde | 1.0.229 | MIT OR Apache-2.0 | workspace crates |
| | serde_core | 1.0.229 | MIT OR Apache-2.0 | serde, serde_json |
| | serde_derive | 1.0.229 | MIT OR Apache-2.0 | serde, serde_core (build only) |
| | serde_derive_internals | 0.30.0 | MIT OR Apache-2.0 | schemars_derive (build only) |
| D | serde_json | 1.0.154 | MIT OR Apache-2.0 | workspace crates |
| | serde_spanned | 0.6.9 | MIT OR Apache-2.0 | toml, toml_edit |
| D | sha2 | 0.11.1 | MIT OR Apache-2.0 | workspace crates |
| | shlex | 2.0.1 | MIT OR Apache-2.0 | cc (build only) |
| | slab | 0.4.12 | MIT | futures-util (wasm only) |
| | smallvec | 1.16.4 | MIT OR Apache-2.0 | rusqlite |
| | strsim | 0.11.1 | MIT | clap_builder |
| | syn | 2.0.119 | MIT OR Apache-2.0 | zerocopy-derive (build only) |
| | syn | 3.0.9 | MIT OR Apache-2.0 | proc-macro crates (build only) |
| D | toml | 0.8.23 | MIT OR Apache-2.0 | workloop-config |
| | toml_datetime | 0.6.11 | MIT OR Apache-2.0 | toml, toml_edit |
| | toml_edit | 0.22.27 | MIT OR Apache-2.0 | toml |
| | toml_write | 0.1.2 | MIT OR Apache-2.0 | toml_edit |
| | typenum | 1.20.1 | MIT OR Apache-2.0 | hybrid-array |
| | unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | proc-macro2, syn (build only — see §4) |
| | utf8parse | 0.2.2 | Apache-2.0 OR MIT | anstream, anstyle-parse |
| D | uuid | 1.29.1 | Apache-2.0 OR MIT | workloop-core |
| | vcpkg | 0.2.15 | MIT/Apache-2.0 | libsqlite3-sys (build only) |
| | version_check | 0.9.5 | MIT/Apache-2.0 | ahash (build only) |
| | wasm-bindgen | 0.2.130 | MIT OR Apache-2.0 | js-sys, uuid (wasm only) |
| | wasm-bindgen-macro | 0.2.130 | MIT OR Apache-2.0 | wasm-bindgen (wasm only) |
| | wasm-bindgen-macro-support | 0.2.130 | MIT OR Apache-2.0 | wasm-bindgen-macro (wasm only) |
| | wasm-bindgen-shared | 0.2.130 | MIT OR Apache-2.0 | wasm-bindgen (wasm only) |
| | windows-link | 0.2.1 | MIT OR Apache-2.0 | windows-sys (windows only) |
| | windows-sys | 0.61.2 | MIT OR Apache-2.0 | anstyle-query, anstyle-wincon (windows only) |
| | winnow | 0.7.15 | MIT | toml_edit |
| | zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | ahash |
| | zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | zerocopy (build only) |
| | zmij | 1.0.23 | MIT | serde_json |

License-field check: **every external crate declares a license field**;
none are missing or ambiguous enough to block parsing. 4 crates
(`fallible-iterator`, `fallible-streaming-iterator`, `vcpkg`,
`version_check`) use the legacy `MIT/Apache-2.0` slash spelling rather
than an SPDX-valid `MIT OR Apache-2.0` — the intent is clear, but a
strict SPDX parser must tolerate the legacy form.
Package-content check: every crate ships its license text file(s) inside
the published package **except `r-efi 6.0.0`** (SPDX expression only;
UEFI-gated — see §5). **No crate ships a `NOTICE` file.**

## 2. Bundled and redistributed components

What the release redistributes, beyond source:

| Component | What it is | License status |
| --- | --- | --- |
| `taisan` binary (`crates/workloop-cli`) | Primary CLI; statically links the unix-linked crates in §1 | Project code; license pending owner selection |
| `taisan-tui` binary (`crates/workloop-tui`) | Management console (deferred from release requirements, still built/distributed) | Same as above |
| `public-issue-selector` binary (`crates/workloop-selector`) | Packaged public reference Selector | Same as above |
| `taisan-worker-claude` binary (`crates/taisan-worker-claude`) | Packaged reference Worker adapter | Same as above. It **invokes an external, user-installed agent runtime CLI** — it does not embed or redistribute any part of that runtime. The runtime is an installation prerequisite, not a bundled component. |
| Test/fixture binaries (`workloop-fixture-worker`, `workloop-fixture-selector`, `workloop-contract-worker`, `s1_bridge`) | Deterministic fixture workers and contract-test helpers used by the test suite and docs evidence | Project code; same license as project once chosen. Ship in source; whether prebuilt copies are release artifacts is a packaging decision. |
| **SQLite amalgamation** (compiled in via `rusqlite` feature `bundled` → `libsqlite3-sys`) | The sqlite3.c amalgamation is compiled into every binary that links `workloop-store` | SQLite itself is **public domain**; no notice legally required. `libsqlite3-sys` crate is MIT. Recommend still recording it in third-party notices for transparency. |
| `skills/taisan/SKILL.md` | Agent skill installed into users' coding agents via `npx skills add` | Project-authored text; same license as project |
| `assets/taisan-hero.jpg` | README/hero image, resized from an owner-approved issue attachment (provenance recorded in `assets/README.md`) | Owner-supplied asset — see §5 for the required confirmation |
| `scripts/` (Python + shell), `fixtures/` (JSON), `docs/` | Verification scripts, seeded fixture data, documentation | Project-authored; same license as project |
| `gh` CLI, agent runtimes, Rust toolchain | External tools invoked or installed at operation time (`scripts/worker/setup.sh` downloads rustup at run time, in the target environment) | **Not redistributed** — external prerequisites |
| CI workflow (`.github/workflows/rust.yml`) | References GitHub-hosted actions by name | Referenced, not vendored; no bundled third-party code |

No `vendor/` directory, no `.crate` archives, and no third-party source
embedded in the tree (checked: no foreign copyright/license headers in
`crates/`, `scripts/`, `skills/`, `docs/`).

## 3. Compatibility summary and candidate licenses

**License families present in the resolved graph** (87 crates): MIT
only (8), MIT/Apache-2.0 dual in either order or spelling (74 total:
64 `MIT OR Apache-2.0`, 6 `Apache-2.0 OR MIT`, 4 `MIT/Apache-2.0`),
BSD-2-Clause OR Apache-2.0 OR MIT (2 — `zerocopy`,
`zerocopy-derive`), Unlicense OR MIT (1 — `memchr`), MIT OR Apache-2.0
OR LGPL-2.1-or-later (1 — `r-efi`, UEFI-gated), and (MIT OR Apache-2.0)
AND Unicode-3.0 (1 — `unicode-ident`).

**What redistribution requires:**

- Every family present is permissive. No copyleft license applies to
  any crate that ships in a release artifact: the only LGPL mention is
  one alternative in `r-efi`'s `OR` expression — it can be declined —
  and that crate compiles only for `target_os = "uefi"`, which no
  planned target uses.
- MIT, Apache-2.0, BSD-2-Clause, and Unicode-3.0 all require shipping
  the license text and copyright notices with binary distributions
  (§4 covers the concrete NOTICE content).
- All dependency licenses permit sublicensing/relicensing of the
  combined work under any permissive or copyleft project license.
  Nothing in the graph constrains the owner's choice.

**Candidate project licenses** (owner selects — no recommendation
made here beyond the rationale):

| Candidate | Rationale | Cost |
| --- | --- | --- |
| **MIT** | Simplest single license; matches the de-facto license of every dependency; maximum friction-free reuse | No patent grant; weaker explicit attribution machinery for large downstreams |
| **Apache-2.0** | Explicit patent grant and contribution terms; §4(d) NOTICE mechanism gives downstream redistributors a defined place for attribution | Slightly heavier license text; must carry an APPENDIX-style notice header convention if the owner wants per-file headers |
| **MIT OR Apache-2.0** (dual) | The standard Rust-ecosystem convention — licensees pick whichever license suits them; compatible with both MIT-only and Apache-2.0-only consumers, and it is the license family 74 of 87 deps already use | Two license files to maintain; contribution terms default to dual licensing, which should be stated in CONTRIBUTING/README |

Copyleft candidates (GPL family, MPL-2.0) are technically permitted by
the dependency set but would impose source-disclosure obligations on
every downstream user of a CLI binary — disproportionate for a
developer tool and not listed as a primary candidate.

## 4. Required-notice analysis

**Apache-2.0 `NOTICE` files:** none. No dependency ships a `NOTICE`
file, so Apache-2.0 §4(d) adds no mandatory notice text to propagate.

**License-text retention (required under every candidate):** binary
distributions must carry the copyright and license text of each
MIT/BSD/Unlicense/Unicode-licensed dep actually shipped in the
artifact, plus the Apache-2.0 text for Apache-licensed deps. For the
unix-linked set that means all crates marked non-"(build only)" and
non-"(wasm/windows/UEFI only)" in §1 — the conservative and simplest
rule is to include **all 87 resolved crates** in one notices file,
since proc-macro/build crates never hurt and target-gated entries
document the full lockfile.

**Per-crate copyright lines:** extractable from each package's shipped
`LICENSE-*` files for 86 of 87 crates (the exception is `r-efi`, §5).
These were verified present in this audit; a generated `THIRD-PARTY`
file should embed them.

**`unicode-ident` special case:** its license is `(MIT OR Apache-2.0)
AND Unicode-3.0` — the Unicode-3.0 terms apply unconditionally, not as
a choice. It ships `LICENSE-UNICODE`, which must be included. It is a
build-time-only dependency (proc-macro chain), so its code does not
land in shipped binaries, but including its notice in the notices file
is required for source distribution and trivially cheap for binaries.

**Draft NOTICE/THIRD-PARTY content per candidate** (same for all three
candidates; the project license choice does not change third-party
obligations):

```text
This distribution includes third-party software components.

Rust crates (see THIRD-PARTY.md / cargo metadata for versions):
  <crate name> <version> — <license expression>
  <copyright line extracted from the crate's LICENSE file>
  ... one entry per resolved crate (87 at audit time) ...

Bundled components:
  SQLite (via libsqlite3-sys "bundled" feature) — public domain.
  The SQLite amalgamation is compiled into the distributed binaries.

Required license texts (reproduced in full in LICENSES/ or appended):
  MIT, Apache-2.0, Unicode-3.0 (unicode-ident),
  BSD-2-Clause (only if the BSD option is elected for zerocopy —
  MIT/Apache-2.0 may be elected instead),
  Unlicense (only if elected for memchr — MIT may be elected instead).
```

If the owner selects **Apache-2.0** for the project, the project also
needs its own `NOTICE`-style attribution block naming the project and
copyright holder; MIT and dual-licensing need only the license
file(s).

## 5. Blockers and open items

| # | Item | Severity | Evidence |
| --- | --- | --- | --- |
| B1 | **Project license unselected** — every workspace crate currently declares no `license` field (cargo metadata shows `[]` for all 14 members) and no `LICENSE` file exists. Blocks the release itself, and blocks AC #3 (LICENSE + consistent Cargo metadata). | **Resolved** — owner selected `MIT OR Apache-2.0`; `LICENSE-MIT`/`LICENSE-APACHE` added, all 15 members declare `license.workspace = true` (verified via `cargo metadata --no-deps`) | `Cargo.toml` files; `cargo metadata` |
| B2 | **`assets/taisan-hero.jpg` redistribution rights unconfirmed** — provenance is documented (resized from an owner-approved issue attachment, `assets/README.md`) but no explicit statement that the owner licenses it for redistribution under the chosen project license. Likely resolvable in one line by the owner; flagged rather than assumed. | **Resolved** — owner confirmed repository inclusion and redistribution rights; the image is excluded from the dual license under its own terms in `assets/LICENSE.md` (scope: that file only) | `assets/README.md`; `assets/LICENSE.md` |
| B3 | **`r-efi 6.0.0` ships no license text file** — SPDX expression `MIT OR Apache-2.0 OR LGPL-2.1-or-later` only. Not a practical blocker: the crate compiles solely for `target_os = "uefi"` (a getrandom backend not used by any planned target) and permissive options exist in the `OR`. Recorded so it is not silently smoothed over; if the project ever targets UEFI the missing license text would need an upstream request. | Anomaly — not blocking for planned targets | registry package contents |
| B4 | **Publication disclosure review** — `docs/source-workflow-audit.md` retains source-internal names; publication previously required a disclosure review covering it and the git history. Dispositioned: the owner ruled under issue #438 that the names and owner identity are acceptable to disclose and the repository publishes in place with history kept (`docs/release-audit.md` §7); no disclosure review gates publication under that ruling. A publication gate rather than a copyright problem (the file is project-authored), recorded here because it gated the same release. | Dispositioned (non-license) — owner ruling #438 | `AGENTS.md` source-references rule; `docs/release-audit.md` §7 |

**Non-blocking observations:**

- No dependency lacks a declared license field; 4 crates use the legacy
  `MIT/Apache-2.0` spelling rather than SPDX-valid `OR` syntax (§1).
- No copyleft obligation reaches any planned build target.
- The SQLite amalgamation (public domain) is the only third-party code
  physically compiled into the binaries beyond the Rust crates.
- Dev-dependency-only crates: none — `rusqlite` (the only external
  dev-dep, used for store fault-injection tests) is already a runtime
  dep of `workloop-store`.
- Regenerating this inventory is a one-command operation once
  `cargo-deny` or `cargo-about` is installed; until then the §1 table
  is the audit record (reproduce with `cargo metadata --locked` +
  `cargo tree --locked`).

## 6. Decision applied (2026-11-25)

Owner selection: **`MIT OR Apache-2.0`** for code and unmarked project
files; `assets/taisan-hero.jpg` is excluded under its own terms in
`assets/LICENSE.md` (the exception scopes to that file only — not the
whole `assets/` directory — and the standard MIT/Apache-2.0 texts carry
no image exceptions).

Applied artifacts: `LICENSE-MIT` (standard MIT text carrying the
copyright line `2026 Shimpei Takamatsu`), `LICENSE-APACHE` (verbatim
Apache Software Foundation text —
https://www.apache.org/licenses/LICENSE-2.0.txt — the project
copyright is recorded in `LICENSE-MIT` and here, not inside the
standard text), `README.md` License section,
`assets/LICENSE.md`, `CONTRIBUTING.md` (same dual license for
contributions, no CLA/copyright assignment), `THIRD-PARTY.md` (87-crate
inventory with per-package copyright or `authors` attribution),
`LICENSES/` (full MIT, Apache-2.0, Unicode-3.0, BSD-2-Clause and
Unlicense texts), `license.workspace = true` on all 15 member crates.

**Distribution-path check (task: bundle completeness):** no crate is
published (no `publish`/`package` configuration exists anywhere in the
workspace) and no binary-tarball machinery exists yet — the source tree
is currently the only distribution form, and it carries the full notice
set in-tree. When binary or packaged releases are produced, the bundle
must carry `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD-PARTY.md`, and
`LICENSES/`; any bundle that also ships `assets/taisan-hero.jpg` must
carry `assets/LICENSE.md` alongside it.
