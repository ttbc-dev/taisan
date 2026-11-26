# Release acceptance: candidate evidence and defect disposition

Status: acceptance evidence record for the **Initial public release —
validated v0.3** milestone (issue #365). This document assembles the
release decision around the retained v0.3 acceptance evidence; it does
not authorize publication, any merge or deploy, credential changes,
repository visibility changes, or any new live run, and it does not
itself approve any unverified claim. Per `AGENTS.md` no private
*source* repository is named: private counterpart systems appear by
role only. The live-run target — *the isolated repository* — is named
verbatim as `shimpeiws/coding-worker-smoke`, a **private/owner-only**
reference under the #438 ruling and
[docs/provenance.md](provenance.md): its issue/PR citations resolve
only for the owner.
Issue/PR numbers and revision SHAs appear throughout as
owner-verifiable evidence locators.

Record history: first recorded 2026-11-23 against candidate `ca45ac8`;
refreshed 2026-11-25 against `119dcb7` (§1). The refresh re-dispositions
every tracked defect against the new candidate and adds the issue-state
ledger (§8); the §3–§5 evidence citations are retained unchanged —
the delta admits no re-exercise, and the reuse caveat in §3 applies.

## 1. Candidate commit

```text
git log -1 origin/main
119dcb72ce047bdcc4c44fb4a7915ffe2d7fd6ee
Merge pull request #435 from shimpeiws/issue-376-agent
```

The candidate is `origin/main` HEAD at record time (2026-11-25) —
**provisional**: it moves as remaining PRs merge (at record time PR
#427 for #323 and PR #431 for #419 were open — #427 has since merged
as `52a5493`, post-pin; #431 stays open). Its history contains
every change the acceptance evidence depends on:

| Content | Merge | What it carries |
| --- | --- | --- |
| Live-run finding fixes | `bfa0412` (PR #413) | `artifact.json` manifest from the worker shim (#410), git-push transport for remote ref writes (#411), declared CA trust store and commit identity (#412) |
| Publish-leg fix | `4464c4f` (PR #421) | git-transport push authenticated as `Basic x-access-token` (#420) |
| Design docs | `9e01af1` (PR #414, #372), `f410092` (PR #415, #373), `ca45ac8` (PR #417, #416) | remote executor boundary, reference environment selection, handoff manifest custody |

Admitted since the previous record (`ca45ac8` → `119dcb7`, oldest
first):

| Content | Merge | What it carries |
| --- | --- | --- |
| Bounded Worker report ingestion | `ddf92fe` (PR #418, #262) | bounded v2 Worker `report.json` ingested as validated evidence; lost-`Running` report ingestion and pending-report cleanup |
| Remote executor transport contract | `4b41907` (PR #423, #374) | versioned remote execution / custody contract design |
| This ledger | `29db0f7` (PR #424) | the first record of this document (#365) |
| Spent-deadline ordering fix | `84525a0` (PR #426, #363) | bound check before paid enforcement cost; dedicated `deadline` marker digest |
| Runtime-auth cleanup | `4df7087` (PR #425, #364) | staged credential removed on refusal, re-resolve, stage failure, and drop |
| Bounded absence proofs | `d81f064` (PR #428, #344) | launch-group clearance, bounded `absence_unprovable` escalation, recoverable outcome |
| Remote bindings; verdict bounds | `5ba0b4b` (PR #429, #375), `8465cb9` (PR #430, #338) | versioned executor wire types and durable remote bindings; bounded verdict-report member sets |
| License prep | `aff8721` (PR #433, #369) | dependency license inventory and selection prep |
| Remote executor endpoint | `2c271f0` (PR #436, #377) | public remote executor reference endpoint |
| Exposure audit | `b0b81f6` (PR #432, #368) | public-release exposure audit report |
| Bounded remote transfer | `119dcb7` (PR #435, #376) | bounded remote input transfer and quarantined artifact collection |

## 2. Evidence classes and record locations

Every citation below is labelled by claim class, per the evidence
rules in [evidence gates](evidence-gates.md) and the
[owner-run checklist](v0.3-owner-acceptance-checklist.md):

- **Owner-run live evidence** — produced by the production host driver
  against the isolated repository with a real conforming Worker. The
  retained record root is the owner-side live-run directory (outside
  this repository): `RUN-RECORD-2026-11-23.md`,
  `RUN-RECORD-2026-11-23-REVERIFY.md`,
  `RUN-RECORD-2026-11-23-REVERIFY2.md`, the `state/`,
  `state-reverify/`, and `state-reverify2/` stores (SQLite
  `store.db`, `bodies/`, `workspaces/`, preflight and egress records),
  and the retained command envelopes under `evidence/` (`07-submit.json`,
  `50-…`–`54-…`, `60-reverify2-submit.json`).
- **Fixture/contract evidence** — in-repo contract tests, fixture
  Workers, and harness transcripts. Never proves live integration.
- **Design contract** — `docs/v0.3.md`, `docs/evidence-gates.md`,
  `docs/lifecycle.md`, `docs/v0.3-owner-acceptance-checklist.md` state
  what evidence each capability requires.

The live runs ran the production host driver (`taisan` built from the
then-current `main`) against the isolated repository on a Linux guest
(Ubuntu Noble arm64, bubblewrap 0.9.0) hosted on a single macOS
machine, with the in-repo reference real-Worker adapter
(`taisan-worker-claude`) driving a real coding-agent CLI runtime under
the `taisan-reference-v0.1` isolation profile. The store records show
the executable `taisan.worker.request/2` body was materialized per
Invocation (`worker_request_bodies` rows `e-1:inv:0`, `e-1:inv:1` in
`state-reverify2/store.db`), so the live chain exercised the v0.3
request transport, not only the v1 path.

## 3. Capability / evidence matrix

Owner checklist scenario IDs (M-1…M-7, M-8/M-9, S-1…S-6) refer to
[v0.3-owner-acceptance-checklist.md](v0.3-owner-acceptance-checklist.md).
"Run 1/2/3" refers to the three retained run records in order
(`RUN-RECORD-2026-11-23`, `-REVERIFY`, `-REVERIFY2`).

| Capability | Evidence (class) | Locators |
| --- | --- | --- |
| Manual intake | Owner-run live | `submit` envelopes `evidence/07-submit.json`, `50-reverify-submit.json`, `60-reverify2-submit.json`; Work/Execution rows `w-2/e-6` (run 1), `w-3/e-3` (run 2), `w-1/e-1` (run 3) |
| Coding (real Worker produces a change) | Owner-run live | Run 1: `greet(name)` implemented, produced `f690255`. Run 2: `inv-3` produced `b4d89bf`. Run 3: `inv-1` produced `8e48712d99edb084c900176f61fde510babc3185`, authored by the profile's declared commit identity |
| Independent verification | Owner-run live | `verification_results` rows `verify-1` (runs 2 and 3): the supervisor's own runner re-executed the declared argv (`python3 -m unittest discover -s tests -v`) against the produced commit in a detached worktree — verdict `pass`; never a Worker-claimed check |
| Invariant assessment | Owner-run live | `invariant_check` Invocations `inv-4` (run 2), `inv-2` (run 3) ran under the `tests-pass` invariant and completed `done`; check Invocation runs read-only against the produced state |
| PR publication (H1) | Owner-run live | Run 1: PR #62 on the isolated repository (publication completed **with recorded operator workarounds** — Git Database API staging plus a bridge push; see §7). Run 3: clean-path proof — `push_ref` intent `act-abf9cb71-…` `applied` (`ref_published work/w-1 → 8e48712`, `relation: equal`, `authenticated_actor: shimpeiws`) then `open_change_request` `act-20c0d696-…` `applied` → PR #64 |
| At least one real review finding | Owner-run live | Run 1: operator-posted inline finding on the real PR (`name=None` → `AttributeError`); the loop recorded the review round and judgment — `review_findings` Attention, `judge-findings` + resume |
| Fix | Owner-run live | Run 1: bounded fix Invocation produced `5c5cc6e` on the accepted head after the judgment |
| Republished head (H2) | Owner-run live | Run 1: `35ce3cb` (merge keeping non-fast-forward) pushed and reflected on the change request |
| Fresh reassessment to Outcome | Owner-run live | Run 1: post-republish verify/review checks and `assess_completion` recorded Execution `e-6` `succeeded` against the verified head `35ce3cb`; no unresolved writer, gate, or effect remained |

Refresh note (`119dcb7`): every row's retained owner-run evidence
predates the `ca45ac8` → `119dcb7` delta. The delta touched the
supervisor's launch-absence proof (#428), the boundary's staged-
credential lifecycle (#425), and the verification ordering on a spent
deadline (#426) — paths the live runs exercised — plus the
remote-executor surface (#375–#377), which is new code the live runs
never reached (§10). Contract tests cover each change on the
candidate (§9), but no live re-run exists on it: the rows above remain
the best retained evidence for the manual capabilities, and whether a
bounded re-exercise on the final candidate is required was the owner's
call under #270 — the issue closed 2026-11-25 accepting the record;
no re-exercise was required. The public evidence record for these
rows now exists as `docs/evidence/v0.3-manual-acceptance.md` — the
retained sanitized run records stay owner-side (§4).

## 4. Attention and restart/recovery evidence (#270)

| Variant | Evidence (class) | Locators |
| --- | --- | --- |
| Attention gates surfaced and resolved by durable commands | Owner-run live | Run 1: `review_findings` → `judge-findings` + resume; `repair:dispatch` refused → workspace normalization; `capacity_exhausted` → cancel; transport-retry pause → resume. Envelopes `evidence/09-attention-1.json`, `14-attention-2.json`, `12/21/22-resume.json`, `15-capacity.json` |
| Bounded recovery to honest terminal states | Owner-run live | Run 1: Executions `e-1`–`e-4` ended `artifact_recovery_exhausted` (bounded recovery honest failure); `w-1/e-5` terminated `cancelled` after transport retry-budget exhaustion |
| Stop/restart with no duplicate writer or effect | Owner-run live, partial | Run 2: `taisan run` restart resolved a cached pre-fix worker argv (recorded cause); each publish intent appears exactly once in `host_action_intents`. Run 3: loop stopped by SIGINT after publish — Execution parked `waiting`/`ci`, a normal post-publish state, not a stall |
| Store recovery attempt | Owner-run live, **negative** | `state/store.db.corrupt-20261123` / `store.db.recovered`: the recovered store proved internally inconsistent (empty `id_counters` collided), so run 2 used a fresh state root (`state-reverify/`). Recorded honestly — this is a recovery-path finding, not a pass |

Record locations against #270 (requirements satisfied at close — the
issue closed 2026-11-25 with owner approval of the record, comment
`7171973494`): the public evidence record now exists in this
repository as `docs/evidence/v0.3-manual-acceptance.md`; the retained
sanitized run records and the authorization record stay owner-side.
The restart-clean store-recovery variant left unexercised by runs 1–6
was evidenced by the #271 pilot (§5): a planned SIGINT restart and an
unplanned guest VM restart both recovered the store correctly —
orphaned `running` invocations were re-minted and scheduled intake
resumed (`loop-03.log`, `32-work-detail-w5-paused.json` in
`acceptance-v03-pilot/`). Owner approval of the acceptance record was
posted per §8 (issue #270 comment `7171973494`). #267 (real-Worker
stop/restart/recovery integration), which feeds this row, remains open
and in flight at record time.

## 5. Scheduled processing (#271) — exercised on the candidate

The authorized scheduled-intake pilot (issue #271 comment
`7171885803`) ran on the clean #367 install
(`/opt/taisan-v03` inside the OrbStack guest `taisan-clean`, Ubuntu
noble arm64) driving a build of `81c62cf`, against the isolated
repository `shimpeiws/coding-worker-smoke` (private/owner-only link).
Record: `RUN-RECORD-2026-11-26-PILOT.md` plus 66 sanitized evidence
files retained owner-side under `acceptance-v03-pilot/` (originals in
the guest at `/opt/taisan-v03/evidence/`).

- Scheduled processing of two distinct real Works (checklist
  S-1…S-6): **met**. Seven Works were admitted across six admitting
  scheduled ticks — intake-2 accepted two candidates in one run
  (w-1, w-2, under the `pending_queue_limit=2` budget); intake-3,
  -4, -13, -14, -16 one each; the first tick, intake-1, had failed
  closed `adapter_source_unavailable` (#468). Every dispatch
  decision was made by the scheduler, none by manual `submit`.
  w-6 (#74 → PR #77) and w-7 (#75 → PR #78) completed the verified
  real execution path —
  coding → verification pass → invariant check → publish → sealed
  bound-actor review round → assessment `complete:true` →
  **`succeeded`**. The other five reached honest terminals without
  claiming success: four publish-refused `stale_head` → bounded
  transport retries exhausted (4 each; `transport_exhausted` →
  `paused_judgment`) → operator cancel (branch-collision defect
  #470), one `waiting/ci` parked on a sticky `inconclusive` lane →
  operator cancel (#469).
- Scheduler scenarios, all with durable rows: `queue_full` skip
  (intake-5..9), replayed candidates, selector outage
  (`exchange_launch` on both scheduled and manual intake, `29`/`26`
  evidence), planned SIGINT restart (inv-4 re-minted as inv-5), an
  **unplanned guest VM restart** (orphaned `running` inv-11 re-minted
  as inv-12; `scheduled_startup` intake resumed — this doubles as the
  §4 restart-clean store-recovery evidence), and disable→
  `scheduled_reenable` (intake-13).
- Duplicate-writer/effect: `issue_claims` held single ownership of
  every selected Issue; no second execution system owned them
  (`36-anomalies-mid.json`, `41-attention-final.json`). No Work was
  dispatched twice.
- Intervention counts: per-Work cancel reasons recorded —
  w-1/w-3/w-4/w-5 publish-branch collisions exhausting the bounded
  transport retries (`transport_exhausted` → `paused_judgment` →
  operator cancel while paused), w-2 sticky-inconclusive lane →
  operator cancel — operator cancels are honest terminals, not
  successes.
- Wall time / usage: recorded per-Work in `43-per-work-usage.json`;
  counters kept honest — model-provider cost telemetry is not
  recorded; unknown stays unknown.
- New defects surfaced (recorded in §6/§7): #468 (`init` emits a
  numeric repository id the selector's gh backend cannot use —
  intake-1 `adapter_source_unavailable`), #469 (sandboxed cache
  symlink → sticky `inconclusive` lane — **fixed post-pin by PR
  #474**, §6/§7), #470
  (`work/w-N` publish-branch collisions across installations), #471
  (replayed candidates consuming `max_candidates` and starving new
  proposals).
- Claim scoping: the §14.5 verification sandbox was engaged in this
  guest (toolchain + caches + crates.io allowlist) — a stricter
  posture than runs 5/6's hygiene mode, recorded so the evidence is
  not over-read.
- Pre-pilot manual-path duplicate-effect evidence (unchanged from
  the previous record): **run 3** (post-#421) is the clean
  publication — `state-reverify2/store.db` holds exactly one
  `push_ref` intent and one `open_change_request` intent, each
  `applied` once (`act-abf9cb71-…`/`act-20c0d696-…`). **Run 2**'s
  publish leg recorded three `push_ref` intents that all `failed`
  identically (HTTP 401 → #420) — the bounded re-issue path, never a
  duplicated effect. **Run 1**'s H1 publication used recorded
  operator workarounds, so it is not clean-path evidence.
- Pre-pilot intervention counts (manual runs, unchanged): run 1 —
  six operator workarounds plus four Attention-gate navigations;
  run 2 — two operator inputs plus earlier aborted attempts;
  run 3 — four operator inputs including the `gh` upgrade that
  became #422.

## 6. Defect disposition against the candidate

Each open defect and unresolved review-finding set named by #365 was
re-checked against the candidate's code — an issue number or label is
not treated as a confirmed defect. Dispositions were verified on
`119dcb7` by re-reading the cited code; rows carried over from the
`ca45ac8` record were re-confirmed on the new candidate, with
locators re-walked where the delta drifted them (no behavior change
found).

Owner-disposition quotes in this section reproduce #365's closing
comment (2026-11-25) verbatim. 'fixed per §6' and the 'Fixed on the
release candidate' group label are the owner's **disposition
wording** — the instruction under which the owner closes the
dispositioned issues — not a code-state claim. The Disposition and
Reason columns state the verified code state: where they say
confirmed present, the defect is present in the candidate's code.
#313/#400 are confirmed present and their issue records
are open with no milestone (no fix release is scheduled; #400
also carries a `needs-human` spec quarantine) — unlike the pilot set
deferred to v0.6. #398/#399 were confirmed present at this
re-check; their repair is verified on the post-pin branch
`issue-398-399-fix`, pending merge into the candidate line (see
their rows; #398's `needs-human` label stands).

| Issue | Disposition | Reason (verified on `119dcb7`) |
| --- | --- | --- |
| #344 — absence proof can stall or conclude falsely | **Fixed** | Closed 2026-11-24 via PR #428 (`d81f064`). Both gaps from the previous record are closed: (a) a bounded count of consecutive unprovable scans now escalates to the distinct `LaunchAbsenceUnprovable` outcome (`observation.rs:107-112`; streak handling `supervisor.rs:4200-4232`) instead of holding `uncertain` forever — the launch is marked `unknown`, resolution continues, and a later provable answer still settles it (contract test `an_escalated_unprovable_launch_recovers_when_absence_becomes_provable`, `shim_contract.rs:784`); (b) absence now requires the third check — no live unrecorded member of the recorded launch group (`launch_group_clearance`, `supervisor.rs:4307+`), so a chdir+argv escapee keeping the group holds absence unproven instead of being abandoned as "absent". Residual tracked separately: #313 (below). |
| #347 — canary reachable under bound system mounts | **Fixed** | Closed 2026-11-21. Verified: `invisible_canary` derives probe-canary placement from the rendered mount plan (skips candidates beneath declared binds, falls back to host temp dir, then `/sys/kernel`), and `plan_denial` refuses plans whose binds cover the sandbox surface (`containment.rs:362-397`). |
| #358 — over-budget request classified as retryable Fault | **Fixed** (issue open; recommend close — owner-disposition wording 'Fixed on the release candidate' per #365's final comment) | `build_request_body` now maps spec-reachable validation failures (e.g. `verification_argv` over `max_verification_commands`) to `DispatchRefusal::RequestInvalid`, a typed refusal the consume path records against the request instead of a pass-aborting `ExecError::Fault` (`request_v2.rs:1018-1025`; refusal recorded via `refuse_dispatch`, `supervisor.rs:2815-2822, 3364-3390`). Only builder-impossible failures stay faults, matching the launch layer's `BodyCorrupt` classification. |
| #359 — re-retained reclaimed body never resolvable | **Fixed core; explicitly deferred residual** (owner-disposition wording 'Fixed on the release candidate' per #365's final comment; issue open) | `SqliteStore::register_body` restores a non-`present` row to `present` on identical-metadata replay (`retention.rs:3116-3118`, commit `19a91e5`). Deferred edges (issue stays open for them): a non-`present` row aliased under a different path still hits `ConstraintConflict` → `Fault` in `finish_invocation`; a `present` row is trusted without a file re-stat (resolves later, honestly, as `missing-file`/`Corrupt`). Narrow triggers, honest downstream failure — recorded, not release-blocking. |
| #361 — 13 unresolved review threads from PR #343 | **Fixed as filed** (recommend close — owner-disposition wording 'Fixed on the release candidate' per #365's final comment; issue open; spot re-confirmed on `119dcb7`) | All 13 unresolved non-outdated threads verified fixed on the candidate: bodies retain/reclaim revival (#1, #8) via `restore_body_presence`; catalogue-path root unified to the state root (#3, commit `ed4a576`); production composition root installs `V2Dispatch` (#2, `main.rs:2126-2149`); `/` toolchain/cache and provider-config mounts refused by `plan_denial` (#4, #5, `containment.rs:362-378`); v2 out-directory evidence collected in `finish_invocation` via `report_v2::collect` (#6 and the duplicate thread, `supervisor.rs:4642-4662`); `workspace_read_only` capability emitted for check/review roles (#7, `request_v2.rs:959-965`); probe-canary temp dir reclaimed (#9, commit `19a91e5`); provider-config digest covers member content and `ensure` re-proves on edit (#10, `boundary.rs:148-179,385-417`); contract-harness profile refused in production installs (`workloop-config` lib.rs:690-696); declared verification egress without a sandbox refused (`handoff.rs:985-991`). The `isOutdated` threads were superseded by follow-up commits on/after PR #343 (spot-verified via contract coverage: `a_v1_review_launch_mounts_the_workspace_read_only`, `a_contained_assessment_binds_the_workspace_read_only` in `shim_contract.rs`; char-boundary-safe probe detail). |
| #363 — spent-deadline check runs after paid work | **Fixed** | Closed 2026-11-23 via PR #426 (`84525a0`). The `timeout.is_zero()` short-circuit now runs before enforcement resolution, toolchain/cache hashing, and worktree maintenance, and records the dedicated `deadline_bound_marker()` digest rather than a digest of enforcement that never ran (`handoff.rs:1418-1444`; enforcement resolves at `handoff.rs:1450`). Contract coverage added in `handoff_contract.rs`. |
| #364 — staged runtime-auth never cleaned up | **Fixed** | Closed 2026-11-23 via PR #425 (`4df7087`). `unstage_credential` removes both `runtime-auth` and the `runtime-auth.tmp` staging sibling on refusal (`boundary.rs:218-224`), stage failure (`boundary.rs:277-281`), re-resolve and no-credential resolve (`boundary.rs:174-207`), and `Drop` (`boundary.rs:341-352`); removal failure refuses the resolve that asks for it. Regression test: `dropping_the_boundary_removes_the_staged_credential` (`boundary.rs:816`). |
| #422 — preflight does not probe `gh` version/features | **Explicitly deferred** | Environment-provisioning gap surfaced by run 3, not a code defect in the candidate: the guest's apt-default `gh` 2.45.0 lacks `gh api --slurp`, collapsing adapter list calls into opaque `read_indeterminate` skips; upgrading to `gh` 2.105.0 unblocked the run. Documented environment requirement and a design-TBD follow-up (preflight version probe and/or fail-loud transport errors); not release-blocking. |
| #420 — git push `Bearer` auth 401 for `gho_` tokens | **Fixed and verified live** | Closed via PR #421 (merged `4464c4f`): push authenticates as `Basic x-access-token:<token>` and unencodable credentials refuse before spawn. Run 3 verified live: `push_ref` applied, `ref_published` recorded, PR #64 opened, where the same credential under `Bearer` had deterministically 401'd in run 2. |
| Run-1 review finding — `greet(name)` with `name=None` raised `AttributeError` | **Fixed** (verified via the loop's own fix path) | The operator-posted inline finding on the isolated repository's PR #62 entered as a real review occurrence, was judged `fix`, and the bounded Fix Invocation produced `5c5cc6e`; republished head `35ce3cb` passed verification/review and `assess_completion` recorded `succeeded`. A defect in the produced change — not a taisan defect — dispositioned by the workflow itself; it is also the §3 real-review-finding → Fix → H2 → reassessment evidence, so it needs no separate repair issue. |

Other open defects re-checked on the candidate (beyond the list #365
names):

| Issue | Disposition | Reason (verified on `119dcb7` unless the row says otherwise) |
| --- | --- | --- |
| #313 — unidentified launch cannot converge with ≥2 unrecorded same-signature processes | **Confirmed present — open defect; the code re-check on the right still shows the behavior** (owner disposition wording, quoted verbatim from #365's final comment 2026-11-25 — a closing-disposition label, not a code-state claim: 'fixed per §6', grouped under 'Fixed on the release candidate'; issue open, no milestone) | Confirmed present and adjacent to the fixed #344 class: an ambiguous match restarts the unprovable streak and reports `Unknown` (`supervisor.rs:4220-4232`) — correct never-guess behavior, but the convergence gap persists: two same-signature lost-identity launches can hold capacity indefinitely. Honest-uncertain wedge, not a false-absence or duplicate-writer violation. |
| #398 — contained verification leaves `masked_files` empty | **Fixed on post-pin branch `issue-398-399-fix` — pending merge into the candidate line; not an ancestor of the pinned `119dcb7`; issue open** (was: confirmed present at the §6 re-check; owner disposition wording 'fixed per §6' per #365's final comment) | Repaired on the branch: both contained-verification `MountPlan`s populate `masked_files` from the resolved boundary credential source — the toolchain-preflight plan (`handoff.rs:1165`) and the run plan (`handoff.rs:1323`). The source is plumbed `drive` → `run_verifications` → `verify` → `verify_with_timeout` → `prepare_enforcement` → `ContainedVerification` (`handoff.rs:269,991,1023,1110,1354,1444`), resolved once per pass via `boundary_grant()` (`supervisor.rs:1849-1868`), and an `Err` grant records `Inconclusive` — fail closed — rather than masking nothing (`handoff.rs:1023`). Regression coverage: `contained_verification_shadows_the_credential_source`, `a_refused_credential_mask_source_records_inconclusive` (`handoff_contract.rs`). Sentinel-verified on a fresh OrbStack Ubuntu 24.04 VM (bubblewrap 0.9.0): the generated argv renders `--ro-bind /dev/null <source>` after every plan bind, and `cat` inside a real sandbox is denied where an unmasked control reads the sentinel. |
| #399 — credential mask only under system-bind prefixes | **Fixed on post-pin branch `issue-398-399-fix` — pending merge into the candidate line; not an ancestor of the pinned `119dcb7`; issue open** (was: confirmed present at the §6 re-check; owner disposition wording 'fixed per §6' per #365's final comment) | Repaired on the branch: `masked_file_dests` (`containment.rs:376-458`) computes every sandbox spelling at which an entry stays reachable — own-path binds literal and symlink-resolved, plus per-member system-bind coverage: each bound member is its own mount of its canonical target, so on a merged-`/usr` layout a file under the shared target is masked at every bound spelling (`/usr/bin/x` and `/bin/x` are separate mounts), excluding spellings under an emitted tmpfs mask-root — and the mask loop renders `--ro-bind /dev/null <dest>` per spelling after all plan binds (`containment.rs:722-726`), so an own-path `extra_binds` entry can no longer re-mount a masked file readable. `plan_denial` fails closed on entries that would stay visible but cannot be shadowed: carried by a fixed-destination bind, or visible and not a regular file (`containment.rs:523-552`). An entry no mount reaches emits nothing by design — a `/dev/null` bind whose destination nothing created would fail the mount setup. Regression coverage: `a_masked_file_outside_system_binds_is_shadowed_when_bound`, `a_visible_but_unmaskable_masked_file_is_refused`, `a_merged_usr_masked_file_is_shadowed_at_every_bound_spelling` (`containment.rs`); sentinel-verified on the same VM — own-path-bind case masked/denied, directory and fixed-destination cases refused at plan time. |
| #400 — v2 `report.json` retention dropped by #360 merge resolution | **Confirmed present — open defect; the code re-check on the right still shows the behavior** (owner disposition wording, quoted verbatim from #365's final comment 2026-11-25 — a closing-disposition label, not a code-state claim: 'fixed per §6', grouped under 'Fixed on the release candidate'; issue open, no milestone) | Confirmed present: `retain_report` does not exist in `handoff.rs` on the candidate — the retention implemented in `706b8e1` was dropped by the merge-conflict resolution `630e53b`. Impact is bounded but real: `report_v2::collect` already retains a bounded prefix of the report content-addressed in the CAS body store (`report_v2.rs:113-125,253-268`), so completion facts and bounded evidence survive — but the full `report.json` dies with the per-Invocation io-tree reclaim, an evidence-completeness gap on the v2 path the release advertises. |
| #437 — `remote_collect` reports zero-byte required artifact `Present` where local reports `Missing` | **Open defect — flagged for owner review in #365; no disposition recorded in its final comment** | Shadow finding on #435, merged with the finding open: `remote_collect.rs` treats `blob_at` → `Some(empty)` as `Present` (size and claimed-digest checks pass at `bytes=0`) while the local supervisor returns `Missing`. v0.4 remote path — outside this release's exercised surface but on the candidate's shipped code; no repair PR yet. |
| #419 — tracked `artifact.json` collision burns ArtifactRecovery | **Open defect — flagged in #365 but carrying no disposition in its closing comment; repair in flight** | PR #431 is open against it at record time; the candidate is provisional until it resolves. |
| #409 — #405 follow-ups (watchdog gap, ppid edges, docs, regression test) | **Open — deferred, not release-blocking** | Residual test-harness/observability coverage items; the merged core fix stands. |
| #353 — `workloop-fixture-worker` orphans leak past test exit | **Open — deferred, not release-blocking** | Test-infrastructure defect; it does not touch production behavior. |
| #302, #306, #139 | **Open — backlog, not re-verified this pass** | Older defects (v2 negotiation capability identifiers, workspace-prepare direct fetch deviating from the rejected transport design, deferred PR #128 shadow findings); recorded open — no disposition claimed. |
| #466 — fast-exiting output flood reports `success` instead of `limit_reached` | **Fixed on `main` post-pin — issue closed** (repair PR #467 merged `80cb059` — not an ancestor of the pinned `119dcb7`; lands on the provisional candidate's `main` line) | Diagnosed to the monitor loop's child-exit drain path skipping the `log_truncated` → `limit = output_limit_reached` promotion (CI run 45690519468 flake exposed the real race). PR #467 promotes truncation once after the loop exits, first-wins preserved, with deterministic regression tests for both fast-exit and still-running floods. |
| #468 — `init` emits a numeric repository id the selector cannot use | **Deferred to v0.6 (milestone 11) — dispositioned in #365's final comment; issue open** | Scheduled intake fails closed out of the box (`adapter_source_unavailable` on tick 1); pilot workaround was a manual config edit. Doc-level workaround exists, so a docs-now/code-later split is plausible. |
| #469 — symlinked `caches/*` dangles in the sandbox + sticky `inconclusive` lane | **Fixed on `main` post-pin** (repair PR #474 merged — not an ancestor of the pinned `119dcb7`; issue closed; discovered in the #271 pilot, lands on the provisional candidate line) | Two coupled gaps it closed: (a) symlinked verification-boundary paths are now refused instead of degrading verification to permanent `inconclusive`; (b) the invariant check re-drives on unjudged conclusive evidence, so a stale inconclusive lane is no longer a dead end. (b) is on the released outcome-classification surface. |
| #470 — `work/w-N` publish-branch collisions across installations | **Deferred to v0.6 (milestone 11) — dispositioned in #365's final comment; issue open** | Guarded refusal behaved correctly (never overwrote a foreign head); the deterministic name just makes collisions certain on shared/repeated installs. 4/7 pilot works hit it (w-1/w-3/w-4/w-5 — §7). |
| #471 — replayed candidates consume `max_candidates` | **Deferred to v0.6 (milestone 11) — dispositioned in #365's final comment; issue open** | A permanently-replayed issue can starve new proposals; pilot unblocked by raising `pending_queue_limit` — tuning workaround exists. |

## 7. Live-run findings tracked to repair

Ordinary-use failures discovered by the live runs and their repair
status on the candidate (repair merges that landed after the `119dcb7`
pin are annotated per row — fixed on `main`, not verified present at
the pinned SHA):

| Finding | Repair | Verified against its trigger |
| --- | --- | --- |
| #410 — worker shim wrote no workspace-root `artifact.json` | PR #413 (merged `bfa0412`) | Run 2: shim-written manifest present with `claimed_head` `b4d89bf`, persisted to `refs/taisan/artifacts/inv-3/artifact.json` — PASS |
| #411 — `push_ref` lacked git-object transport | PR #413 (+ PR #421 for auth) | Run 2: real `git push` over the scrubbed environment attempted — mechanism PASS, publication FAILED on auth (#420); run 3 after #421: full publish leg applied — PASS |
| #412 — contained sandbox lacked CA trust/commit identity | PR #413 | Run 2: declared CA binds mounted read-only; TLS to the model API worked inside the sandbox with no `--runtime-arg` bind; commit authored by the profile `commit_identity` — PASS |
| #420 — push auth scheme rejected by github.com | PR #421 (merged `4464c4f`) | Run 3: `push_ref` + `open_change_request` both `applied`; corroborating `GIT_CURL_VERBOSE` probe under the identical environment returned HTTP 200 vs the previous 401 — PASS |
| #422 — `gh` version/features unprobed by preflight | None (open, deferred) | — |
| #344 — unmappable cwd forced permanent `Err`; escaped descendants invisible | PR #428 (merged `d81f064`) | Contract tests on the candidate: consecutive unprovable scans converge to `LaunchAbsenceUnprovable`, an escalated launch still settles when absence becomes provable or a rebind succeeds, and recorded group clearance holds absence unproven for chdir+argv escapees (`shim_contract.rs`) — PASS |
| #363 — spent-deadline check ran after paid work | PR #426 (merged `84525a0`) | `handoff.rs:1424` short-circuits before enforcement/worktree cost with the `deadline_bound_marker()` digest; `handoff_contract.rs` covers the marker path — PASS |
| #364 — staged runtime-auth never cleaned up | PR #425 (merged `4df7087`) | `boundary.rs` unstages the credential and its staging sibling on refusal, re-resolve, stage failure, and `Drop`; `dropping_the_boundary_removes_the_staged_credential` — PASS |
| #437 — `remote_collect` zero-byte artifact reported `Present` | None (open) | — |
| #447 — refused worker launch consumes role invocation budget | PR #451 (merged `179f6fe`, post-pin — not an ancestor of `119dcb7`) | Run 4 (#270, `28afb72`): `inv-3` (fix) refused pre-spawn on a dirty workspace yet `work_usage.fix_invocations_used=1` — the one-shot judged-fix budget burned by a launch that produced nothing. Repair binds the refund to the live attempt; run 5 drove on `179f6fe` and exercised the judged-fix legs run 4 could not reach |
| #448 — resume after consumed `next_role` re-queues as `coding`, not the judged role | PR #451 (merged `179f6fe`, post-pin — not an ancestor of `119dcb7`) | Run 4: operator `resume` after `inv-3`'s refusal issued `e-1:inv:3` as `coding` pinned to the Work base — a judged fix proved effectively single-shot even though the refusal was environmental. Repair re-arms the judged role on a pre-spawn refusal; run 5's `next_role=fix` requeues (`inv-15`/`inv-19`) ran the judged role |
| #449 — worker runtime residue (`__pycache__`) dirties workspace, refuses next launch | PR #451 (merged `179f6fe`, post-pin — not an ancestor of `119dcb7`) | Run 4: `launch_refused: workspace conflict: workspace is not clean` on untracked interpreter caches left by the worker's own verification; manual removal + resume unblocked the gate. Repair tolerates interpreter-cache residue in workspace reuse — run 5's coding Invocations produced no such refusal |
| #450 — republish of a divergent head holds `read_indeterminate` forever | PR #451 (merged `179f6fe`, post-pin — not an ancestor of `119dcb7`) | Run 4: round-2 `push_ref` `act-27b7a832` (produced `2d0163c`, `expected_prior` `fdc0bb8a`) held `read_indeterminate` permanently — `compare` 404 collapses to Indeterminate instead of Absent; teardown additionally showed the pending intent defers Execution cancel settlement (`stopping` held as the honest terminal). Repair classifies a compare 404 as not-contained; run 5's republish H2 legs converged (`act-4921f5fd`/`act-a8e1201a` `applied`, `expected_prior` fast-forward) |
| #453 — cancel does not withdraw pending outbox intents | None (open) | Run 4/5: run-4's `e-1` sits durably `stopping` — a cancelled Execution's held claim + capacity slot had to be released by hand before run 5 could submit (`issue_claims`/`slots` rows) |
| #454 — capability reports only refresh on unscoped passes | None (open) | Run 5 (`179f6fe`): a `run --work`-scoped loop deadlocked publication on a poisoned capability ledger (obs 41 all-`unknown`); one bounded unscoped pass recorded a fresh authorized `CapabilityReport` (obs 55) and unblocked the publish gate |
| #468 — `init` emits a numeric repo id the selector rejects | None (open) | Pilot: intake-1 `scheduled_startup` failed `adapter_source_unavailable` until the config id was hand-edited to the slug (`09-intake-1-detail.json`, `02b-check-after-id-fix.txt`) |
| #469 — sandbox-dangling `caches` symlink + sticky `inconclusive` lane | PR #474 (merged, post-pin — not an ancestor of `119dcb7`) | Pilot: w-2 `waiting/ci` after publish+sealed review — every verification reported `cache_unreadable` until the symlink was replaced with a real dir; `resume`/`retry` correctly refused (`22-cache-fix.txt`, `23-resume-w2.json`) |
| #470 — `work/w-N` branch collision across installations | None (open) | Pilot: w-1/w-3/w-4/w-5 publish `stale_head` → bounded retries (4 each) → `transport_exhausted` (`paused_judgment` attention, "transport retry bound reached") → operator cancel while paused (w-1/w-3/w-4 via `acknowledge`, w-5 direct); guarded refusal confirmed never to overwrite a foreign head (`15-acknowledge-w1.json` et al.) |
| #471 — replayed candidates starve the queue | None (open) | Pilot: issue #70 replayed into the single `max_candidates` slot each tick until `pending_queue_limit` was raised 2→4 (`38-intake-runs-final.json`) |
| #466 — post-exit flood misclassified `success` | PR #467 (merged `80cb059`, post-pin — not an ancestor of `119dcb7`) | CI flake exposed the race; fix verified by deterministic wire-contract tests on both exit paths |
| #455 — remote leftover head kills a Work's publish path | None (open) | Run 5: `work/w-2` residue (`35ce3cb`) → `stale_head`, then three deterministic `merge-base` missing-object refusals burned `transport_retries_used` to 4 > 3 → `transport_exhausted`; post-resume `maybe_publish` mints zero intents (work.rs:4155 early-return, no budget reset, `resume` requeues `coding` only) — no operator release; folded evidence: non-transport refusals consume `recovery.transport.*` budget (#447 class) |
| #456 — `unavailable`/`provider_unavailable` check at exit 0 is lane-terminal | None (open) | Run 5 ×3 reproduction: `inv-16`/`inv-18`/`inv-20` (invariant_check) reported `provider_unavailable` at exit 0 — `checked`=true so no bounded re-request, assessment stays `complete:false`, completion permanently closed; worker log shows `api_error: connection_failed` after ~473 s while coding/fix Invocations went 4/4 |

Repairs in flight at record time: PR #427 (sibling-PR conflict
detection, #323) resolved post-pin — merged `52a5493` on `main`,
not an ancestor of the pinned `119dcb7`, landing on the provisional
candidate line — while PR #431 (`artifact.json` collision, #419)
remains open against the candidate; the candidate is provisional
until it resolves.

Run 4 (#270, owner-authorized, guest `taisan-v03`, driver built from
`28afb72`) ended on the owner STOP decision at the #450 wedge and was
torn down honestly: Execution `e-1` accepted `cancel` and sits durably
`stopping` — terminal settle is gated on outbox reconciliation by
design, and `act-27b7a832` never reconciled. Its retained record is
`RUN-RECORD-2026-11-25-RUN4.md` with the sanitized evidence copy at the
owner-side `acceptance-v03/` location; M-1…M-5 (intake, coding,
independent verification, publish, review→judgment seal/release) all
produced live evidence, while the judged-fix H2 leg, the republish
converged path, and the verified-outcome settle stayed unexercised —
each blocked by a filed defect (#447/#448/#449/#450).

Run 5 (#270, owner-authorized, guest `taisan-v03`, driver built from
`179f6fe` — post-pin) reached **M-1…M-6 PASS** on independent
Execution chains (e-3 → PR #66, e-4 → PR #67) — proven twice for
M-1, M-3, M-4, M-5 and M-6; M-2 (independent invariant coverage) is
proven on the e-3 chain only, e-4's `inv-18` having completed
`unavailable`/`provider_unavailable` — covering: intake, real coding,
independent verification, H1 publish, bound-actor review finding →
`paused_judgment`, `judge-findings`/`release-judgment` → judged fix,
and the republish-converged H2 leg (`expected_prior` fast-forward,
PR check runs observed SUCCESS under a `required_checks []`
installation) — including the path run 4 never reached. **M-7
verified outcome stayed unexercised**: the post-republish
invariant checks (`inv-16`, `inv-20`) reported `provider_unavailable`
at exit 0 — lane-terminal under #456, with the provider failure
reproduced three times (`connection_failed`). Two earlier structural
deadlocks were hit and honestly terminated: #455 (e-2 `cancelled` —
publish path dead on transport-budget exhaustion with no release) and
the #453/#454 chains (pre-run surgery + one bounded unscoped pass,
recorded as operator inputs). Termination was the owner **Z1**
decision; e-4 is left parked `waiting(ci)` with `automatic_time_limit`
as the expected terminus. Retained record:
`RUN-RECORD-2026-11-25-RUN5.md` (owner-side), sanitized evidence copy
`acceptance-v03-run5/`; the repo-facing filled record is
`docs/evidence/v0.3-manual-acceptance.md`.

Recorded observations not yet filed as issues (kept visible, not
silently dropped):

- The reference shim's fixed argv lacks `--verbose`, which the
  exercised runtime version requires for stream-json mode — currently
  carried by the declared `--runtime-arg` knob (noted in #420's
  acceptance criteria as a separate small follow-up).
- `CapabilityReport` is issued on the unscoped pass only; a
  `run --work`-scoped loop deadlocks publication — clarify intended
  scoping (run 1 deferred observation; now filed as #454).
- Runs 1–3 left remote `work/w-2` at `35ce3cb` on the smoke repository
  (teardown gap, ops-side not a code defect; owner-deleted during run
  5 — its collision with the publish lease produced #455's first
  stage). Retained `work/w-1` (run 4's PR #65) stayed untouched at
  `fdc0bb8a`, verified at run-5 teardown.
- Run 5 operator error recorded for honesty: a scoped loop briefly
  launched with `export HOME=/root` broke `gh` config resolution for
  the guest user and paused e-3 on `source read unavailable`;
  relaunching without the export resolved it — operator error, not a
  defect.
- macOS cannot host containment natively (no bubblewrap); the tracked
  answer is the minimal on-demand Linux guest (#373 design).

## 8. Issue-state ledger — implemented issues and tracker state

The milestone's open-issue count overstates remaining work: several
open issues are already implemented by merged PRs whose footers did
not carry `Closes`. The two release tasks whose owner decisions were
open at record time (#368 route, #369 license) are decided and
closed, and #365 itself closed 2026-11-25 with the owner's final
defect dispositions in its closing comment (#437 excepted — still
awaiting a disposition, §6). States verified
2026-11-26:

| Issue | State on the candidate | Implementing merge | Residual |
| --- | --- | --- | --- |
| #263 — connect produced commits to handoff/verification | Implemented | PR #348 (`d7f5a9f`) | None found; recommend owner close |
| #264 — real invariant/code-review assessments | Implemented | PR #349 (`b93b72d`) | recommend close |
| #266 — production host effects/reconciliation | Implemented | PR #351 (`9642c2a`) | recommend close |
| #269 — clean-install + real-Worker acceptance docs | Implemented — **closed by owner (2026-11-25)** | PR #356 (`9a79664`); runbook corrections merged via #465 (`827b9c0`, post-pin) | reproduction done — #367 closed the loop (row below) |
| #410 / #411 / #412 — live-run finding fixes | Implemented | PR #413 (`bfa0412`) | verified live in runs 2–3 (§7); recommend close |
| #366 — pluggable public Selector/Worker references | Closed | PR #404 (`f883d6e`) | done |
| #368 — public-content/history/metadata audit | Audit delivered — **closed by owner (2026-11-25)** | PR #432 (`b0b81f6`) | `release-audit.md` §5 items resolved under the #438 ruling (recorded in its §7): in-place publication, history kept, private names acceptable — no disclosure review gates publication |
| #369 — license selection prep | License selected and applied — **closed by owner (2026-11-25)** | PR #433 (`aff8721`) prep; PR #464 (merged `2f1a825`) applies | Owner chose MIT OR Apache-2.0: `LICENSE-MIT`/`LICENSE-APACHE` placed, workspace + 15-crate `license` metadata, README/`assets/LICENSE.md` exception for `taisan-hero.jpg` only, CONTRIBUTING dual-contribution note, `THIRD-PARTY.md` (87-crate inventory) + `LICENSES/` texts staged for source distribution |
| #270 — manual real-Worker acceptance | M-1…M-7 exercised live — **closed by owner (2026-11-25)** | runs 1–6 retained owner-side | Run 6 done (run-6 authorization issue #270 comment `7161879297`; driver `c445feee` incl. the #458 fix, post-pin): M-7 verified outcome exercised — post-republish `invariant_check` → assessment `complete:true` → `succeeded` on e-5 (`RUN-RECORD-2026-11-25-RUN6.md`, sanitized copy `acceptance-v03-run6/`); M-7 row filled in `docs/evidence/v0.3-manual-acceptance.md`. Attribution recorded honestly: run 6's H1 review round sealed with zero occurrences, so the finding→judged-fix→republish legs (M-4/M-5/M-6) are evidenced by run 5's e-3/e-4 chains — the M-1…M-6 rows are recorded via the run-5 record (#457, merged post-pin). Owner approval of the record posted 2026-11-25 (comment `7171973494`, recorded in record §8); the restart-clean store-recovery variant left unexercised by runs 1–6 was evidenced by the #271 pilot (§5 — planned SIGINT plus an unplanned guest VM restart, orphaned invocations re-minted). Residual: none against #270 itself |
| #271 — scheduled pilot acceptance | **Exercised — all acceptance criteria met — closed by owner (2026-11-25)** | `RUN-RECORD-2026-11-26-PILOT.md` + 66 sanitized evidence files owner-side (`acceptance-v03-pilot/`; guest originals `/opt/taisan-v03/evidence/`) | Authorized pilot (comment `7171885803`) on the clean #367 install: 7 Works admitted across scheduled ticks, w-6/w-7 `succeeded` via the verified real path; queue_full/replay/selector-outage/restart×2 (planned + unplanned VM)/disable-reenable all durable; `issue_claims` single ownership; per-Work interventions, wall time and usage recorded honestly. Surfaced defects #468–#471 (§6/§7 — #469 fixed post-pin by PR #474; the rest deferred to v0.6 milestone 11 per #365) |
| #367 — clean-Linux-install reproduction | **Done — closed by owner (2026-11-25)** | `RUN-RECORD-2026-11-25-CLEAN.md` + 26 sanitized evidence files owner-side (`acceptance-v03-clean/`) | Fresh OrbStack guest `taisan-clean` (Ubuntu noble arm64): runbook steps verified verbatim or as typed documented failures; 4 binaries installed; full ops cycle on the fixture binding; all 10 sandbox probes passed; real Worker `describe` + offline `select` proven. Doc gaps fixed and merged via #465 (`827b9c0`, post-pin). One verification gap stands: `git clone` unverifiable while the repo is private — no longer release-blocking under the #438 publication ruling; it closes when the owner flips visibility |
| #370 / #371 — candidate freeze; go/no-go | #370 in flight (this PR); #371 not started | — | gated on this ledger and the owner's candidate sign-off — defect dispositions are recorded in #365's closing comment (#437 excepted, §6); the route (#438) and license (#464) are decided |
| #267 — stop/restart/recovery integration | In flight (`agent:running`) | — | feeds the §4 restart/recovery row |

## 9. Verification baseline on the candidate

This change adds documentation only; no code, test, or configuration
changed since the last green baseline. The refreshed candidate
`119dcb7` nevertheless had the full baseline rerun as #365 requires —
the previous record's run covered `ca45ac8`, not this candidate
(build before test — contract tests spawn
`target/debug/workloop-fixture-worker`):

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Passed — 1753 tests, 0 failures (fixture-selector/fixture-worker contract tests ran against `target/debug/` binaries produced by the build gate; one timing-sensitive deadline assertion in `gh_cli_transport` failed under full-suite load on the first run and passed on rerun — recorded, not hidden) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |

Scope honesty: the earlier green runs on merged PRs cover the code
they tested; they do not cover this document's claims — the
capability evidence above rests on the retained owner-run records,
not on `cargo test`. Conversely the cargo baseline exercises fixture
and contract paths only; it is not evidence for any live-run claim in
this document.

## 10. Known limitations

- **Single Linux host**: containment requires bubblewrap namespaces;
  the reference deployment is one Linux environment (the runs used a
  single OrbStack guest on one macOS machine). No multi-host or
  distributed execution is claimed.
- **Remote execution landed but unexercised**: the remote executor
  surface (contract #374, wire types/bindings #375, reference endpoint
  #377, bounded transfer/collection #376) is on the candidate as v0.4
  groundwork — with one open defect (#437). No end-to-end remote run
  exists and none is claimed or required for this release; all live
  evidence is local-host, and remote execution stays outside this
  release gate.
- **No automatic merge or deploy**: publication ends at a reviewed
  change request; merge and deploy remain human acts. Every run
  finished with the PR open and nothing merged.
- **Remote review is a near-term follow-up**: no remote/distributed
  review path is part of this release.
- **Scheduled intake now exercised at pilot scale** (§5): the #271
  pilot admitted 7 Works across scheduled ticks with 2 verified
  `succeeded` completions — a tiny-pilot sample; observations are
  recorded without claiming causal cost/quality improvement.
- **Run 1 publish leg used operator workarounds** (§3): the clean
  publish path is proven by run 3 on post-#421 code, not by run 1's
  H1.
- **One earlier recovered-store attempt was internally inconsistent**
  (§4): the run-1/2 attempt honestly recorded a negative result; the
  restart-clean variant was subsequently evidenced positively by the
  #271 pilot's two real restarts (§5).
- **Defect dispositions refreshed** (§6): the defects previously
  assessed release-blocking and deferred — #344 via #428, #364 via
  #425, #363 via #426 — are fixed on this candidate; #447–#450 are
  fixed by PR #451 and #466 by PR #467 — both merged on `main`
  **after** the pinned candidate (`179f6fe`/`80cb059` are not
  ancestors of `119dcb7`), so they land on the provisional candidate
  line as it advances. The flagged defects are dispositioned in
  #365's closing comment (2026-11-25): #313 (ambiguous-match
  convergence wedge) and #400 (v2 report-retention regression)
  remain confirmed present on the candidate and stay open with no
  milestone (§6; #400 also needs-human); #398/#399 (credential
  masking — the same class #364 gated on) were likewise confirmed
  present at the re-check and are repaired on the post-pin branch
  `issue-398-399-fix`, pending merge into the candidate line
  (#398's `needs-human` label stands) —
  owner-recorded 'fixed per §6', the owner's disposition wording
  for issue closure, not a code-state claim;
  the pilot-discovered #468, #470, #471 are deferred to v0.6
  (milestone 11) and the earlier deferred set #453–#456 stays
  deferred to v0.6; #469, the one touching the released
  outcome-classification surface, was repaired post-pin by PR #474.
  #437 (`remote_collect` zero-byte artifact) was flagged for owner
  review but carries no disposition in the comment — still
  awaiting one. The `release-audit.md` §5 exposure
  items are resolved under the #438 publication-route ruling —
  in-place publication with history kept (recorded in its §7).
