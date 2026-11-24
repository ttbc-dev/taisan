# Release acceptance: candidate evidence and defect disposition

Status: acceptance evidence record for the **Initial public release —
validated v0.3** milestone (issue #365). This document assembles the
release decision around the retained v0.3 acceptance evidence; it does
not authorize publication, any merge or deploy, credential changes,
repository visibility changes, or any new live run, and it does not
itself approve any unverified claim. Per `AGENTS.md` no private source
repository is named: the live-run target is described as *the isolated
repository* and private counterpart systems are named by role only.
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
#427 for #323 and PR #431 for #419 are open). Its history contains
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
bounded re-exercise on the final candidate is required is #270's call,
not assumed here. All rows also remain **pending #270** for the
sanitized public evidence record and the explicit authorization
record (§4) — the runs happened; the public record of them does not
yet exist.

## 4. Attention and restart/recovery evidence (#270)

| Variant | Evidence (class) | Locators |
| --- | --- | --- |
| Attention gates surfaced and resolved by durable commands | Owner-run live | Run 1: `review_findings` → `judge-findings` + resume; `repair:dispatch` refused → workspace normalization; `capacity_exhausted` → cancel; transport-retry pause → resume. Envelopes `evidence/09-attention-1.json`, `14-attention-2.json`, `12/21/22-resume.json`, `15-capacity.json` |
| Bounded recovery to honest terminal states | Owner-run live | Run 1: Executions `e-1`–`e-4` ended `artifact_recovery_exhausted` (bounded recovery honest failure); `w-1/e-5` terminated `cancelled` after transport retry-budget exhaustion |
| Stop/restart with no duplicate writer or effect | Owner-run live, partial | Run 2: `taisan run` restart resolved a cached pre-fix worker argv (recorded cause); each publish intent appears exactly once in `host_action_intents`. Run 3: loop stopped by SIGINT after publish — Execution parked `waiting`/`ci`, a normal post-publish state, not a stall |
| Store recovery attempt | Owner-run live, **negative** | `state/store.db.corrupt-20261123` / `store.db.recovered`: the recovered store proved internally inconsistent (empty `id_counters` collided), so run 2 used a fresh state root (`state-reverify/`). Recorded honestly — this is a recovery-path finding, not a pass |

Residual gaps against #270: the owner-facing sanitized public evidence
record and the explicit authorization record the issue requires are
held owner-side with the run records rather than in this repository;
a fully restart-clean store-recovery variant is not yet evidenced (the
one attempt was inconsistent). #267 (real-Worker stop/restart/recovery
integration), which feeds this row, remains open and in flight at
record time.

## 5. Scheduled processing (#271) — not exercised on this candidate

The retained stores contain `intake_settings` rows but **zero**
`intake_runs` rows (`state/`, `state-reverify/`, `state-reverify2/`),
and the live-run configuration (`config.toml`) carries
`[intake] enabled = false`. Every Work in the runs was admitted by
manual `submit` and driven by `taisan run`/`run --work`. Therefore:

- Scheduled processing of two distinct real Works (checklist S-1…S-6):
  **no evidence**. The scheduler scenarios — concurrent Works,
  duplicate-dispatch exclusion against the legacy system, restart
  reconciliation of a due/in-flight run, disable semantics — are
  unexercised on the candidate. #271 remains open and this criterion
  must not be claimed.
- Duplicate-writer/effect evidence (manual path only): **run 3**
  (post-#421) is the clean publication — `state-reverify2/store.db`
  holds exactly one `push_ref` intent and one `open_change_request`
  intent, each `applied` once
  (`act-abf9cb71-…`/`act-20c0d696-…`). **Run 2**'s publish leg
  recorded three `push_ref` intents that all `failed` identically
  (`act-a53d7e32-…`, `act-6bcbbde5-…`, `act-a27c9d6b-…`, HTTP 401
  → #420) — the bounded re-issue path after a definite failure, never
  an applied or duplicated effect; no remote ref was created. **Run
  1**'s H1 publication used recorded operator workarounds (Git
  Database API staging plus a bridge push) rather than the adapter
  intent path, so it is not clean-path duplicate-effect evidence. No
  Work was dispatched twice in any run.
- Intervention counts (manual runs): run 1 lists six operator
  workarounds plus four Attention-gate navigations; run 2 lists two
  required operator inputs (ambient `GH_TOKEN` by design, declared
  `--runtime-arg --verbose` knob) plus earlier aborted attempts
  (`w-1`/`w-2` cancelled); run 3 lists four operator inputs including
  an in-guest `gh` upgrade that became issue #422.
- Wall time: partial data — `work_usage` for run 3 `w-1` records
  first dispatch 1795432780 → last progress 1795433811 (≈ 17 minutes
  for the coding + invariant-check + publish legs). No scheduled-run
  wall-time comparison exists.
- Usage availability: `work_usage` rows record counters honestly
  (run 3 `w-1`: `fix_invocations_used=0`, `transport_retries_used=0`,
  `automatic_seconds_used=0`); model-provider cost telemetry is not
  recorded — unknown stays unknown.

## 6. Defect disposition against the candidate

Each open defect and unresolved review-finding set named by #365 was
re-checked against the candidate's code — an issue number or label is
not treated as a confirmed defect. Dispositions were verified on
`119dcb7` by re-reading the cited code; rows carried over from the
`ca45ac8` record were re-confirmed on the new candidate, with
locators re-walked where the delta drifted them (no behavior change
found).

| Issue | Disposition | Reason (verified on `119dcb7`) |
| --- | --- | --- |
| #344 — absence proof can stall or conclude falsely | **Fixed** | Closed 2026-11-24 via PR #428 (`d81f064`). Both gaps from the previous record are closed: (a) a bounded count of consecutive unprovable scans now escalates to the distinct `LaunchAbsenceUnprovable` outcome (`observation.rs:107-112`; streak handling `supervisor.rs:4200-4232`) instead of holding `uncertain` forever — the launch is marked `unknown`, resolution continues, and a later provable answer still settles it (contract test `an_escalated_unprovable_launch_recovers_when_absence_becomes_provable`, `shim_contract.rs:784`); (b) absence now requires the third check — no live unrecorded member of the recorded launch group (`launch_group_clearance`, `supervisor.rs:4307+`), so a chdir+argv escapee keeping the group holds absence unproven instead of being abandoned as "absent". Residual tracked separately: #313 (below). |
| #347 — canary reachable under bound system mounts | **Fixed** | Closed 2026-11-21. Verified: `invisible_canary` derives probe-canary placement from the rendered mount plan (skips candidates beneath declared binds, falls back to host temp dir, then `/sys/kernel`), and `plan_denial` refuses plans whose binds cover the sandbox surface (`containment.rs:362-397`). |
| #358 — over-budget request classified as retryable Fault | **Fixed** (issue open; recommend close) | `build_request_body` now maps spec-reachable validation failures (e.g. `verification_argv` over `max_verification_commands`) to `DispatchRefusal::RequestInvalid`, a typed refusal the consume path records against the request instead of a pass-aborting `ExecError::Fault` (`request_v2.rs:1018-1025`; refusal recorded via `refuse_dispatch`, `supervisor.rs:2815-2822, 3364-3390`). Only builder-impossible failures stay faults, matching the launch layer's `BodyCorrupt` classification. |
| #359 — re-retained reclaimed body never resolvable | **Fixed core; explicitly deferred residual** | `SqliteStore::register_body` restores a non-`present` row to `present` on identical-metadata replay (`retention.rs:3116-3118`, commit `19a91e5`). Deferred edges (issue stays open for them): a non-`present` row aliased under a different path still hits `ConstraintConflict` → `Fault` in `finish_invocation`; a `present` row is trusted without a file re-stat (resolves later, honestly, as `missing-file`/`Corrupt`). Narrow triggers, honest downstream failure — recorded, not release-blocking. |
| #361 — 13 unresolved review threads from PR #343 | **Fixed as filed** (recommend close; spot re-confirmed on `119dcb7`) | All 13 unresolved non-outdated threads verified fixed on the candidate: bodies retain/reclaim revival (#1, #8) via `restore_body_presence`; catalogue-path root unified to the state root (#3, commit `ed4a576`); production composition root installs `V2Dispatch` (#2, `main.rs:2126-2149`); `/` toolchain/cache and provider-config mounts refused by `plan_denial` (#4, #5, `containment.rs:362-378`); v2 out-directory evidence collected in `finish_invocation` via `report_v2::collect` (#6 and the duplicate thread, `supervisor.rs:4642-4662`); `workspace_read_only` capability emitted for check/review roles (#7, `request_v2.rs:959-965`); probe-canary temp dir reclaimed (#9, commit `19a91e5`); provider-config digest covers member content and `ensure` re-proves on edit (#10, `boundary.rs:148-179,385-417`); contract-harness profile refused in production installs (`workloop-config` lib.rs:690-696); declared verification egress without a sandbox refused (`handoff.rs:985-991`). The `isOutdated` threads were superseded by follow-up commits on/after PR #343 (spot-verified via contract coverage: `a_v1_review_launch_mounts_the_workspace_read_only`, `a_contained_assessment_binds_the_workspace_read_only` in `shim_contract.rs`; char-boundary-safe probe detail). |
| #363 — spent-deadline check runs after paid work | **Fixed** | Closed 2026-11-23 via PR #426 (`84525a0`). The `timeout.is_zero()` short-circuit now runs before enforcement resolution, toolchain/cache hashing, and worktree maintenance, and records the dedicated `deadline_bound_marker()` digest rather than a digest of enforcement that never ran (`handoff.rs:1418-1444`; enforcement resolves at `handoff.rs:1450`). Contract coverage added in `handoff_contract.rs`. |
| #364 — staged runtime-auth never cleaned up | **Fixed** | Closed 2026-11-23 via PR #425 (`4df7087`). `unstage_credential` removes both `runtime-auth` and the `runtime-auth.tmp` staging sibling on refusal (`boundary.rs:218-224`), stage failure (`boundary.rs:277-281`), re-resolve and no-credential resolve (`boundary.rs:174-207`), and `Drop` (`boundary.rs:341-352`); removal failure refuses the resolve that asks for it. Regression test: `dropping_the_boundary_removes_the_staged_credential` (`boundary.rs:816`). |
| #422 — preflight does not probe `gh` version/features | **Explicitly deferred** | Environment-provisioning gap surfaced by run 3, not a code defect in the candidate: the guest's apt-default `gh` 2.45.0 lacks `gh api --slurp`, collapsing adapter list calls into opaque `read_indeterminate` skips; upgrading to `gh` 2.105.0 unblocked the run. Documented environment requirement and a design-TBD follow-up (preflight version probe and/or fail-loud transport errors); not release-blocking. |
| #420 — git push `Bearer` auth 401 for `gho_` tokens | **Fixed and verified live** | Closed via PR #421 (merged `4464c4f`): push authenticates as `Basic x-access-token:<token>` and unencodable credentials refuse before spawn. Run 3 verified live: `push_ref` applied, `ref_published` recorded, PR #64 opened, where the same credential under `Bearer` had deterministically 401'd in run 2. |
| Run-1 review finding — `greet(name)` with `name=None` raised `AttributeError` | **Fixed** (verified via the loop's own fix path) | The operator-posted inline finding on the isolated repository's PR #62 entered as a real review occurrence, was judged `fix`, and the bounded Fix Invocation produced `5c5cc6e`; republished head `35ce3cb` passed verification/review and `assess_completion` recorded `succeeded`. A defect in the produced change — not a taisan defect — dispositioned by the workflow itself; it is also the §3 real-review-finding → Fix → H2 → reassessment evidence, so it needs no separate repair issue. |

Other open defects re-checked on the candidate (beyond the list #365
names):

| Issue | Disposition | Reason (verified on `119dcb7` unless the row says otherwise) |
| --- | --- | --- |
| #313 — unidentified launch cannot converge with ≥2 unrecorded same-signature processes | **Open defect — disposition pending owner call (deferred vs blocking)** | Confirmed present and adjacent to the fixed #344 class: an ambiguous match restarts the unprovable streak and reports `Unknown` (`supervisor.rs:4220-4232`) — correct never-guess behavior, but the convergence gap persists: two same-signature lost-identity launches can hold capacity indefinitely. Honest-uncertain wedge, not a false-absence or duplicate-writer violation. |
| #398 — contained verification leaves `masked_files` empty | **Open defect — flag for owner release-blocking review** | Confirmed present: the contained-verification `MountPlan`s carry `masked_files: vec![]` (`handoff.rs:1119`, `handoff.rs:1277`) while `contained_exec` pushes `grant.credential_source` into the mask (`supervisor.rs:1474-1475`); a `file:` credential source under a system-bind prefix stays readable inside contained verification. Same credential-hygiene class for which #364 was release-blocking before #425. |
| #399 — credential mask only under system-bind prefixes | **Open defect — flag with #398** | Confirmed present: the `/dev/null` shadow renders only when `system_bind_covers(file)` (`containment.rs:562-567`); a credential source outside `/usr`/`/bin`/`/sbin`/`/lib`/`/lib64` (e.g. `/etc`, `/run`) gets no mask and can still be bound read-only by the argv-derived own-path `extra_binds`. |
| #400 — v2 `report.json` retention dropped by #360 merge resolution | **Open defect — retention regression; flag for owner review** | Confirmed present: `retain_report` does not exist in `handoff.rs` on the candidate — the retention implemented in `706b8e1` was dropped by the merge-conflict resolution `630e53b`. Impact is bounded but real: `report_v2::collect` already retains a bounded prefix of the report content-addressed in the CAS body store (`report_v2.rs:113-125,253-268`), so completion facts and bounded evidence survive — but the full `report.json` dies with the per-Invocation io-tree reclaim, an evidence-completeness gap on the v2 path the release advertises. |
| #437 — `remote_collect` reports zero-byte required artifact `Present` where local reports `Missing` | **Open defect (new since the previous record)** | Shadow finding on #435, merged with the finding open: `remote_collect.rs` treats `blob_at` → `Some(empty)` as `Present` (size and claimed-digest checks pass at `bytes=0`) while the local supervisor returns `Missing`. v0.4 remote path — outside this release's exercised surface but on the candidate's shipped code; no repair PR yet. |
| #419 — tracked `artifact.json` collision burns ArtifactRecovery | **Open defect — repair in flight** | PR #431 is open against it at record time; the candidate is provisional until it resolves. |
| #409 — #405 follow-ups (watchdog gap, ppid edges, docs, regression test) | **Open — deferred, not release-blocking** | Residual test-harness/observability coverage items; the merged core fix stands. |
| #353 — `workloop-fixture-worker` orphans leak past test exit | **Open — deferred, not release-blocking** | Test-infrastructure defect; it does not touch production behavior. |
| #302, #306, #139 | **Open — backlog, not re-verified this pass** | Older defects (v2 negotiation capability identifiers, workspace-prepare direct fetch deviating from the rejected transport design, deferred PR #128 shadow findings); recorded open — no disposition claimed. |

## 7. Live-run findings tracked to repair

Ordinary-use failures discovered by the live runs and their repair
status on the candidate:

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

Repairs in flight at record time: PR #427 (sibling-PR conflict
detection, #323) and PR #431 (`artifact.json` collision, #419) are
open against the candidate; the candidate is provisional until they
resolve.

Recorded observations not yet filed as issues (kept visible, not
silently dropped):

- The reference shim's fixed argv lacks `--verbose`, which the
  exercised runtime version requires for stream-json mode — currently
  carried by the declared `--runtime-arg` knob (noted in #420's
  acceptance criteria as a separate small follow-up).
- `CapabilityReport` is issued on the unscoped pass only; a
  `run --work`-scoped loop deadlocks publication — clarify intended
  scoping (run 1 deferred observation).
- macOS cannot host containment natively (no bubblewrap); the tracked
  answer is the minimal on-demand Linux guest (#373 design).

## 8. Issue-state ledger — open issues already implemented

The milestone's open-issue count overstates remaining work: several
open issues are already implemented by merged PRs whose footers did
not carry `Closes`, and two release tasks delivered their documents
while their owner decisions remain open. States verified 2026-11-25:

| Issue | State on the candidate | Implementing merge | Residual |
| --- | --- | --- | --- |
| #263 — connect produced commits to handoff/verification | Implemented | PR #348 (`d7f5a9f`) | None found; recommend owner close |
| #264 — real invariant/code-review assessments | Implemented | PR #349 (`b93b72d`) | recommend close |
| #266 — production host effects/reconciliation | Implemented | PR #351 (`9642c2a`) | recommend close |
| #269 — clean-install + real-Worker acceptance docs | Implemented | PR #356 (`9a79664`) | reproduction tracked by #367 — pending owner run |
| #410 / #411 / #412 — live-run finding fixes | Implemented | PR #413 (`bfa0412`) | verified live in runs 2–3 (§7); recommend close |
| #366 — pluggable public Selector/Worker references | Closed | PR #404 (`f883d6e`) | done |
| #368 — public-content/history/metadata audit | Audit delivered | PR #432 (`b0b81f6`) | `release-audit.md` §5 blockers stay gated on the publication-route decision — an open owner choice, not picked here |
| #369 — license selection prep | Prep delivered | PR #433 (`aff8721`) | license selection, `LICENSE`, and package metadata pending owner choice |
| #270 — manual real-Worker acceptance | Partial | runs 1–3 retained owner-side | sanitized public evidence record, explicit authorization record, restart-clean store-recovery variant — pending owner run |
| #271 — scheduled pilot acceptance | Not started | — | pending owner run (§5) |
| #367 — clean-Linux-install reproduction | Not started | — | pending owner run; reuses #269 runbook and #270/#271 evidence where it qualifies |
| #370 / #371 — candidate freeze; go/no-go | Not started | — | gated on this ledger, the route decision, and the license selection |
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
- **Scheduled intake unexercised** (§5): the v0.2 scheduler path is
  fixture/contract-tested but has no live run on the candidate; #271
  remains open.
- **Run 1 publish leg used operator workarounds** (§3): the clean
  publish path is proven by run 3 on post-#421 code, not by run 1's
  H1.
- **One recovered-store attempt was internally inconsistent** (§4):
  restart/recovery evidence is partial.
- **Defect dispositions refreshed** (§6): the defects previously
  assessed release-blocking and deferred — #344 via #428, #364 via
  #425, #363 via #426 — are fixed on this candidate; no issue named
  by #365 carries a release-blocking disposition now. New flags for
  owner review: #398/#399 (credential masking — the same class #364
  gated on), #313 (ambiguous-match convergence wedge), and #400
  (v2 report-retention regression). The `release-audit.md` §5
  exposure blockers remain gated on the publication-route decision,
  which stays an open owner choice.
