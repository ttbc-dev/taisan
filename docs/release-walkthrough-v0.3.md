# Release walkthrough outline — validated v0.3

Status: **draft outline, pending the frozen candidate** (issue #370).
This document sketches the sanitized, repeatable walkthrough the
release will ship so a reader can see verified behavior end to end. It
fixes the walkthrough's shape, its evidence sources, and its
sanitization rules; it does **not** produce the walkthrough itself —
the recorded artifact (transcript or capture) is cut against the
pinned candidate commit once the freeze lands, and nothing here claims
a walkthrough already exists. No private source repository, live-run
target, or owner-local path is named: the acceptance target is *the
isolated repository* and private counterparts appear by role only.

## Two artifact forms — pick per section, never blur them

The walkthrough has two honest forms, and every section below states
which it is:

- **Retained run** — a sanitized excerpt of the owner-run live
  evidence already recorded in
  [release-acceptance-v0.3.md](release-acceptance-v0.3.md) (the three
  retained run records `RUN-RECORD-2026-11-23`, `-REVERIFY`,
  `-REVERIFY2` and their durable stores/envelopes, held owner-side).
  A retained-run section quotes evidence that already exists; it is
  reviewable now but it is bound to the candidate those runs exercised
  (`ca45ac8`), so anything the freeze changes must be re-marked.
- **Live demonstration** — a fresh run on the frozen candidate commit
  that a reader could in principle re-execute from the release's own
  instructions. It proves reproducibility but requires real
  credentials, a real Worker runtime, and an isolated repository —
  owner authorization, exactly like the acceptance runs.

The current plan is a hybrid: retained-run excerpts where the
acceptance record already holds the proof (each marked with its run
locator), plus a live-demonstration segment only if the owner
re-authorizes a run on the frozen commit. A section whose only
evidence predates the freeze is labelled "retained — predates
candidate" rather than presented as current.

## Proposed walkthrough spine

Each step names its evidence source and which form supplies it.

1. **Installation and preflight** *(repeatable; fixture-backed)* —
   clean build, `taisan init`, `taisan check`, installation record —
   the [v0.3 installation guide](v0.3-installation.md) already carries
   captured non-live transcripts under `fixtures/v0.3-doc/`. This
   segment is repeatable by any reader on Linux *without* live
   authorization because it exercises the fixture surfaces only; the
   walkthrough says so explicitly.
2. **Issue-to-PR** *(retained run)* — manual `submit`, Coding
   dispatch on the `taisan.worker.request/2` wire, handoff,
   supervisor verification on the produced commit, `push_ref` +
   `open_change_request` applied. Evidence locator:
   release-acceptance §3 rows (run 3 clean path — publish intents
   `act-abf9cb71-…`, `act-20c0d696-…` → PR #64 on the isolated
   repository). The retained excerpt shows the command envelopes and
   `work-detail` projection, not the private host objects.
3. **A real review finding** *(retained run)* — the operator-posted
   inline finding on the real change request (`name=None` →
   `AttributeError`, run 1, PR #62): the walkthrough shows the finding
   entering as a real review occurrence and the recorded
   `ReviewRound`/judgment — the workflow's finding→judgment path, not
   a scripted review.
4. **Fix and reassessment** *(retained run)* — bounded Fix Invocation
   producing `5c5cc6e`, republished head `35ce3cb`, fresh
   verification/review checks, `assess_completion` → `succeeded`
   (release-acceptance §3 rows). This is the segment that
   distinguishes the demo from a toy: the finding, fix, republish, and
   reassessment all happened against real host state.
5. **Operator inspection** *(retained run + repeatable fixture
   commands)* — `work-detail`, `work-list`, `attention`, `capacity`,
   `anomalies`, `evidence`: the walkthrough shows the operator-facing
   read surface at each stage. The fixture-captured outputs in
   `fixtures/v0.3-doc/` make this segment repeatable; the retained run
   shows it against real Work.
6. **Recovery** *(retained run)* — stop/restart with no duplicate
   writer or effect (runs 2–3), bounded recovery to honest terminal
   states (`artifact_recovery_exhausted`, cancellation after
   retry-budget exhaustion), and — recorded honestly — the failed
   store-recovery attempt (`store.db.corrupt-20261123` →
   `store.db.recovered` inconsistent; release-acceptance §4). The
   walkthrough includes the negative result; hiding it would
   overstate the recovery story.
7. **Boundaries, stated not demonstrated** — what the walkthrough
   deliberately does not show: scheduled intake (unexercised, #271),
   remote execution and remote review (follow-up, not in this
   release), automatic merge/deploy (not a capability). One line
   each, cited to the release notes' known-limitations section.

## Sanitization rules for the produced artifact

- The isolated repository, its PRs, and all private run state are
  described by role and issue/PR number — the same locator convention
  release-acceptance-v0.3.md uses. No private host names, paths, or
  actor identities beyond what the acceptance record already
  publishes.
- Credential surfaces show reference names (`env:NAME`,
  `file:/abs/path`) only — never values; a captured surface
  containing a credential value is an incident, not a screenshot.
- Owner-side run records stay owner-side: the walkthrough quotes
  bounded excerpts (command envelopes, `work-detail` lines, intent
  ids) rather than shipping the private stores.
- Whether the produced artifact is committed into the published tree
  is decided with the publication route — under route B/C a
  walkthrough citing issue/PR numbers keeps working (numbers remain
  valid); a decision this document records but does not make.

## Open items before the walkthrough is produced

- [ ] Frozen candidate commit exists (blocked on #365/#367/#368/#369).
- [ ] Owner decision: retained-run only, or retained + fresh
      live-demonstration segment on the frozen commit.
- [ ] Owner decision: artifact form — committed doc, retained
      transcript bundle, or recording — and its publication treatment.
- [ ] Re-record any retained-run segment whose candidate differs from
      the frozen commit, or mark it "retained — predates candidate".
