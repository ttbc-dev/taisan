# Release audit — public content, history, and repository metadata

Audit for issue #368, performed 2026-11-24 against `origin/main` at
`d81f064`. Read-only: no files were modified, no history rewritten, no
metadata changed, no visibility touched. Raw findings are cited by
file:line and leak class; no secret value or private repository name is
reproduced here — names are described by role using the legend in
`AGENTS.md` / `docs/source-workflow-audit.md`.

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
the single permitted exception per `AGENTS.md`. It correctly self-declares
that publication requires a disclosure review covering it and the git
history (`docs/source-workflow-audit.md:23-33`).

The scan confirms the real names of the ops, worker, selector, and infra
repos, two ops-side tool names (a terminal multiplexer and a container
manager), and `/Users/<owner>` checkout paths appear inside that file as
permitted — **but the same identifiers also leak outside it**, so the
exception does not hold in practice:

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
    owner-specific ops context).
- **`docs/name-migration.md:15,29`** — records the former repository
  name verbatim. Judgment call for the owner: once published, GitHub's
  rename redirect discloses the former name anyway, but the milestone
  text requires public material to be free of private-repository
  references.
- **The smoke repo is pervasive**: its `<owner>/<smoke-repo>` name
  appears 161 times across 29 files — `docs/s1/*` (9 files),
  `docs/evidence/u3.6d.md`, `u3.6e.md`, `u8.1*.md`, `u8.2`–`u8.5`,
  `docs/remote-executor.md:20`, `fixtures/s1/*`,
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
- **Identifying runtime configuration**: the dedicated reviewer GitHub
  App's slug, app id, installation id, and bot user id
  (`docs/evidence/u8.1-live-stage-c.md:37-42`,
  `docs/s1/stage-c-runbook.md:42-46`); the smoke repo's numeric and node
  ids (`docs/evidence/u8.1-live-stage-c.md:52`); repository secret names
  (`S1_REVIEWER_*`, `docs/evidence/u8.1-live-stage-c.md:42`); the
  scenario git identity `s1-worker` hard-coded in
  `scripts/s1/worker-shim.sh:33-34` and `worker-shim-nc.sh`.
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
  container-manager references — 4; `launchd` — 4; `/Users/<owner>`
  paths — 8 commits / 18 occurrences. Blob-level occurrence counts on
  main history: smoke repo 311, ops repo 35, former name 33, selector
  repo 14, worker repo 12, infra repo 12, six ops-adjacent repo names
  ~17, owner scratch-clone directory names (one embedding the former
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
  manifest (`docs/source-workflow-audit.md:575-595`).
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
  reviewer-App slug 1, provider-specific ops context (`mimo`/`opencode`)
  5, the infra repo's CLI name 1; owner-home paths (`~/.config/…` and
  an ops-state directory) in 7 issues. Examples: infra-repo and
  ops-repo issue links, remote-run evidence-bundle paths under the
  ops-state directory, container names prefixed with the worker-repo
  abbreviation.
- **Issue comments** (search index `in:comments`): worker repo name 40
  issues, selector repo name 38, former name 28, ops repo name 3.
- **PR bodies** (163 PRs): worker repo name 44, selector repo name 39
  (including `<selector-repo> verify` provenance lines and
  `<!-- <selector-repo>-propose {…} -->` machine markers), former name
  4, ops repo name 4, owner-home paths in 3.
- **PR review comments**: 383 matching lines — selector repo name 327
  occurrences, worker repo name 6, smoke repo 4.
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
| Former repo name | `name-migration.md` | 33 occurrences | 28 issues (comments), 4 PRs |
| Owner-local paths | none at HEAD | 18 `/Users/<owner>` occurrences | `~/.config` and an ops-state dir in issues/PRs |
| Ops-adjacent repo names | `extraction-coverage.json` only | +1 file | none found |
| Author identities | n/a | **all 1,017 commits** | n/a |
| Real secrets | none | none | none found |

## 5. Residual exposure — release blockers until dispositioned

1. Every `github.com/<owner>/<smoke-repo>` link and every
   `<smoke-repo>#N` issue/PR citation in `docs/s1/`, `docs/evidence/`,
   and `fixtures/s1/` resolves only for the owner; publicly they 404 and
   disclose the worker repo's name.
2. `scripts/s1/` scenario tooling and `scripts/s1/worker-shim*.sh`
   clone the private smoke repo — the S1/U8 materials cannot run
   publicly as written.
3. `scripts/s1/mint-app-token.py` depends on an owner-local config path
   (`~/.config/<worker-repo>/…`).
4. `docs/extraction-coverage.json` retains partially-redacted and
   unredacted private names (§2.1).
5. `docs/name-migration.md` names the former repository (§2.1).
6. Milestone #2's description and tracker bodies/comments preserve the
   private names even if the tree and history were fully cleaned —
   metadata cleanup is required under any publication route.
7. `docs/s1/live-authorization.md`, `docs/u3.6e-authorization.md`, and
   the u8 live-evidence records are owner-operations documents; useful
   as requirements/invariants but drafted as personal ops records.

## 6. Options for owner decision

No action was taken. Three routes:

- **A — Publish in place, keep history.** Requires first: fix the
  `extraction-coverage.json` redaction misses; decide `name-migration.md`
  and the smoke-repo surface (generalize or accept); then rewrite all
  1,017 commits to strip names, paths, and author identities; edit
  milestone #2; edit or delete ~100+ issues/PRs/comments. Highest effort
  and still leaves residual exposure in unreachable-but-cached objects.
- **B — Fresh publication.** Publish a new repository from a sanitized
  snapshot (squashed or grafted history with neutral author identity),
  keep this repository private as the ops record. Milestone #8's
  evidence links must then be re-pointed; the tracker stays private,
  which resolves the metadata surface entirely. Cleanest isolation;
  loses per-commit history.
- **C — Publish in place, truncated history.** Force-push a single
  squashed root commit to `main`, delete or archive pushed branches,
  scrub tracker metadata as in A. Keeps issue/PR numbers meaningful for
  the audit record while removing blob history. Still requires the
  metadata pass.

Whichever route is chosen, `docs/source-workflow-audit.md` needs its own
disclosure review first — it is designed to retain the names this audit
flags.

## Appendix — verification commands

- Tree scans: `rg -l -i '<name patterns>' --hidden -g '!.git'` over the
  worktree; counts via `rg -o | wc -l` (e.g. smoke repo: 161 hits / 29
  files; `s1-reviewer`: 57).
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
