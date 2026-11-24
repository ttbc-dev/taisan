# Release audit — public content, history, and repository metadata

Audit for issue #368, performed 2026-11-24 against `origin/main` at
`d81f064`. Read-only: no files were modified, no history rewritten, no
metadata changed, no visibility touched. Raw findings are cited by
file:line and leak class; no secret value or private source-repository
name is reproduced here — source names are described by role using the
legend in `AGENTS.md` / `docs/source-workflow-audit.md`. The one name
spelled out is `shimpeiws/agent-workloop`, this repository's own former
name, which the name-migration analysis necessarily cites.

> **Owner-controlled record — not publication-facing.** This document is
> a map of every remaining leak location; publishing it would hand an
> adversary the leak index itself. It needs the same disposition as
> `docs/source-workflow-audit.md` — a disclosure review, or exclusion
> from the published tree and its reachable history — before the
> repository goes public (see §6). `AGENTS.md` names it a second
> owner-controlled exception under the same terms — but that clause is
> added by the same pull request that adds this file, so it is a
> proposed disposition pending owner ratification, not a prior owner
> decision. Committing the file already places the leak index in
> reachable history, and merging this pull request puts it on `main`'s
> history. Per route (§6): under A the published tree must be scrubbed
> or the file removed and its blob history rewritten; under B a
> sanitized snapshot can exclude it; under C the squashed root commit
> publishes the current tree as-is, so the file must be removed or
> scrubbed before the squash — it does not stay out by itself.
> Accepting the merge is the owner's ratification of keeping this
> record tracked under the second exception — it does not replace the
> disclosure review required before publication (§5 item 6, §6), and
> this record does not make that decision.

The audited publication route is the current one: the existing private
GitHub repository made public in place, exposing the tracked tree, the
full reachable history of every pushed ref, and all tracker metadata.

## Role legend for this report

| Term used below | Meaning |
| --- | --- |
| **the ops repo** / **the worker repo** / **the selector repo** / **the infra repo** | The four private source repositories defined by the role legend in `docs/source-workflow-audit.md`. Their real names are never quoted in this report. |
| **the smoke repo** | The private GitHub repository used as the isolated S1/U8 live-acceptance target. Its name embeds the worker repo's name plus a suffix, so every occurrence also discloses the worker repo's name. |
| **the former name** | This repository's pre-rename GitHub name, recorded in `docs/name-migration.md`. |
| **ops-adjacent repo names** | Other private repositories in the owner's namespace that appear only as incidental data (sample fixtures, webhook targets). |

## 1. Inventory of exposure surfaces

Under the current publication route, everything below becomes public:

- **Tracked tree**: 14 Cargo crates under `crates/` (~150 source/test
  files), `docs/` (~130 Markdown files including `docs/evidence/` and
  `docs/s1/`), `fixtures/` (s1, v0.3-doc, rui-evidence-v1; `store.db` is
  a 0-byte placeholder), `scripts/` (Python checkers, `scripts/s1/`
  scenario materials, `scripts/worker/setup.sh`), `skills/taisan/`,
  `assets/`, one workflow `.github/workflows/rust.yml`, `Cargo.toml` /
  `Cargo.lock` / `README.md` / `AGENTS.md` / `.gitignore`.
- **Reachable git history**: 1,017 commits on `origin/main`, plus ~73
  branches already pushed to `origin` (their names are published
  metadata). One local-only stash exists and is not published by a
  normal push. No tags, no git notes, no LFS objects.
- **Commit author/committer identities** on every published commit.
- **GitHub metadata**: 261 issues and 163 PRs (titles, bodies,
  comments, review comments, HTML-marker comments), 11 milestone
  descriptions, label names, repository description/topics/homepage,
  pushed branch names, project boards (enabled), releases (none),
  workflows (1).
- **Repository settings**: description and topics are clean; wiki and
  discussions are disabled; no releases exist; no license is set
  (tracked elsewhere in milestone #8 scope).

## 2. Private-source scan

### 2.1 Current tree — the sanctioned file exists, but leaks occur outside it

`docs/source-workflow-audit.md` is the owner-controlled audit record and
the first permitted exception per `AGENTS.md`. It correctly self-declares
that publication requires a disclosure review covering it and the git
history (`docs/source-workflow-audit.md:25-35`).

The scan confirms the real names of the ops, worker, selector, and infra
repos, two ops-side tool names (a terminal multiplexer and a container
manager), and `/Users/<owner>` checkout paths appear inside that file as
permitted. At HEAD, of those identifiers only the **worker repo's name**
also leaks outside it — embedded in the smoke repo's name and in
`~/.config/<worker-repo>/` paths. The ops, selector, and infra repo
names, both tool names, and real `/Users/<owner>` owner paths do not
occur outside the sanctioned file in the current tree (`/Users/` hits
only a test literal — see Appendix); each of them surfaces instead in
history or tracker metadata (§2.2, §2.3) — but the further leak classes
below mean the exception still does not hold in practice:

- **`docs/extraction-coverage.json`** — the generated coverage manifest
  redacts source identifiers as `[source-symbol]`/`[source-file]`, but
  the redaction missed:
  - one ops-adjacent repo name in `<owner>/<repo>` form (line 5372),
  - three ops-adjacent repo names quoted inside a pinned fixture —
    two in `<owner>/<repo>` form, one bare (line 14076),
  - a partially-redacted launchd service name of the form
    `[source-file]<suffix>.gateway` whose remaining suffix still
    discloses the ops-side messaging system (line 5372),
  - a private scheduled GitHub Actions workflow name describing a
    nightly runtime job (lines ~3035, ~5386, ~6695, ~26558 —
    owner-specific ops context),
  - owner checkout-root paths inside a rationale string: `~/src`,
    `~/src/[source-file]om/<o>/<r>`, and the derived `~/src/<r>` /
    `~/<r>` candidate patterns — the owner's `~/src` checkout layout
    left unredacted (line 6636).
- **`docs/name-migration.md:15,29`** — records this repository's own
  former name verbatim (`shimpeiws/agent-workloop` →
  `shimpeiws/taisan`). This is the repository's self-rename, not a
  private source repository, and GitHub's rename redirect discloses it
  the moment the repo goes public — so it is an owner judgment call,
  not a source-name leak. Recorded for completeness, not counted as a
  release blocker.
- **The smoke repo is pervasive**: its `<owner>/<smoke-repo>` name
  appears 161 times across 29 files — the sanctioned
  `docs/source-workflow-audit.md` plus, outside it, `docs/s1/*`
  (8 files), `docs/evidence/u3.6d.md`, `u3.6e.md`, `u8.1*.md`,
  `u8.2`–`u8.5`, `docs/remote-executor.md:20`, `fixtures/s1/*`,
  `scripts/s1/{check-prerequisites.py,worker-shim.sh,worker-shim-nc.sh}`,
  `scripts/tests/test_s1_materials.py`,
  `crates/workloop-cli/src/bin/s1_bridge.rs:1970`,
  `crates/workloop-github/src/evidence.rs:766-804`.
- **Worker-repo config path**: `~/.config/<worker-repo>/github-apps/...`
  in `scripts/s1/mint-app-token.py:5,28`,
  `docs/s1/stage-c-runbook.md:46,114`,
  `docs/evidence/u8.1-live-stage-c.md:41` — discloses the worker repo's
  name through a local config path even where the repo itself is not
  named.
- **Per-run owner state directories**: `~/s1-*` run dirs recorded in
  `docs/evidence/u8.1-live-stage-c.md:62,71,112,201,211`,
  `docs/evidence/u8.2.md:48`, `docs/evidence/u8.3.md:59`, and
  `docs/s1/live-runbook.md:159` — owner-specific operational context;
  the extracted dir names are all `s1-*` scenario names embedding no
  source names (see Appendix); together with the `~/src` checkout-root
  strings in `docs/extraction-coverage.json:6636` (counted above as a
  redaction miss), they round out the home-relative `~/` owner-path
  surface at HEAD.
- **Identifying runtime configuration**: the dedicated reviewer GitHub
  App's slug, app id, installation id, and bot user id
  (`docs/evidence/u8.1-live-stage-c.md:37-42`,
  `docs/s1/stage-c-runbook.md:42-46`); the smoke repo's numeric and node
  ids (`docs/evidence/u8.1-live-stage-c.md:52`); repository secret names
  (`S1_REVIEWER_*`, `docs/evidence/u8.1-live-stage-c.md:42`); the
  scenario git identity `s1-worker` hard-coded in
  `scripts/s1/worker-shim.sh:34-35` and `worker-shim-nc.sh:35-36`.
- **Internal scripts**: `scripts/s1/mint-app-token.py` is an
  owner-operations tool reading owner-local App credentials; the
  `worker-shim*.sh` scripts clone the private smoke repo
  (`worker-shim.sh:26`, `worker-shim-nc.sh:27`).

### 2.2 Git history — extensive, not covered by a clean HEAD

Every class above also exists in reachable history, plus classes that no
longer exist at HEAD:

- **Source repo names in historical blobs on `origin/main`**
  (`git log -S` hit counts): former name — 11 commits; smoke repo — 44;
  ops repo — 21; selector repo — 17; worker and infra repos — 12 + 12;
  container-manager references — 4; terminal-multiplexer references —
  2; `launchd` — 4; `/Users/<owner>`
  paths — 8 commits / 18 occurrences. Blob-level occurrence counts on
  main history: smoke repo 311, ops repo 35, former name 33, selector
  repo 14, worker repo 12, infra repo 12, six ops-adjacent repo names
  ~17, container manager 35, terminal multiplexer 43, owner
  scratch-clone directory names (one embedding the former
  name) 8.
- **Pre-scrub document versions**: real source names appeared in
  `docs/source-workflow-audit.md` (introduced `4ff12a5`, `d756186`,
  `d5abffd`), `docs/provenance.md`, `docs/v0.1.md`,
  `docs/module-contracts.md`, and
  `docs/proposal-wayfinder-retro-ratchet-followup.md` before the scrub
  commits `b12013b`, `d5ba752`, `0248cfb`, `5847f52`, `6be6b35`. The
  names remain reachable in those blobs.
- **Local checkout paths of all four source repos**
  (`/Users/<owner>/.../<repo>` for ops, worker, selector, infra) were
  committed in `d756186` and remain in the audit file's embedded
  manifest (`docs/source-workflow-audit.md:577-597`).
- **Owner-local absolute paths** in historical
  `fixtures/v0.3-doc/{profile/config.toml,store.config.json}` and
  `docs/evidence/u8.4.md` (sanitized at HEAD by `331eb5c`, `d27bc52`,
  `2309d7a`, `761ad38`; the originals persist in history).
- **Author/committer identities baked into 1,017 commits** — these
  cannot be removed without rewriting history:
  - 253 commits authored as the scenario worker identity
    (`s1-worker@localhost`),
  - 112 as identities named after the worker repo
    (`<worker-repo>@localhost` and two variants),
  - 4 as `opencode-worker`, 2 as a bot identity also named after the
    worker repo,
  - 573 under the owner's personal Gmail address / full name.
- **Commit subjects**: e.g. `dd38de6` names the worker repo in the
  context of remote lanes and a private model provider;
  `80d821b`/`aa5e454`/`2916a27` name agent runtimes and providers.
  `s1-bridge` subjects are internal codenames.

### 2.3 GitHub metadata — the largest uncontrolled surface

- **Milestone #2** ("v0.1 scope") description names all four source
  repositories verbatim and carries a selector-tool HTML marker
  (`<!-- <selector-repo>:plan=… -->`). Editable in place; no history
  rewrite possible — edit or delete.
- **Issue bodies** (261 issues; counts are per-issue, overlapping):
  selector repo name 29, worker repo name 21, ops repo name 12, infra
  repo 9, former name 6, smoke repo 5, container-manager references 1,
  reviewer-App slug 1, provider-specific ops context (a private model
  provider name and a public agent runtime) 5, the infra repo's CLI
  name 1; owner-home paths in 7 issues — `~/.config/…`, an ops-state
  directory, a `~/.cargo` toolchain dir, and a `~/.taisan-v02-live`
  live-run state dir. Examples: infra-repo and
  ops-repo issue links, remote-run evidence-bundle paths under the
  ops-state directory, container names prefixed with the worker-repo
  abbreviation.
- **Issue comments** (search index `in:comments`): worker repo name 40
  issues, selector repo name 38, former name 28, ops repo name 3;
  owner-home paths — `~/.config/…`, the ops-state directory, and
  `~/.taisan-v02-live` run dirs.
- **PR bodies** (163 PRs): worker repo name 44, selector repo name 39
  (including `<selector-repo> verify` provenance lines and
  `<!-- <selector-repo>-propose {…} -->` machine markers), former name
  4, ops repo name 4, owner-home paths in 4 — `~/.config/…`, the
  ops-state directory, and generic tool dirs (`~/.local`, `~/.agents`).
- **PR review comments**: 383 matching lines — selector repo name 327
  occurrences, worker repo name 6, smoke repo 4; owner-home paths —
  `~/.config/…` and a quoted `~/s1-u84` store path. (Review-bot finding
  comments also echo `~/src` / `~/s1-*` strings quoted from this audit;
  machine-generated metadata, not a further leak class.)
- **Pushed branch names**: ~73 `origin/*` refs; two carry
  provider/selector hints (`aw-opencode-config`,
  `selector-issue-15-…`). No pushed branch carries a source repo name.
- **Clean surfaces**: labels contain no private names; repository
  description/topics/homepage are clean; no releases; wiki and
  discussions disabled; `.github/workflows/rust.yml` is clean.

## 3. Credential and config scan

- **Tree**: pattern scan for token/key shapes (`ghp_*`, `github_pat_*`,
  `gh[ous]_*`, `sk-*`, `xox*`, `AKIA*`, `glpat-*`, JWT-shaped `eyJ*`,
  PEM blocks, `Bearer` values, password/secret assignments). Result:
  **no real credentials**. All hits are test sentinels
  (`crates/workloop-config/tests/profile_contract.rs:304`,
  `crates/workloop-core/src/credential.rs:116`,
  `crates/taisan-worker-claude/tests/wire_contract.rs:1528`) or variable
  names. `fixtures/v0.3-doc/profile/config.toml:50` references a
  credential by `file:` path only — correct pattern.
- **History**: identical scan over `git log -p origin/main`. Only the
  same sentinel strings appear — including `ghp_sentinel…` inside the
  sanctioned audit file. **No committed secrets found.**
- **Identifying runtime config** (not secrets, but owner-specific): the
  reviewer App ids and the smoke repo's numeric ids listed in §2.1, and
  owner-home state paths in issue bodies (§2.3).

## 4. History vs. current tree

A clean HEAD certifies nothing here. Divergence table:

| Leak class | HEAD tree | `origin/main` history | Tracker metadata |
| --- | --- | --- | --- |
| Four source repo names | sanctioned audit file (canonical); worker-repo name also leaks via `~/.config/<worker-repo>/` paths and inside the smoke-repo name | pre-scrub blobs of 5+ docs; 40+ commits | milestone #2; ~50 issues/PRs |
| Smoke repo name | 29 files / 161 hits | 311 occurrences | 5 issues, review comments |
| Former repo name (self-rename — not a source repo) | `name-migration.md` | 33 occurrences | 28 issues (comments), 4 PRs |
| Owner-local paths | no `/Users/<owner>` absolute paths outside the sanctioned audit file (tree scan: only a test literal, see Appendix); `~/`-relative owner paths do occur — the worker-repo config path (counted above), `~/s1-*` run dirs in `docs/evidence/` and `docs/s1/`, and `~/src` checkout-root strings in `docs/extraction-coverage.json` | 18 `/Users/<owner>` occurrences | `~/.config`, ops-state dir, `~/` tool dirs, and `~/` run dirs in issues/PRs/comments |
| Ops-adjacent repo names | `extraction-coverage.json` only | +1 file | none found |
| Author identities | n/a | **all 1,017 commits** | n/a |
| Real secrets | none | none | none found |

## 5. Residual exposure — release blockers and owner decisions until dispositioned

1. **Release blocker** — every `github.com/<owner>/<smoke-repo>` link
   and every `<smoke-repo>#N` issue/PR citation in `docs/s1/`,
   `docs/evidence/`, and `fixtures/s1/` resolves only for the owner;
   publicly they 404 and disclose the worker repo's name.
2. **Release blocker** — `scripts/s1/` scenario tooling and
   `scripts/s1/worker-shim*.sh` clone the private smoke repo — the
   S1/U8 materials cannot run publicly as written.
3. **Release blocker** — `scripts/s1/mint-app-token.py` depends on an
   owner-local config path (`~/.config/<worker-repo>/…`).
4. **Release blocker** — `docs/extraction-coverage.json` retains
   partially-redacted and unredacted private names and owner-local
   checkout paths (§2.1).
5. **Owner judgment call, not a release blocker** —
   `docs/name-migration.md` names the former repository (§2.1): the
   repository's own self-rename rather than a source-name leak, and
   the rename redirect discloses it on publication regardless.
6. **Release blocker** — this document itself is a map of every
   remaining leak location and must be excluded from the published
   tree or dispositioned with `source-workflow-audit.md` (§6); merging
   this pull request places that map in `main`'s reachable history and
   ratifies keeping it tracked, without replacing the disclosure
   review required before publication.
7. **Release blocker** — milestone #2's description and tracker
   bodies/comments preserve the private names even if the tree and
   history were fully cleaned; metadata cleanup is required under any
   publication route.
8. **Release blocker** — `docs/s1/live-authorization.md`,
   `docs/u3.6e-authorization.md`, and the u8 live-evidence records are
   owner-operations documents; useful as requirements/invariants but
   drafted as personal ops records.

## 6. Options for owner decision

No action was taken. Three routes:

- **A — Publish in place, keep history.** Requires first: fix the
  `extraction-coverage.json` redaction misses; decide `name-migration.md`
  and the smoke-repo surface (generalize or accept); then rewrite all
  1,017 commits to strip names, paths, and author identities; remove or
  scrub the owner-controlled audit records in the published tree (§5
  item 6); edit milestone #2; edit or delete ~100+ issues/PRs/comments.
  Highest effort and still leaves residual exposure in
  unreachable-but-cached objects.
- **B — Fresh publication.** Publish a new repository from a sanitized
  snapshot (squashed or grafted history with neutral author identity),
  keep this repository private as the ops record. Milestone #8's
  evidence links must then be re-pointed; the tracker stays private,
  which resolves the metadata surface entirely. Cleanest isolation;
  loses per-commit history.
- **C — Publish in place, truncated history.** Force-push a single
  squashed root commit to `main`, delete or archive pushed branches,
  scrub tracker metadata as in A. The squashed root publishes the
  current tree as-is, so the owner-controlled audit records
  (`docs/source-workflow-audit.md` and this file) must be removed or
  scrubbed before the squash — otherwise the leak index ships (§5
  item 6). Keeps issue/PR numbers meaningful for the audit record
  while removing blob history. Still requires the metadata pass.

Whichever route is chosen, `docs/source-workflow-audit.md` needs its own
disclosure review first — it is designed to retain the names this audit
flags — and the same holds for this document, which maps where they
survive.

## Appendix — verification commands

- Tree scans: `rg -l -i '<name patterns>' --hidden -g '!.git'` over the
  worktree; counts via `rg -o | wc -l` (e.g. smoke repo: 161 hits / 29
  files; `s1-reviewer`: 57).
- Owner-path scans: `rg -n '/Users/'` outside the sanctioned audit
  files hits only the test literal in
  `crates/workloop-exec/src/containment.rs:1432` (a pattern string, not
  an owner path); `rg -o '~/s1-[A-Za-z0-9._-]+'` extracts only `s1-*`
  scenario names (`s1-stagec-*`, `s1-u82`, `s1-u83`, `s1-success-*`),
  embedding no source names; a wider `~/`-relative scan additionally
  surfaces the `~/src` checkout-root strings in
  `docs/extraction-coverage.json:6636`, listed as a redaction miss in
  §2.1. Tracker-side: `gh api repos/shimpeiws/taisan/{issues,pulls,
  issues/comments,pulls/comments} --paginate` piped to
  `rg -o '~/[A-Za-z0-9._-]+'` yields `~/.config`, the ops-state dir,
  `~/.local`, `~/.cargo`, `~/.agents`, `~/.taisan-v02-live`, a quoted
  `~/s1-u84` path, and review-bot `~/src`/`~/s1-*` echoes (§2.3).
- History scans: `git log origin/main -S <term> --oneline`,
  `git log -p origin/main | rg -o 'shimpeiws/[A-Za-z0-9._-]+' | sort |
  uniq -c`, `git log --format='%an <%ae>' | sort | uniq -c`.
- Secret scan: `rg` patterns `ghp_*/github_pat_*/gh[ous]_*/sk-*/xox*/
  AKIA*/glpat-*/eyJ…JWT/-----BEGIN/Bearer` over tree and `git log -p`.
- Metadata: `gh issue view 368`, `gh api repos/shimpeiws/taisan/
  {milestones,issues,pulls,pulls/comments} --paginate`, `gh api
  search/issues?q=repo:shimpeiws/taisan+in:comments+<term>`,
  `gh repo view --json …`, `gh release list`, `gh label list`.
- `cargo fmt --check` not run: no code was touched.
