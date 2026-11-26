# Release walkthrough — validated v0.3 (retained run)

Status: **retained-run walkthrough, produced for issue #370**. This
document is the sanitized walkthrough the release ships so a reader
can see verified behavior end to end. It is a **retained run, not a
live demo**: every step quotes evidence already recorded by the
owner-run acceptance runs — nothing here is a fresh execution, and
re-executing it requires the owner-authorized environment described
below. The evidence predates the release freeze: the retained drivers
span `28afb72`/`179f6fe`/`c445feee` (runs 4–6), earlier heads (runs
1–3), and `81c62cf` (the pilot install); none is the frozen candidate —
every section is
labelled "retained" and is re-marked at freeze (see the closing
checklist). A fresh live-demonstration segment on the frozen commit
remains a separate owner decision (closing checklist); this document
deliberately ships the retained-run form only.

## Evidence sources and sanitization

Three owner-side record sets supply every quoted fact:

- **Clean install (#367)** — `RUN-RECORD-2026-11-25-CLEAN.md` plus 26
  sanitized files under `acceptance-v03-clean/` (owner-side):
  fresh OrbStack guest `taisan-clean` (Ubuntu noble arm64),
  documented-runbook install of `81c62cf`.
- **Scheduled pilot (#271)** — `RUN-RECORD-2026-11-26-PILOT.md` plus
  66 sanitized files under `acceptance-v03-pilot/` (owner-side;
  guest originals at `/opt/taisan-v03/evidence/`): the same
  `taisan-clean` guest, production host driver built from `81c62cf`,
  `[intake] enabled`, real reference Worker `taisan-worker-claude`
  `0.0.0` driving `claude-code` `2.1.331 (Claude Code)`, in-process
  `github` host driver, capacity 1.
- **Manual runs (#270)** — `acceptance-v03-run5/` and
  `acceptance-v03-run6/` under the same owner-side root, with the
  repo-facing record
  [evidence/v0.3-manual-acceptance.md](evidence/v0.3-manual-acceptance.md);
  owner approval at issue #270 comment `7171973494`.

The live target throughout is *the isolated repository*
`shimpeiws/coding-worker-smoke` — private/owner-only: issue and PR
numbers below cite its tracker and resolve only for the owner.
Sanitized filenames (`NN-*.json`, `*.txt`, `*.log`) are locators
inside the retained evidence copies; credential surfaces show
reference names only (`env:`/`file:`), never values. The §14.5
verification sandbox posture in the pilot guest was the stricter
toolchain+caches+crates.io allowlist — recorded so the evidence is
not over-read.

## 1. Installation and preflight — retained clean run

From `RUN-RECORD-2026-11-25-CLEAN.md` on a guest created empty for
the purpose:

- The published runbook
  ([v0.3-installation.md](v0.3-installation.md)) was followed
  verbatim; every step either matched its documented output or
  failed exactly as documented (typed refusals, never silent
  divergence). Doc gaps found during the run were fixed and merged
  via PR #465.
- `cargo install` produced the four packaged binaries: `taisan`,
  `taisan-tui`, `public-issue-selector`, `taisan-worker-claude`.
- `taisan check` reported all stages `pass`; `check --probe` failed
  closed at `capability.unknown` — the documented limitation.
- All 10 containment/sandbox probes passed under bubblewrap.
- A full operator cycle ran end to end on the fixture binding:
  submit → contained worker launch → report ingestion → artifact
  handoff → `waiting` at the publish boundary (the fixture driver
  emits no `CapabilityReport`, so fixture runs stop there by
  design).
- The real Worker boundary was proven without spend: the
  `taisan-worker-claude` `describe` declaration and an offline
  `select` from `public-issue-selector` both returned correctly.
- Sole gap the run could not close: `git clone` of the public URL —
  the repository was still private, so the step stays unverifiable
  until the owner flips visibility. The #438 ruling decided in-place
  publication, so the gap no longer blocks the release decision — it
  closes at publication, not by more testing.

## 2. Scheduled admission — retained run

With `[intake] enabled` on the clean install, the scheduler — not
`submit` — made every dispatch decision:

- **First tick failed closed.** `scheduled_startup` intake-1
  returned `adapter_source_unavailable` (`09-intake-1-detail.json`):
  `init` had written the numeric repository id into `config.toml`,
  which the selector's GitHub backend cannot read — filed as #468.
  The operator set the id to the `owner/name` slug
  (`02b-check-after-id-fix.txt`) and intake proceeded.
- **7 Works admitted across six admitting scheduled ticks** —
  intake-2 accepted two Works in one run (w-1, w-2), intake-3, -4,
  -13, -14, -16 one each (`38-intake-runs-final.json`), each a
  distinct real Issue on the isolated repository (intake-1's
  `scheduled_startup` tick failed closed
  `adapter_source_unavailable`, #468 — above). `issue_claims`
  held single ownership of every selected Issue; no Work was
  dispatched twice.
- **Scheduler scenarios exercised with durable rows:** `queue_full`
  skips while capacity was occupied (intake-5..9), replayed
  candidates re-presented each tick, a selector outage surfacing as
  `exchange_launch` on both scheduled and manual intake (evidence
  `26`/`29`), and disable → `scheduled_reenable` (intake-13).
- **Replay starvation, observed and worked around:** issue #70
  replayed into the single `max_candidates` slot every tick,
  starving new proposals until `pending_queue_limit` was raised 2→4
  (`38-intake-runs-final.json`) — filed as #471.

## 3. Two Works through the verified path — retained run

w-6 (issue #74 → PR #77) and w-7 (issue #75 → PR #78) ran the full
chain to a verified Outcome:

1. Scheduled intake admitted the Work; a Coding Invocation on the
   `taisan.worker.request/2` wire materialized its immutable
   request and produced a commit under the declared commit
   identity.
2. The supervisor re-ran the declared verification argv against the
   produced commit in a detached worktree — `pass` — then an
   `invariant_check` Invocation ran read-only and completed.
3. Publication applied as recorded intents with receipts: `push_ref`
   `applied` (`ref_published`, authenticated actor) then
   `open_change_request` `applied` — PR #77 and PR #78 on the
   isolated repository.
4. A bound-actor review round sealed on each head.
5. `assess_completion` recorded `complete:true` and the Execution
   settled **`succeeded`** — the same terminal code run 6 reached
   on e-5 in the manual chain
   ([evidence/v0.3-manual-acceptance.md](evidence/v0.3-manual-acceptance.md)
   §3 M-7).

Per-Work wall time and usage counters are recorded honestly in
`43-per-work-usage.json`; model-provider cost telemetry is not
recorded — unknown stays unknown.

## 4. Operator inspection — retained run

The operator read surface projected the same durable chain
throughout the pilot:

- `work-list` / `work-detail` showed each Work's admission,
  Invocations, intents, and terminal state — including the paused
  and cancelled Works below (`32-work-detail-w5-paused.json`,
  `24-work-detail-w2-pre-cancel.json`, `wd-w-1..5-final.json`).
- `attention` carried only evidence-backed items — each
  `paused_judgment` from a guarded publish refusal — and ended
  empty (`41-attention-final.json`).
- `capacity`/`anomalies` showed single-slot occupancy and no
  doubled reservations or double dispatch
  (`36-anomalies-mid.json`).
- Operator acts were durable commands with accepted envelopes
  (cancel, resume, disable/enable intake), not store edits.

## 5. Inspection and recovery under real restarts — retained run

The pilot produced the restart-clean recovery evidence the manual
runs lacked — twice, once planned and once not:

- **Planned SIGINT restart:** the in-flight `inv-4` was orphaned at
  stop and re-minted as `inv-5` on relaunch — one `Done`, one out
  dir, no duplicate writer.
- **Unplanned guest VM restart:** the `taisan-clean` guest was
  restarted mid-run. Orphaned `running` `inv-11` was re-minted as
  `inv-12`, `scheduled_startup` intake resumed on the next tick,
  and the store came back consistent (`loop-03.log`,
  `32-work-detail-w5-paused.json`). No duplicate writer, no
  duplicated effect — `issue_claims` single ownership held across
  the restart.
- This contrasts with, and supersedes, the one earlier negative
  result: the run-1/2 `store.db.corrupt-20261123` recovery attempt
  that produced an internally inconsistent store (release-acceptance
  §4 — retained honestly as a recovery-path finding).

## 6. Honest terminals — retained run

The walkthrough keeps the failures; a walkthrough that hid them
would overstate the release:

- **w-1, w-3, w-4, w-5:** publish refused `stale_head` — the
  deterministic `work/w-N` branch names collided with branches left
  by earlier installations on the shared repository. Guarded
  refusal behaved correctly (never overwrote a foreign head);
  bounded transport retries elapsed (4 each) and every one of the
  four paused `paused_judgment` on the `transport_exhausted`
  decision ("transport retry bound reached"), then was
  operator-cancelled while paused — w-1/w-3/w-4 after an
  `acknowledge`, w-5 directly (`15-acknowledge-w1.json`,
  `ack-w-3.json`/`ack-w-4.json`, `cancel-w-*.json`,
  `wd-w-{1,3,4,5}-final.json`) — filed as #470.
- **w-2:** every verification reported `cache_unreadable` — the
  `caches/cargo-registry` symlink dangled inside the sandbox —
  leaving the lane `waiting/ci` on a sticky `inconclusive` item
  with no re-drive path; `resume`/`retry` correctly refused and the
  Work was cancelled after the symlink was replaced with a real
  directory (`22-cache-fix.txt`, `23-resume-w2.json`,
  `24-work-detail-w2-pre-cancel.json`) — filed as #469.

## 7. Boundaries — stated, not demonstrated

What the walkthrough deliberately does not show, one line each,
cited to the release notes' known-limitations section
([release-notes-v0.3.md](release-notes-v0.3.md)):

- Remote execution and remote review — on the candidate as v0.4
  groundwork, unexercised end to end; follow-ups.
- Automatic merge or deploy — publication ends at a reviewed change
  request; every run finished with its PR open and nothing merged.
- Pilot scale — 7 admitted / 2 succeeded on one installation is a
  tiny sample; no cost or quality inference is drawn.
- The `review` Worker role is declared and contract-tested; the
  live review findings were bound-actor-posted, not Worker-produced.

## Sanitization rules applied

- The isolated repository is named verbatim
  (`shimpeiws/coding-worker-smoke`) and its issue/PR numbers and all
  private run state are cited by number — private/owner-only
  references that resolve only for the owner, matching the
  convention in release-acceptance-v0.3.md. No private host names,
  paths, or actor identities beyond what the acceptance
  record already publishes.
- Credential surfaces show reference names (`env:NAME`,
  `file:/abs/path`) only — never values; a captured surface
  containing a credential value is an incident, not a citation.
- Owner-side run records stay owner-side: this walkthrough quotes
  bounded locators (command envelopes, `work-detail` rows, intent
  ids, sanitized filenames) rather than shipping the private
  stores.

## Remaining steps before this walkthrough ships

- [ ] Frozen candidate commit exists (see
      [release-freeze-checklist.md](release-freeze-checklist.md)).
- [ ] Each retained section re-marked: anything the freeze changes
      is labelled "retained — predates candidate" or re-recorded.
- [ ] Owner decision: whether to add a fresh live-demonstration
      segment on the frozen commit (re-authorization required).
