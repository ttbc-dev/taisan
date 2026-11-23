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

## 1. Candidate commit

```text
git log -1 origin/main
ca45ac8fbf5647df7dbdcac59185b8442c9e4890
Merge pull request #417 from shimpeiws/issue-416-decide-handoff-manifest-custody
```

The candidate is `origin/main` HEAD at record time (2026-11-23). Its
history contains every change the acceptance evidence depends on:

| Content | Merge | What it carries |
| --- | --- | --- |
| Live-run finding fixes | `bfa0412` (PR #413) | `artifact.json` manifest from the worker shim (#410), git-push transport for remote ref writes (#411), declared CA trust store and commit identity (#412) |
| Publish-leg fix | `4464c4f` (PR #421) | git-transport push authenticated as `Basic x-access-token` (#420) |
| Design docs | `9e01af1` (PR #414, #372), `f410092` (PR #415, #373), `ca45ac8` (PR #417, #416) | remote executor boundary, reference environment selection, handoff manifest custody |

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
one attempt was inconsistent).

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
not treated as a confirmed defect.

| Issue | Disposition | Reason (verified on `ca45ac8`) |
| --- | --- | --- |
| #344 — absence proof can stall or conclude falsely | **Release-blocking** | Partially fixed by PR #406 (kernel-thread exclusion by structural property, `/proc` fallback, root-run test panic fixed). Still open: (a) a persistently unmappable non-kernel cwd keeps a launch `uncertain` forever — no bounded escalation to a distinct "absence unprovable" outcome (`launch_dir_clear` fails closed into `report_unidentified_unknown` each pass, `supervisor.rs:3926-3949`); (b) Finding 2's third check is absent — the absence proof still verifies only signature matches plus directory occupancy (`resolve_unidentified_launch`, `supervisor.rs:3871-3913`); the recorded-leader process-group membership check exists only in `proof_identities` for stop proofs (`supervisor.rs:4019-4041`). A chdir + argv-change escapee keeping the group can still be abandoned as "absent", enabling a second writer on resume — this defeats the no-duplicate-writer guarantee the release claims. |
| #347 — canary reachable under bound system mounts | **Fixed** | Closed 2026-11-21. Verified: `invisible_canary` derives probe-canary placement from the rendered mount plan (skips candidates beneath declared binds, falls back to host temp dir, then `/sys/kernel`), and `plan_denial` refuses plans whose binds cover the sandbox surface (`containment.rs:362-397`). |
| #358 — over-budget request classified as retryable Fault | **Fixed** (issue open; recommend close) | `build_request_body` now maps spec-reachable validation failures (e.g. `verification_argv` over `max_verification_commands`) to `DispatchRefusal::RequestInvalid`, a typed refusal the consume path records against the request instead of a pass-aborting `ExecError::Fault` (`request_v2.rs:1018-1025`, `supervisor.rs:2700-2708`). Only builder-impossible failures stay faults, matching the launch layer's `BodyCorrupt` classification. |
| #359 — re-retained reclaimed body never resolvable | **Fixed core; explicitly deferred residual** | `SqliteStore::register_body` restores a non-`present` row to `present` on identical-metadata replay (`retention.rs:3116-3118`, commit `19a91e5`). Deferred edges (issue stays open for them): a non-`present` row aliased under a different path still hits `ConstraintConflict` → `Fault` in `finish_invocation`; a `present` row is trusted without a file re-stat (resolves later, honestly, as `missing-file`/`Corrupt`). Narrow triggers, honest downstream failure — recorded, not release-blocking. |
| #361 — 13 unresolved review threads from PR #343 | **Fixed as filed** (recommend close) | All 13 unresolved non-outdated threads verified fixed on the candidate: bodies retain/reclaim revival (#1, #8) via `restore_body_presence`; catalogue-path root unified to the state root (#3, commit `ed4a576`); production composition root installs `V2Dispatch` (#2, `main.rs:2126-2149`); `/` toolchain/cache and provider-config mounts refused by `plan_denial` (#4, #5, `containment.rs:362-378`); v2 out-directory evidence collected in `finish_invocation` via `report_v2::collect` (#6 and the duplicate thread, `supervisor.rs:4162-4178`); `workspace_read_only` capability emitted for check/review roles (#7, `request_v2.rs:959-965`); probe-canary temp dir reclaimed (#9, commit `19a91e5`); provider-config digest covers member content and `ensure` re-proves on edit (#10, `boundary.rs:155-170,305-397`); contract-harness profile refused in production installs (`workloop-config` lib.rs:690-696); declared verification egress without a sandbox refused (`handoff.rs:949-962`). The `isOutdated` threads were superseded by follow-up commits on/after PR #343 (spot-verified: read-only `/workspace` for check/review roles, `supervisor.rs:2550-2555`; char-boundary-safe probe detail). |
| #363 — spent-deadline check runs after paid work | **Explicitly deferred** | Confirmed present: `verify_with_timeout` still runs `prepare_enforcement` (digest of toolchain/caches, `handoff.rs:1391`) and `git worktree prune`/`remove_dir_all` (`handoff.rs:1439-1440`) before the `timeout.is_zero()` short-circuit (`handoff.rs:1444`). Consequence is bounded wasted cost on a spent deadline; the recorded `Inconclusive` evidence is honest (the `environment_digest` reflects enforcement that did run — no fabricated success). The early-return + dedicated marker-digest fix remains tracked by the open issue. |
| #364 — staged runtime-auth never cleaned up | **Release-blocking** | Confirmed present: `resolve()` stages `<runtime_dir>/runtime-auth` before `validate_provider_config`/proxy start and returns `Err` with the file left behind (`boundary.rs:190-223`); `Drop` shuts down only the proxy (`boundary.rs:291-300`); no `remove_file`/`unstage` exists for the staged path. Credential material persists at a predictable path past refused resolutions, re-resolves, and process lifetime — contradicting the boundary's own contract that the secret exists in the staged file only while the grant needs it. Residual risk is bounded (0600, owner-only, mounted only for grants carrying `credential_file`) but this is a credential-hygiene defect in the boundary the release advertises; the owner may re-disposition to deferred. |
| #422 — preflight does not probe `gh` version/features | **Explicitly deferred** | Environment-provisioning gap surfaced by run 3, not a code defect in the candidate: the guest's apt-default `gh` 2.45.0 lacks `gh api --slurp`, collapsing adapter list calls into opaque `read_indeterminate` skips; upgrading to `gh` 2.105.0 unblocked the run. Documented environment requirement and a design-TBD follow-up (preflight version probe and/or fail-loud transport errors); not release-blocking. |
| #420 — git push `Bearer` auth 401 for `gho_` tokens | **Fixed and verified live** | Closed via PR #421 (merged `4464c4f`): push authenticates as `Basic x-access-token:<token>` and unencodable credentials refuse before spawn. Run 3 verified live: `push_ref` applied, `ref_published` recorded, PR #64 opened, where the same credential under `Bearer` had deterministically 401'd in run 2. |
| Run-1 review finding — `greet(name)` with `name=None` raised `AttributeError` | **Fixed** (verified via the loop's own fix path) | The operator-posted inline finding on the isolated repository's PR #62 entered as a real review occurrence, was judged `fix`, and the bounded Fix Invocation produced `5c5cc6e`; republished head `35ce3cb` passed verification/review and `assess_completion` recorded `succeeded`. A defect in the produced change — not a taisan defect — dispositioned by the workflow itself; it is also the §3 real-review-finding → Fix → H2 → reassessment evidence, so it needs no separate repair issue. |

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

## 8. Verification baseline on the candidate

This change adds documentation only; no code, test, or configuration
changed since the last green baseline. The candidate nevertheless had
the full baseline rerun as #365 requires (build before test — contract
tests spawn `target/debug/workloop-fixture-worker`):

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Passed — 1594 tests, 0 failures (fixture-selector/fixture-worker contract tests ran against `target/debug/` binaries produced by the build gate) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |

Scope honesty: the earlier green runs on merged PRs cover the code
they tested; they do not cover this document's claims — the
capability evidence above rests on the retained owner-run records,
not on `cargo test`. Conversely the cargo baseline exercises fixture
and contract paths only; it is not evidence for any live-run claim in
this document.

## 9. Known limitations

- **Single Linux host**: containment requires bubblewrap namespaces;
  the reference deployment is one Linux environment (the runs used a
  single OrbStack guest on one macOS machine). No multi-host or
  distributed execution is claimed.
- **No remote execution**: the remote executor boundary is designed
  (#372/#373 docs) but not implemented for this release; all live
  evidence is local-host.
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
- **Release-blocking dispositions open** (§6): #344 and #364 are
  assessed as gating defects; #365's checklist row for defect
  disposition records them rather than closing them.
